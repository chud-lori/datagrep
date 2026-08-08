//! Query lifecycle and cancellation (design §3.3).
//!
//! `QueryMgr` is the wiring between the three tasks a running query is made of
//! — cursor → [`crate::feeder`] → [`crate::store`] — plus the one piece of
//! behaviour the design is most emphatic about:
//!
//! > "The stop button **always** returns control instantly (drop feeder, close
//! > cursor, free store). The status line then tells the truth: *'stopped
//! > receiving results; the server may still be executing this query.'*"
//!
//! So [`QueryMgr::cancel`] is deliberately **not** `async`. It does the local
//! half unconditionally and synchronously — cancel the query token, which stops
//! the feeder, closes the cursor and stops the store — and returns a
//! [`CancelReport`] that already knows what kind of cancel this engine
//! supports. The server half is fired off in the background and its real
//! outcome arrives later as [`QueryEvent::CancelOutcome`]. A UI that awaited
//! the server would be a UI whose stop button hangs on exactly the query that
//! most needs stopping.
//!
//! Everything a frontend needs to follow a query is on one broadcast channel of
//! [`QueryEvent`]s, fed by a supervisor task that watches the store's `watch`
//! channel. No polling anywhere (§3.4).

use std::collections::HashMap;
use std::fmt;
use std::ops::Range;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

use datagrep_api::driver::{CancelKind, CancelOutcome, Canceller};
use datagrep_api::error::DbError;
use datagrep_api::request::Request;
use tokio::sync::{broadcast, mpsc};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::Instrument;

use crate::feeder::{spawn_feeder, ParkReason, DATA_CHANNEL_BOUND};
use crate::session::ConnLease;
use crate::store::{
    GlobalBudget, MemoryPolicy, ResultStore, RowWindow, StorePhase, StoreState, WindowStatus,
};
use crate::{lock, read, write};

/// How long the background half of a cancel is given before we stop waiting.
/// It is fire-and-forget either way; this only bounds the task.
const SERVER_CANCEL_TIMEOUT: Duration = Duration::from_secs(5);

/// Capacity of the event broadcast. Lagging subscribers lose events rather than
/// applying backpressure to the core — a slow UI must never stall a query.
const EVENT_CHANNEL_CAPACITY: usize = 256;

/// Identity of a running (or finished) query.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct QueryId(pub u64);

impl fmt::Display for QueryId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "q{}", self.0)
    }
}

/// End-of-query totals for the status line and query history.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct QueryStats {
    pub rows: u64,
    pub batches: u64,
    /// Bytes the driver reported reading.
    pub bytes: u64,
    /// Wall-clock from `execute` returning to the terminal state.
    pub elapsed_micros: u64,
    /// Time to the first admitted chunk — the number P8 measures.
    pub first_batch_micros: Option<u64>,
    /// Server-reported execution time, when the protocol carries one.
    pub server_elapsed_micros: Option<u64>,
    pub spilled_bytes: u64,
    /// Affected-row count from an `Ack`-shaped result (INSERT/UPDATE/DDL).
    /// `None` for row-producing results and for engines that don't report one.
    pub affected: Option<u64>,
}

/// What a stop actually achieved, stated honestly (design §3.3).
///
/// `local_stopped` is always `true` — that half never fails. `outcome` is
/// `None` until the server half reports back, and stays `None` forever on an
/// engine that has no server-side cancel; [`CancelReport::message`] is the
/// sentence the status line shows in each case.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CancelReport {
    /// We stopped consuming, closed the cursor, and released the store.
    pub local_stopped: bool,
    /// What cancelling can do at all on this engine.
    pub kind: CancelKind,
    /// What the server said, once it says anything.
    pub outcome: Option<CancelOutcome>,
    /// The exact words for the status line — never embellished.
    pub message: Arc<str>,
}

