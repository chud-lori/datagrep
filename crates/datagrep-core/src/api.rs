//! `CoreApi` — the façade every frontend links against.
//!
//! Same core, three faces: GUI, TUI and CLI all link this in-process. There is
//! no IPC boundary anywhere; the desktop UI is just another `CoreApi` caller.
//!
//! So this is a plain struct with async methods, not a trait: there is no
//! second implementation to abstract over, and a trait here would only buy
//! dynamic dispatch nobody asked for. It becomes a trait the day a second
//! backend exists, and not before.
//!
//! One method is deliberately **not** async underneath: nothing in
//! [`CoreApi::cancel`] awaits before it returns, so the stop button can never
//! hang. It is spelled `async` only so every frontend calls the façade the
//! same way.
//!
//! The profile store here is in memory. Persisting profiles to SQLite —
//! folders, history, tabs, and a `secret_ref` rather than a secret — is
//! `datagrep-profiles`' job; this core only ever holds the resolved shape.

use std::collections::HashMap;
use std::fmt;
use std::ops::Range;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};

use datagrep_api::caps::Capabilities;
use datagrep_api::catalog::{ListOpts, ObjectNode, Page};
use datagrep_api::config::{ConfigError, ConnectionConfig};
use datagrep_api::driver::Driver;
use datagrep_api::error::DbError;
use datagrep_api::request::Request;
use datagrep_api::shape::ObjectPath;
use tokio::sync::broadcast;

use crate::query::{CancelReport, QueryEvent, QueryId, QueryMgr};
use crate::registry::DriverRegistry;
use crate::session::{guarded, PoolPolicy, Session, SessionRegistry};
use crate::store::{GlobalBudget, MemoryPolicy, RowWindow};
use crate::timer::TimerWheel;
use crate::{read, write};

/// Identity of a saved connection profile.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ProfileId(pub u64);

impl fmt::Display for ProfileId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "profile{}", self.0)
    }
}

/// Which environment a profile points at. Load-bearing, not decoration: `Prod`
/// is what turns on red window chrome, confirm-on-write, and the rest of the
/// blast-radius guardrails.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum Env {
    #[default]
    Dev,
    Staging,
    Prod,
}

/// A saved connection, minus its secrets. Safe to persist, export, and diff —
/// secrets live in the OS keychain and profiles hold only a reference to them,
/// so a leaked or shared profile file is not a leaked password.
#[derive(Debug, Clone, PartialEq)]
pub struct Profile {
    pub id: ProfileId,
    pub name: Arc<str>,
    /// Registry id of the driver (`postgres`, `sqlite`, …). The **only** place
    /// above `datagrep-api` where a driver is named, and it is data, not a branch.
    pub driver: Arc<str>,
    pub config: ConnectionConfig,
    pub env: Env,
    /// Client-side read-only assertion — one layer of the write guardrails,
    /// above the engine's own read-only enforcement.
    pub read_only: bool,
}

/// The core, as one object.
///
/// Construct it inside a tokio runtime: the shared [`TimerWheel`] spawns its
/// (single, armed-on-demand) worker task.
pub struct CoreApi {
    drivers: Arc<DriverRegistry>,
    timer: Arc<TimerWheel>,
    sessions: Arc<SessionRegistry>,
    queries: Arc<QueryMgr>,
    profiles: RwLock<HashMap<ProfileId, Profile>>,
    next_profile: AtomicU64,
}

impl CoreApi {
    /// A core with the default published memory contract.
    pub fn new() -> Self {
        Self::with_policy(MemoryPolicy::default(), PoolPolicy::default())
    }

    /// A core with explicit policies — what the benchmark harness and the
    /// `--report-footprint` flag use to prove the numbers.
    pub fn with_policy(memory: MemoryPolicy, pool: PoolPolicy) -> Self {
        let drivers = Arc::new(DriverRegistry::new());
        let timer = Arc::new(TimerWheel::new());
        let budget = GlobalBudget::from_policy(&memory);
        Self {
            sessions: Arc::new(SessionRegistry::with_policy(
                drivers.clone(),
                timer.clone(),
                pool,
            )),
            queries: Arc::new(QueryMgr::new(memory, budget)),
            drivers,
            timer,
            profiles: RwLock::new(HashMap::new()),
            next_profile: AtomicU64::new(1),
        }
    }

    // ---- drivers ------------------------------------------------------

