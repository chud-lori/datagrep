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
                conn.query_drop(format!("KILL QUERY {}", self.conn_id))
                    .await
                    .map_err(map_mysql_error)?;
                Ok(CancelOutcome::Requested)
            }
            .instrument(tracing::info_span!(
                "mysql_kill_query",
                conn_id = self.conn_id
            )),
        )
    }
}
