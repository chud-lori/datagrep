use std::time::Duration;

use mysql_async::consts::ColumnFlags;
use mysql_async::prelude::Queryable;
use mysql_async::{Column, Conn, Params, Row};
use tokio::sync::{mpsc, oneshot, OwnedMutexGuard};

use datagrep_api::driver::IsolationLevel;
use datagrep_api::error::DbError;
use datagrep_api::shape::{Identity, RowSchema};
use datagrep_api::value::Value as ApiValue;

use crate::error::map_mysql_error;
use crate::sql::Flavor;
use crate::value::{decode_value, field_def_of, to_my_value};

pub enum ExecOutcome {
    Ack {
        affected: u64,
        message: Option<String>,
        warnings: u16,
    },
    Cursor {
        cursor_id: u64,
        schema: RowSchema,
    },
}

pub struct Fetched {
    pub rows: Vec<Vec<ApiValue>>,
    pub done: bool,
    pub warnings: u16,
}

pub enum ActorCmd {
    Execute {
        statements: Vec<String>,
        params: Vec<ApiValue>,
        timeout: Option<Duration>,
        row_limit: Option<u64>,
        reply: oneshot::Sender<Result<ExecOutcome, DbError>>,
    },
    FetchBatch {
        cursor_id: u64,
        max_rows: u32,
        reply: oneshot::Sender<Result<Fetched, DbError>>,
    },
    CloseCursor {
        cursor_id: u64,
    },
    Commit {
        reply: oneshot::Sender<Result<(), DbError>>,
    },
    Rollback {
        reply: oneshot::Sender<Result<(), DbError>>,
    },
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    Simple,
    Transaction,
}

pub fn spawn_simple(
    guard: OwnedMutexGuard<Option<Conn>>,
    flavor: Flavor,
) -> mpsc::Sender<ActorCmd> {
    spawn(guard, flavor, Mode::Simple, None, false)
}

pub fn spawn_transaction(
    guard: OwnedMutexGuard<Option<Conn>>,
    flavor: Flavor,
    isolation: Option<IsolationLevel>,
    read_only: bool,
) -> mpsc::Sender<ActorCmd> {
    spawn(guard, flavor, Mode::Transaction, isolation, read_only)
}

fn spawn(
    mut guard: OwnedMutexGuard<Option<Conn>>,
    flavor: Flavor,
    mode: Mode,
    isolation: Option<IsolationLevel>,
    read_only: bool,
) -> mpsc::Sender<ActorCmd> {
    let (tx, mut rx) = mpsc::channel(4);
    tokio::spawn(async move {
        let Some(conn) = guard.as_mut() else {
            drain_replying_closed(&mut rx).await;
            return;
        };

        if mode == Mode::Transaction {
            if let Err(e) = open_transaction(conn, isolation, read_only).await {
                let recoverable = e.is_recoverable();
                drain_replying(&mut rx, &e).await;
                if !recoverable {
                    guard.take(); // poison: evict the Conn entirely
                }
                return;
            }
        }

        let poisoned = run(conn, &mut rx, flavor, mode).await;
        if poisoned {
            guard.take();
        }
        // Dropping `guard` here releases the connection to the next caller.
    });
    tx
}

async fn open_transaction(
    conn: &mut Conn,
    isolation: Option<IsolationLevel>,
    read_only: bool,
) -> Result<(), DbError> {
    if let Some(level) = isolation {
        let sql = match level {
            IsolationLevel::ReadUncommitted => "SET TRANSACTION ISOLATION LEVEL READ UNCOMMITTED",
            IsolationLevel::ReadCommitted => "SET TRANSACTION ISOLATION LEVEL READ COMMITTED",
            IsolationLevel::RepeatableRead => "SET TRANSACTION ISOLATION LEVEL REPEATABLE READ",
            IsolationLevel::Serializable => "SET TRANSACTION ISOLATION LEVEL SERIALIZABLE",
        };
        conn.query_drop(sql).await.map_err(map_mysql_error)?;
    }
    let start = if read_only {
        "START TRANSACTION READ ONLY"
    } else {
        "START TRANSACTION"
    };
    conn.query_drop(start).await.map_err(map_mysql_error)
}