    /// Register a driver under its registry id.
    ///
    /// Not async because it cannot block: registration is a hashmap insert and
    /// **constructs nothing** — the driver is built on first use. That is why
    /// adding an engine costs nothing at startup, which is the whole answer to
    /// DBeaver's per-driver classloader.
    pub fn register_driver(
        &self,
        id: impl Into<Arc<str>>,
        ctor: impl Fn() -> Arc<dyn Driver> + Send + Sync + 'static,
    ) {
        self.drivers.register(id, ctor);
    }

    /// Registered driver ids.
    pub fn drivers(&self) -> Vec<Arc<str>> {
        self.drivers.ids()
    }

    // ---- profiles -----------------------------------------------------

    /// Add a profile to the in-memory store and return its id. Opens nothing.
    pub async fn add_profile(
        &self,
        name: impl Into<Arc<str>>,
        config: ConnectionConfig,
    ) -> ProfileId {
        let id = ProfileId(self.next_profile.fetch_add(1, Ordering::SeqCst));
        let profile = Profile {
            id,
            name: name.into(),
            driver: config.driver.clone(),
            config,
            env: Env::default(),
            read_only: false,
        };
        write(&self.profiles).insert(id, profile);
        id
    }

    /// Add a fully specified profile (env, read-only flag and all).
    pub async fn add_profile_full(&self, mut profile: Profile) -> ProfileId {
        let id = ProfileId(self.next_profile.fetch_add(1, Ordering::SeqCst));
        profile.id = id;
        write(&self.profiles).insert(id, profile);
        id
    }

    /// One profile.
    pub async fn profile(&self, id: ProfileId) -> Option<Profile> {
        read(&self.profiles).get(&id).cloned()
    }

    /// Every profile, ordered by id.
    pub async fn profiles(&self) -> Vec<Profile> {
        let mut all: Vec<_> = read(&self.profiles).values().cloned().collect();
        all.sort_by_key(|p| p.id);
        all
    }

    // ---- connections --------------------------------------------------

    /// Open a session for a profile and prove it can reach the server.
    ///
    /// Startup never calls this: connecting is lazy, so opening the app dials
    /// nothing. When the *user* asks to connect, one socket is opened,
    /// validated, and returned to the pool, where the idle reaper will close it
    /// again if nothing follows.
    pub async fn connect(&self, id: ProfileId) -> Result<Arc<Session>, DbError> {
        let session = self.session(id)?;
        // Acquiring and immediately releasing dials once and warms the pool
        // without creating a floor (`min_idle` stays 0).
        let lease = session.acquire().await?;
        drop(lease);
        Ok(session)
    }

    /// The session for a profile, creating the (empty, unconnected) pool if
    /// needed. Opens no socket.
    pub fn session(&self, id: ProfileId) -> Result<Arc<Session>, DbError> {
        let profile = read(&self.profiles)
            .get(&id)
            .cloned()
            .ok_or_else(|| unknown_profile(id))?;
        self.sessions
            .open(id, &profile.driver, profile.config.clone())
    }

    /// Post-handshake capabilities of a profile's connection — what the UI
    /// disables controls from, instead of offering a control that only fails
    /// once the user clicks it.
    pub async fn capabilities(&self, id: ProfileId) -> Result<Capabilities, DbError> {
        let session = self.session(id)?;
        let lease = session.acquire().await?;
        Ok(lease.capabilities().clone())
    }

    /// Disconnect a profile: close its pool, drop its sockets.
    pub async fn disconnect(&self, id: ProfileId) {
        self.sessions.close(id);
    }

    // ---- queries ------------------------------------------------------

    /// Run a request on a profile's connection.
    ///
    /// Returns as soon as the server accepts it; the result streams into a
    /// [`crate::store::ResultStore`] behind a bounded channel. The connection
    /// is held for the query's whole life and released when it ends.
    pub async fn run_query(&self, id: ProfileId, req: Request) -> Result<QueryId, DbError> {
        let session = self.session(id)?;
        let lease = session.acquire().await?;
        self.queries.run(lease, req).await
    }

    /// **The export endpoint**: run `req` and stream its result **straight
    /// into `sink`, never through the result store**.
    ///
    /// Export runs its own cursor at full fetch size straight to a file. That
    /// is what makes "Export all" different from "load all".
    ///
    /// One driver chunk is in flight at a time; nothing is admitted to any
    /// store, so [`CoreApi::result_bytes`] does not move no matter how many
    /// rows are exported. See [`crate::export`] for the sink contract.
    pub async fn run_export(
        &self,
        id: ProfileId,
        req: Request,
        sink: &mut dyn crate::export::ExportSink,
    ) -> Result<crate::export::ExportStats, DbError> {
        let session = self.session(id)?;
        let lease = session.acquire().await?;
        crate::export::run_export_on(&lease, req, sink).await
    }

