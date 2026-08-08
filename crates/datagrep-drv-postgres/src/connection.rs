//! [`PgConnection`] (ticket item 2): wraps a `tokio_postgres::Client` plus
//! the spawned connection task.

use std::sync::Arc;

use async_trait::async_trait;
use tokio::sync::{oneshot, Mutex};
use tokio_postgres::Client;

use datagrep_api::caps::Capabilities;
use datagrep_api::catalog::Catalog;
use datagrep_api::driver::{
    Canceller, Connection, Cursor, Enforcement, ServerInfo, Transaction, TxOpts,
};
use datagrep_api::error::DbError;
use datagrep_api::request::{DdlOp, Mutation, MutationBatch, Op, Request};
use datagrep_api::value::Value;

use crate::actor::{self, ActorCmd, ExecOutcome};
use crate::canceller::PgCanceller;
use crate::catalog::PgCatalog;
use crate::cursor::{AckCursor, PgCursor};
use crate::error::map_pg_error;
use crate::sql;
use crate::transaction::PgTransaction;
use crate::value::PgParam;

/// A cheap, PII-free label for tracing spans — never the statement text
/// itself (design §3.8/telemetry rule: query text is never logged).
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

pub struct PgConnection {
    client: Arc<Mutex<Option<Client>>>,
    server_info: ServerInfo,
    // Extracted eagerly (`Client::cancel_token` is a cheap, lock-free `&self`
    // call) because `Connection::canceller` is *not* async — there is no way
    // to `.await` the shared client mutex from inside it, and `CancelToken`
    // is `Clone`/self-contained (process id + secret key), so grabbing one
    // copy up front and cloning it per `canceller()` call is both correct
    // and avoids ever blocking that method.
    cancel_token: tokio_postgres::CancelToken,
}

impl PgConnection {
    pub fn new(client: Client, server_info: ServerInfo) -> Self {
        let cancel_token = client.cancel_token();
        Self {
            client: Arc::new(Mutex::new(Some(client))),
            server_info,
            cancel_token,
        }
    }

    /// Compile a `Request` down to `(sql, params)`. `Native` text passes
    /// through verbatim (never translated — design §3.6); `Op` is compiled
    /// by `sql.rs`, always via bound `$n` parameters (design §3.8).
    fn compile(req: &Request) -> Result<(String, Vec<Value>), DbError> {
        match req {
            Request::Native { text, params, .. } => Ok((text.to_string(), params.clone())),
            Request::Op(op) => Self::compile_op(op),
        }
    }

    fn compile_op(op: &Op) -> Result<(String, Vec<Value>), DbError> {
        match op {
            Op::Scan { path, filter, order, project, limit, .. } => {
                sql::compile_scan(path, filter, order, project, *limit)
            }
            Op::Count { path, filter, exact } => sql::compile_count(path, filter, *exact),
            Op::Explain { inner, analyze } => {
                let (inner_sql, params) = Self::compile(inner)?;
                Ok((sql::wrap_explain(&inner_sql, *analyze), params))
            }
            Op::Ddl(DdlOp::Native { text }) => Ok((text.to_string(), Vec::new())),
            Op::Mutate(batch) => Err(DbError::Unsupported {
                feature: format!(
                    "Op::Mutate must go through PgConnection::execute_mutation, not the generic SQL compiler ({} mutation(s))",
                    batch.mutations.len()
                ),
            }),
        }
    }

