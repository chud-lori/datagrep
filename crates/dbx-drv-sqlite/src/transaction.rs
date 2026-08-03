//! An open transaction, pinned to its connection's worker thread — the
//! `JobSender` it holds routes `BEGIN`/statements/`COMMIT`/`ROLLBACK` to the
//! exact same `rusqlite::Connection` the owning `SqliteConnection` uses, so
//! there is no pool that could silently move a `BEGIN` to a different
//! socket (design §3.5).
//!
//! **Savepoints (nested transactions).** `dbx-api`'s `Transaction` trait has
//! no `begin`-from-a-transaction method, so nesting isn't a typed API here —
//! it's just SQL. Once a `Transaction` is open, executing `Request::Native`
//! text (`SAVEPOINT s1`, `RELEASE s1`, `ROLLBACK TO s1`) through
//! [`SqliteTransaction::execute`] works exactly as it would on any other
//! SQLite session, because it's the same `conn.execute_batch` path a plain
//! statement takes.

use async_trait::async_trait;
use dbx_api::{Cursor, DbError, Request, Transaction};

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
