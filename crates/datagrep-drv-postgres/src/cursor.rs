use std::sync::Arc;

use async_trait::async_trait;
use tokio::sync::{mpsc, oneshot};

use datagrep_api::driver::{Batch, Cursor, CursorStats, FetchHint, Payload, ResumeToken};
use datagrep_api::error::DbError;
use datagrep_api::shape::{RowSchema, Shape};

use crate::actor::{decode_rows, ActorCmd};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionOwnership {
    Owned,
    Borrowed,
}

pub struct PgCursor {
    cmd_tx: mpsc::Sender<ActorCmd>,
    portal_id: u64,
    shape: Shape,
    stats: CursorStats,
    exhausted: bool,
    closed: bool,
    ownership: SessionOwnership,
    released: bool,
}

impl PgCursor {
    pub fn new(
        cmd_tx: mpsc::Sender<ActorCmd>,
        portal_id: u64,
        schema: RowSchema,
        ownership: SessionOwnership,
    ) -> Self {
        Self {
            cmd_tx,
            portal_id,
            shape: Shape::Table(Arc::new(schema)),
            stats: CursorStats::default(),
            exhausted: false,
            closed: false,
            ownership,
            released: false,
        }
    }

    async fn release_session(&mut self) {
        if self.released {
            return;
        }
        self.released = true;
        match self.ownership {
            SessionOwnership::Owned => {
                let (reply_tx, reply_rx) = oneshot::channel();
                if self
                    .cmd_tx
                    .send(ActorCmd::Rollback { reply: reply_tx })
                    .await
                    .is_ok()
                {
                    let _ = reply_rx.await;
                }
            }
            SessionOwnership::Borrowed => {
                let _ = self
                    .cmd_tx
                    .send(ActorCmd::CloseCursor {
                        portal_id: self.portal_id,
                    })
                    .await;
            }
        }
    }
}

#[async_trait]
impl Cursor for PgCursor {
    fn shape(&self) -> &Shape {
        &self.shape
    }

    #[tracing::instrument(skip(self), fields(portal_id = self.portal_id, rows_so_far = self.stats.rows))]
    async fn next_batch(&mut self, hint: FetchHint) -> Result<Option<Batch>, DbError> {
        if self.closed {
            return Err(DbError::Closed);
        }
        if self.exhausted {
            return Ok(None);
        }
        let max_rows = hint.max_rows.max(1) as i32;
        let (reply_tx, reply_rx) = oneshot::channel();
        self.cmd_tx
            .send(ActorCmd::FetchBatch {
                portal_id: self.portal_id,
                max_rows,
                reply: reply_tx,
            })
            .await
            .map_err(|_| DbError::Closed)?;
        let rows = reply_rx.await.map_err(|_| DbError::Closed)??;

        if rows.is_empty() {
            self.exhausted = true;
            self.release_session().await;
            return Ok(None);
        }
        if rows.len() < max_rows as usize {
            self.exhausted = true;
        }

        let n = rows.len() as u64;
        let bytes: u64 = rows.iter().map(|r| r.raw_size_bytes() as u64).sum();
        let decoded = decode_rows(rows);

        self.stats.rows += n;
        self.stats.bytes += bytes;
        self.stats.batches += 1;

        if self.exhausted {
            self.release_session().await;
        }

        Ok(Some(Batch {
            seq: self.stats.batches - 1,
            payload: Payload::Rows(decoded),
            schema_delta: Vec::new(),
            notices: Vec::new(),
        }))
    }

    fn resume_token(&self) -> Option<ResumeToken> {
        None
    }

    fn stats(&self) -> CursorStats {
        self.stats
    }

    async fn close(&mut self) -> Result<(), DbError> {
        if self.closed {
            return Ok(());
        }
        self.closed = true;
        self.release_session().await;
        Ok(())
    }
}

pub struct AckCursor {
    shape: Shape,
    done: bool,
}

impl AckCursor {
    pub fn new(affected: u64) -> Self {
        Self {
            shape: Shape::Ack {
                affected: Some(affected),
                message: None,
            },
            done: false,
        }
    }
}

#[async_trait]
impl Cursor for AckCursor {
    fn shape(&self) -> &Shape {
        &self.shape
    }

    async fn next_batch(&mut self, _hint: FetchHint) -> Result<Option<Batch>, DbError> {
        if self.done {
            return Ok(None);
        }
        self.done = true;
        Ok(Some(Batch::default()))
    }

    fn resume_token(&self) -> Option<ResumeToken> {
        None
    }

    fn stats(&self) -> CursorStats {
        CursorStats::default()
    }

    async fn close(&mut self) -> Result<(), DbError> {
        self.done = true;
        Ok(())
    }
}