    async fn execute_native_or_scan(
        &self,
        text: String,
        params: Vec<Value>,
    ) -> Result<Box<dyn Cursor>, DbError> {
        let guard = self.client.lock().await;
        let client = guard.as_ref().ok_or(DbError::Closed)?;
        let stmt = client.prepare(&text).await.map_err(map_pg_error)?;
        let is_select_ish = !stmt.columns().is_empty();
        drop(guard);

        if !is_select_ish {
            let guard = self.client.lock().await;
            let client = guard.as_ref().ok_or(DbError::Closed)?;
            let bound: Vec<PgParam<'_>> = params.iter().map(PgParam).collect();
            let refs: Vec<&(dyn tokio_postgres::types::ToSql + Sync)> = bound
                .iter()
                .map(|p| p as &(dyn tokio_postgres::types::ToSql + Sync))
                .collect();
            let affected = client.execute(&stmt, &refs).await.map_err(map_pg_error)?;
            return Ok(Box::new(AckCursor::new(affected)));
        }

        // SELECT-ish: portals require a transaction (ticket note). Wrapped
        // read-only, since this is a transparent, single-statement wrapper
        // the caller never explicitly commits — see `actor.rs` module docs.
        //
        // Known gap: a write that also returns rows (`INSERT ... RETURNING`)
        // reaches this branch too (it has columns) and will fail inside a
        // READ ONLY transaction. Distinguishing "returns rows because it's a
        // SELECT" from "returns rows because of a RETURNING clause" needs
        // the statement's command tag, which `Statement::columns()` alone
        // doesn't give us. Documented as a known limitation in the crate
        // report rather than papered over.
        let owned_guard = self.client.clone().lock_owned().await;
        let cmd_tx = actor::spawn(owned_guard, true, None);
        let (reply_tx, reply_rx) = oneshot::channel();
        cmd_tx
            .send(ActorCmd::Execute {
                text,
                params,
                reply: reply_tx,
            })
            .await
            .map_err(|_| DbError::Closed)?;
        match reply_rx.await.map_err(|_| DbError::Closed)?? {
            ExecOutcome::Ack { affected } => Ok(Box::new(AckCursor::new(affected))),
            ExecOutcome::Cursor { portal_id, schema } => {
                Ok(Box::new(PgCursor::new(cmd_tx, portal_id, schema)))
            }
        }
    }

