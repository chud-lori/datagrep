//! [`RedisCanceller`] (design §3.3): "commands are atomic; a 'long query' is
//! our own SCAN loop → just stop" is the default and applies to almost
//! every command. The one real exception is a command that blocks the
//! connection waiting on the server (`BLPOP`, `WAIT`, `XREAD BLOCK`, …) —
//! for those, "just stop" leaves the connection hung until the server-side
//! timeout, so a genuine server-side kill (`CLIENT KILL ID` from a second
//! connection) is used instead.
//!
//! [`kind`](Canceller::kind) and the outcome of [`cancel`](Canceller::cancel)
//! are decided dynamically from `blocking_client_id` — which command family
//! is currently in flight on this connection — rather than being a single
//! static fact about the connection, because it isn't one: most calls are
//! `ClientAbandon`, one specific shape of call is `ServerSide`. See
//! `driver.rs`'s `REDIS_CAPS` doc comment for why that also means
//! `Caps::SERVER_CANCEL` is not set as a blanket flag.

use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::Arc;

use dbx_api::driver::{BoxFuture, CancelFlag, CancelKind, CancelOutcome, Canceller};
use dbx_api::error::DbError;

use crate::error::map_redis_error;

/// `0` means "no blocking command currently in flight on this connection".
/// Shared between `RedisConnection::execute` (which sets it just before
/// dispatching a detected blocking command, and clears it right after) and
/// this canceller (which reads it to decide `kind()`/`cancel()`).
pub type BlockingClientId = Arc<AtomicI64>;

pub struct RedisCanceller {
    flag: CancelFlag,
    blocking_client_id: BlockingClientId,
    client: redis::Client,
}

impl RedisCanceller {
    pub fn new(
        flag: CancelFlag,
        blocking_client_id: BlockingClientId,
        client: redis::Client,
    ) -> Self {
        Self {
            flag,
            blocking_client_id,
            client,
        }
    }

    fn current_blocking_id(&self) -> Option<i64> {
        match self.blocking_client_id.load(Ordering::Acquire) {
            0 => None,
            id => Some(id),
        }
    }
}

impl Canceller for RedisCanceller {
    fn kind(&self) -> CancelKind {
        match self.current_blocking_id() {
            Some(_) => CancelKind::ServerSide,
            None => CancelKind::ClientAbandon,
        }
    }

    fn cancel(&self) -> BoxFuture<'_, Result<CancelOutcome, DbError>> {
        Box::pin(async move {
            // Always set: stops our own SCAN loop at its next round-trip
            // check (`RedisPairsCursor`/`ListCursor`/`StreamCursor`),
            // regardless of whether a blocking command also needs killing.
            self.flag.cancel();

            let Some(id) = self.current_blocking_id() else {
                return Ok(CancelOutcome::ClientAbandoned);
            };

            let mut conn = self
                .client
                .get_multiplexed_async_connection()
                .await
                .map_err(map_redis_error)?;
            let killed: i64 = redis::cmd("CLIENT")
                .arg("KILL")
                .arg("ID")
                .arg(id)
                .query_async(&mut conn)
                .await
                .map_err(map_redis_error)?;
            tracing::info!(
                client_id = id,
                killed,
                "issued CLIENT KILL ID for a blocking command"
            );
            if killed > 0 {
                Ok(CancelOutcome::ServerCancelled)
            } else {
                // The blocking command may have completed on its own
                // between the check above and the kill landing — not a
                // failure, just a race we lost harmlessly.
                Ok(CancelOutcome::ClientAbandoned)
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kind_follows_blocking_client_id() {
        let id: BlockingClientId = Arc::new(AtomicI64::new(0));
        let flag = CancelFlag::new();
        let client = redis::Client::open("redis://localhost:6379").unwrap();
        let canceller = RedisCanceller::new(flag, id.clone(), client);
        assert_eq!(canceller.kind(), CancelKind::ClientAbandon);
        id.store(42, Ordering::Release);
        assert_eq!(canceller.kind(), CancelKind::ServerSide);
    }
}