async fn drain_replying_closed(rx: &mut mpsc::Receiver<ActorCmd>) {
    while let Some(cmd) = rx.recv().await {
        reply_error(cmd, DbError::Closed);
    }
}

async fn drain_replying(rx: &mut mpsc::Receiver<ActorCmd>, err: &DbError) {
    while let Some(cmd) = rx.recv().await {
        reply_error(cmd, clone_err(err));
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
        ActorCmd::Commit { reply } | ActorCmd::Rollback { reply } => {
            let _ = reply.send(Err(err));
        }
    }
}

async fn run(
    conn: &mut Conn,
    rx: &mut mpsc::Receiver<ActorCmd>,
    flavor: Flavor,
    mode: Mode,
) -> bool {
    let mut next_cursor_id: u64 = 0;
    let mut deferred: Option<ActorCmd> = None;
    let mut committed_or_rolled_back = false;

    'outer: loop {
        let cmd = match deferred.take() {
            Some(c) => c,
            None => match rx.recv().await {
                Some(c) => c,
                None => break 'outer,
            },
        };
        match cmd {
            ActorCmd::Execute {
                statements,
                params,
                timeout,
                row_limit,
                reply,
            } => {
                let outcome = handle_execute(
                    conn,
                    rx,
                    flavor,
                    statements,
                    params,
                    timeout,
                    row_limit,
                    reply,
                    &mut next_cursor_id,
                )
                .await;
                match outcome {
                    ExecuteEnd::Continue => {
                        if mode == Mode::Simple {
                            break 'outer;
                        }
                    }
                    ExecuteEnd::Deferred(cmd) => deferred = Some(cmd),
                    ExecuteEnd::Poisoned => return true,
                }
            }
            ActorCmd::FetchBatch { reply, .. } => {
                let _ = reply.send(Ok(Fetched {
                    rows: Vec::new(),
                    done: true,
                    warnings: 0,
                }));
            }
            ActorCmd::CloseCursor { .. } => {}
            ActorCmd::Commit { reply } => {
                let result = run_tx_end(conn, mode, "COMMIT").await;
                let poison = matches!(&result, Err(e) if !e.is_recoverable());
                let _ = reply.send(result);
                if poison {
                    return true;
                }
                committed_or_rolled_back = true;
                break 'outer;
            }
            ActorCmd::Rollback { reply } => {
                let result = run_tx_end(conn, mode, "ROLLBACK").await;
                let poison = matches!(&result, Err(e) if !e.is_recoverable());
                let _ = reply.send(result);
                if poison {
                    return true;
                }
                committed_or_rolled_back = true;
                break 'outer;
            }
        }
    }

    if mode == Mode::Transaction && !committed_or_rolled_back {
        if let Err(e) = conn.query_drop("ROLLBACK").await {
            tracing::warn!(error = %e, "implicit rollback failed");
            return !map_mysql_error(e).is_recoverable();
        }
    }
    false
}

async fn run_tx_end(conn: &mut Conn, mode: Mode, sql: &str) -> Result<(), DbError> {
    if mode != Mode::Transaction {
        return Err(DbError::Unsupported {
            feature: format!("{sql} outside an explicit transaction"),
        });
    }
    conn.query_drop(sql).await.map_err(map_mysql_error)
}

enum ExecuteEnd {
    Continue,
    Deferred(ActorCmd),
    Poisoned,
}

#[allow(clippy::too_many_arguments)]
async fn handle_execute(
    conn: &mut Conn,
    rx: &mut mpsc::Receiver<ActorCmd>,
    flavor: Flavor,
    statements: Vec<String>,
    params: Vec<ApiValue>,
    timeout: Option<Duration>,
    row_limit: Option<u64>,
    reply: oneshot::Sender<Result<ExecOutcome, DbError>>,
    next_cursor_id: &mut u64,
) -> ExecuteEnd {
    let Some((last, preceding)) = statements.split_last() else {
        let _ = reply.send(Err(DbError::Query {
            code: None,
            message: "statement contains no executable SQL".into(),
            position: None,
        }));
        return ExecuteEnd::Continue;
    };

    let timeout_was_set = match timeout {
        Some(t) => match set_server_deadline(conn, flavor, t).await {
            Ok(()) => true,
            Err(e) => {
                let poisoned = !e.is_recoverable();
                let _ = reply.send(Err(e));
                return if poisoned {
                    ExecuteEnd::Poisoned
                } else {
                    ExecuteEnd::Continue
                };
            }
        },
        None => false,
    };

    let end = handle_script(
        conn,
        rx,
        preceding,
        last,
        params,
        row_limit,
        reply,
        next_cursor_id,
    )
    .await;

    if timeout_was_set && !matches!(end, ExecuteEnd::Poisoned) {
        if let Err(e) = reset_server_deadline(conn, flavor).await {
            tracing::warn!(error = %e, "failed to reset server-side statement deadline");
            if !e.is_recoverable() {
                return ExecuteEnd::Poisoned;
            }
        }
    }
    end
}

