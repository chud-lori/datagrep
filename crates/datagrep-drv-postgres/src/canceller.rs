//! [`PgCanceller`] (ticket item 4, design §3.3): out-of-band cancellation on
//! a second socket. Postgres's own protocol gives no acknowledgement, so the
//! honest outcome is always [`CancelOutcome::Requested`] — never
//! `ServerCancelled` — matching the design table's "racy by protocol design,
//! server sends no ack".

use tokio_postgres::CancelToken;
use tracing::Instrument as _;

use datagrep_api::driver::{BoxFuture, CancelKind, CancelOutcome, Canceller};
use datagrep_api::error::DbError;

use crate::error::map_pg_error;

pub struct PgCanceller {
    token: CancelToken,
}

impl PgCanceller {
    pub fn new(token: CancelToken) -> Self {
        Self { token }
    }
}

impl Canceller for PgCanceller {
    fn kind(&self) -> CancelKind {
        CancelKind::ServerSide
    }

    fn cancel(&self) -> BoxFuture<'_, Result<CancelOutcome, DbError>> {
        Box::pin(
            async move {
                self.token
                    .cancel_query(tokio_postgres::NoTls)
                    .await
                    .map_err(map_pg_error)?;
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
