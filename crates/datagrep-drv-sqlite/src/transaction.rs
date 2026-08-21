use async_trait::async_trait;
use datagrep_api::{Cursor, DbError, Request, Transaction};

use crate::connection::JobSender;
use crate::cursor::SqliteCursor;

pub struct SqliteTransaction {
    jobs: JobSender,
}

impl SqliteTransaction {
    pub(crate) fn new(jobs: JobSender) -> Self {
        Self { jobs }
    }
}

#[async_trait]
impl Transaction for SqliteTransaction {
    async fn execute(&self, req: Request) -> Result<Box<dyn Cursor>, DbError> {
        let outcome = self.jobs.execute(req).await?;
        Ok(Box::new(SqliteCursor::new(outcome, self.jobs.clone())))
    }

    async fn commit(self: Box<Self>) -> Result<(), DbError> {
        self.jobs.commit().await
    }

    async fn rollback(self: Box<Self>) -> Result<(), DbError> {
        self.jobs.rollback().await
    }
}
