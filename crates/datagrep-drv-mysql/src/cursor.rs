//! [`MySqlCursor`]: the streaming cursor over one result set served by the
//! actor, plus the trivial `Ack` cursor for row-less statements.

use std::sync::Arc;

use async_trait::async_trait;
use tokio::sync::{mpsc, oneshot};

use datagrep_api::driver::{
    Batch, Cursor, CursorStats, FetchHint, Notice, NoticeSeverity, Payload, ResumeToken,
};
use datagrep_api::error::DbError;
use datagrep_api::shape::{RowSchema, Shape};

use crate::actor::ActorCmd;

/// Pull-based cursor (design §3.2): each `next_batch` asks the actor for at
/// most `hint.max_rows` rows; between pulls nothing is read off the socket,
/// which is the entire backpressure story.
pub struct MySqlCursor {
    cmd_tx: mpsc::Sender<ActorCmd>,
    cursor_id: u64,
    shape: Shape,
    stats: CursorStats,
    exhausted: bool,
    closed: bool,
}

impl MySqlCursor {
    pub fn new(cmd_tx: mpsc::Sender<ActorCmd>, cursor_id: u64, schema: RowSchema) -> Self {
        Self {
            cmd_tx,
            cursor_id,
            shape: Shape::Table(Arc::new(schema)),
            stats: CursorStats::default(),
            exhausted: false,
            closed: false,
        }
    }
}

#[async_trait]
impl Cursor for MySqlCursor {
    fn shape(&self) -> &Shape {
        &self.shape
    }

    #[tracing::instrument(skip(self), fields(cursor_id = self.cursor_id, rows_so_far = self.stats.rows))]
    async fn next_batch(&mut self, hint: FetchHint) -> Result<Option<Batch>, DbError> {
        if self.closed {
            return Err(DbError::Closed);
        }
        if self.exhausted {
            return Ok(None);
        }
        let (reply_tx, reply_rx) = oneshot::channel();
        self.cmd_tx
            .send(ActorCmd::FetchBatch {
                cursor_id: self.cursor_id,
                max_rows: hint.max_rows.max(1),
                reply: reply_tx,
            })
            .await
            .map_err(|_| DbError::Closed)?;
        let fetched = reply_rx.await.map_err(|_| DbError::Closed)??;

        if fetched.done {
            self.exhausted = true;
        }
        if fetched.rows.is_empty() {
            return Ok(None);
        }

        let n = fetched.rows.len() as u64;
        // Approximate wire size: the MySQL text/binary row size isn't
        // surfaced by mysql_async, so estimate from decoded values only for
        // the stats line — never used for correctness.
        let bytes: u64 = fetched.rows.iter().flatten().map(approx_value_bytes).sum();
        self.stats.rows += n;
        self.stats.bytes += bytes;
        self.stats.batches += 1;

        let notices = if fetched.done && fetched.warnings > 0 {
            vec![Notice {
                severity: NoticeSeverity::Warning,
                code: None,
                message: Arc::from(format!(
                    "server reported {} warning(s); run SHOW WARNINGS for detail",
                    fetched.warnings
                )),
            }]
        } else {
            Vec::new()
        };

        Ok(Some(Batch {
            seq: self.stats.batches - 1,
            payload: Payload::Rows(fetched.rows),
            schema_delta: Vec::new(),
            notices,
        }))
    }

    fn resume_token(&self) -> Option<ResumeToken> {
        // v1: none. The result set lives on the pinned connection; once the
        // stream is dropped the server-side state is gone — a keyset resume
        // belongs to `Op::Scan { resume }` layered above, same stance as the
        // sibling drivers.
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
        // Best-effort: the actor drains the remaining rows so the
        // connection is not poisoned (see actor.rs module docs). If the
        // actor already exited, there is nothing left to drain.
        let _ = self
            .cmd_tx
            .send(ActorCmd::CloseCursor {
                cursor_id: self.cursor_id,
            })
            .await;
        Ok(())
    }
}

fn approx_value_bytes(v: &datagrep_api::Value) -> u64 {
    use datagrep_api::Value;
    match v {
        Value::Null | Value::Absent | Value::Bool(_) => 1,
        Value::I64(_) | Value::U64(_) | Value::F64(_) => 8,
        Value::Decimal(s) | Value::Str(s) | Value::Json(s) => s.len() as u64,
        Value::Bytes(b) => b.len() as u64,
        Value::Date(_) => 4,
        Value::Time { .. } | Value::Timestamp { .. } => 8,
        Value::Unsupported { raw, .. } => raw.len() as u64,
        _ => 16,
    }
}

/// One-shot `Ack`-shaped cursor for statements that produce no rows.
pub struct AckCursor {
    shape: Shape,
    notices: Vec<Notice>,
    done: bool,
}

impl AckCursor {
    pub fn new(affected: u64, message: Option<String>, warnings: u16) -> Self {
        let notices = if warnings > 0 {
            vec![Notice {
                severity: NoticeSeverity::Warning,
                code: None,
                message: Arc::from(format!(
                    "server reported {warnings} warning(s); run SHOW WARNINGS for detail"
                )),
            }]
        } else {
            Vec::new()
        };
        Self {
            shape: Shape::Ack {
                affected: Some(affected),
                message: message.map(Arc::from),
            },
            notices,
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
        Ok(Some(Batch {
            notices: std::mem::take(&mut self.notices),
            ..Batch::default()
        }))
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
