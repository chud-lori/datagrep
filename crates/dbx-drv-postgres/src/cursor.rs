//! [`PgCursor`] (ticket item 3) and the trivial `Ack` cursor for
//! non-`SELECT` statements.

use std::sync::Arc;

use async_trait::async_trait;
use tokio::sync::{mpsc, oneshot};

use dbx_api::driver::{Batch, Cursor, CursorStats, FetchHint, Payload, ResumeToken};
use dbx_api::error::DbError;
use dbx_api::shape::{RowSchema, Shape};

use crate::actor::{decode_rows, ActorCmd};

/// A streaming cursor over one bound portal (design §3.2: pulls exactly one
/// chunk per `next_batch`, driver picks the real size within `hint`).
pub struct PgCursor {
    cmd_tx: mpsc::Sender<ActorCmd>,
    portal_id: u64,
    shape: Shape,
    stats: CursorStats,
    exhausted: bool,
    closed: bool,
}

impl PgCursor {
    pub fn new(cmd_tx: mpsc::Sender<ActorCmd>, portal_id: u64, schema: RowSchema) -> Self {
        Self {
            cmd_tx,
            portal_id,
            shape: Shape::Table(Arc::new(schema)),
            stats: CursorStats::default(),
            exhausted: false,
            closed: false,
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
            return Ok(None);
        }
        // A short batch (fewer rows than requested) means the portal is
        // drained; avoid one more pointless round trip next call.
        if rows.len() < max_rows as usize {
            self.exhausted = true;
        }

        let n = rows.len() as u64;
        let bytes: u64 = rows.iter().map(|r| r.raw_size_bytes() as u64).sum();
        let decoded = decode_rows(rows);

        self.stats.rows += n;
        self.stats.bytes += bytes;
        self.stats.batches += 1;

        Ok(Some(Batch {
            seq: self.stats.batches - 1,
            payload: Payload::Rows(decoded),
            schema_delta: Vec::new(),
            // Postgres NOTICE plumbing is not wired up in v1 — the
            // background `Connection` future receives `AsyncMessage::Notice`
            // but nothing here polls it yet (see crate-level gap notes).
            notices: Vec::new(),
        }))
    }

    fn resume_token(&self) -> Option<ResumeToken> {
        // v1: always None. The portal backing this cursor lives inside a
        // transaction that ends when the cursor is dropped/closed (the
        // ticket's own note) — there is nothing to resume *into*, since the
        // server-side portal is gone the moment the wrapping transaction
        // commits or rolls back. Keyset-based resume (re-issuing `Op::Scan`
        // with a `resume` bound derived from the last row) is a `dbx-core`
        // concern layered on top of `Op::Scan { resume }`, not something
        // this cursor type can honestly hand back on its own.
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
        // Best-effort: if the actor already exited (e.g. explicit
        // transaction committed elsewhere), the send silently no-ops.
        let _ = self
            .cmd_tx
            .send(ActorCmd::CloseCursor {
                portal_id: self.portal_id,
            })
            .await;
        Ok(())
    }
}

/// A one-shot `Ack`-shaped cursor for non-`SELECT` statements (design:
/// `Shape::Ack { affected, message }`).
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
