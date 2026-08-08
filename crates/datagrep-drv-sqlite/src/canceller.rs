//! Out-of-band cancellation (design §3.3: SQLite / DuckDB row = `ServerSide`
//! via `sqlite3_interrupt`).
//!
//! [`rusqlite::InterruptHandle`] is the one piece of rusqlite state that is
//! genuinely `Send + Sync` and safe to call from *any* thread while the
//! worker thread is blocked deep inside `sqlite3_step` — that is the whole
//! point of it, and why it bypasses the worker's command channel entirely
//! rather than being just another `WorkerMsg`. A message would have to wait
//! in line behind the very step call it's trying to abort.

use std::sync::Arc;

use datagrep_api::{BoxFuture, CancelKind, CancelOutcome, Canceller, DbError};

/// `rusqlite::InterruptHandle` does not implement `Clone` itself (it wraps
/// an internal `Arc` but doesn't expose one) — so `SqliteConnection` holds
/// one behind our own `Arc` and hands out clones of *that* to every
/// `Canceller` it mints, since `Connection::canceller()` can be called more
/// than once and must keep working after the first caller drops its handle.
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
        // `interrupt()` only sets a flag `sqlite3_step` checks between VM
        // opcodes — it returns before the running statement has actually
        // noticed and unwound. We have no ack that it did, so `Requested`
        // (not `ServerCancelled`) is the honest outcome (design §3.3).
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
        // `interrupt()` (called synchronously inside `cancel()`, before the
        // returned future is ever polled — see the method body) only does
        // something to a statement that is genuinely executing. Rather than
        // racing two OS threads against a huge query — which is exactly the
        // kind of test that goes flaky for an unrelated reason under a busy
        // CI runner — this drives the interrupt from a `progress_handler`
        // callback, which SQLite calls synchronously *during* the running
        // step. That makes the "does an in-flight step actually see the
        // interrupt" question deterministic; the real cross-thread
        // scheduling (another task calling `cancel()` while the worker
        // thread is mid-step) is exercised end-to-end by
        // `tests/cancel.rs`'s `interrupt_mid_scan_...` test.
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
