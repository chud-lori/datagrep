//! [`PgConnection`] (ticket item 2): one *logical* connection to a Postgres
//! server, backed by a small [`crate::pool::PgPool`] of physical sessions.
//!
//! # Why a logical connection is not a single socket
//!
//! A streaming cursor and an interactive transaction both **pin** the socket
//! they run on: `tokio_postgres::Transaction<'a>` borrows `&'a mut Client`,
//! and a portal only exists inside a transaction (design §3.5 — "a pool that
//! silently moves a BEGIN to a different socket is a correctness bug").
//!
//! Earlier this driver drew the wrong conclusion from that and made *every*
//! operation queue behind the pinned socket: `catalog()` and the next
//! `execute()` awaited the same `Mutex<Client>` with no timeout. Holding an
//! open cursor and doing anything else — the GUI's "results grid open, click
//! the schema tree" — hung forever with the server sitting `idle in
//! transaction` (TEST-REPORT.md F2).
//!
//! What actually follows from the constraint is the opposite: a cursor pins
//! *its* session, so everything else must use a different one. Hence:
//!
//! * a cursor gives its session back the moment its portal is drained, not
//!   when the handle is dropped ([`crate::cursor::PgCursor`]);
//! * anything that needs the server while a session is pinned acquires
//!   another from the pool, dialled lazily with the same config;
//! * at the pool's cap the wait is bounded and ends in
//!   `DbError::ResourceExhausted` naming what holds the sessions — never a
//!   silent freeze.
//!
//! The full reasoning, including the server-side lock angle, is in the
//! `pool.rs` module docs.

use std::sync::Arc;

use async_trait::async_trait;
use tokio::sync::oneshot;

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
use crate::cursor::{AckCursor, PgCursor, SessionOwnership};
use crate::error::map_pg_error;
use crate::pool::PgPool;
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
    pool: Arc<PgPool>,
    server_info: ServerInfo,
}

impl PgConnection {
    pub fn new(pool: Arc<PgPool>, server_info: ServerInfo) -> Self {
        Self { pool, server_info }
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
        // One session for the whole call: prepare on it, and — if this turns
        // out to be SELECT-ish — hand that same session to the cursor's
        // actor. Preparing on one socket and binding on another would be
        // both wasteful and, for temp tables/search_path, wrong.
        let session = self.pool.acquire().await?;
        let stmt = session.prepare(&text).await.map_err(map_pg_error)?;
        let is_select_ish = !stmt.columns().is_empty();

        if !is_select_ish {
            let bound: Vec<PgParam<'_>> = params.iter().map(PgParam).collect();
            let refs: Vec<&(dyn tokio_postgres::types::ToSql + Sync)> = bound
                .iter()
                .map(|p| p as &(dyn tokio_postgres::types::ToSql + Sync))
                .collect();
            let affected = session.execute(&stmt, &refs).await.map_err(map_pg_error)?;
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
        drop(stmt);
        let cmd_tx = actor::spawn(session.into_guard(), true, None);
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
            ExecOutcome::Ack { affected } => {
                // No portal was bound, so nothing will ever release this
                // actor's session for us — end its transaction now instead of
                // leaving a socket pinned until the task happens to be
                // scheduled after `cmd_tx` drops.
                Self::rollback(&cmd_tx).await;
                Ok(Box::new(AckCursor::new(affected)))
            }
            ExecOutcome::Cursor { portal_id, schema } => Ok(Box::new(PgCursor::new(
                cmd_tx,
                portal_id,
                schema,
                SessionOwnership::Owned,
            ))),
        }
    }

    async fn execute_mutate(&self, batch: &MutationBatch) -> Result<Box<dyn Cursor>, DbError> {
        if batch.mutations.is_empty() {
            return Ok(Box::new(AckCursor::new(0)));
        }
        let cmd_tx = actor::spawn(self.pool.acquire().await?.into_guard(), false, None);

        let mut total_affected = 0u64;
        for m in &batch.mutations {
            let compiled = sql::compile_mutation(m)?;
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
        let session = self.pool.acquire().await?;
        session
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
        // A logical connection can own several physical sessions, so "cancel
        // this connection's work" means cancelling each of them; the pool is
        // snapshotted at `cancel()` time so sessions dialled after this call
        // are covered too.
        Arc::new(PgCanceller::new(self.pool.clone()))
    }

    fn catalog(&self) -> Arc<dyn Catalog> {
        // Deliberately the *pool*, not a pinned client: catalog browsing must
        // keep working while a result cursor is open (that interleaving is
        // exactly what used to deadlock).
        Arc::new(PgCatalog::new(self.pool.clone()))
    }

    async fn begin(&self, opts: TxOpts) -> Result<Box<dyn Transaction>, DbError> {
        let cmd_tx = actor::spawn(
            self.pool.acquire().await?.into_guard(),
            opts.read_only,
            opts.isolation,
        );
        Ok(Box::new(PgTransaction::new(cmd_tx)))
    }

    async fn set_read_only(&self, on: bool) -> Result<Enforcement, DbError> {
        // Recorded on the pool, not fired once at one socket: every session
        // this connection owns — including ones dialled later, and ones
        // pinned by a cursor right now — reconciles to this before it is
        // handed out again.
        self.pool.set_read_only(on).await?;
        Ok(Enforcement::Server)
    }

    async fn close(&self) -> Result<(), DbError> {
        // Idempotent, and deliberately non-blocking: idle sessions have their
        // `Client` dropped here so the sockets shut down promptly, a session
        // still pinned by a live cursor is released by its own actor, and
        // every subsequent operation sees the closed flag and returns
        // `DbError::Closed`, per the trait's contract. Waiting for a pinned
        // session here would re-create the hang this design removes.
        self.pool.close();
        Ok(())
    }
}
