use async_trait::async_trait;
use datagrep_api::{Batch, Cursor, CursorStats, DbError, FetchHint, ResumeToken, Shape};

use crate::connection::{ExecOutcome, JobSender};

pub struct SqliteCursor {
    shape: Shape,
    jobs: JobSender,
    id: Option<u64>,
    resume_token: Option<ResumeToken>,
    stats: CursorStats,
    exhausted: bool,
}

impl SqliteCursor {
    pub(crate) fn new(outcome: ExecOutcome, jobs: JobSender) -> Self {
        match outcome {
            ExecOutcome::Ack { affected } => Self {
                shape: Shape::Ack {
                    affected,
                    message: None,
                },
                jobs,
                id: None,
                resume_token: None,
                stats: CursorStats {
                    rows: affected.unwrap_or(0),
                    bytes: 0,
                    batches: 0,
                    server_elapsed_micros: None,
                },
                // Nothing to fetch or release server-side.
                exhausted: true,
            },
            ExecOutcome::Cursor { id, schema } => Self {
                shape: Shape::Table(schema),
                jobs,
                id: Some(id),
                resume_token: None,
                stats: CursorStats::default(),
                exhausted: false,
            },
        }
    }
}

#[async_trait]
impl Cursor for SqliteCursor {
    fn shape(&self) -> &Shape {
        &self.shape
    }

    async fn next_batch(&mut self, hint: FetchHint) -> Result<Option<Batch>, DbError> {
        let Some(id) = self.id else {
            return Ok(None);
        };
        if self.exhausted {
            return Ok(None);
        }
        let reply = self.jobs.fetch_batch(id, hint).await?;
        if reply.resume_token.is_some() {
            self.resume_token = reply.resume_token;
        }
        self.stats = reply.stats;
        if reply.batch.is_none() {
            self.exhausted = true;
        }
        Ok(reply.batch)
    }

    fn resume_token(&self) -> Option<ResumeToken> {
        self.resume_token.clone()
    }

    fn stats(&self) -> CursorStats {
        self.stats
    }

    async fn close(&mut self) -> Result<(), DbError> {
        if self.exhausted {
            return Ok(());
        }
        self.exhausted = true;
        if let Some(id) = self.id {
            self.jobs.close_cursor(id).await?;
        }
        Ok(())
    }
}