async fn set_server_deadline(conn: &mut Conn, flavor: Flavor, t: Duration) -> Result<(), DbError> {
    // The formatted value is a number we computed, never user text.
    let sql = match flavor {
        Flavor::MySql => format!("SET SESSION max_execution_time = {}", t.as_millis().max(1)),
        Flavor::MariaDb => format!(
            "SET SESSION max_statement_time = {:.3}",
            t.as_secs_f64().max(0.001)
        ),
    };
    conn.query_drop(sql).await.map_err(map_mysql_error)
}

async fn reset_server_deadline(conn: &mut Conn, flavor: Flavor) -> Result<(), DbError> {
    let sql = match flavor {
        Flavor::MySql => "SET SESSION max_execution_time = DEFAULT",
        Flavor::MariaDb => "SET SESSION max_statement_time = DEFAULT",
    };
    conn.query_drop(sql).await.map_err(map_mysql_error)
}

#[allow(clippy::too_many_arguments)]
async fn handle_script(
    conn: &mut Conn,
    rx: &mut mpsc::Receiver<ActorCmd>,
    preceding: &[String],
    last: &str,
    params: Vec<ApiValue>,
    row_limit: Option<u64>,
    reply: oneshot::Sender<Result<ExecOutcome, DbError>>,
    next_cursor_id: &mut u64,
) -> ExecuteEnd {
    for stmt in preceding {
        match run_and_drain(conn, stmt).await {
            Ok(()) => {}
            Err(e) => {
                let poisoned = !e.is_recoverable();
                let _ = reply.send(Err(e));
                return if poisoned {
                    ExecuteEnd::Poisoned
                } else {
                    ExecuteEnd::Continue
                };
            }
        }
    }

    if params.is_empty() {
        match conn.query_iter(last).await {
            Ok(qr) => finish_result(qr, rx, row_limit, reply, next_cursor_id).await,
            Err(e) => exec_error(map_mysql_error(e), reply),
        }
    } else {
        let mut bound = Vec::with_capacity(params.len());
        for p in &params {
            match to_my_value(p) {
                Ok(v) => bound.push(v),
                Err(e) => {
                    let _ = reply.send(Err(e));
                    return ExecuteEnd::Continue;
                }
            }
        }
        match conn.exec_iter(last, Params::Positional(bound)).await {
            Ok(qr) => finish_result(qr, rx, row_limit, reply, next_cursor_id).await,
            Err(e) => exec_error(map_mysql_error(e), reply),
        }
    }
}

fn exec_error(e: DbError, reply: oneshot::Sender<Result<ExecOutcome, DbError>>) -> ExecuteEnd {
    let poisoned = !e.is_recoverable();
    let _ = reply.send(Err(e));
    if poisoned {
        ExecuteEnd::Poisoned
    } else {
        ExecuteEnd::Continue
    }
}