impl CancelReport {
    fn pending(kind: CancelKind) -> Self {
        let message: Arc<str> = match kind {
            CancelKind::ServerSide => {
                Arc::from("stopped receiving results; asking the server to cancel…")
            }
            CancelKind::ClientAbandon => Arc::from(
                "stopped receiving results; the server may still be executing this query.",
            ),
            CancelKind::DeadlineOnly => Arc::from(
                "stopped receiving results; the query runs until its server-side deadline.",
            ),
        };
        Self {
            local_stopped: true,
            kind,
            outcome: None,
            message,
        }
    }

    fn resolved(kind: CancelKind, outcome: Result<CancelOutcome, DbError>) -> Self {
        let (outcome, message): (Option<CancelOutcome>, Arc<str>) = match outcome {
            Ok(CancelOutcome::ServerCancelled) => (
                Some(CancelOutcome::ServerCancelled),
                Arc::from("stopped; the server confirmed it killed the query."),
            ),
            Ok(CancelOutcome::Requested) => (
                Some(CancelOutcome::Requested),
                // PG's CancelRequest is racy by protocol design and never acks.
                Arc::from("stopped; a cancel was sent but this protocol gives no acknowledgement."),
            ),
            Ok(CancelOutcome::ClientAbandoned) => (
                Some(CancelOutcome::ClientAbandoned),
                Arc::from(
                    "stopped receiving results; the server may still be executing this query.",
                ),
            ),
            Err(err) => (
                None,
                Arc::from(format!(
                    "stopped receiving results; the cancel request failed ({err}). \
                     The server may still be executing this query."
                )),
            ),
        };
        Self {
            local_stopped: true,
            kind,
            outcome,
            message,
        }
    }
}

/// Everything a frontend needs to follow a query, on one broadcast channel.
#[derive(Debug, Clone, PartialEq)]
pub enum QueryEvent {
    /// The server accepted the request and a cursor exists. Emitted before any
    /// data — this is what turns the spinner on.
    Accepted {
        qid: QueryId,
    },
    /// The first chunk is in the store and the grid can paint (§5, P8).
    FirstBatch {
        qid: QueryId,
        micros: u64,
    },
    Progress {
        qid: QueryId,
        rows: u64,
    },
    /// Fetching stopped on purpose; the reason is shown, not hidden.
    Parked {
        qid: QueryId,
        reason: ParkReason,
    },
    /// The soft row cap was reached — the UI offers "[Load more] [Export all]".
    Capped {
        qid: QueryId,
        rows: u64,
    },
    Done {
        qid: QueryId,
        stats: QueryStats,
    },
    Failed {
        qid: QueryId,
        message: Arc<str>,
    },
    /// The truth about a stop, possibly arriving well after the button (§3.3).
    CancelOutcome {
        qid: QueryId,
        report: CancelReport,
    },
}

impl QueryEvent {
    pub fn qid(&self) -> QueryId {
        match self {
            QueryEvent::Accepted { qid }
            | QueryEvent::FirstBatch { qid, .. }
            | QueryEvent::Progress { qid, .. }
            | QueryEvent::Parked { qid, .. }
            | QueryEvent::Capped { qid, .. }
            | QueryEvent::Done { qid, .. }
            | QueryEvent::Failed { qid, .. }
            | QueryEvent::CancelOutcome { qid, .. } => *qid,
        }
    }
}

/// One tracked query.
struct Query {
    store: Arc<ResultStore>,
    canceller: Arc<dyn Canceller>,
    cancel: CancellationToken,
    supervisor: Mutex<Option<JoinHandle<()>>>,
}

/// Owner of every running query's lifecycle.
pub struct QueryMgr {
    policy: MemoryPolicy,
    budget: GlobalBudget,
    events: broadcast::Sender<QueryEvent>,
    queries: RwLock<HashMap<QueryId, Arc<Query>>>,
    next_id: AtomicU64,
}

impl QueryMgr {
    /// A manager over a shared result budget. Every `QueryMgr` in a process
    /// should share one [`GlobalBudget`] — that is what makes the 256 MB in
    /// §3.2 a *process* number rather than a per-tab one.
    pub fn new(policy: MemoryPolicy, budget: GlobalBudget) -> Self {
        let (events, _) = broadcast::channel(EVENT_CHANNEL_CAPACITY);
        Self {
            policy,
            budget,
            events,
            queries: RwLock::new(HashMap::new()),
            next_id: AtomicU64::new(0),
        }
    }

