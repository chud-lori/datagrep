//! [`PgCursor`] and the trivial `Ack` cursor for non-`SELECT` statements.

use std::sync::Arc;

use async_trait::async_trait;
use tokio::sync::{mpsc, oneshot};

use datagrep_api::driver::{Batch, Cursor, CursorStats, FetchHint, Payload, ResumeToken};
use datagrep_api::error::DbError;
use datagrep_api::shape::{RowSchema, Shape};

use crate::actor::{decode_rows, ActorCmd};

/// Who owns the transaction this cursor's portal lives in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionOwnership {
    /// The cursor owns the transparent read-only transaction
    /// [`crate::connection::PgConnection::execute`] wrapped around it, and
    /// with it the pooled session that transaction pins. It must end that
    /// transaction as soon as the portal is drained — see `pool.rs`.
    Owned,
    /// The portal lives inside a caller-visible
    /// [`crate::transaction::PgTransaction`]. Draining the cursor must close
    /// the portal but must never touch the caller's transaction.
    Borrowed,
}

/// A streaming cursor over one bound portal: pulls exactly one chunk per
/// `next_batch`, with the driver picking the real size within `hint`.
///
/// A cursor holds its pooled session only until the portal is **drained**
/// (short or empty batch), or until it is explicitly closed — not until the
/// handle is dropped. Holding it to drop was the deadlock reported in
/// TEST-REPORT.md F2: a fully-read cursor still in scope blocked every later
/// catalog lookup and query on the same connection, forever.
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

    /// Give the pooled session back. For an [`SessionOwnership::Owned`]
    /// cursor that means rolling back the transparent read-only wrapper
    /// transaction — which also drops the server-side `ACCESS SHARE` locks it
    /// was holding, so a following `DROP TABLE` on the same relation does not
    /// block. For a borrowed one it only closes the portal.
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
                    // Wait for the rollback to land: the point is that the
                    // *server* is done with this transaction before the next
                    // statement runs, not merely that we asked.
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
        // A short batch (fewer rows than requested) means the portal is
        // drained; avoid one more pointless round trip next call — and hand
        // the pooled session straight back rather than waiting for drop.
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
            // Postgres NOTICE plumbing is not wired up in v1 — the
            // background `Connection` future receives `AsyncMessage::Notice`
            // but nothing here polls it yet (see crate-level gap notes).
            notices: Vec::new(),
        }))
    }

    fn resume_token(&self) -> Option<ResumeToken> {
        // v1: always None. The portal backing this cursor lives inside a
        // transaction that ends as soon as the cursor is drained, closed or
        // dropped — there is nothing to resume *into*, since the
        // server-side portal is gone the moment the wrapping transaction
        // commits or rolls back. Keyset-based resume (re-issuing `Op::Scan`
        // with a `resume` bound derived from the last row) is a `datagrep-core`
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
        // transaction committed elsewhere), the sends silently no-op.
        self.release_session().await;
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
