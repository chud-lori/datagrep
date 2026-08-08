//! The per-transaction actor task — the answer to a real constraint in this
//! crate: `tokio_postgres::Transaction<'a>` borrows `&'a mut Client`, but
//! `Connection::execute` must return a `'static` `Box<dyn Cursor>` (design
//! §3.1). Rather than fight that with unsafe self-referential structs, the
//! `Transaction` (and any portals bound within it) live entirely on this
//! task's stack; everything crossing back out to the rest of the driver is
//! an owned, `'static` channel handle.
//!
//! One actor = one Postgres `Transaction`. It is used both for the
//! transparent read-only wrapper `PgConnection::execute` opens around a
//! streaming SELECT (design ticket note: "tokio-postgres portals require a
//! transaction — wrap read queries in a transparent read-only transaction"),
//! and for an explicit interactive [`crate::transaction::PgTransaction`]
//! opened via `begin()`. Dropping every `Sender<ActorCmd>` clone (all
//! cursors plus the owning `PgConnection`/`PgTransaction` handle) is treated
//! as an implicit rollback — a safe default for "the caller went away".
//!
//! While it runs, the actor **pins** one physical session out of
//! [`crate::pool::PgPool`] (design §3.5: moving an open transaction to another
//! socket would be a correctness bug). It does *not* pin the whole logical
//! connection: anything else — catalog browsing, the next `execute()` — takes
//! a different session from the pool. See the `pool.rs` module docs for why
//! that indirection exists.

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

/// What top-level `execute()` got back for one statement.
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

/// Spawn the actor and return the command channel. `read_only`/`isolation`
/// set the `START TRANSACTION` this actor opens immediately. The session's
/// `Client` is `Option`-wrapped because [`crate::pool::PgPool::close`] takes
/// it out of its slot to make every later operation observably `Closed`.
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
                // Nobody has a reply channel yet except the first `Execute`
                // caller (who hasn't sent anything); stash the error and hand
                // it to whatever command arrives first, then exit.
                run_broken(rx, map_pg_error(e)).await;
                return;
            }
        };
        run(txn, rx).await;
        // `guard` (and with it, this one pooled session) is held for the
        // actor's whole lifetime — see the module doc: deliberate session
        // pinning, not an oversight. It is released here, when the actor
        // returns; cursors ask for that explicitly via `ActorCmd::Rollback`
        // as soon as their portal is drained, so an idle-but-alive cursor
        // handle no longer holds a socket hostage.
    });
    tx
}

/// Drain the command queue replying `Err` to everything — used when the
/// opening `START TRANSACTION` itself failed.
async fn run_broken(mut rx: mpsc::Receiver<ActorCmd>, err: DbError) {
    while let Some(cmd) = rx.recv().await {
        reply_error(cmd, clone_err(&err));
    }
}

fn clone_err(e: &DbError) -> DbError {
    // DbError is not Clone (thiserror enum wrapping String); rebuild a
    // Protocol-flavored equivalent so every waiting caller still gets told.
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
                // All senders dropped without an explicit commit/rollback —
                // roll back as the safe default (see module doc).
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
            // Postgres's RowDescription (what `Statement::columns()` is built
            // from) does not report nullability, so we never assert
            // `NULLABLE` here rather than guess — "nullable unknown" reads as
            // an unset flag rather than a wrong claim either way.
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

/// Best-effort primary key resolution for `RowSchema::identity` (design:
/// "no PK ⇒ EDITABLE_RESULTS is false" — we simply leave `identity: None`
/// whenever this can't cheaply and unambiguously determine one).
///
/// Only handles the common single-table case: every returned column must
/// report the same `table_oid`, and every primary-key column of that table
/// must be among the selected columns. Joins, expressions, and `DISTINCT`
/// queries correctly fall back to `None`.
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

/// Decode a full `Vec<Row>` into datagrep-api rows, in column order.
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
