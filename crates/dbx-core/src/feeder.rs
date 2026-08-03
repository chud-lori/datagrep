//! The feeder task — where the memory contract is actually enforced (§3.2).
//!
//! ```text
//! ┌──────────┐ next_batch(hint) ┌───────────┐ mpsc::channel(2) ┌────────────┐
//! │dyn Cursor│◀─────────────────│FeederTask │─────────────────▶│ResultStore │
//! │ (driver) │──── Batch ──────▶│ 1/query   │   Batch (owned)  │ 1/query    │
//! └──────────┘                  └───────────┘                  └────────────┘
//! ```
//!
//! One feeder task per running query. It is the *only* thing that ever calls
//! [`Cursor::next_batch`], and it calls it exactly as often as the store can
//! absorb — no more. When the store stops admitting, the feeder blocks on the
//! channel, stops calling `next_batch`, the driver stops reading the socket,
//! the TCP window closes, and the server stops producing. Backpressure reaches
//! the database for free on every engine with a real cursor.
//!
//! Two implementation choices carry that guarantee:
//!
//! - **A permit is reserved *before* the fetch.** `tx.reserve().await` first,
//!   `next_batch` second. So the number of chunks alive between driver and
//!   store is exactly [`DATA_CHANNEL_BOUND`], never bound+1 — the feeder never
//!   holds a fetched-but-unsendable chunk.
//! - **Every await is inside a `select!` with the query's cancellation token**
//!   (§3.4). There is no await in this file that a stop button cannot
//!   interrupt, which is what makes §3.3's "always returns control instantly"
//!   true rather than aspirational.

use std::fmt;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use dbx_api::driver::{Batch, Cursor, CursorStats, FetchHint};
use dbx_api::error::DbError;
use tokio::sync::{mpsc, watch, Notify};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::lock;

/// **The bound of the data path, and the whole backpressure story (§3.2).**
///
/// > "The `mpsc` bound of 2 is the whole backpressure story. The feeder can't
/// > run more than two chunks ahead. \[…\] **If any channel in the data path is
/// > unbounded, we have re-implemented DBeaver.**"
///
/// Raising this raises the app's floor memory by one chunk per running query
/// and buys nothing: chunk 1 already renders before chunk 2 is requested.
pub const DATA_CHANNEL_BOUND: usize = 2;

/// Why the feeder is not currently pulling. Surfaced in the status line so a
/// stalled result set is never mysterious.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ParkReason {
    /// The bounded data channel is full — the store is behind (§3.2).
    Backpressure,
    /// The global result budget is exhausted; another result set must shrink
    /// or close first (§3.2 `total_result_budget`).
    MemoryBudget,
    /// This query's own hot window is full and spill is unavailable or full.
    HotWindow,
    /// Nobody is looking: the viewport is far behind, so we stopped fetching
    /// until `get_rows` asks for rows we do not have (§3.6 window resolver).
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

/// The feeder's publicly observable state, published on a `watch` channel so
/// the store, the query supervisor, and the UI all read the same truth without
/// polling.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FeedState {
    /// Pulling chunks.
    Streaming,
    /// Deliberately not pulling; see [`ParkReason`].
    Parked(ParkReason),
    /// The soft row cap was reached. The result is intact and complete up to
    /// the cap; the UI offers "[Load more] [Export all]" (§3.2).
    Capped,
    /// The cursor reported end of stream.
    Done,
    /// The user stopped it (§3.3). Not a failure, and never dressed as one.
    Cancelled,
    /// The driver returned an error; the message is carried for the status
    /// line and the full [`DbError`] is available from
    /// [`FeederHandle::take_error`].
    Failed(Arc<str>),
}

impl FeedState {
    /// True once the feeder task has stopped for good.
    pub fn is_terminal(&self) -> bool {
        matches!(
            self,
            FeedState::Capped | FeedState::Done | FeedState::Cancelled | FeedState::Failed(_)
        )
    }
}

