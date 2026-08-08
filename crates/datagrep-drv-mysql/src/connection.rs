//! [`MySqlConnection`]: wraps one `mysql_async::Conn` (owned by whichever
//! actor task currently holds the mutex guard) plus the kill-pool used for
//! out-of-band cancellation.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use mysql_async::prelude::Queryable;
use mysql_async::{Conn, Pool};
use tokio::sync::{oneshot, Mutex};

use datagrep_api::caps::Capabilities;
use datagrep_api::catalog::Catalog;
use datagrep_api::driver::{
    Canceller, Connection, Cursor, Enforcement, ServerInfo, Transaction, TxOpts,
};
use datagrep_api::error::DbError;
use datagrep_api::request::{DdlOp, ExecOpts, Mutation, MutationBatch, Op, Request};
use datagrep_api::value::Value;

use datagrep_lang::sql::splitter;

use crate::actor::{self, ActorCmd, ExecOutcome};
use crate::canceller::MySqlCanceller;
use crate::catalog::MySqlCatalog;
use crate::cursor::{AckCursor, MySqlCursor};
use crate::error::map_mysql_error;
use crate::sql::{self, Flavor};
use crate::transaction::MySqlTransaction;

/// A cheap, PII-free label for tracing spans — never the statement text
/// itself. Query text can carry customer data, so it is never logged.
fn request_kind(req: &Request) -> &'static str {
    match req {
        Request::Native { .. } => "native",
        Request::Op(Op::Scan { .. }) => "scan",
        Request::Op(Op::Count { .. }) => "count",
        Request::Op(Op::Mutate(_)) => "mutate",
        Request::Op(Op::Explain { .. }) => "explain",
        Request::Op(Op::Ddl(_)) => "ddl",
    }
}

/// A compiled request, ready for the actor.
pub(crate) struct Compiled {
    pub statements: Vec<String>,
    pub params: Vec<Value>,
    pub timeout: Option<Duration>,
    pub row_limit: Option<u64>,
}

pub struct MySqlConnection {
    conn: Arc<Mutex<Option<Conn>>>,
    server_info: ServerInfo,
    caps: Capabilities,
    flavor: Flavor,
    conn_id: u32,
    kill_pool: Pool,
}

impl MySqlConnection {
    pub fn new(
        conn: Conn,
        server_info: ServerInfo,
        caps: Capabilities,
        flavor: Flavor,
        kill_pool: Pool,
    ) -> Self {
        let conn_id = conn.id();
        Self {
            conn: Arc::new(Mutex::new(Some(conn))),
            server_info,
            caps,
            flavor,
            conn_id,
            kill_pool,
        }
    }

    /// Compile a `Request` to a statement script + bound params.
    ///
    /// `Native` text passes through verbatim — what the user typed is what
    /// the server runs, never a translation of it — but it is split into
    /// statements with datagrep-lang's MySQL-aware splitter (backticks,
    /// `#` comments, `DELIMITER`) because the wire protocol runs
    /// one statement per round trip: preceding statements execute to
    /// completion, the last one streams. That is this driver's
    /// `MULTI_STATEMENT` support.
    pub(crate) fn compile(req: &Request, flavor: Flavor) -> Result<Compiled, DbError> {
        match req {
            Request::Native { text, params, opts } => {
                let statements = split_statements(text);
                if statements.len() > 1 && !params.is_empty() {
                    return Err(DbError::Unsupported {
                        feature: format!(
                            "positional parameters with a multi-statement script \
                             ({} statements) — parameters bind to exactly one statement",
                            statements.len()
                        ),
                    });
                }
                Ok(Compiled {
                    statements,
                    params: params.clone(),
                    timeout: opts.timeout,
                    row_limit: opts.row_limit,
                })
            }
            Request::Op(op) => Self::compile_op(op, flavor, &ExecOpts::default()),
        }
    }