    /// A manager owning a budget derived from its own policy.
    pub fn with_policy(policy: MemoryPolicy) -> Self {
        let budget = GlobalBudget::from_policy(&policy);
        Self::new(policy, budget)
    }

    pub fn policy(&self) -> &MemoryPolicy {
        &self.policy
    }

    pub fn budget(&self) -> &GlobalBudget {
        &self.budget
    }

    /// Follow every query in the process. Subscribers that fall behind lose
    /// events; they never stall a query.
    pub fn subscribe(&self) -> broadcast::Receiver<QueryEvent> {
        self.events.subscribe()
    }

    /// Start a query on `conn` and return as soon as the server accepts it.
    ///
    /// The lease is held for the query's whole life: a cursor outliving its
    /// connection is not a state this core can reach. It is released when the
    /// query reaches a terminal state or is cancelled.
    pub async fn run(&self, conn: ConnLease, req: Request) -> Result<QueryId, DbError> {
        let started = Instant::now();
        // An explicit `row_limit` is the caller saying "I want exactly this
        // many rows" — it lifts the grid's soft row cap up to that number, so
        // `--limit 600000` is never silently clipped back to 500k. Spill
        // (§3.2) keeps memory bounded either way.
        let row_limit = match &req {
            Request::Native { opts, .. } => opts.row_limit,
            Request::Op(_) => None,
        };
        // `execute` returns when the server accepts the request; it never waits
        // for or buffers the result (design §3.1).
        let cursor = conn.execute(req).await?;
        let shape = cursor.shape().clone();

        // The query's node of the token tree: session → connection → query.
        let cancel = conn.cancel_token().child_token();
        let canceller = conn.canceller();
        let fetch_rows = conn.capabilities().default_fetch_rows;

        let (tx, rx) = mpsc::channel(DATA_CHANNEL_BOUND);
        let mut feeder_policy = self.policy.feeder_policy(fetch_rows);
        if let Some(limit) = row_limit {
            feeder_policy.soft_row_cap = feeder_policy.soft_row_cap.max(limit);
        }
        let feeder = spawn_feeder(cursor, tx, feeder_policy, cancel.clone());
        let store = ResultStore::spawn(
            shape,
            rx,
            feeder,
            self.policy.clone(),
            self.budget.clone(),
            cancel.clone(),
        );

        let qid = QueryId(self.next_id.fetch_add(1, Ordering::SeqCst));
        let supervisor = tokio::spawn(
            supervise(qid, store.clone(), self.events.clone(), conn, started)
                .instrument(tracing::info_span!("query", qid = qid.0)),
        );

        write(&self.queries).insert(
            qid,
            Arc::new(Query {
                store,
                canceller,
                cancel,
                supervisor: Mutex::new(Some(supervisor)),
            }),
        );
        let _ = self.events.send(QueryEvent::Accepted { qid });
        tracing::debug!(%qid, "query accepted");
        Ok(qid)
    }

    /// **The stop button.** Does the local half unconditionally and returns
    /// immediately; the server half is fired in the background and its outcome
    /// arrives as [`QueryEvent::CancelOutcome`] (design §3.3).
    ///
    /// Not `async` on purpose: there is no await between pressing stop and
    /// getting control back.
    pub fn cancel(&self, qid: QueryId) -> Option<CancelReport> {
        let query = read(&self.queries).get(&qid).cloned()?;

        // ---- local half: unconditional, synchronous ----------------------
        // Cancelling the query token stops the feeder (which closes the
        // cursor) and the store's writer task; the connection lease is
        // released by the supervisor as it exits.
        query.store.stop();
        query.cancel.cancel();

        let kind = query.canceller.kind();
        let report = CancelReport::pending(kind);
        tracing::debug!(%qid, ?kind, "local cancel done");

        // ---- server half: fire and forget, reported honestly -------------
        let canceller = query.canceller.clone();
        let events = self.events.clone();
        tokio::spawn(async move {
            let outcome =
                match tokio::time::timeout(SERVER_CANCEL_TIMEOUT, canceller.cancel()).await {
                    Ok(result) => result,
                    Err(_) => Err(DbError::Timeout),
                };
            let report = CancelReport::resolved(kind, outcome);
            tracing::debug!(%qid, ?report.outcome, "server cancel resolved");
            let _ = events.send(QueryEvent::CancelOutcome { qid, report });
        });

        Some(report)
    }

