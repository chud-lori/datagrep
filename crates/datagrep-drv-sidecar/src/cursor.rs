use std::sync::Arc;

use async_trait::async_trait;
use serde_json::json;

use datagrep_api::driver::{Batch, Cursor, CursorStats, FetchHint, Payload, ResumeToken};
use datagrep_api::error::DbError;
use datagrep_api::shape::{LogicalType, Shape};

use crate::connection::Link;
use crate::process::KILL_GRACE;
use crate::wire::{decode_cell, FetchReply};

pub struct SidecarCursor {
    link: Arc<Link>,
    id: u64,
    shape: Shape,
    types: Vec<LogicalType>,
    stats: CursorStats,
    done: bool,
    closed: bool,
}

impl SidecarCursor {
    pub(crate) fn new(link: Arc<Link>, id: u64, shape: Shape) -> Self {
        let types = match &shape {
            Shape::Table(schema) => schema.fields.iter().map(|f| f.logical).collect(),
            _ => Vec::new(),
        };
        Self {
            link,
            id,
            shape,
            types,
            stats: CursorStats::default(),
            // Cursor 0 is an ack the sidecar never registered.
            done: id == 0,
            closed: false,
        }
    }
}

#[async_trait]
impl Cursor for SidecarCursor {
    fn shape(&self) -> &Shape {
        &self.shape
    }

    async fn next_batch(&mut self, hint: FetchHint) -> Result<Option<Batch>, DbError> {
        if self.closed {
            return Err(DbError::Closed);
        }
        if self.done {
            return Ok(None);
        }
        let (reply, bytes) = self
            .link
            .call(
                "fetch",
                json!({
                    "cursor": self.id,
                    "max_rows": hint.max_rows,
                    "max_bytes": hint.max_bytes,
                    "target_ms": hint.target_ms,
                }),
            )
            .await?;
        let reply: FetchReply = serde_json::from_value(reply)
            .map_err(|e| DbError::Protocol(format!("bad fetch reply: {e}")))?;
        let Some(batch) = reply.batch else {
            self.done = true;
            return Ok(None);
        };
        if batch.rows.len() > hint.max_rows.max(1) as usize {
            return Err(DbError::Protocol(format!(
                "sidecar returned {} rows for a {}-row fetch",
                batch.rows.len(),
                hint.max_rows
            )));
        }
        let mut rows = Vec::with_capacity(batch.rows.len());
        for row in batch.rows {
            if row.len() != self.types.len() {
                return Err(DbError::Protocol(format!(
                    "row of {} cells for {} columns",
                    row.len(),
                    self.types.len()
                )));
            }
            let cells = row
                .into_iter()
                .zip(&self.types)
                .map(|(cell, ty)| decode_cell(cell, *ty))
                .collect::<Result<Vec<_>, _>>()
                .map_err(|e| DbError::Protocol(format!("bad cell: {e}")))?;
            rows.push(cells);
        }
        let seq = self.stats.batches;
        self.stats.batches += 1;
        self.stats.rows += rows.len() as u64;
        self.stats.bytes += bytes as u64;
        Ok(Some(Batch {
            seq,
            payload: Payload::Rows(rows),
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
        if self.closed || self.id == 0 {
            self.closed = true;
            return Ok(());
        }
        self.closed = true;
        let close = self.link.call("close_cursor", json!({ "cursor": self.id }));
        match tokio::time::timeout(KILL_GRACE, close).await {
            Ok(result) => result.map(|_| ()),
            Err(_) => {
                self.link.process.kill();
                Err(DbError::Timeout)
            }
        }
    }
}

impl Drop for SidecarCursor {
    fn drop(&mut self) {
        if self.closed || self.id == 0 {
            return;
        }
        let link = self.link.clone();
        let id = self.id;
        if let Ok(rt) = tokio::runtime::Handle::try_current() {
            rt.spawn(async move {
                let _ = link.call("close_cursor", json!({ "cursor": id })).await;
            });
        }
    }
}
