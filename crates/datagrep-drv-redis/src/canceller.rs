use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::Arc;

use datagrep_api::driver::{BoxFuture, CancelFlag, CancelKind, CancelOutcome, Canceller};
use datagrep_api::error::DbError;

use crate::error::map_redis_error;

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
