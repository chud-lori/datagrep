//! [`MySqlCanceller`] (design §3.3): `KILL QUERY <conn_id>` issued from a
//! *second*, pooled connection — the pinned primary connection is busy
//! executing the very statement being killed, so it cannot deliver the kill
//! itself. The pool holds no connections until the first cancel (min 0).

use mysql_async::prelude::Queryable;
use mysql_async::Pool;
use tracing::Instrument as _;

use datagrep_api::driver::{BoxFuture, CancelKind, CancelOutcome, Canceller};
use datagrep_api::error::DbError;

use crate::error::map_mysql_error;

pub struct MySqlCanceller {
    pool: Pool,
    conn_id: u32,
}

impl MySqlCanceller {
    pub fn new(pool: Pool, conn_id: u32) -> Self {
        Self { pool, conn_id }
    }
}

impl Canceller for MySqlCanceller {
    fn kind(&self) -> CancelKind {
        CancelKind::ServerSide
    }

    fn cancel(&self) -> BoxFuture<'_, Result<CancelOutcome, DbError>> {
        Box::pin(
            async move {
                let mut conn = self.pool.get_conn().await.map_err(map_mysql_error)?;
                // `conn_id` is the u32 the server itself reported at
                // handshake — a number we own, not user input; KILL takes no
                // bound parameters.
                conn.query_drop(format!("KILL QUERY {}", self.conn_id))
                    .await
                    .map_err(map_mysql_error)?;
                // The OK to `KILL QUERY` means the server set the kill flag
                // on that thread — the victim statement dies at its next
                // check point, which is asynchronous. `Requested` is the
                // honest outcome; claiming `ServerCancelled` would assert an
                // ack the protocol doesn't give us.
                Ok(CancelOutcome::Requested)
            }
            .instrument(tracing::info_span!(
                "mysql_kill_query",
                conn_id = self.conn_id
            )),
        )
    }
}