    /// Resolve a row window. Asking for rows beyond what has been fetched is
    /// what resumes the feeder — scrolling is the pull signal.
    pub async fn get_rows(&self, qid: QueryId, range: Range<u64>) -> Result<RowWindow, DbError> {
        self.queries
            .get_rows(qid, range)
            .await
            .ok_or(DbError::Closed)
    }

    /// **The stop button.** Nothing here awaits: the local half is done by the
    /// time this returns, and the server half is reported later as
    /// [`QueryEvent::CancelOutcome`].
    pub async fn cancel(&self, qid: QueryId) -> Result<CancelReport, DbError> {
        self.queries.cancel(qid).ok_or(DbError::Closed)
    }

    /// Close a result tab: forget the query and give its memory back. This,
    /// not cancelling, is what returns the bytes.
    pub async fn close_query(&self, qid: QueryId) {
        self.queries.close(qid);
    }

    /// Follow every query in the process — one channel, and nothing polls it.
    pub fn subscribe_events(&self) -> broadcast::Receiver<QueryEvent> {
        self.queries.subscribe()
    }

    /// The query manager, for callers that want the store directly.
    pub fn queries(&self) -> &Arc<QueryMgr> {
        &self.queries
    }

    // ---- catalog ------------------------------------------------------

    /// One page of catalog children under `parent`.
    ///
    /// Expand-on-demand only, always bounded by [`ListOpts`], and never a
    /// whole-catalog crawl: eager introspection on connect is the incumbents'
    /// defining mistake and is refused by construction. The call is run inside
    /// its own task so a panicking catalog implementation cannot take the app
    /// with it.
    pub async fn list_catalog(
        &self,
        id: ProfileId,
        parent: &ObjectPath,
        opts: ListOpts,
    ) -> Result<Page<ObjectNode>, DbError> {
        let session = self.session(id)?;
        let lease = session.acquire().await?;
        let catalog = lease.catalog();
        let parent = parent.clone();
        guarded(async move { catalog.children(&parent, opts).await }).await
    }

    // ---- lifecycle ----------------------------------------------------

    /// The shared deadline wheel. Everything with a deadline schedules here;
    /// it disarms completely when nothing is pending, so an idle app costs no
    /// timer wakeups at all.
    pub fn timer(&self) -> &Arc<TimerWheel> {
        &self.timer
    }

    /// Resident result bytes across every result set — the number the memory
    /// contract is about, readable at any time (this is what
    /// `--report-footprint` prints).
    pub fn result_bytes(&self) -> usize {
        self.queries.budget().used()
    }

    /// Stop every query and close every socket.
    pub async fn shutdown(&self) {
        self.queries.shutdown();
        self.sessions.shutdown();
    }
}

impl Default for CoreApi {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Debug for CoreApi {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CoreApi")
            .field("drivers", &self.drivers.ids())
            .field("profiles", &read(&self.profiles).len())
            .field("sessions", &self.sessions.len())
            .field("result_bytes", &self.result_bytes())
            .finish()
    }
}

