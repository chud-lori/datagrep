use async_trait::async_trait;
use tokio::sync::{mpsc, oneshot};

use datagrep_api::driver::{Cursor, Transaction};
use datagrep_api::error::DbError;
use datagrep_api::request::{Op, Request};

use crate::actor::{ActorCmd, ExecOutcome};
use crate::connection::MySqlConnection;
use crate::cursor::{AckCursor, MySqlCursor};
use crate::sql::Flavor;

pub struct MySqlTransaction {
    cmd_tx: mpsc::Sender<ActorCmd>,
    flavor: Flavor,
}

impl MySqlTransaction {
    pub fn new(cmd_tx: mpsc::Sender<ActorCmd>, flavor: Flavor) -> Self {
        Self { cmd_tx, flavor }
    }
}

#[async_trait]
impl Transaction for MySqlTransaction {
    async fn execute(&self, req: Request) -> Result<Box<dyn Cursor>, DbError> {
        if matches!(req, Request::Op(Op::Mutate(_))) {
            return Err(DbError::Unsupported {
                feature: "Op::Mutate inside an explicit interactive transaction is not \
                          implemented in v1; issue the equivalent Native \
                          UPDATE/INSERT/DELETE text instead"
                    .into(),
            });
        }
        let compiled = MySqlConnection::compile(&req, self.flavor)?;
        let (reply_tx, reply_rx) = oneshot::channel();
        self.cmd_tx
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
            ExecOutcome::Cursor { cursor_id, schema } => Ok(Box::new(MySqlCursor::new(
                self.cmd_tx.clone(),
                cursor_id,
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
