//! [`PgCanceller`] (ticket item 4, design §3.3): out-of-band cancellation on
//! a second socket. Postgres's own protocol gives no acknowledgement, so the
//! honest outcome is always [`CancelOutcome::Requested`] — never
//! `ServerCancelled` — matching the design table's "racy by protocol design,
//! server sends no ack".

use std::sync::Arc;

use tracing::Instrument as _;

use datagrep_api::driver::{BoxFuture, CancelKind, CancelOutcome, Canceller};
use datagrep_api::error::DbError;

use crate::error::map_pg_error;
use crate::pool::PgPool;

/// Cancels every physical session the logical connection owns. One logical
/// connection can hold several sockets (see `pool.rs`) — a cursor pins one
/// while the next query runs on another — so cancelling only the first would
/// silently miss whichever session the user's slow query actually landed on.
/// Cancelling an idle backend is a no-op server-side, so the broad sweep is
/// safe.
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
                // Only fail if we had sessions to cancel and could not reach a
                // single one — one stale token (a session whose backend has
                // already gone away) must not turn a successful cancel into
                // an error the UI shows.
                if sent == 0 {
                    if let Some(e) = last_err {
                        return Err(e);
                    }
                }
                // Postgres's CancelRequest has no reply message at all (see
                // design §3.3 table); reaching this point only means the
                // cancel socket connected and wrote the request, not that the
                // server acted on it before the original query finished on
                // its own.
                Ok(CancelOutcome::Requested)
            }
            .instrument(tracing::info_span!("pg_cancel_query")),
        )
    }
}
