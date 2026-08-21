use std::collections::HashMap;

use tokio::sync::{mpsc, oneshot, OwnedMutexGuard};
use tokio_postgres::{IsolationLevel as PgIsolation, Portal, Row, Transaction as PgTxn};

use datagrep_api::driver::IsolationLevel;
use datagrep_api::error::DbError;
use datagrep_api::shape::{FieldDef, FieldFlags, Identity, RowSchema};
use datagrep_api::value::Value;

use crate::error::map_pg_error;
use crate::pool::PgSession;
use crate::value::{logical_type_of, DecodedCell, PgParam};

pub enum ExecOutcome {
    Ack { affected: u64 },
    Cursor { portal_id: u64, schema: RowSchema },
}

pub enum ActorCmd {
    Execute {
        text: String,
        params: Vec<Value>,
        reply: oneshot::Sender<Result<ExecOutcome, DbError>>,
    },
    FetchBatch {
        portal_id: u64,
        max_rows: i32,
        reply: oneshot::Sender<Result<Vec<Row>, DbError>>,
    },
    CloseCursor {
        portal_id: u64,
    },
    Commit {
        reply: oneshot::Sender<Result<(), DbError>>,
    },
    Rollback {
        reply: oneshot::Sender<Result<(), DbError>>,
    },
}

fn to_pg_isolation(level: IsolationLevel) -> PgIsolation {
    match level {
        IsolationLevel::ReadUncommitted => PgIsolation::ReadUncommitted,
        IsolationLevel::ReadCommitted => PgIsolation::ReadCommitted,
        IsolationLevel::RepeatableRead => PgIsolation::RepeatableRead,
        IsolationLevel::Serializable => PgIsolation::Serializable,
    }
}

pub fn spawn(
    mut guard: OwnedMutexGuard<PgSession>,
    read_only: bool,
    isolation: Option<IsolationLevel>,
) -> mpsc::Sender<ActorCmd> {
    let (tx, rx) = mpsc::channel(4);
    tokio::spawn(async move {
        let client = match guard.client_mut() {
            Some(c) => c,
            None => {
                run_broken(rx, DbError::Closed).await;
                return;
            }
        };
        let mut builder = client.build_transaction().read_only(read_only);
        if let Some(level) = isolation {
            builder = builder.isolation_level(to_pg_isolation(level));
        }
        let txn = match builder.start().await {
            Ok(t) => t,
            Err(e) => {
                run_broken(rx, map_pg_error(e)).await;
                return;
            }
        };
        run(txn, rx).await;
    });
    tx
}

async fn run_broken(mut rx: mpsc::Receiver<ActorCmd>, err: DbError) {
    while let Some(cmd) = rx.recv().await {
        reply_error(cmd, clone_err(&err));
    }
}

fn clone_err(e: &DbError) -> DbError {
    DbError::Protocol(e.to_string())
}

fn reply_error(cmd: ActorCmd, err: DbError) {
    match cmd {
        ActorCmd::Execute { reply, .. } => {
            let _ = reply.send(Err(err));
        }
        ActorCmd::FetchBatch { reply, .. } => {
            let _ = reply.send(Err(err));
        }
        ActorCmd::CloseCursor { .. } => {}
        ActorCmd::Commit { reply } => {
            let _ = reply.send(Err(err));
        }
        ActorCmd::Rollback { reply } => {
            let _ = reply.send(Err(err));
        }
    }
}

