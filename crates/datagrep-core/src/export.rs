//! The store-free export path.
//!
//! Export is a separate streaming endpoint: it runs its own cursor at full
//! fetch size straight to a file and never goes through the result store.
//!
//! [`run_export_on`] drives one cursor directly into an [`ExportSink`]: each
//! chunk is pulled, handed to the sink, and dropped before the next pull.
//! The whole in-flight buffer is therefore **exactly one driver chunk**
//! (itself bounded by [`EXPORT_FETCH_HINT`]'s 4 MB byte ceiling) — no result
//! store, no global-budget accounting, no spill file, no hot window. That is
//! what makes "Export all" ≠ "load all": a 10M-row export writes to disk at
//! whatever rate the sink can absorb without the process's resident result
//! bytes ([`crate::store::GlobalBudget`]) moving at all, which is also the
//! white-box counter the tests below assert on.
//!
//! Backpressure still reaches the socket: the loop does not call
//! `next_batch` again until the sink has returned, so a slow disk stalls the
//! driver, which stalls the server — the same pull-only story as the feeder,
//! minus the store.

use datagrep_api::driver::{Batch, Cursor, FetchHint};
use datagrep_api::error::DbError;
use datagrep_api::request::Request;
use datagrep_api::shape::Shape;

use crate::feeder::payload_rows;
use crate::session::ConnLease;

/// The fetch hint for the export path: full fetch size, since nothing here is
/// waiting to be shown on screen. Rows are effectively bounded by the 4 MB
/// byte ceiling; the generous row cap keeps a
/// 10M-row export from doing 100k round trips, while one chunk stays small
/// enough that dropping the future (Ctrl-C) never abandons much work.
pub const EXPORT_FETCH_HINT: FetchHint = FetchHint {
    max_rows: 100_000,
    max_bytes: 4 * 1024 * 1024,
    target_ms: 250,
};

/// Where an export's chunks go. Implemented by frontends over their format
/// writers (CSV, NDJSON, Arrow IPC, …).
///
/// The contract that keeps the path streaming: `chunk` receives each batch
/// **by value** and the driver is not pulled again until it returns. A sink
/// that writes and forgets keeps the whole pipeline at one chunk of memory;
/// a sink that accumulates has re-implemented the store it was built to
/// bypass.
pub trait ExportSink: Send {
    /// Called exactly once, with the cursor's shape, before any chunk.
    fn begin(&mut self, shape: &Shape) -> Result<(), DbError>;

    /// One driver chunk. Return [`SinkFlow::Stop`] to end the export early
    /// (deadline, user interrupt); the cursor is closed and [`run_export_on`]
    /// returns normally with [`ExportStats::stopped`] set.
    fn chunk(&mut self, batch: Batch) -> Result<SinkFlow, DbError>;
}

/// Whether the sink wants the next chunk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SinkFlow {
    Continue,
    /// Stop pulling; close the cursor and return what was written so far.
    Stop,
}

/// End-of-export totals.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ExportStats {
    /// Rows handed to the sink.
    pub rows: u64,
    /// Chunks handed to the sink.
    pub batches: u64,
    /// Bytes the driver reported reading.
    pub bytes: u64,
    /// True when the sink stopped the export before end of stream.
    pub stopped: bool,
}

/// Run `req` on `lease`'s connection and stream the result straight into
/// `sink`, never touching a result store. The cursor is closed on every exit
/// path, success or failure, so an aborted export leaves no server-side
/// portal open.
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
        // Handed over by value and dropped inside the sink: this is the
        // path's entire buffer.
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

    /// A sink that writes-and-forgets, recording only counters — including
    /// the largest single chunk it ever held, the white-box proof that
    /// nothing upstream accumulated.
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

    /// A core over a no-spill policy plus a mock driver, with a profile ready
    /// to export from.
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

    /// **Export never goes through the store.** ~200k rows stream through
    /// `run_export`; the global result budget (the white-box counter for
    /// resident result bytes) must stay at zero the whole time, no query is
    /// ever registered, and the sink never holds more than one bounded chunk.
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
        // The store-free proof: nothing was ever admitted to any result
        // store, so the process-wide resident result byte counter never moved
        // and no query id was ever allocated.
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

    /// **The soft row cap never touches the export path.** The 500k cap in
    /// the policy above exists for the grid's result store; export bypasses
    /// the store entirely, so a 600k-row export delivers every row — exactly,
    /// not "roughly", and never silently clipped at the cap.
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

    /// A sink can stop the export early (deadline, Ctrl-C); the cursor is
    /// closed and the totals say so honestly.
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