async fn finish_result<P: mysql_async::prelude::Protocol>(
    mut qr: mysql_async::QueryResult<'_, '_, P>,
    rx: &mut mpsc::Receiver<ActorCmd>,
    row_limit: Option<u64>,
    reply: oneshot::Sender<Result<ExecOutcome, DbError>>,
    next_cursor_id: &mut u64,
) -> ExecuteEnd {
    let Some(columns) = qr.columns() else {
        // No pending result at all (e.g. an empty OK): a plain Ack.
        let outcome = ExecOutcome::Ack {
            affected: qr.affected_rows(),
            message: non_empty(qr.info().into_owned()),
            warnings: qr.warnings(),
        };
        let _ = reply.send(Ok(outcome));
        return ExecuteEnd::Continue;
    };

    if columns.is_empty() {
        // A statement that can never produce rows (UPDATE/DDL/…).
        let affected = qr.affected_rows();
        let message = non_empty(qr.info().into_owned());
        let warnings = qr.warnings();
        if let Err(e) = qr.drop_result().await {
            let e = map_mysql_error(e);
            let poisoned = !e.is_recoverable();
            let _ = reply.send(Err(e));
            return if poisoned {
                ExecuteEnd::Poisoned
            } else {
                ExecuteEnd::Continue
            };
        }
        let _ = reply.send(Ok(ExecOutcome::Ack {
            affected,
            message,
            warnings,
        }));
        return ExecuteEnd::Continue;
    }

    // Row-producing: hand out a cursor id and stream.
    let cursor_id = *next_cursor_id;
    *next_cursor_id += 1;
    let schema = schema_of(&columns);
    let _ = reply.send(Ok(ExecOutcome::Cursor { cursor_id, schema }));

    stream_result(&mut qr, rx, cursor_id, &columns, row_limit).await
}

async fn stream_result<P: mysql_async::prelude::Protocol>(
    qr: &mut mysql_async::QueryResult<'_, '_, P>,
    rx: &mut mpsc::Receiver<ActorCmd>,
    cursor_id: u64,
    columns: &[Column],
    row_limit: Option<u64>,
) -> ExecuteEnd {
    let mut sent_rows: u64 = 0;
    loop {
        let cmd = match rx.recv().await {
            Some(c) => c,
            None => {
                return match drain(qr).await {
                    Ok(()) => ExecuteEnd::Continue,
                    Err(_) => ExecuteEnd::Poisoned,
                };
            }
        };
        match cmd {
            ActorCmd::FetchBatch {
                cursor_id: id,
                max_rows,
                reply,
            } if id == cursor_id => {
                let mut want = max_rows.max(1) as u64;
                if let Some(limit) = row_limit {
                    want = want.min(limit.saturating_sub(sent_rows));
                }
                let mut rows: Vec<Vec<ApiValue>> = Vec::with_capacity(want as usize);
                let mut done = false;
                let mut fetch_err: Option<DbError> = None;
                while (rows.len() as u64) < want {
                    match qr.next().await {
                        Ok(Some(row)) => rows.push(decode_row(row, columns)),
                        Ok(None) => {
                            done = true;
                            break;
                        }
                        Err(e) => {
                            fetch_err = Some(map_mysql_error(e));
                            break;
                        }
                    }
                }
                if let Some(e) = fetch_err {
                    let poisoned = !e.is_recoverable();
                    let drain_failed = drain(qr).await.is_err();
                    let _ = reply.send(Err(e));
                    return if poisoned || drain_failed {
                        ExecuteEnd::Poisoned
                    } else {
                        ExecuteEnd::Continue
                    };
                }
                sent_rows += rows.len() as u64;
                if !done && row_limit.is_some_and(|limit| sent_rows >= limit) {
                    done = true;
                }
                let warnings = if done { qr.warnings() } else { 0 };
                let _ = reply.send(Ok(Fetched {
                    rows,
                    done,
                    warnings,
                }));
                if done {
                    return match drain(qr).await {
                        Ok(()) => ExecuteEnd::Continue,
                        Err(_) => ExecuteEnd::Poisoned,
                    };
                }
            }
            ActorCmd::FetchBatch { reply, .. } => {
                // A stale cursor's fetch: that set is gone.
                let _ = reply.send(Ok(Fetched {
                    rows: Vec::new(),
                    done: true,
                    warnings: 0,
                }));
            }
            ActorCmd::CloseCursor { cursor_id: id } if id == cursor_id => {
                return match drain(qr).await {
                    Ok(()) => ExecuteEnd::Continue,
                    Err(_) => ExecuteEnd::Poisoned,
                };
            }
            ActorCmd::CloseCursor { .. } => {}
            other => {
                return match drain(qr).await {
                    Ok(()) => ExecuteEnd::Deferred(other),
                    Err(_) => {
                        reply_error(other, DbError::Closed);
                        ExecuteEnd::Poisoned
                    }
                };
            }
        }
    }
}