    /// The result store of a query, for as long as it is tracked.
    pub fn store(&self, qid: QueryId) -> Option<Arc<ResultStore>> {
        read(&self.queries).get(&qid).map(|q| q.store.clone())
    }

    /// The current snapshot of a query's result set.
    pub fn state(&self, qid: QueryId) -> Option<Arc<StoreState>> {
        self.store(qid).map(|s| s.state())
    }

    /// Resolve a row window (design §3.2, §3.6). Asking for rows past the
    /// frontier is what un-parks the feeder.
    pub async fn get_rows(&self, qid: QueryId, range: Range<u64>) -> Option<RowWindow> {
        let store = self.store(qid)?;
        Some(store.get_rows(range).await)
    }

    /// Forget a query and release everything it holds — the tab was closed.
    /// This, not `cancel`, is what returns the memory (design §5, P7).
    pub fn close(&self, qid: QueryId) {
        let Some(query) = write(&self.queries).remove(&qid) else {
            return;
        };
        stop_query(&query);
    }

    /// Queries currently tracked (running or finished-but-open).
    pub fn open_queries(&self) -> Vec<QueryId> {
        let mut ids: Vec<_> = read(&self.queries).keys().copied().collect();
        ids.sort_unstable();
        ids
    }

    /// Stop and forget everything.
    pub fn shutdown(&self) {
        for (_, query) in write(&self.queries).drain() {
            stop_query(&query);
        }
    }
}

/// Stop a query's tasks and let its socket go. Shared by `close` and
/// `shutdown` so both take exactly the same path.
fn stop_query(query: &Query) {
    query.store.stop();
    query.cancel.cancel();
    let handle = lock(&query.supervisor).take();
    if let Some(handle) = handle {
        handle.abort();
    }
}

impl fmt::Debug for QueryMgr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("QueryMgr")
            .field("queries", &self.open_queries().len())
            .field("budget", &self.budget)
            .finish()
    }
}

/// Watches one store and turns its state changes into [`QueryEvent`]s. Holds
/// the connection lease, so the socket goes back to the pool exactly when the
/// query is over — cancelled, capped, failed or complete.
async fn supervise(
    qid: QueryId,
    store: Arc<ResultStore>,
    events: broadcast::Sender<QueryEvent>,
    lease: ConnLease,
    started: Instant,
) {
    let mut watch = store.subscribe();
    let mut last_rows = 0u64;
    let mut announced_first = false;
    let mut announced_park: Option<ParkReason> = None;

    loop {
        let state = watch.borrow_and_update().clone();

        if !announced_first {
            if let Some(micros) = state.first_batch_micros {
                announced_first = true;
                let _ = events.send(QueryEvent::FirstBatch { qid, micros });
            }
        }
        if state.rows > last_rows {
            last_rows = state.rows;
            let _ = events.send(QueryEvent::Progress {
                qid,
                rows: state.rows,
            });
        }
        match &state.phase {
            StorePhase::Parked(reason) if announced_park != Some(*reason) => {
                announced_park = Some(*reason);
                let _ = events.send(QueryEvent::Parked {
                    qid,
                    reason: *reason,
                });
            }
            StorePhase::Parked(_) => {}
            _ => announced_park = None,
        }

        if state.phase.is_terminal() {
            emit_terminal(qid, &events, &state, started, store.feeder().cursor_stats());
            break;
        }
        if watch.changed().await.is_err() {
            break;
        }
    }

    // The socket is only free once nothing can still pull on it.
    drop(lease);
    tracing::debug!(%qid, rows = last_rows, "query supervisor exited");
}