fn unknown_profile(id: ProfileId) -> DbError {
    DbError::Config(ConfigError::InvalidValue {
        key: "profile".into(),
        reason: format!("no profile with id {}", id.0),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::query::QueryEvent;
    use crate::store::{SpillPolicy, WindowStatus};
    use crate::testing::{MockDriver, MockPlan};
    use std::time::{Duration, Instant};

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

    fn core(plan: MockPlan) -> (CoreApi, Arc<crate::testing::MockCounters>) {
        let core = CoreApi::with_policy(policy(), PoolPolicy::default());
        let driver = Arc::new(MockDriver::with_plan(plan));
        let counters = driver.counters();
        core.register_driver("mock", move || driver.clone());
        (core, counters)
    }

    fn mock_config() -> ConnectionConfig {
        ConnectionConfig {
            driver: Arc::from("mock"),
            values: Default::default(),
        }
    }

    /// The whole façade in one pass: register, add a profile, connect, query,
    /// window, browse the catalog, close.
    #[tokio::test]
    async fn the_facade_runs_a_query_end_to_end() {
        let (core, counters) = core(MockPlan {
            batches: Some(3),
            rows_per_batch: 20,
            ..MockPlan::default()
        });
        assert_eq!(core.drivers(), vec![Arc::from("mock")]);

        let id = core.add_profile("local", mock_config()).await;
        assert_eq!(core.profiles().await.len(), 1);
        assert_eq!(
            core.profile(id).await.expect("profile").name.as_ref(),
            "local"
        );

        // Adding a profile connects to nothing — connecting is lazy.
        assert_eq!(
            counters.connects(),
            0,
            "adding a profile dialled the server"
        );

        let session = core.connect(id).await.expect("connect");
        assert_eq!(counters.connects(), 1);
        assert_eq!(session.idle_count(), 1, "the socket went back to the pool");

        let mut events = core.subscribe_events();
        let qid = core
            .run_query(id, Request::native("select 1"))
            .await
            .expect("run");
        assert!(matches!(
            events.recv().await,
            Ok(QueryEvent::Accepted { .. })
        ));
        assert!(until(3_000, || core.result_bytes() > 0).await);
        assert!(
            until(3_000, || core
                .queries()
                .state(qid)
                .map(|s| s.phase.is_terminal())
                .unwrap_or(false))
            .await
        );

        let window = core.get_rows(qid, 10..30).await.expect("window");
        assert_eq!(window.status, WindowStatus::Ready);
        assert_eq!(window.rows(), 20);

        let page = core
            .list_catalog(id, &ObjectPath::root(), ListOpts::default())
            .await
            .expect("catalog");
        assert_eq!(page.items.len(), 2);
        assert_eq!(page.items[0].path.to_string(), "users");

        core.close_query(qid).await;
        assert!(
            until(2_000, || core.result_bytes() == 0).await,
            "closing the tab did not return the memory"
        );
        core.shutdown().await;
    }

    /// Unknown ids are errors with something a user could act on, never panics.
    #[tokio::test]
    async fn unknown_ids_are_errors_not_panics() {
        let (core, _) = core(MockPlan::default());
        assert!(matches!(
            core.run_query(ProfileId(42), Request::native("x")).await,
            Err(DbError::Config(_))
        ));
        assert!(matches!(
            core.get_rows(QueryId(42), 0..10).await,
            Err(DbError::Closed)
        ));
        assert!(matches!(
            core.cancel(QueryId(42)).await,
            Err(DbError::Closed)
        ));
        core.shutdown().await;
    }

    /// A profile whose driver was never registered fails at session open with
    /// a configuration error — the registry stays the only coupling to a
    /// concrete engine.
    #[tokio::test]
    async fn a_profile_for_an_unregistered_driver_fails_cleanly() {
        let core = CoreApi::with_policy(policy(), PoolPolicy::default());
        let id = core
            .add_profile(
                "ghost",
                ConnectionConfig {
                    driver: Arc::from("nope"),
                    values: Default::default(),
                },
            )
            .await;
        assert!(matches!(core.connect(id).await, Err(DbError::Config(_))));
        core.shutdown().await;
    }

    /// The stop button, from the façade: it returns without awaiting anything
    /// meaningful, and the truth about the server follows on the event channel.
    #[tokio::test]
    async fn cancel_through_the_facade_returns_immediately() {
        let (core, counters) = core(MockPlan {
            batch_delay: Some(Duration::from_millis(10)),
            ..MockPlan::infinite(20)
        });
        let id = core.add_profile("local", mock_config()).await;
        let mut events = core.subscribe_events();
        let qid = core
            .run_query(id, Request::native("select * from events"))
            .await
            .expect("run");
        assert!(until(2_000, || core.result_bytes() > 0).await);

        let pressed = Instant::now();
        let report = core.cancel(qid).await.expect("cancel");
        assert!(report.local_stopped);
        assert!(
            pressed.elapsed() < Duration::from_millis(50),
            "cancel took {:?}",
            pressed.elapsed()
        );

        let mut saw_outcome = false;
        let deadline = Instant::now() + Duration::from_secs(3);
        while Instant::now() < deadline {
            match tokio::time::timeout(Duration::from_millis(50), events.recv()).await {
                Ok(Ok(QueryEvent::CancelOutcome { report, .. })) => {
                    assert!(report.outcome.is_some());
                    saw_outcome = true;
                    break;
                }
                Ok(Ok(_)) => {}
                Ok(Err(_)) => break,
                Err(_) => {}
            }
        }
        assert!(saw_outcome, "no CancelOutcome reached the frontend");
        assert_eq!(counters.cursor_closes(), 1);
        core.shutdown().await;
    }
}