    fn compile_op(op: &Op, flavor: Flavor, opts: &ExecOpts) -> Result<Compiled, DbError> {
        let single = |sql: String, params: Vec<Value>| Compiled {
            statements: vec![sql],
            params,
            timeout: opts.timeout,
            row_limit: opts.row_limit,
        };
        match op {
            Op::Scan {
                path,
                filter,
                order,
                project,
                limit,
                ..
            } => {
                let (sql, params) = sql::compile_scan(path, filter, order, project, *limit)?;
                Ok(single(sql, params))
            }
            Op::Count {
                path,
                filter,
                exact,
            } => {
                let (sql, params) = sql::compile_count(path, filter, *exact)?;
                Ok(single(sql, params))
            }
            Op::Explain { inner, analyze } => {
                let mut compiled = match inner.as_ref() {
                    Request::Native { text, params, opts } => {
                        let statements = split_statements(text);
                        if statements.len() != 1 {
                            return Err(DbError::Unsupported {
                                feature: "EXPLAIN over a multi-statement script".into(),
                            });
                        }
                        Compiled {
                            statements,
                            params: params.clone(),
                            timeout: opts.timeout,
                            row_limit: opts.row_limit,
                        }
                    }
                    Request::Op(inner_op) => Self::compile_op(inner_op, flavor, opts)?,
                };
                let last = compiled.statements.pop().ok_or_else(|| DbError::Query {
                    code: None,
                    message: "nothing to explain".into(),
                    position: None,
                })?;
                compiled
                    .statements
                    .push(sql::wrap_explain(&last, *analyze, flavor));
                Ok(compiled)
            }
            Op::Ddl(DdlOp::Native { text }) => Ok(Compiled {
                statements: split_statements(text),
                params: Vec::new(),
                timeout: opts.timeout,
                row_limit: None,
            }),
            Op::Mutate(batch) => Err(DbError::Unsupported {
                feature: format!(
                    "Op::Mutate must go through MySqlConnection::execute_mutate, not the \
                     generic compiler ({} mutation(s))",
                    batch.mutations.len()
                ),
            }),
        }
    }

    async fn execute_compiled(&self, compiled: Compiled) -> Result<Box<dyn Cursor>, DbError> {
        let guard = self.conn.clone().lock_owned().await;
        if guard.is_none() {
            return Err(DbError::Closed);
        }
        let cmd_tx = actor::spawn_simple(guard, self.flavor);
        let (reply_tx, reply_rx) = oneshot::channel();
        cmd_tx
            .send(ActorCmd::Execute {
                statements: compiled.statements,
                params: compiled.params,
                timeout: compiled.timeout,
                row_limit: compiled.row_limit,
                reply: reply_tx,
            })
            .await
            .map_err(|_| DbError::Closed)?;
        match reply_rx.await.map_err(|_| DbError::Closed)?? {
            ExecOutcome::Ack {
                affected,
                message,
                warnings,
            } => Ok(Box::new(AckCursor::new(affected, message, warnings))),
            ExecOutcome::Cursor { cursor_id, schema } => {
                Ok(Box::new(MySqlCursor::new(cmd_tx, cursor_id, schema)))
            }
        }
    }

    async fn execute_mutate(&self, batch: &MutationBatch) -> Result<Box<dyn Cursor>, DbError> {
        if batch.mutations.is_empty() {
            return Ok(Box::new(AckCursor::new(0, None, 0)));
        }
        let guard = self.conn.clone().lock_owned().await;
        if guard.is_none() {
            return Err(DbError::Closed);
        }
        // The batch runs inside one explicit transaction so a violated
        // exactly-one-row invariant rolls the whole batch back.
        let cmd_tx = actor::spawn_transaction(guard, self.flavor, None, false);

        let mut total_affected = 0u64;
        for m in &batch.mutations {
            let compiled = sql::compile_mutation(m)?;
            let (reply_tx, reply_rx) = oneshot::channel();
            if cmd_tx
                .send(ActorCmd::Execute {
                    statements: vec![compiled.sql],
                    params: compiled.params,
                    timeout: None,
                    row_limit: None,
                    reply: reply_tx,
                })
                .await
                .is_err()
            {
                return Err(DbError::Closed);
            }
            let affected = match reply_rx.await.map_err(|_| DbError::Closed)? {
                Ok(ExecOutcome::Ack { affected, .. }) => affected,
                Ok(ExecOutcome::Cursor { .. }) => {
                    Self::rollback_actor(&cmd_tx).await;
                    return Err(DbError::Unsupported {
                        feature: "mutation statement unexpectedly returned rows".into(),
                    });
                }
                Err(e) => {
                    Self::rollback_actor(&cmd_tx).await;
                    return Err(e);
                }
            };
            // Every generated mutation must affect exactly one row or the
            // batch rolls back: an identity matching zero or many rows means
            // the grid's picture of the table is stale, and editing on a
            // stale picture rewrites the wrong rows.
            if !matches!(m, Mutation::Insert { .. }) && affected != 1 {
                Self::rollback_actor(&cmd_tx).await;
                return Err(DbError::Query {
                    code: None,
                    message: format!(
                        "row identity changed — refresh (expected exactly 1 row affected, got {affected})"
                    ),
                    position: None,
                });
            }
            total_affected += affected;
        }

        let (reply_tx, reply_rx) = oneshot::channel();
        if cmd_tx
            .send(ActorCmd::Commit { reply: reply_tx })
            .await
            .is_err()
        {
            return Err(DbError::Closed);
        }
        reply_rx.await.map_err(|_| DbError::Closed)??;
        Ok(Box::new(AckCursor::new(total_affected, None, 0)))
    }