fn emit_terminal(
    qid: QueryId,
    events: &broadcast::Sender<QueryEvent>,
    state: &StoreState,
    started: Instant,
    cursor: datagrep_api::driver::CursorStats,
) {
    let stats = QueryStats {
        rows: state.rows,
        batches: state.batches,
        bytes: cursor.bytes,
        elapsed_micros: started.elapsed().as_micros() as u64,
        first_batch_micros: state.first_batch_micros,
        server_elapsed_micros: cursor.server_elapsed_micros,
        spilled_bytes: state.spilled_bytes,
        affected: state.affected,
    };
    match &state.phase {
        StorePhase::Capped => {
            // Capped is still a finished result — the UI needs both the banner
            // and the totals.
            let _ = events.send(QueryEvent::Capped {
                qid,
                rows: state.rows,
            });
            let _ = events.send(QueryEvent::Done { qid, stats });
        }
        StorePhase::Complete => {
            let _ = events.send(QueryEvent::Done { qid, stats });
        }
        StorePhase::Failed(message) => {
            let _ = events.send(QueryEvent::Failed {
                qid,
                message: message.clone(),
            });
        }
        // A cancel reports through `CancelOutcome`, which says more than a
        // `Done` ever could. Nothing else to announce.
        StorePhase::Cancelled => {}
        StorePhase::Loading | StorePhase::Parked(_) => {}
    }
}