async fn run(txn: PgTxn<'_>, mut rx: mpsc::Receiver<ActorCmd>) {
    let mut portals: HashMap<u64, Portal> = HashMap::new();
    let mut next_id: u64 = 0;

    loop {
        let cmd = match rx.recv().await {
            Some(c) => c,
            None => {
                let _ = txn.rollback().await;
                return;
            }
        };
        match cmd {
            ActorCmd::Execute {
                text,
                params,
                reply,
            } => {
                let outcome = execute_one(&txn, &text, &params, &mut portals, &mut next_id).await;
                let _ = reply.send(outcome);
            }
            ActorCmd::FetchBatch {
                portal_id,
                max_rows,
                reply,
            } => {
                let result = match portals.get(&portal_id) {
                    Some(portal) => txn
                        .query_portal(portal, max_rows)
                        .await
                        .map_err(map_pg_error),
                    None => Err(DbError::Closed),
                };
                let _ = reply.send(result);
            }
            ActorCmd::CloseCursor { portal_id } => {
                portals.remove(&portal_id);
            }
            ActorCmd::Commit { reply } => {
                let result = txn.commit().await.map_err(map_pg_error);
                let _ = reply.send(result);
                return;
            }
            ActorCmd::Rollback { reply } => {
                let result = txn.rollback().await.map_err(map_pg_error);
                let _ = reply.send(result);
                return;
            }
        }
    }
}

async fn execute_one(
    txn: &PgTxn<'_>,
    text: &str,
    params: &[Value],
    portals: &mut HashMap<u64, Portal>,
    next_id: &mut u64,
) -> Result<ExecOutcome, DbError> {
    let stmt = txn.prepare(text).await.map_err(map_pg_error)?;
    let bound: Vec<PgParam<'_>> = params.iter().map(PgParam).collect();
    let refs: Vec<&(dyn tokio_postgres::types::ToSql + Sync)> = bound
        .iter()
        .map(|p| p as &(dyn tokio_postgres::types::ToSql + Sync))
        .collect();

    if stmt.columns().is_empty() {
        let affected = txn.execute(&stmt, &refs).await.map_err(map_pg_error)?;
        return Ok(ExecOutcome::Ack { affected });
    }

    let identity = detect_identity(txn, &stmt).await;
    let fields = stmt
        .columns()
        .iter()
        .map(|c| FieldDef {
            name: std::sync::Arc::from(c.name()),
            logical: logical_type_of(c.type_()),
            flags: FieldFlags::empty(),
            native_type: Some(std::sync::Arc::from(c.type_().name())),
        })
        .collect();
    let schema = RowSchema { fields, identity };

    let portal = txn.bind(&stmt, &refs).await.map_err(map_pg_error)?;
    let id = *next_id;
    *next_id += 1;
    portals.insert(id, portal);
    Ok(ExecOutcome::Cursor {
        portal_id: id,
        schema,
    })
}

async fn detect_identity(txn: &PgTxn<'_>, stmt: &tokio_postgres::Statement) -> Option<Identity> {
    let cols = stmt.columns();
    if cols.is_empty() {
        return None;
    }
    let table_oid = cols[0].table_oid()?;
    if !cols.iter().all(|c| c.table_oid() == Some(table_oid)) {
        return None;
    }
    let rows = txn
        .query(
            "SELECT a.attnum FROM pg_index i \
             JOIN pg_attribute a ON a.attrelid = i.indrelid AND a.attnum = ANY(i.indkey) \
             WHERE i.indrelid = $1 AND i.indisprimary",
            &[&table_oid],
        )
        .await
        .ok()?;
    if rows.is_empty() {
        return None;
    }
    let pk_attnums: Vec<i16> = rows.iter().map(|r| r.get::<_, i16>(0)).collect();

    let mut field_indices = Vec::with_capacity(pk_attnums.len());
    for attnum in &pk_attnums {
        let idx = cols.iter().position(|c| c.column_id() == Some(*attnum))?;
        field_indices.push(idx as u32);
    }
    Some(Identity { field_indices })
}

pub fn decode_rows(rows: Vec<Row>) -> Vec<Vec<Value>> {
    rows.into_iter()
        .map(|row| {
            (0..row.len())
                .map(|i| {
                    row.try_get::<_, DecodedCell>(i)
                        .map(|c| c.0)
                        .unwrap_or(Value::Null)
                })
                .collect()
        })
        .collect()
}