    async fn rollback_actor(cmd_tx: &tokio::sync::mpsc::Sender<ActorCmd>) {
        let (reply_tx, reply_rx) = oneshot::channel();
        if cmd_tx
            .send(ActorCmd::Rollback { reply: reply_tx })
            .await
            .is_ok()
        {
            let _ = reply_rx.await;
        }
    }
}

/// Split source text into individual statements via datagrep-lang's
/// MySQL-dialect splitter (handles backticked identifiers, `#` and `--`
/// comments, string literals, and `DELIMITER` meta-commands).
pub(crate) fn split_statements(text: &str) -> Vec<String> {
    splitter::split(text, datagrep_api::SqlDialect::Mysql)
        .into_iter()
        .map(|span| text[span.range].trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

#[async_trait]
impl Connection for MySqlConnection {
    fn capabilities(&self) -> Capabilities {
        self.caps.clone()
    }

    fn server_info(&self) -> &ServerInfo {
        &self.server_info
    }

    async fn ping(&self) -> Result<(), DbError> {
        let mut guard = self.conn.lock().await;
        let conn = guard.as_mut().ok_or(DbError::Closed)?;
        conn.ping().await.map_err(map_mysql_error)
    }

    #[tracing::instrument(skip(self, req), fields(kind = request_kind(&req)))]
    async fn execute(&self, req: Request) -> Result<Box<dyn Cursor>, DbError> {
        if let Request::Op(Op::Mutate(batch)) = &req {
            return self.execute_mutate(batch).await;
        }
        if let Request::Op(Op::Explain { analyze: true, .. }) = &req {
            if !self
                .caps
                .flags
                .contains(datagrep_api::Caps::EXPLAIN_ANALYZE)
            {
                return Err(DbError::Unsupported {
                    feature: format!(
                        "EXPLAIN ANALYZE requires MySQL 8.0.18+ or MariaDB 10.1+ (server is {} {})",
                        self.server_info.product, self.server_info.version
                    ),
                });
            }
        }
        let compiled = Self::compile(&req, self.flavor)?;
        tracing::debug!(
            statements = compiled.statements.len(),
            param_count = compiled.params.len(),
            "compiled request"
        );
        self.execute_compiled(compiled).await
    }

    fn canceller(&self) -> Arc<dyn Canceller> {
        Arc::new(MySqlCanceller::new(self.kill_pool.clone(), self.conn_id))
    }

    fn catalog(&self) -> Arc<dyn Catalog> {
        Arc::new(MySqlCatalog::new(self.conn.clone()))
    }

    async fn begin(&self, opts: TxOpts) -> Result<Box<dyn Transaction>, DbError> {
        let guard = self.conn.clone().lock_owned().await;
        if guard.is_none() {
            return Err(DbError::Closed);
        }
        // The actor holds the connection mutex for the transaction's whole
        // life, so the BEGIN can never migrate to another socket.
        let cmd_tx = actor::spawn_transaction(guard, self.flavor, opts.isolation, opts.read_only);
        Ok(Box::new(MySqlTransaction::new(cmd_tx, self.flavor)))
    }

    async fn set_read_only(&self, on: bool) -> Result<Enforcement, DbError> {
        let mut guard = self.conn.lock().await;
        let conn = guard.as_mut().ok_or(DbError::Closed)?;
        let sql = if on {
            "SET SESSION TRANSACTION READ ONLY"
        } else {
            "SET SESSION TRANSACTION READ WRITE"
        };
        conn.query_drop(sql).await.map_err(map_mysql_error)?;
        // The server itself now refuses writes on this session.
        Ok(Enforcement::Server)
    }

    async fn close(&self) -> Result<(), DbError> {
        // Idempotent: `.take()` on an already-empty slot is a no-op. Every
        // subsequent operation observes `None` → `DbError::Closed`.
        let conn = {
            let mut guard = self.conn.lock().await;
            guard.take()
        };
        if let Some(conn) = conn {
            if let Err(e) = conn.disconnect().await {
                tracing::debug!(error = %e, "graceful disconnect failed");
            }
        }
        // Tear the kill-pool down too (no-op if it never opened a conn).
        let pool = self.kill_pool.clone();
        if let Err(e) = pool.disconnect().await {
            tracing::debug!(error = %e, "kill-pool disconnect failed");
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use datagrep_api::{FieldPath, ObjectPath, Predicate};
    use std::sync::Arc as StdArc;

    #[test]
    fn split_statements_consumes_datagrep_lang() {
        let stmts = split_statements("SELECT 1; SELECT 2;");
        assert_eq!(stmts, vec!["SELECT 1", "SELECT 2"]);
        // MySQL specifics the splitter owns: # comments and backticks.
        let stmts = split_statements("# leading comment\nSELECT `a;b` FROM t; SELECT 2");
        assert_eq!(stmts.len(), 2);
        assert!(stmts[0].contains("`a;b`"), "backticked ; must not split");
        // DELIMITER meta-command.
        let stmts = split_statements(
            "DELIMITER //\nCREATE PROCEDURE p() BEGIN SELECT 1; END //\nDELIMITER ;\nSELECT 3;",
        );
        assert_eq!(stmts.len(), 2);
        assert!(stmts[0].starts_with("CREATE PROCEDURE"));
        assert_eq!(stmts[1], "SELECT 3");
    }

    #[test]
    fn compile_native_multi_statement_with_params_is_refused() {
        let req = Request::Native {
            text: StdArc::from("SELECT ?; SELECT ?"),
            params: vec![Value::I64(1)],
            opts: ExecOpts::default(),
        };
        assert!(matches!(
            MySqlConnection::compile(&req, Flavor::MySql),
            Err(DbError::Unsupported { .. })
        ));
    }

    #[test]
    fn compile_scan_produces_single_parameterized_statement() {
        let req = Request::Op(Op::Scan {
            path: ObjectPath::new(vec![StdArc::from("app"), StdArc::from("users")]),
            filter: Some(Predicate::Eq {
                field: FieldPath::field("id"),
                value: Value::I64(7),
            }),
            order: vec![],
            project: None,
            limit: Some(10),
            resume: None,
        });
        let c = MySqlConnection::compile(&req, Flavor::MySql).unwrap();
        assert_eq!(c.statements.len(), 1);
        assert!(c.statements[0].contains("`app`.`users`"));
        assert!(c.statements[0].contains("`id` = ?"));
        assert_eq!(c.params, vec![Value::I64(7)]);
    }

    #[test]
    fn compile_explain_analyze_uses_flavor_spelling() {
        let inner = Request::native("SELECT * FROM t");
        let req = |analyze| Op::Explain {
            inner: Box::new(inner.clone()),
            analyze,
        };
        let c = MySqlConnection::compile(&Request::Op(req(true)), Flavor::MySql).unwrap();
        assert_eq!(c.statements, vec!["EXPLAIN ANALYZE SELECT * FROM t"]);
        let c = MySqlConnection::compile(&Request::Op(req(true)), Flavor::MariaDb).unwrap();
        assert_eq!(c.statements, vec!["ANALYZE SELECT * FROM t"]);
        let c = MySqlConnection::compile(&Request::Op(req(false)), Flavor::MariaDb).unwrap();
        assert_eq!(c.statements, vec!["EXPLAIN SELECT * FROM t"]);
    }
}
