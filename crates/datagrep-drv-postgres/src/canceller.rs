use std::sync::Arc;

use tracing::Instrument as _;

use datagrep_api::driver::{BoxFuture, CancelKind, CancelOutcome, Canceller};
use datagrep_api::error::DbError;

use crate::error::map_pg_error;
use crate::pool::PgPool;

pub struct PgCanceller {
    pool: Arc<PgPool>,
}

impl PgCanceller {
    pub fn new(pool: Arc<PgPool>) -> Self {
        Self { pool }
    }
}

impl Canceller for PgCanceller {
    fn kind(&self) -> CancelKind {
        CancelKind::ServerSide
    }

    fn cancel(&self) -> BoxFuture<'_, Result<CancelOutcome, DbError>> {
        Box::pin(
            async move {
                let tokens = self.pool.cancel_tokens();
                let mut sent = 0usize;
                let mut last_err = None;
                for token in &tokens {
                    match token.cancel_query(tokio_postgres::NoTls).await {
                        Ok(()) => sent += 1,
                        Err(e) => last_err = Some(map_pg_error(e)),
                    }
                }
                if sent == 0 {
                    if let Some(e) = last_err {
                        return Err(e);
                    }
                }
                Ok(CancelOutcome::Requested)
            }
            .instrument(tracing::info_span!("pg_cancel_query")),
        )
    }
}
