//! The per-request/per-transaction actor task.
//!
//! Same constraint the Postgres driver hit, different type: mysql_async's
//! `QueryResult<'a, 't, P>` mutably borrows its `Conn` for as long as the
//! result is being streamed, while `Connection::execute` must hand back a
//! `'static` `Box<dyn Cursor>`. (The Mongo driver escaped this
//! because its `ClientSession`/cursor pair is fully owned; MySQL is not so
//! lucky.) Rather than an unsafe self-referential struct, the `Conn` and the
//! in-flight `QueryResult` live entirely on this task's stack; everything
//! crossing back out is an owned, `'static` channel handle.
//!
//! **The undrained-result gotcha, handled here**: a MySQL connection with an
//! unconsumed result set is poisoned — the leftover packets surface as an
//! error on the *next* statement. Every exit path below (batch exhaustion,
//! cursor close, cursor drop, cancel-induced error, a new statement arriving
//! while a result is open) either fully drains the `QueryResult` or observes
//! that the server already terminated it, before the `Conn` is ever used
//! again. The integration test `undrained_result_does_not_poison_connection`
//! exercises exactly this.
//!
//! Backpressure: rows are pulled off the socket one
//! `QueryResult::next()` at a time, only inside `FetchBatch` handling —
//! nobody calls `next_batch`, nothing is read, the TCP window closes, the
//! server stops producing. Nothing here ever calls `collect`.

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

/// What `execute()` got back for one request.
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

/// One pulled chunk, already decoded to seam values.
pub struct Fetched {
    pub rows: Vec<Vec<ApiValue>>,
    /// The result set ended with (or before) this pull.
    pub done: bool,
    /// Server warning count, reported once the set is finished.
    pub warnings: u16,
}

pub enum ActorCmd {
    /// A pre-split script: statements 0..n-1 run to completion (drained),
    /// the last one streams. Splitting happened upstream via datagrep-lang.
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

/// Whether the actor wraps a single `execute()` call or an interactive
/// transaction.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    /// Exits as soon as the one request (and its streaming tail) completes.
    Simple,
    /// Opens `START TRANSACTION`, serves many requests, exits on
    /// commit/rollback; channel drop = rollback (safe default).
    Transaction,
}

/// Spawn an actor for one `Connection::execute` call.
pub fn spawn_simple(
    guard: OwnedMutexGuard<Option<Conn>>,
    flavor: Flavor,
) -> mpsc::Sender<ActorCmd> {
    spawn(guard, flavor, Mode::Simple, None, false)
}

/// Spawn an actor wrapping an explicit transaction. `isolation`/`read_only`
/// shape the `START TRANSACTION` it opens immediately.
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
            // A transport/protocol failure mid-stream: the Conn's state is
            // no longer trustworthy. Take it so every later use of this
            // connection observes `Closed` rather than inheriting a
            // half-broken session.
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

/// Answer every queued/future command with `Closed`.
async fn drain_replying_closed(rx: &mut mpsc::Receiver<ActorCmd>) {
    while let Some(cmd) = rx.recv().await {
        reply_error(cmd, DbError::Closed);
    }
}

/// Answer every queued/future command with a copy of `err`.
async fn drain_replying(rx: &mut mpsc::Receiver<ActorCmd>, err: &DbError) {
    while let Some(cmd) = rx.recv().await {
        reply_error(cmd, clone_err(err));
    }
}

fn clone_err(e: &DbError) -> DbError {
    // DbError is not Clone; a Protocol-flavored copy still tells every
    // waiting caller what happened.
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

/// The command loop. Returns `true` when the `Conn` should be poisoned.
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
                            // The one request this actor exists for is fully
                            // finished (drained/closed) — release the Conn
                            // immediately instead of waiting for the cursor
                            // handle to be dropped.
                            break 'outer;
                        }
                    }
                    ExecuteEnd::Deferred(cmd) => deferred = Some(cmd),
                    ExecuteEnd::Poisoned => return true,
                }
            }
            ActorCmd::FetchBatch { reply, .. } => {
                // No result set is open; the cursor this belongs to is long
                // finished.
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

    // All senders dropped without an explicit end: roll back as the safe
    // default for "the caller went away" (matches the sibling drivers).
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

/// How one `Execute` (plus its streaming tail) ended.
enum ExecuteEnd {
    /// Fully finished; the Conn is clean and reusable.
    Continue,
    /// A command arrived while a result was open; the result was drained and
    /// the command still needs processing.
    Deferred(ActorCmd),
    /// Transport/protocol failure — the Conn must not be reused.
    Poisoned,
}

/// Run a pre-split script. Statements `0..n-1` are executed and fully
/// drained; the last statement streams through `FetchBatch` commands.
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

    // Always push a server-side deadline where one exists, so even a
    // statement we cannot cancel is bounded. MySQL's `max_execution_time`
    // applies to SELECT only; MariaDB's `max_statement_time` applies to all
    // statements. Both are reset afterwards.
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
    // Statements before the last: run to completion, results fully drained
    // (a mid-script SELECT is legal; its rows are simply not streamed).
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

    // The final statement: parameterized → binary protocol (real bound
    // params, so a value can never be re-parsed as SQL); no params → text
    // protocol. The two protocols
    // return differently-typed `QueryResult`s, hence the split into a
    // shared generic continuation.
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

/// Shared (protocol-generic) tail of `handle_script`: classify the pending
/// result as Ack or streaming cursor and serve it.
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

/// Serve `FetchBatch` commands against one open result set until it is
/// exhausted, closed, abandoned, or preempted. Every branch leaves the
/// underlying `Conn` fully drained (the undrained-result gotcha).
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
                // Cursor (and everything else) dropped mid-stream: drain so
                // the Conn is clean for the next actor.
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
                            // The server terminated the set (this is also the
                            // KILL QUERY path: ER_QUERY_INTERRUPTED arrives
                            // here and maps to Cancelled). mysql_async clears
                            // the pending set on a read error, but there may
                            // be trailing sets — drained below.
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
                    // Driver-enforced row cap (`ExecOpts::row_limit`): stop
                    // at the source and discard the remainder.
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
                // A new Execute (or Commit/Rollback) preempts the open
                // cursor: drain first, then let the outer loop process it.
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

/// Consume everything left in a result (remaining rows AND remaining result
/// sets) so the connection is clean. An `ER_QUERY_INTERRUPTED` while draining
/// counts as success — the server has already ended the set.
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
                    // Set terminated by the server (kill/timeout); trailing
                    // sets, if any, keep draining.
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

/// Execute one non-final script statement and consume its results entirely.
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

/// Build the seam schema from result-set column metadata.
pub fn schema_of(columns: &[Column]) -> RowSchema {
    let fields = columns.iter().map(field_def_of).collect();
    RowSchema {
        fields,
        identity: detect_identity(columns),
    }
}

/// Best-effort row identity from column metadata alone (no extra round trip:
/// the MySQL column-definition packet carries table, original name, and
/// PRI_KEY per column). Single-table results only; joins/expressions fall
/// back to `None` → not editable: with no identity there is no safe way to
/// name the row an edit is meant to hit.
///
/// Known limitation, stated: with a composite primary key only partially
/// selected, the selected key columns still carry PRI_KEY_FLAG, so the
/// derived identity can be too narrow. The backstop — every generated
/// mutation must affect exactly 1 row or the batch rolls back — is what makes
/// this safe rather than silently wrong.
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

/// Decode one wire row into seam values, in column order.
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
