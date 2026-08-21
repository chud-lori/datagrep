use datagrep_api::driver::{Batch, Cursor, FetchHint};
use datagrep_api::error::DbError;
use datagrep_api::request::Request;
use datagrep_api::shape::Shape;

use crate::feeder::payload_rows;
use crate::session::ConnLease;

pub const EXPORT_FETCH_HINT: FetchHint = FetchHint {
    max_rows: 100_000,
    max_bytes: 4 * 1024 * 1024,
    target_ms: 250,
};

pub trait ExportSink: Send {
    fn begin(&mut self, shape: &Shape) -> Result<(), DbError>;

    fn chunk(&mut self, batch: Batch) -> Result<SinkFlow, DbError>;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SinkFlow {
    Continue,
    Stop,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ExportStats {
    pub rows: u64,
    pub batches: u64,
    pub bytes: u64,
    pub stopped: bool,
}

pub async fn run_export_on(
    lease: &ConnLease,
    req: Request,
    sink: &mut dyn ExportSink,
) -> Result<ExportStats, DbError> {
    let mut cursor = lease.execute(req).await?;
    let result = drive(cursor.as_mut(), sink).await;
    if let Err(err) = cursor.close().await {
        tracing::warn!(%err, "closing cursor after export");
    }
    result
}

async fn drive(cursor: &mut dyn Cursor, sink: &mut dyn ExportSink) -> Result<ExportStats, DbError> {
    sink.begin(cursor.shape())?;
    let mut stats = ExportStats::default();
    loop {
        let Some(batch) = cursor.next_batch(EXPORT_FETCH_HINT).await? else {
            break;
        };
        stats.rows += payload_rows(&batch) as u64;
        stats.batches += 1;
        if sink.chunk(batch)? == SinkFlow::Stop {
            stats.stopped = true;
            break;
        }
    }
    stats.bytes = cursor.stats().bytes;
    Ok(stats)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::{CoreApi, ProfileId};
    use crate::session::PoolPolicy;
    use crate::store::{MemoryPolicy, SpillPolicy};
    use crate::testing::{MockDriver, MockPlan};
    use datagrep_api::config::ConnectionConfig;
    use datagrep_api::driver::Payload;
    use std::sync::Arc;

    #[derive(Default)]
    struct CountingSink {
        rows: u64,
        chunks: u64,
        max_chunk_rows: usize,
        saw_shape: bool,
        stop_after_rows: Option<u64>,
    }

    impl ExportSink for CountingSink {
        fn begin(&mut self, shape: &Shape) -> Result<(), DbError> {
            self.saw_shape = matches!(shape, Shape::Table(_));
            Ok(())
        }

        fn chunk(&mut self, batch: Batch) -> Result<SinkFlow, DbError> {
            let rows = match &batch.payload {
                Payload::Rows(rows) => rows.len(),
                _ => 0,
            };
            self.rows += rows as u64;
            self.chunks += 1;
            self.max_chunk_rows = self.max_chunk_rows.max(rows);
            // `batch` drops here — nothing is kept.
            if self.stop_after_rows.is_some_and(|cap| self.rows >= cap) {
                return Ok(SinkFlow::Stop);
            }
            Ok(SinkFlow::Continue)
        }
    }

    async fn core_with(plan: MockPlan) -> (CoreApi, ProfileId, Arc<crate::testing::MockCounters>) {
        let policy = MemoryPolicy {
            total_result_budget: 16 * 1024 * 1024,
            per_query_hot: 16 * 1024 * 1024,
            hot_window_rows: usize::MAX,
            soft_row_cap: 500_000,
            spill: SpillPolicy::Disabled,
        };
        let core = CoreApi::with_policy(policy, PoolPolicy::default());
        let driver = Arc::new(MockDriver::with_plan(plan));
        let counters = driver.counters();
        core.register_driver("mock", move || driver.clone());
        let id = core
            .add_profile(
                "local",
                ConnectionConfig {
                    driver: Arc::from("mock"),
                    values: Default::default(),
                },
            )
            .await;
        (core, id, counters)
    }

    #[tokio::test]
    async fn export_streams_200k_rows_without_store_growth() {
        let (core, id, _counters) = core_with(MockPlan {
            batches: Some(400),
            rows_per_batch: 500,
            ..MockPlan::default()
        })
        .await;

        let mut sink = CountingSink::default();
        let stats = core
            .run_export(id, Request::native("select * from events"), &mut sink)
            .await
            .expect("export");

        assert_eq!(stats.rows, 200_000);
        assert_eq!(sink.rows, 200_000);
        assert!(sink.saw_shape, "the sink saw the shape before any chunk");
        assert!(!stats.stopped);
        assert!(
            sink.max_chunk_rows <= EXPORT_FETCH_HINT.max_rows as usize,
            "a single chunk exceeded the fetch hint"
        );
        assert_eq!(
            core.result_bytes(),
            0,
            "export leaked result bytes into the store budget"
        );
        assert!(
            core.queries().open_queries().is_empty(),
            "export registered a query in the store path"
        );
        core.shutdown().await;
    }

    #[tokio::test]
    async fn export_ignores_the_soft_row_cap_and_delivers_every_row() {
        let (core, id, _counters) = core_with(MockPlan {
            batches: Some(1_200),
            rows_per_batch: 500,
            ..MockPlan::default()
        })
        .await;

        let mut sink = CountingSink::default();
        let stats = core
            .run_export(id, Request::native("select * from events"), &mut sink)
            .await
            .expect("export");

        assert_eq!(
            stats.rows, 600_000,
            "export was capped — it must deliver every row"
        );
        assert_eq!(sink.rows, 600_000);
        assert!(!stats.stopped);
        core.shutdown().await;
    }

    #[tokio::test]
    async fn a_sink_can_stop_an_export_early() {
        let (core, id, counters) = core_with(MockPlan::infinite(500)).await;

        let mut sink = CountingSink {
            stop_after_rows: Some(1_000),
            ..CountingSink::default()
        };
        let stats = core
            .run_export(id, Request::native("select * from events"), &mut sink)
            .await
            .expect("export");
        assert!(stats.stopped);
        assert!(stats.rows >= 1_000);
        assert_eq!(counters.cursor_closes(), 1, "cursor left open after stop");
        core.shutdown().await;
    }
}