    async fn execute_mutate(&self, batch: &MutationBatch) -> Result<Box<dyn Cursor>, DbError> {
        if batch.mutations.is_empty() {
            return Ok(Box::new(AckCursor::new(0)));
        }
        let owned_guard = self.client.clone().lock_owned().await;
        let cmd_tx = actor::spawn(owned_guard, false, None);

        let mut total_affected = 0u64;
        for m in &batch.mutations {
            let key_fields = self.resolve_key_fields(m).await?;
            let compiled = sql::compile_mutation(m, &key_fields)?;
            let (reply_tx, reply_rx) = oneshot::channel();
            if cmd_tx
                .send(ActorCmd::Execute {
                    text: compiled.sql,
                    params: compiled.params,
                    reply: reply_tx,
                })
                .await
                .is_err()
            {
                return Err(DbError::Closed);
            }
            let outcome = reply_rx.await.map_err(|_| DbError::Closed)?;
            let affected = match outcome {
                Ok(ExecOutcome::Ack { affected }) => affected,
                Ok(ExecOutcome::Cursor { .. }) => {
                    let _ = Self::rollback(&cmd_tx).await;
                    return Err(DbError::Unsupported {
                        feature: "mutation statement unexpectedly returned rows".into(),
                    });
                }
                Err(e) => {
                    let _ = Self::rollback(&cmd_tx).await;
                    return Err(e);
                }
            };
            // Design §3.8: every generated mutation "must affect exactly one
            // row or it rolls back with 'row identity changed — refresh'".
            if !matches!(m, Mutation::Insert { .. }) && affected != 1 {
                let _ = Self::rollback(&cmd_tx).await;
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
        Ok(Box::new(AckCursor::new(total_affected)))
    }

    async fn rollback(cmd_tx: &tokio::sync::mpsc::Sender<ActorCmd>) {
        let (reply_tx, reply_rx) = oneshot::channel();
        if cmd_tx
            .send(ActorCmd::Rollback { reply: reply_tx })
            .await
            .is_ok()
        {
            let _ = reply_rx.await;
        }
    }

    /// Resolve the identity column names a `Mutation::Update`/`Delete`'s
    /// positional `key: Vec<Value>` refers to.
    ///
    /// Gap: `datagrep_api::request::Mutation` carries `key` as bare values with no
    /// field names (unlike `sets`, which pairs each value with a
    /// `FieldPath`) — see the crate report. We recover the primary key
    /// column order with a live `pg_index` lookup each call; this is extra
    /// round trips on the write path but never guesses silently.
    async fn resolve_key_fields(&self, m: &Mutation) -> Result<Vec<Arc<str>>, DbError> {
        let (path, key_len) = match m {
            Mutation::Update { path, key, .. } => (path, key.len()),
            Mutation::Delete { path, key } => (path, key.len()),
            Mutation::Insert { .. } => return Ok(Vec::new()),
        };
        let table = sql::quote_object_path(path)?;
        let guard = self.client.lock().await;
        let client = guard.as_ref().ok_or(DbError::Closed)?;
        let rows = client
            .query(
                &format!(
                    "SELECT a.attname FROM pg_index i \
                     JOIN pg_attribute a ON a.attrelid = i.indrelid AND a.attnum = ANY(i.indkey) \
                     WHERE i.indrelid = {table}::regclass AND i.indisprimary \
                     ORDER BY array_position(i.indkey, a.attnum)"
                ),
                &[],
            )
            .await
            .map_err(map_pg_error)?;
        drop(guard);
        let names: Vec<Arc<str>> = rows
            .iter()
            .map(|r| Arc::from(r.get::<_, String>(0)))
            .collect();
        if names.len() != key_len {
            return Err(DbError::Unsupported {
                feature: format!(
                    "resolved {} primary key column(s) for {path} but the mutation supplied {key_len} key value(s)",
                    names.len()
                ),
            });
        }
        Ok(names)
    }
}

#[async_trait]
impl Connection for PgConnection {
    fn capabilities(&self) -> Capabilities {
        crate::driver::pg_capabilities()
    }

    fn server_info(&self) -> &ServerInfo {
        &self.server_info
    }

    async fn ping(&self) -> Result<(), DbError> {
        let guard = self.client.lock().await;
        let client = guard.as_ref().ok_or(DbError::Closed)?;
        client
            .simple_query("SELECT 1")
            .await
            .map_err(map_pg_error)?;
        Ok(())
    }

    #[tracing::instrument(skip(self, req), fields(kind = request_kind(&req)))]
    async fn execute(&self, req: Request) -> Result<Box<dyn Cursor>, DbError> {
        if let Request::Op(Op::Mutate(batch)) = &req {
            return self.execute_mutate(batch).await;
        }
        let (text, params) = Self::compile(&req)?;
        tracing::debug!(param_count = params.len(), "compiled request to SQL");
        self.execute_native_or_scan(text, params).await
    }

    fn canceller(&self) -> Arc<dyn Canceller> {
        Arc::new(PgCanceller::new(self.cancel_token.clone()))
    }

    fn catalog(&self) -> Arc<dyn Catalog> {
        Arc::new(PgCatalog::new(self.client.clone()))
    }

    async fn begin(&self, opts: TxOpts) -> Result<Box<dyn Transaction>, DbError> {
        let owned_guard = self.client.clone().lock_owned().await;
        let cmd_tx = actor::spawn(owned_guard, opts.read_only, opts.isolation);
        Ok(Box::new(PgTransaction::new(cmd_tx)))
    }

    async fn set_read_only(&self, on: bool) -> Result<Enforcement, DbError> {
        let guard = self.client.lock().await;
        let client = guard.as_ref().ok_or(DbError::Closed)?;
        let sql = if on {
            "SET SESSION CHARACTERISTICS AS TRANSACTION READ ONLY"
        } else {
            "SET SESSION CHARACTERISTICS AS TRANSACTION READ WRITE"
        };
        client.batch_execute(sql).await.map_err(map_pg_error)?;
        Ok(Enforcement::Server)
    }

    async fn close(&self) -> Result<(), DbError> {
        // Idempotent: `.take()` on an already-`None` slot is a no-op. Taking
        // the `Client` (rather than just dropping our `Arc`) drops its last
        // strong reference here so the background connection task's channel
        // closes and the socket shuts down promptly, and every subsequent
        // operation on this connection (which all go through the same
        // `Arc<Mutex<Option<Client>>>`) sees `None` and returns
        // `DbError::Closed`, per the trait's contract.
        let mut guard = self.client.lock().await;
        guard.take();
        Ok(())
    }
}