/// Convenience for callers that want a window or nothing.
impl QueryMgr {
    /// A window, or an empty `Pending` one when the query is unknown.
    pub async fn rows_or_pending(&self, qid: QueryId, range: Range<u64>) -> RowWindow {
        match self.get_rows(qid, range.clone()).await {
            Some(window) => window,
            None => RowWindow {
                range,
                status: WindowStatus::Cancelled,
                slices: Vec::new(),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::ProfileId;
    use crate::registry::DriverRegistry;
    use crate::session::{PoolPolicy, SessionRegistry};
    use crate::store::SpillPolicy;
    use crate::testing::{MockCounters, MockDriver, MockPlan};
    use crate::timer::TimerWheel;
    use datagrep_api::config::ConnectionConfig;

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

    fn policy() -> MemoryPolicy {
        MemoryPolicy {
            total_result_budget: 16 * 1024 * 1024,
            per_query_hot: 16 * 1024 * 1024,
            hot_window_rows: usize::MAX,
            soft_row_cap: 500_000,
            spill: SpillPolicy::Disabled,
        }
    }

    struct Harness {
        sessions: SessionRegistry,
        queries: QueryMgr,
        counters: Arc<MockCounters>,
        _timer: Arc<TimerWheel>,
    }

    fn harness(plan: MockPlan) -> Harness {
        Harness::build(plan, policy())
    }

    impl Harness {
        fn build(plan: MockPlan, policy: MemoryPolicy) -> Harness {
            let drivers = Arc::new(DriverRegistry::new());
            let driver = Arc::new(MockDriver::with_plan(plan));
            let counters = driver.counters();
            drivers.register("mock", move || driver.clone());
            let timer = Arc::new(TimerWheel::new());
            Harness {
                sessions: SessionRegistry::with_policy(
                    drivers,
                    timer.clone(),
                    PoolPolicy::default(),
                ),
                queries: QueryMgr::with_policy(policy),
                counters,
                _timer: timer,
            }
        }

        async fn lease(&self) -> ConnLease {
            let session = self
                .sessions
                .open(
                    ProfileId(1),
                    "mock",
                    ConnectionConfig {
                        driver: Arc::from("mock"),
                        values: Default::default(),
                    },
                )
                .expect("open");
            session.acquire().await.expect("acquire")
        }
    }

    /// Collect events until `f` returns true or the deadline passes.
    async fn drain_until(
        rx: &mut broadcast::Receiver<QueryEvent>,
        deadline_ms: u64,
        mut f: impl FnMut(&QueryEvent) -> bool,
    ) -> Vec<QueryEvent> {
        let mut seen = Vec::new();
        let deadline = Instant::now() + Duration::from_millis(deadline_ms);
        while Instant::now() < deadline {
            match tokio::time::timeout(Duration::from_millis(50), rx.recv()).await {
                Ok(Ok(event)) => {
                    let done = f(&event);
                    seen.push(event);
                    if done {
                        break;
                    }
                }
                Ok(Err(_)) => break,
                Err(_) => {}
            }
        }
        seen
    }

    /// The whole pipeline in one test: cursor → feeder → store → events, with
    /// rows readable through the window resolver at the end.
    #[tokio::test]
    async fn a_query_streams_end_to_end_and_reports_its_lifecycle() {
        let h = harness(MockPlan {
            batches: Some(4),
            rows_per_batch: 25,
            ..MockPlan::default()
        });
        let mut events = h.queries.subscribe();
        let qid = h
            .queries
            .run(h.lease().await, Request::native("select 1"))
            .await
            .expect("run");

        let seen = drain_until(&mut events, 3_000, |e| matches!(e, QueryEvent::Done { .. })).await;

        assert!(seen
            .iter()
            .any(|e| matches!(e, QueryEvent::Accepted { .. })));
        assert!(
            seen.iter()
                .any(|e| matches!(e, QueryEvent::FirstBatch { .. })),
            "no FirstBatch event: {seen:?}"
        );
        assert!(seen
            .iter()
            .any(|e| matches!(e, QueryEvent::Progress { .. })));
        let done = seen
            .iter()
            .find_map(|e| match e {
                QueryEvent::Done { stats, .. } => Some(*stats),
                _ => None,
            })
            .expect("Done event");
        assert_eq!(done.rows, 100);
        assert_eq!(done.batches, 4);
        assert!(done.first_batch_micros.is_some());

        let window = h.queries.get_rows(qid, 0..10).await.expect("window");
        assert_eq!(window.status, WindowStatus::Ready);
        assert_eq!(window.rows(), 10);

        // The socket went back to the pool when the query finished.
        let session = h.sessions.get(ProfileId(1)).expect("session");
        assert!(
            until(2_000, || session.idle_count() == 1).await,
            "the connection lease was not released"
        );
        h.queries.shutdown();
        h.sessions.shutdown();
    }

    /// **§3.3 — the stop button returns instantly and tells the truth.**
    ///
    /// `cancel` is synchronous: the local half (feeder aborted, cursor closed,
    /// store released) is done before it returns, and the server half is
    /// reported later, exactly as it happened.
    #[tokio::test]
    async fn cancel_stops_locally_at_once_and_reports_the_server_half_later() {
        let h = harness(MockPlan {
            batch_delay: Some(Duration::from_millis(10)),
            rows_per_batch: 20,
            ..MockPlan::infinite(20)
        });
        let mut events = h.queries.subscribe();
        let qid = h
            .queries
            .run(h.lease().await, Request::native("select * from events"))
            .await
            .expect("run");
        assert!(
            until(2_000, || h.queries.state(qid).map(|s| s.rows).unwrap_or(0)
                > 0)
            .await
        );

        let pressed = Instant::now();
        let report = h.queries.cancel(qid).expect("tracked query");
        let elapsed = pressed.elapsed();

        assert!(report.local_stopped, "the local half is unconditional");
        assert_eq!(report.kind, CancelKind::ServerSide);
        assert_eq!(report.outcome, None, "the server has not answered yet");
        assert!(
            elapsed < Duration::from_millis(50),
            "the stop button blocked for {elapsed:?}"
        );

        // The local half really happened.
        let store = h.queries.store(qid).expect("store");
        assert!(until(2_000, || store.state().phase == StorePhase::Cancelled).await);
        assert!(
            until(2_000, || h.counters.cursor_closes() == 1).await,
            "the cursor was not closed"
        );

        // And the truth about the server arrives afterwards.
        let seen = drain_until(&mut events, 3_000, |e| {
            matches!(e, QueryEvent::CancelOutcome { .. })
        })
        .await;
        let outcome = seen
            .iter()
            .find_map(|e| match e {
                QueryEvent::CancelOutcome { report, .. } => Some(report.clone()),
                _ => None,
            })
            .expect("CancelOutcome event");
        assert_eq!(outcome.outcome, Some(CancelOutcome::ServerCancelled));
        assert!(outcome.message.contains("server confirmed"));
        assert_eq!(h.counters.cancels(), 1);

        // The rows that did arrive are still readable — a cancel is not a wipe.
        let window = h.queries.get_rows(qid, 0..10).await.expect("window");
        assert_eq!(window.status, WindowStatus::Ready);
        h.queries.shutdown();
        h.sessions.shutdown();
    }

    /// On an engine with no server-side cancel the status line says so, in
    /// exactly the words §3.3 specifies. No pretending.
    #[tokio::test]
    async fn a_client_abandon_engine_says_the_server_may_still_be_running() {
        let h = harness(MockPlan {
            cancel_kind: CancelKind::ClientAbandon,
            cancel_outcome: CancelOutcome::ClientAbandoned,
            batch_delay: Some(Duration::from_millis(5)),
            ..MockPlan::infinite(20)
        });
        let mut events = h.queries.subscribe();
        let qid = h
            .queries
            .run(h.lease().await, Request::native("scan"))
            .await
            .expect("run");
        assert!(
            until(2_000, || h.queries.state(qid).map(|s| s.rows).unwrap_or(0)
                > 0)
            .await
        );

        let report = h.queries.cancel(qid).expect("tracked");
        assert_eq!(report.kind, CancelKind::ClientAbandon);
        assert!(
            report.message.contains("may still be executing"),
            "message was {:?}",
            report.message
        );

        let seen = drain_until(&mut events, 3_000, |e| {
            matches!(e, QueryEvent::CancelOutcome { .. })
        })
        .await;
        let outcome = seen
            .iter()
            .find_map(|e| match e {
                QueryEvent::CancelOutcome { report, .. } => Some(report.clone()),
                _ => None,
            })
            .expect("CancelOutcome");
        assert_eq!(outcome.outcome, Some(CancelOutcome::ClientAbandoned));
        assert!(outcome.message.contains("may still be executing"));
        h.queries.shutdown();
        h.sessions.shutdown();
    }

    /// **Gap 1 — an acknowledgement's affected-row count reaches the
    /// frontend.** An INSERT produces `Shape::Ack { affected }` and no rows;
    /// the count must arrive in `QueryEvent::Done { stats }` (and in the
    /// store snapshot), not die between the driver and the store.
    #[tokio::test]
    async fn an_ack_result_reports_its_affected_rows_in_done() {
        let h = harness(MockPlan {
            payload: crate::testing::MockPayload::Ack { affected: Some(2) },
            batches: Some(1),
            ..MockPlan::default()
        });
        let mut events = h.queries.subscribe();
        let qid = h
            .queries
            .run(
                h.lease().await,
                Request::native("insert into t values (1),(2)"),
            )
            .await
            .expect("run");

        let seen = drain_until(&mut events, 3_000, |e| matches!(e, QueryEvent::Done { .. })).await;
        let done = seen
            .iter()
            .find_map(|e| match e {
                QueryEvent::Done { stats, .. } => Some(*stats),
                _ => None,
            })
            .expect("Done event");
        assert_eq!(done.affected, Some(2), "affected count lost before Done");
        assert_eq!(done.rows, 0, "an Ack has no rows");

        // The snapshot carries it too, for frontends that read state directly.
        let state = h.queries.state(qid).expect("state");
        assert_eq!(state.affected, Some(2));
        h.queries.shutdown();
        h.sessions.shutdown();
    }

    /// A driver failure surfaces as a `Failed` event carrying the server's own
    /// message, and the socket still goes back to the pool.
    #[tokio::test]
    async fn a_failing_query_reports_and_releases_its_connection() {
        let h = harness(MockPlan {
            fail_after: Some(1),
            rows_per_batch: 10,
            ..MockPlan::infinite(10)
        });
        let mut events = h.queries.subscribe();
        h.queries
            .run(h.lease().await, Request::native("select boom"))
            .await
            .expect("run");

        let seen = drain_until(&mut events, 3_000, |e| {
            matches!(e, QueryEvent::Failed { .. })
        })
        .await;
        let message = seen
            .iter()
            .find_map(|e| match e {
                QueryEvent::Failed { message, .. } => Some(message.clone()),
                _ => None,
            })
            .expect("Failed event");
        assert!(message.contains("mock failure"), "message was {message:?}");

        let session = h.sessions.get(ProfileId(1)).expect("session");
        assert!(
            until(2_000, || session.idle_count() == 1).await,
            "a failed query leaked its connection"
        );
        h.queries.shutdown();
        h.sessions.shutdown();
    }

    /// The soft row cap reaches the frontend as `Capped` plus the totals, not
    /// as a silently short result.
    #[tokio::test]
    async fn the_soft_row_cap_is_announced() {
        let h = Harness::build(
            MockPlan {
                rows_per_batch: 50,
                ..MockPlan::infinite(50)
            },
            MemoryPolicy {
                soft_row_cap: 120,
                ..policy()
            },
        );
        let mut events = h.queries.subscribe();
        h.queries
            .run(h.lease().await, Request::native("select * from events"))
            .await
            .expect("run");

        let seen = drain_until(&mut events, 3_000, |e| matches!(e, QueryEvent::Done { .. })).await;
        assert!(
            seen.iter()
                .any(|e| matches!(e, QueryEvent::Capped { rows: 120, .. })),
            "no Capped event at exactly the cap: {seen:?}"
        );
        h.queries.shutdown();
        h.sessions.shutdown();
    }

    /// An explicit `row_limit` lifts the soft row cap up to that number —
    /// `--limit 600000` must never be silently clipped back to the default
    /// cap, and the boundary is exact.
    #[tokio::test]
    async fn an_explicit_row_limit_lifts_the_soft_cap() {
        let h = Harness::build(
            MockPlan {
                rows_per_batch: 50,
                ..MockPlan::infinite(50)
            },
            MemoryPolicy {
                soft_row_cap: 120,
                ..policy()
            },
        );
        let mut events = h.queries.subscribe();
        let req = Request::Native {
            text: Arc::from("select * from events"),
            params: Vec::new(),
            opts: datagrep_api::request::ExecOpts {
                timeout: None,
                row_limit: Some(300),
                read_only_assert: false,
            },
        };
        h.queries.run(h.lease().await, req).await.expect("run");

        let seen = drain_until(&mut events, 3_000, |e| matches!(e, QueryEvent::Done { .. })).await;
        assert!(
            seen.iter()
                .any(|e| matches!(e, QueryEvent::Capped { rows: 300, .. })),
            "the explicit limit did not lift the cap: {seen:?}"
        );
        h.queries.shutdown();
        h.sessions.shutdown();
    }

    /// Closing a query is what returns its memory — cancel keeps the rows.
    #[tokio::test]
    async fn closing_a_query_returns_its_budget() {
        let h = harness(MockPlan {
            batches: Some(4),
            rows_per_batch: 50,
            ..MockPlan::default()
        });
        let qid = h
            .queries
            .run(h.lease().await, Request::native("select 1"))
            .await
            .expect("run");
        assert!(until(2_000, || h.queries.budget().used() > 0).await);

        h.queries.close(qid);
        assert!(
            until(2_000, || h.queries.budget().used() == 0).await,
            "closing the query did not return its bytes"
        );
        assert!(h.queries.store(qid).is_none());
        assert!(h.queries.cancel(qid).is_none(), "a closed query is gone");
        h.sessions.shutdown();
    }

    /// Cancelling an unknown query is a no-op, not a panic — the UI can press
    /// stop on a tab that finished a millisecond ago.
    #[tokio::test]
    async fn cancelling_an_unknown_query_is_a_no_op() {
        let h = harness(MockPlan::default());
        assert!(h.queries.cancel(QueryId(999)).is_none());
        assert!(h.queries.get_rows(QueryId(999), 0..10).await.is_none());
        let window = h.queries.rows_or_pending(QueryId(999), 0..10).await;
        assert_eq!(window.status, WindowStatus::Cancelled);
    }
}