async fn drain<P: mysql_async::prelude::Protocol>(
    qr: &mut mysql_async::QueryResult<'_, '_, P>,
) -> Result<(), DbError> {
    loop {
        match qr.next().await {
            Ok(Some(_)) => {}
            Ok(None) => {
                if qr.is_empty() {
                    return Ok(());
                }
                // next() advanced to the following result set; keep going.
            }
            Err(e) => {
                let e = map_mysql_error(e);
                if e.is_recoverable() {
                    if qr.is_empty() {
                        return Ok(());
                    }
                } else {
                    return Err(e);
                }
            }
        }
    }
}

async fn run_and_drain(conn: &mut Conn, stmt: &str) -> Result<(), DbError> {
    let qr = conn.query_iter(stmt).await.map_err(map_mysql_error)?;
    qr.drop_result().await.map_err(map_mysql_error)
}

fn non_empty(s: String) -> Option<String> {
    if s.is_empty() {
        None
    } else {
        Some(s)
    }
}

pub fn schema_of(columns: &[Column]) -> RowSchema {
    let fields = columns.iter().map(field_def_of).collect();
    RowSchema {
        fields,
        identity: detect_identity(columns),
    }
}

fn detect_identity(columns: &[Column]) -> Option<Identity> {
    let first_table = columns
        .first()
        .map(|c| (c.schema_str().into_owned(), c.org_table_str().into_owned()))?;
    if first_table.1.is_empty() {
        return None;
    }
    if !columns.iter().all(|c| {
        c.schema_str() == first_table.0.as_str() && c.org_table_str() == first_table.1.as_str()
    }) {
        return None;
    }
    let field_indices: Vec<u32> = columns
        .iter()
        .enumerate()
        .filter(|(_, c)| c.flags().contains(ColumnFlags::PRI_KEY_FLAG))
        .map(|(i, _)| i as u32)
        .collect();
    if field_indices.is_empty() {
        None
    } else {
        Some(Identity { field_indices })
    }
}

fn decode_row(row: Row, columns: &[Column]) -> Vec<ApiValue> {
    row.unwrap()
        .into_iter()
        .zip(columns.iter())
        .map(|(v, col)| decode_value(col, v))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use mysql_async::consts::ColumnType;

    fn pk_col(name: &[u8]) -> Column {
        Column::new(ColumnType::MYSQL_TYPE_LONG)
            .with_schema(b"app")
            .with_org_table(b"users")
            .with_table(b"users")
            .with_name(name)
            .with_org_name(name)
            .with_flags(ColumnFlags::PRI_KEY_FLAG | ColumnFlags::NOT_NULL_FLAG)
    }

    fn plain_col(name: &[u8]) -> Column {
        Column::new(ColumnType::MYSQL_TYPE_VAR_STRING)
            .with_schema(b"app")
            .with_org_table(b"users")
            .with_table(b"users")
            .with_name(name)
            .with_org_name(name)
    }

    #[test]
    fn identity_from_pk_flags_single_table() {
        let cols = [pk_col(b"id"), plain_col(b"name")];
        let schema = schema_of(&cols);
        assert_eq!(
            schema.identity,
            Some(Identity {
                field_indices: vec![0]
            })
        );
        assert_eq!(schema.fields.len(), 2);
    }

    #[test]
    fn identity_none_for_multi_table_or_expression_results() {
        // Two different origin tables → never guess.
        let other = Column::new(ColumnType::MYSQL_TYPE_LONG)
            .with_schema(b"app")
            .with_org_table(b"orders")
            .with_name(b"order_id")
            .with_flags(ColumnFlags::PRI_KEY_FLAG);
        assert_eq!(schema_of(&[pk_col(b"id"), other]).identity, None);
        // Expression columns have no origin table → never guess.
        let expr = Column::new(ColumnType::MYSQL_TYPE_LONGLONG).with_name(b"count(*)");
        assert_eq!(schema_of(&[expr]).identity, None);
        // No PK selected → no identity.
        assert_eq!(schema_of(&[plain_col(b"name")]).identity, None);
    }
}