/// Adaptive fetch-sizing policy (design §3.2).
///
/// > "Start at `caps.default_fetch_rows` \[…\]. After each batch target a
/// > 40–120 ms wall-clock window and a 4 MB byte ceiling:
/// > `next = clamp(prev * target_ms / actual_ms, 100, 100_000)`."
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FeederPolicy {
    /// First hint, from `Capabilities::default_fetch_rows` (PG 500,
    /// ClickHouse 65 536, Mongo 101).
    pub start_fetch_rows: u32,
    /// Lower clamp on the adaptive size.
    pub min_fetch_rows: u32,
    /// Upper clamp — a 10M-row export must not do 100k round trips, but one
    /// pull must still be interruptible.
    pub max_fetch_rows: u32,
    /// Midpoint of the target window; the multiplier aims here.
    pub target_ms: u32,
    /// Below this a chunk is too small to be worth the round trip.
    pub window_lo_ms: u32,
    /// Above this a chunk delays the first screenful and blocks cancellation.
    pub window_hi_ms: u32,
    /// Byte ceiling per chunk, independent of the row count.
    pub max_batch_bytes: u32,
    /// Rows after which the feeder stops and reports [`FeedState::Capped`].
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
    /// The design's policy seeded from a connection's advertised start size.
    pub fn for_fetch_rows(start_fetch_rows: u32) -> Self {
        Self {
            start_fetch_rows: start_fetch_rows.max(1),
            ..Self::default()
        }
    }

    /// `next = clamp(prev * target_ms / actual_ms, min, max)` — applied only
    /// when the last chunk fell outside the 40–120 ms window, so a
    /// well-behaved stream does not churn its fetch size every round trip.
    fn next_rows(&self, prev: u32, actual_ms: u64) -> u32 {
        if actual_ms >= self.window_lo_ms as u64 && actual_ms <= self.window_hi_ms as u64 {
            return prev;
        }
        let scaled = (prev as u64).saturating_mul(self.target_ms as u64) / actual_ms.max(1);
        scaled.clamp(self.min_fetch_rows as u64, self.max_fetch_rows as u64) as u32
    }

    /// Clamp the row hint so the chunk also respects the 4 MB byte ceiling,
    /// using the bytes/row the cursor has reported so far.
    fn cap_by_bytes(&self, rows: u32, bytes_per_row: f64) -> u32 {
        if bytes_per_row <= 0.0 {
            return rows;
        }
        let by_bytes = (self.max_batch_bytes as f64 / bytes_per_row) as u64;
        rows.min(by_bytes.clamp(self.min_fetch_rows as u64, self.max_fetch_rows as u64) as u32)
    }
}

/// Control block shared by the handle and the task.
#[derive(Debug)]
struct FeedCtl {
    paused: AtomicBool,
    reason: Mutex<ParkReason>,
    /// Woken by [`FeederHandle::resume`]; the task waits on it while parked.
    wake: Notify,
    rows: AtomicU64,
    batches: AtomicU64,
    stats: Mutex<CursorStats>,
    error: Mutex<Option<DbError>>,
    /// Last fetch hint actually used, for tests and the status line.
    last_hint_rows: AtomicU64,
}

/// Handle to one running feeder: observe its state, park and resume it, or
/// stop it. Dropping the handle does **not** stop the feeder — the query owns
/// the cancellation token; see [`FeederHandle::stop`].
#[derive(Debug)]
pub struct FeederHandle {
    ctl: Arc<FeedCtl>,
    state: watch::Receiver<FeedState>,
    cancel: CancellationToken,
    task: Mutex<Option<JoinHandle<()>>>,
}

impl FeederHandle {
    /// Current state, cheap and lock-free-ish.
    pub fn state(&self) -> FeedState {
        self.state.borrow().clone()
    }

    /// A receiver for state changes — the no-polling way to follow a query.
    pub fn watch(&self) -> watch::Receiver<FeedState> {
        self.state.clone()
    }

    /// Rows handed to the store so far.
    pub fn rows(&self) -> u64 {
        self.ctl.rows.load(Ordering::SeqCst)
    }

    /// Chunks handed to the store so far.
    pub fn batches(&self) -> u64 {
        self.ctl.batches.load(Ordering::SeqCst)
    }

    /// The driver's own running totals, as of the last pull.
    pub fn cursor_stats(&self) -> CursorStats {
        *lock(&self.ctl.stats)
    }

    /// The row hint used for the most recent pull — the adaptive size (§3.2).
    pub fn last_fetch_rows(&self) -> u32 {
        self.ctl.last_hint_rows.load(Ordering::SeqCst) as u32
    }

    /// Take the failure that ended this feeder, if any.
    pub fn take_error(&self) -> Option<DbError> {
        lock(&self.ctl.error).take()
    }

    /// Stop pulling, for `reason`. Idempotent; the current in-flight pull
    /// completes (a driver cannot be interrupted mid-chunk without cancelling
    /// the whole query), then the feeder waits. A feeder blocked on the data
    /// channel is woken so it can move to the parked state rather than sitting
    /// on a reservation nobody will fill.
    pub fn park(&self, reason: ParkReason) {
        *lock(&self.ctl.reason) = reason;
        self.ctl.paused.store(true, Ordering::SeqCst);
        self.ctl.wake.notify_waiters();
    }

    /// Resume pulling. Idempotent; safe to call when not parked.
    pub fn resume(&self) {
        self.ctl.paused.store(false, Ordering::SeqCst);
        self.ctl.wake.notify_waiters();
    }

    /// True while the feeder is deliberately not pulling.
    pub fn is_parked(&self) -> bool {
        self.ctl.paused.load(Ordering::SeqCst)
    }

