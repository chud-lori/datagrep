use std::sync::Arc;

use datagrep_api::{BoxFuture, CancelKind, CancelOutcome, Canceller, DbError};

pub struct SqliteCanceller {
    handle: Arc<rusqlite::InterruptHandle>,
}

impl SqliteCanceller {
    pub(crate) fn new(handle: Arc<rusqlite::InterruptHandle>) -> Self {
        Self { handle }
    }
}

impl Canceller for SqliteCanceller {
    fn kind(&self) -> CancelKind {
        CancelKind::ServerSide
    }

    fn cancel(&self) -> BoxFuture<'_, Result<CancelOutcome, DbError>> {
        self.handle.interrupt();
        Box::pin(async { Ok(CancelOutcome::Requested) })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn cancel_reports_server_side_and_requested() {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        let handle = Arc::new(conn.get_interrupt_handle());
        let canceller = SqliteCanceller::new(handle);
        assert_eq!(canceller.kind(), CancelKind::ServerSide);
        let outcome = canceller.cancel().await.expect("cancel failed");
        assert_eq!(outcome, CancelOutcome::Requested);
    }

    #[test]
    fn cancel_aborts_a_running_step() {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        let handle = Arc::new(conn.get_interrupt_handle());
        let canceller = SqliteCanceller::new(handle);

        conn.progress_handler(
            1,
            Some(move || {
                drop(canceller.cancel());
                false
            }),
        );

        let err = conn
            .execute_batch(
                "WITH RECURSIVE c(x) AS ( \
                     SELECT 1 UNION ALL SELECT x + 1 FROM c LIMIT 1000000 \
                 ) SELECT count(*) FROM c;",
            )
            .expect_err("interrupted statement should error");
        assert!(err.to_string().to_lowercase().contains("interrupt"));
    }
}
