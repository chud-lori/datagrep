//! [`PgTransaction`]: the explicit, interactive transaction returned by
//! [`crate::connection::PgConnection::begin`] — pinned to its connection's
//! socket for its whole life (design §3.5: "a pool that silently moves a
//! BEGIN to a different socket is a correctness bug"), which falls out for
//! free here because the backing actor holds the connection's client mutex
//! locked for exactly that long (see `actor.rs`).

use async_trait::async_trait;
use tokio::sync::{mpsc, oneshot};

use dbx_api::driver::{Cursor, Transaction};
use dbx_api::error::DbError;
use dbx_api::request::{Op, Request};
use dbx_api::value::Value;

use crate::actor::{ActorCmd, ExecOutcome};
use crate::cursor::{AckCursor, PgCursor};

pub struct PgTransaction {
    cmd_tx: mpsc::Sender<ActorCmd>,
}

impl PgTransaction {
    pub fn new(cmd_tx: mpsc::Sender<ActorCmd>) -> Self {
        Self { cmd_tx }
    }

    fn compile(req: &Request) -> Result<(String, Vec<Value>), DbError> {
        match req {
            Request::Native { text, params, .. } => Ok((text.to_string(), params.clone())),
            Request::Op(Op::Scan { path, filter, order, project, limit, .. }) => {
                crate::sql::compile_scan(path, filter, order, project, *limit)
            }
            Request::Op(Op::Count { path, filter, exact }) => crate::sql::compile_count(path, filter, *exact),
            Request::Op(Op::Explain { inner, analyze }) => {
                let (inner_sql, params) = Self::compile(inner)?;
                Ok((crate::sql::wrap_explain(&inner_sql, *analyze), params))
            }
            Request::Op(Op::Ddl(dbx_api::request::DdlOp::Native { text })) => Ok((text.to_string(), Vec::new())),
            Request::Op(Op::Mutate(_)) => Err(DbError::Unsupported {
                feature: "Op::Mutate inside an explicit interactive transaction is not implemented in v1 \
                          (only PgConnection::execute's auto-committing mutation path is); issue the \
                          equivalent Native UPDATE/INSERT/DELETE text instead"
                    .into(),
            }),
        }
    }
}

#[async_trait]
impl Transaction for PgTransaction {
    async fn execute(&self, req: Request) -> Result<Box<dyn Cursor>, DbError> {
        let (text, params) = Self::compile(&req)?;
        let (reply_tx, reply_rx) = oneshot::channel();
        self.cmd_tx
            .send(ActorCmd::Execute {
                text,
                params,
                reply: reply_tx,
            })
            .await
            .map_err(|_| DbError::Closed)?;
        match reply_rx.await.map_err(|_| DbError::Closed)?? {
            ExecOutcome::Ack { affected } => Ok(Box::new(AckCursor::new(affected))),
            ExecOutcome::Cursor { portal_id, schema } => Ok(Box::new(PgCursor::new(
                self.cmd_tx.clone(),
                portal_id,
                schema,
            ))),
        }
    }

    async fn commit(self: Box<Self>) -> Result<(), DbError> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.cmd_tx
            .send(ActorCmd::Commit { reply: reply_tx })
            .await
            .map_err(|_| DbError::Closed)?;
        reply_rx.await.map_err(|_| DbError::Closed)?
    }

    async fn rollback(self: Box<Self>) -> Result<(), DbError> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.cmd_tx
            .send(ActorCmd::Rollback { reply: reply_tx })
            .await
            .map_err(|_| DbError::Closed)?;
        reply_rx.await.map_err(|_| DbError::Closed)?
    }
}