    /// The local half of a stop (§3.3): cancel the token so every await in the
    /// task unwinds, and let it close the cursor. Returns immediately.
    pub fn stop(&self) {
        self.cancel.cancel();
        // A parked feeder is waiting on the notify, not on the token.
        self.ctl.wake.notify_waiters();
    }

    /// Wait for the task to finish. Used by tests and by orderly shutdown; the
    /// stop button never waits for this.
    pub async fn join(&self) {
        let handle = lock(&self.task).take();
        if let Some(handle) = handle {
            let _ = handle.await;
        }
    }
}

impl Drop for FeederHandle {
    /// Dropping the last handle to a feeder must not leave it pulling rows
    /// nobody will ever read — that is the leak the whole design refuses.
    fn drop(&mut self) {
        self.cancel.cancel();
        self.ctl.wake.notify_waiters();
    }
}

/// Spawn the feeder for one query (design §3.2).
///
/// `cancel` should be the query's node in the session → connection → query
/// token tree (§3.4); cancelling any ancestor stops this feeder too.
pub fn spawn_feeder(
    cursor: Box<dyn Cursor>,
    tx: mpsc::Sender<Batch>,
    policy: FeederPolicy,
    cancel: CancellationToken,
) -> FeederHandle {
    assert!(
        tx.max_capacity() <= DATA_CHANNEL_BOUND,
        "unbounded (or over-bounded) channel in the data path — design §3.2"
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

/// Outcome of the pull loop, before the cursor is closed.
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

    // §3.3: the cursor is always released, on every exit path, so a cancelled
    // query never leaves a server-side portal open.
    if let Err(err) = cursor.close().await {
        tracing::warn!(%err, "closing cursor after feeder exit");
    }
    // Dropping the sender is what tells the store the stream is over; the
    // terminal state is published first so the store never sees a closed
    // channel with a stale `Streaming` state.
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

        // ---- park gate -------------------------------------------------
        // A parked feeder holds no permit and no chunk: it costs nothing but
        // the task's own stack while the user is not looking.
        //
        // `enable()` registers this task as a waiter *before* the flag is
        // read, so a `resume` landing in between cannot be lost — the classic
        // lost-wakeup that turns a parked query into a hung one.
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

        // ---- reserve before fetching -----------------------------------
        // This ordering is the memory bound: at most DATA_CHANNEL_BOUND
        // chunks exist between driver and store, and the feeder never holds a
        // fetched chunk it cannot hand over.
        //
        // The wake branch exists so a `park` issued while we are blocked here
        // is honoured: we drop the reservation attempt and re-enter the gate.
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
        let rows = payload_rows(&batch) as u64;
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

/// Rows in a chunk, per its payload shape. `Ack`/`Empty` results carry none.
fn payload_rows(batch: &Batch) -> usize {
    use dbx_api::driver::Payload;
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

    /// Poll a condition to a deadline. Tests here are about *task scheduling*,
    /// so they wait for a real settle rather than a fixed sleep.
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

    /// **Test 1 — the test of the whole design (§3.2).**
    ///
    /// An endless producer and a consumer that never reads. If the data path
    /// were unbounded the cursor would be called forever and RSS would track
    /// the server's output rate. Instead `next_batch` must stall at exactly
    /// [`DATA_CHANNEL_BOUND`] calls and stay there: the feeder parked, the
    /// driver stopped reading its socket, and the server stopped producing.
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

    /// Draining the channel must let the feeder advance again, one chunk per
    /// freed slot — backpressure that never releases is just a deadlock.
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

    /// **Test 2 — cancellation mid-stream (§3.3).** The feeder stops within one
    /// batch, the state becomes `Cancelled`, and the cursor is closed so no
    /// server-side portal is left open.
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

    /// A parked feeder is still instantly cancellable — the stop button cannot
    /// be defeated by the feeder happening to be asleep.
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

    /// **Test 3 — the soft row cap is honoured (§3.2).** `SELECT * FROM events`
    /// on a 2 TB table stops at the cap with the banner state, instead of
    /// streaming until something dies.
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
        assert_eq!(rows, 300, "capped on the first chunk that crossed 250");
        assert_eq!(
            counters.next_batch_calls(),
            3,
            "the cursor was pulled again after the cap was reached"
        );
        assert_eq!(counters.cursor_closes(), 1);
    }

    /// Park/resume is the store's lever for the budget and the viewport; a
    /// resumed feeder must actually pull again.
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

    /// A driver error ends the stream as `Failed`, keeps the real `DbError`
    /// for the caller, and still closes the cursor.
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

    /// §3.2's adaptive sizing arithmetic, exactly as written in the design.
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

    /// The 4 MB ceiling overrides the row target — one wide-row chunk must not
    /// blow the per-query hot budget just because it was fast.
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

    /// The feeder must actually adapt in flight, not just own the formula.
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

    /// The invariant, asserted in code rather than trusted to review: an
    /// unbounded data-path channel is a panic, not a subtle regression.
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
