use std::sync::Arc;

use async_trait::async_trait;
use tokio::sync::oneshot;

use datagrep_api::caps::Capabilities;
use datagrep_api::catalog::Catalog;
use datagrep_api::driver::{
    Canceller, Connection, Cursor, Enforcement, ServerInfo, Transaction, TxOpts,
};
use datagrep_api::error::DbError;
use datagrep_api::request::{Mutation, MutationBatch, Op, Request};
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
            Op::Ddl(ddl) => Ok((sql::compile_ddl(ddl)?, Vec::new())),
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
        Arc::new(PgCanceller::new(self.pool.clone()))
    }

    fn catalog(&self) -> Arc<dyn Catalog> {
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
        self.pool.set_read_only(on).await?;
        Ok(Enforcement::Server)
    }

    async fn close(&self) -> Result<(), DbError> {
        self.pool.close();
        Ok(())
    }
}
