use std::fmt;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use datagrep_api::driver::{Batch, Cursor, CursorStats, FetchHint};
use datagrep_api::error::DbError;
use tokio::sync::{mpsc, watch, Notify};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::lock;

pub const DATA_CHANNEL_BOUND: usize = 2;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ParkReason {
    Backpressure,
    MemoryBudget,
    HotWindow,
    ViewportIdle,
}

impl fmt::Display for ParkReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            ParkReason::Backpressure => "waiting for the result store",
            ParkReason::MemoryBudget => "result memory budget reached",
            ParkReason::HotWindow => "hot window full",
            ParkReason::ViewportIdle => "paused — scroll to load more",
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FeedState {
    Streaming,
    Parked(ParkReason),
    Capped,
    Done,
    Cancelled,
    Failed(Arc<str>),
}

impl FeedState {
    pub fn is_terminal(&self) -> bool {
        matches!(
            self,
            FeedState::Capped | FeedState::Done | FeedState::Cancelled | FeedState::Failed(_)
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FeederPolicy {
    pub start_fetch_rows: u32,
    pub min_fetch_rows: u32,
    pub max_fetch_rows: u32,
    pub target_ms: u32,
    pub window_lo_ms: u32,
    pub window_hi_ms: u32,
    pub max_batch_bytes: u32,
    pub soft_row_cap: u64,
}

impl Default for FeederPolicy {
    fn default() -> Self {
        Self {
            start_fetch_rows: 500,
            min_fetch_rows: 100,
            max_fetch_rows: 100_000,
            target_ms: 80,
            window_lo_ms: 40,
            window_hi_ms: 120,
            max_batch_bytes: 4 * 1024 * 1024,
            soft_row_cap: 500_000,
        }
    }
}

impl FeederPolicy {
    pub fn for_fetch_rows(start_fetch_rows: u32) -> Self {
        Self {
            start_fetch_rows: start_fetch_rows.max(1),
            ..Self::default()
        }
    }

    fn next_rows(&self, prev: u32, actual_ms: u64) -> u32 {
        if actual_ms >= self.window_lo_ms as u64 && actual_ms <= self.window_hi_ms as u64 {
            return prev;
        }
        let scaled = (prev as u64).saturating_mul(self.target_ms as u64) / actual_ms.max(1);
        scaled.clamp(self.min_fetch_rows as u64, self.max_fetch_rows as u64) as u32
    }

    fn cap_by_bytes(&self, rows: u32, bytes_per_row: f64) -> u32 {
        if bytes_per_row <= 0.0 {
            return rows;
        }
        let by_bytes = (self.max_batch_bytes as f64 / bytes_per_row) as u64;
        rows.min(by_bytes.clamp(self.min_fetch_rows as u64, self.max_fetch_rows as u64) as u32)
    }
}

#[derive(Debug)]
struct FeedCtl {
    paused: AtomicBool,
    reason: Mutex<ParkReason>,
    wake: Notify,
    rows: AtomicU64,
    batches: AtomicU64,
    stats: Mutex<CursorStats>,
    error: Mutex<Option<DbError>>,
    last_hint_rows: AtomicU64,
}

#[derive(Debug)]
pub struct FeederHandle {
    ctl: Arc<FeedCtl>,
    state: watch::Receiver<FeedState>,
    cancel: CancellationToken,
    task: Mutex<Option<JoinHandle<()>>>,
}

impl FeederHandle {
    pub fn state(&self) -> FeedState {
        self.state.borrow().clone()
    }

    pub fn watch(&self) -> watch::Receiver<FeedState> {
        self.state.clone()
    }

    pub fn rows(&self) -> u64 {
        self.ctl.rows.load(Ordering::SeqCst)
    }

    pub fn batches(&self) -> u64 {
        self.ctl.batches.load(Ordering::SeqCst)
    }

    pub fn cursor_stats(&self) -> CursorStats {
        *lock(&self.ctl.stats)
    }

    pub fn last_fetch_rows(&self) -> u32 {
        self.ctl.last_hint_rows.load(Ordering::SeqCst) as u32
    }

    pub fn take_error(&self) -> Option<DbError> {
        lock(&self.ctl.error).take()
    }

    pub fn park(&self, reason: ParkReason) {
        *lock(&self.ctl.reason) = reason;
        self.ctl.paused.store(true, Ordering::SeqCst);
        self.ctl.wake.notify_waiters();
    }

    pub fn resume(&self) {
        self.ctl.paused.store(false, Ordering::SeqCst);
        self.ctl.wake.notify_waiters();
    }

    pub fn is_parked(&self) -> bool {
        self.ctl.paused.load(Ordering::SeqCst)
    }

    pub fn stop(&self) {
        self.cancel.cancel();
        // A parked feeder is waiting on the notify, not on the token.
        self.ctl.wake.notify_waiters();
    }

    pub async fn join(&self) {
        let handle = lock(&self.task).take();
        if let Some(handle) = handle {
            let _ = handle.await;
        }
    }
}

impl Drop for FeederHandle {
    fn drop(&mut self) {
        self.cancel.cancel();
        self.ctl.wake.notify_waiters();
    }
}

pub fn spawn_feeder(
    cursor: Box<dyn Cursor>,
    tx: mpsc::Sender<Batch>,
    policy: FeederPolicy,
    cancel: CancellationToken,
) -> FeederHandle {
    assert!(
        tx.max_capacity() <= DATA_CHANNEL_BOUND,
        "unbounded (or over-bounded) channel in the data path: it would let the \
         feeder queue chunks without limit and defeat backpressure"
    );

    let ctl = Arc::new(FeedCtl {
        paused: AtomicBool::new(false),
        reason: Mutex::new(ParkReason::Backpressure),
        wake: Notify::new(),
        rows: AtomicU64::new(0),
        batches: AtomicU64::new(0),
        stats: Mutex::new(CursorStats::default()),
        error: Mutex::new(None),
        last_hint_rows: AtomicU64::new(policy.start_fetch_rows as u64),
    });
    let (state_tx, state_rx) = watch::channel(FeedState::Streaming);

    let task = tokio::spawn(run_feeder(
        cursor,
        tx,
        policy,
        cancel.clone(),
        ctl.clone(),
        state_tx,
    ));

    FeederHandle {
        ctl,
        state: state_rx,
        cancel,
        task: Mutex::new(Some(task)),
    }
}

enum Ending {
    Done,
    Capped,
    Cancelled,
    Failed(DbError),
}

async fn run_feeder(
    mut cursor: Box<dyn Cursor>,
    tx: mpsc::Sender<Batch>,
    policy: FeederPolicy,
    cancel: CancellationToken,
    ctl: Arc<FeedCtl>,
    state: watch::Sender<FeedState>,
) {
    let span = tracing::info_span!("feeder", bound = DATA_CHANNEL_BOUND);
    let _enter = span.enter();

    let ending = pull_loop(&mut cursor, &tx, policy, &cancel, &ctl, &state).await;

    if let Err(err) = cursor.close().await {
        tracing::warn!(%err, "closing cursor after feeder exit");
    }
    let final_state = match ending {
        Ending::Done => FeedState::Done,
        Ending::Capped => FeedState::Capped,
        Ending::Cancelled => FeedState::Cancelled,
        Ending::Failed(err) => {
            let message: Arc<str> = Arc::from(err.to_string());
            *lock(&ctl.error) = Some(err);
            FeedState::Failed(message)
        }
    };
    tracing::debug!(
        rows = ctl.rows.load(Ordering::SeqCst),
        batches = ctl.batches.load(Ordering::SeqCst),
        state = ?final_state,
        "feeder finished"
    );
    let _ = state.send(final_state);
    drop(tx);
}

async fn pull_loop(
    cursor: &mut Box<dyn Cursor>,
    tx: &mpsc::Sender<Batch>,
    policy: FeederPolicy,
    cancel: &CancellationToken,
    ctl: &Arc<FeedCtl>,
    state: &watch::Sender<FeedState>,
) -> Ending {
    let mut rows_hint = policy.start_fetch_rows.clamp(
        policy.min_fetch_rows.min(policy.start_fetch_rows.max(1)),
        policy.max_fetch_rows,
    );
    let mut prev_bytes = 0u64;

    'pull: loop {
        if cancel.is_cancelled() {
            return Ending::Cancelled;
        }
        if ctl.rows.load(Ordering::SeqCst) >= policy.soft_row_cap {
            return Ending::Capped;
        }

        loop {
            let woken = ctl.wake.notified();
            tokio::pin!(woken);
            woken.as_mut().enable();
            if !ctl.paused.load(Ordering::SeqCst) {
                break;
            }
            let reason = *lock(&ctl.reason);
            if *state.borrow() != FeedState::Parked(reason) {
                let _ = state.send(FeedState::Parked(reason));
            }
            tokio::select! {
                _ = &mut woken => {}
                _ = cancel.cancelled() => return Ending::Cancelled,
            }
        }
        if *state.borrow() != FeedState::Streaming {
            let _ = state.send(FeedState::Streaming);
        }

        let woken = ctl.wake.notified();
        tokio::pin!(woken);
        woken.as_mut().enable();
        let permit = tokio::select! {
            biased;
            _ = cancel.cancelled() => return Ending::Cancelled,
            _ = &mut woken => continue 'pull,
            permit = tx.reserve() => match permit {
                Ok(permit) => permit,
                // The store is gone; nothing will read another chunk.
                Err(_) => return Ending::Done,
            },
        };

        // ---- pull one chunk --------------------------------------------
        let hint = FetchHint {
            max_rows: rows_hint,
            max_bytes: policy.max_batch_bytes,
            target_ms: policy.target_ms,
        };
        ctl.last_hint_rows
            .store(hint.max_rows as u64, Ordering::SeqCst);
        let started = Instant::now();
        let pulled = tokio::select! {
            biased;
            _ = cancel.cancelled() => return Ending::Cancelled,
            result = cursor.next_batch(hint) => result,
        };
        let elapsed_ms = started.elapsed().as_millis() as u64;

        let batch = match pulled {
            Ok(Some(batch)) => batch,
            Ok(None) => return Ending::Done,
            Err(DbError::Cancelled) => return Ending::Cancelled,
            Err(err) => return Ending::Failed(err),
        };

        // ---- account, adapt, hand over ---------------------------------
        let mut batch = batch;
        let mut rows = payload_rows(&batch) as u64;
        let admitted = ctl.rows.load(Ordering::SeqCst);
        let remaining = policy.soft_row_cap.saturating_sub(admitted);
        if rows > remaining {
            truncate_payload(&mut batch.payload, remaining as usize);
            rows = remaining;
        }
        let stats = cursor.stats();
        *lock(&ctl.stats) = stats;
        let bytes_this_chunk = stats.bytes.saturating_sub(prev_bytes);
        prev_bytes = stats.bytes;

        let total = ctl.rows.fetch_add(rows, Ordering::SeqCst) + rows;
        ctl.batches.fetch_add(1, Ordering::SeqCst);
        permit.send(batch);

        let bytes_per_row = if rows > 0 {
            bytes_this_chunk as f64 / rows as f64
        } else {
            0.0
        };
        rows_hint = policy.cap_by_bytes(policy.next_rows(rows_hint, elapsed_ms), bytes_per_row);
        tracing::trace!(rows, total, elapsed_ms, next_hint = rows_hint, "chunk fed");

        if total >= policy.soft_row_cap {
            return Ending::Capped;
        }
    }
}

fn truncate_payload(payload: &mut datagrep_api::driver::Payload, keep: usize) {
    use datagrep_api::driver::Payload;
    match payload {
        Payload::Rows(rows) => rows.truncate(keep),
        Payload::Docs(docs) => docs.truncate(keep),
        Payload::Pairs(pairs) => pairs.truncate(keep),
        Payload::Graph(chunk) => chunk.nodes.truncate(keep),
        Payload::Empty => {}
    }
}

pub(crate) fn payload_rows(batch: &Batch) -> usize {
    use datagrep_api::driver::Payload;
    match &batch.payload {
        Payload::Rows(rows) => rows.len(),
        Payload::Docs(docs) => docs.len(),
        Payload::Pairs(pairs) => pairs.len(),
        Payload::Graph(chunk) => chunk.nodes.len(),
        Payload::Empty => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::{MockCursor, MockPlan};
    use std::time::Duration;

    async fn until(deadline_ms: u64, mut cond: impl FnMut() -> bool) -> bool {
        let start = Instant::now();
        while start.elapsed() < Duration::from_millis(deadline_ms) {
            if cond() {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
        cond()
    }

    #[tokio::test]
    async fn feeder_parks_when_the_consumer_stops_reading() {
        let (cursor, counters) = MockCursor::standalone(MockPlan::infinite(100));
        let (tx, _rx) = mpsc::channel(DATA_CHANNEL_BOUND);
        let cancel = CancellationToken::new();
        let feeder = spawn_feeder(Box::new(cursor), tx, FeederPolicy::default(), cancel);

        // Let it run as far as it possibly can.
        assert!(
            until(500, || counters.next_batch_calls() >= DATA_CHANNEL_BOUND).await,
            "feeder never filled the channel"
        );
        tokio::time::sleep(Duration::from_millis(120)).await;

        assert_eq!(
            counters.next_batch_calls(),
            DATA_CHANNEL_BOUND,
            "the feeder ran ahead of the store; the data path is not bounded"
        );
        // And it stays stalled: no slow leak of extra pulls.
        tokio::time::sleep(Duration::from_millis(150)).await;
        assert_eq!(counters.next_batch_calls(), DATA_CHANNEL_BOUND);
        assert_eq!(
            feeder.rows(),
            (DATA_CHANNEL_BOUND * 100) as u64,
            "exactly the admitted chunks were accounted"
        );
    }

    #[tokio::test]
    async fn draining_the_channel_releases_the_feeder() {
        let (cursor, counters) = MockCursor::standalone(MockPlan::infinite(10));
        let (tx, mut rx) = mpsc::channel(DATA_CHANNEL_BOUND);
        let feeder = spawn_feeder(
            Box::new(cursor),
            tx,
            FeederPolicy::default(),
            CancellationToken::new(),
        );

        assert!(until(500, || counters.next_batch_calls() >= DATA_CHANNEL_BOUND).await);
        for _ in 0..5 {
            rx.recv().await.expect("chunk");
        }
        assert!(
            until(500, || counters.next_batch_calls()
                >= DATA_CHANNEL_BOUND + 5)
            .await,
            "feeder did not resume after the store drained"
        );
        drop(rx);
        drop(feeder);
    }

    #[tokio::test]
    async fn cancel_mid_stream_stops_within_one_batch_and_closes_the_cursor() {
        let plan = MockPlan {
            batch_delay: Some(Duration::from_millis(10)),
            ..MockPlan::infinite(50)
        };
        let (cursor, counters) = MockCursor::standalone(plan);
        let (tx, mut rx) = mpsc::channel(DATA_CHANNEL_BOUND);
        let cancel = CancellationToken::new();
        let feeder = spawn_feeder(
            Box::new(cursor),
            tx,
            FeederPolicy::default(),
            cancel.clone(),
        );

        // Consume a few chunks so we are genuinely mid-stream.
        for _ in 0..3 {
            rx.recv().await.expect("chunk");
        }
        let calls_at_cancel = counters.next_batch_calls();

        cancel.cancel();
        feeder.join().await;

        assert_eq!(feeder.state(), FeedState::Cancelled);
        assert_eq!(counters.cursor_closes(), 1, "cursor must be closed on stop");
        assert!(
            counters.next_batch_calls() <= calls_at_cancel + 1,
            "cancel took more than one in-flight batch to land: {} -> {}",
            calls_at_cancel,
            counters.next_batch_calls()
        );
    }

    #[tokio::test]
    async fn a_parked_feeder_still_cancels_instantly() {
        let (cursor, _counters) = MockCursor::standalone(MockPlan::infinite(10));
        let (tx, _rx) = mpsc::channel(DATA_CHANNEL_BOUND);
        let feeder = spawn_feeder(
            Box::new(cursor),
            tx,
            FeederPolicy::default(),
            CancellationToken::new(),
        );
        feeder.park(ParkReason::MemoryBudget);
        assert!(
            until(500, || feeder.state()
                == FeedState::Parked(ParkReason::MemoryBudget))
            .await
        );

        feeder.stop();
        feeder.join().await;
        assert_eq!(feeder.state(), FeedState::Cancelled);
    }

    #[tokio::test]
    async fn soft_row_cap_stops_the_stream() {
        let policy = FeederPolicy {
            soft_row_cap: 250,
            ..FeederPolicy::default()
        };
        let (cursor, counters) = MockCursor::standalone(MockPlan::infinite(100));
        let (tx, mut rx) = mpsc::channel(DATA_CHANNEL_BOUND);
        let feeder = spawn_feeder(Box::new(cursor), tx, policy, CancellationToken::new());

        let mut rows = 0u64;
        while let Some(batch) = rx.recv().await {
            rows += payload_rows(&batch) as u64;
        }
        feeder.join().await;

        assert_eq!(feeder.state(), FeedState::Capped);
        assert_eq!(
            rows, 250,
            "the chunk that crossed the cap must be trimmed to land exactly on it"
        );
        assert_eq!(
            counters.next_batch_calls(),
            3,
            "the cursor was pulled again after the cap was reached"
        );
        assert_eq!(counters.cursor_closes(), 1);
    }

    #[tokio::test]
    async fn park_then_resume_round_trip() {
        let (cursor, counters) = MockCursor::standalone(MockPlan::infinite(5));
        let (tx, mut rx) = mpsc::channel(DATA_CHANNEL_BOUND);
        let feeder = spawn_feeder(
            Box::new(cursor),
            tx,
            FeederPolicy::default(),
            CancellationToken::new(),
        );
        feeder.park(ParkReason::ViewportIdle);
        // Drain whatever was already in flight, then confirm it stops.
        assert!(until(500, || feeder.is_parked()).await);
        while rx.try_recv().is_ok() {}
        tokio::time::sleep(Duration::from_millis(60)).await;
        let parked_at = counters.next_batch_calls();
        tokio::time::sleep(Duration::from_millis(80)).await;
        assert_eq!(
            counters.next_batch_calls(),
            parked_at,
            "parked feeder pulled"
        );

        feeder.resume();
        assert!(
            until(500, || counters.next_batch_calls() > parked_at).await,
            "resume did not restart the feeder"
        );
        assert_eq!(feeder.state(), FeedState::Streaming);
    }

    #[tokio::test]
    async fn driver_error_becomes_failed_state() {
        let plan = MockPlan {
            fail_after: Some(2),
            ..MockPlan::infinite(10)
        };
        let (cursor, counters) = MockCursor::standalone(plan);
        let (tx, mut rx) = mpsc::channel(DATA_CHANNEL_BOUND);
        let feeder = spawn_feeder(
            Box::new(cursor),
            tx,
            FeederPolicy::default(),
            CancellationToken::new(),
        );
        while rx.recv().await.is_some() {}
        feeder.join().await;

        assert!(matches!(feeder.state(), FeedState::Failed(_)));
        assert!(matches!(feeder.take_error(), Some(DbError::Query { .. })));
        assert_eq!(counters.cursor_closes(), 1);
    }

    #[test]
    fn adaptive_fetch_sizing_matches_the_design_formula() {
        let p = FeederPolicy::default();
        // Inside the 40–120 ms window: leave it alone.
        assert_eq!(p.next_rows(500, 80), 500);
        assert_eq!(p.next_rows(500, 40), 500);
        assert_eq!(p.next_rows(500, 120), 500);
        // Too fast → fetch more: 500 * 80 / 10 = 4000.
        assert_eq!(p.next_rows(500, 10), 4_000);
        // Too slow → fetch less: 500 * 80 / 400 = 100.
        assert_eq!(p.next_rows(500, 400), 100);
        // Clamps hold at both ends.
        assert_eq!(p.next_rows(500, 100_000), p.min_fetch_rows);
        assert_eq!(p.next_rows(90_000, 1), p.max_fetch_rows);
        // A sub-millisecond chunk must not divide by zero.
        assert_eq!(p.next_rows(500, 0), 40_000);
    }

    #[test]
    fn byte_ceiling_caps_the_row_hint() {
        let p = FeederPolicy::default();
        // 4 MB / 8 KB per row = 512 rows, well under the 100k row target.
        assert_eq!(p.cap_by_bytes(100_000, 8_192.0), 512);
        // Narrow rows are unaffected.
        assert_eq!(p.cap_by_bytes(1_000, 24.0), 1_000);
        // No observation yet → no cap.
        assert_eq!(p.cap_by_bytes(1_000, 0.0), 1_000);
    }

    #[tokio::test]
    async fn feeder_grows_its_fetch_hint_on_a_fast_cursor() {
        let (cursor, _counters) = MockCursor::standalone(MockPlan::infinite(10));
        let (tx, mut rx) = mpsc::channel(DATA_CHANNEL_BOUND);
        let feeder = spawn_feeder(
            Box::new(cursor),
            tx,
            FeederPolicy::for_fetch_rows(500),
            CancellationToken::new(),
        );
        for _ in 0..4 {
            rx.recv().await.expect("chunk");
        }
        assert!(
            until(500, || feeder.last_fetch_rows() > 500).await,
            "a sub-millisecond cursor should have grown the fetch size, still {}",
            feeder.last_fetch_rows()
        );
        drop(rx);
    }

    #[tokio::test]
    #[should_panic(expected = "unbounded")]
    async fn an_over_bounded_channel_is_rejected() {
        let (cursor, _) = MockCursor::standalone(MockPlan::default());
        let (tx, _rx) = mpsc::channel(1_000_000);
        let _ = spawn_feeder(
            Box::new(cursor),
            tx,
            FeederPolicy::default(),
            CancellationToken::new(),
        );
    }
}
