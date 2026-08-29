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
use datagrep_api::safety::{Attestation, SafetyLevel};
use datagrep_api::shape::ObjectPath;
use tokio::sync::broadcast;

use crate::query::{CancelReport, QueryEvent, QueryId, QueryMgr};
use crate::registry::DriverRegistry;
use crate::safety::{SafetyDecision, SafetyGate};
use crate::session::{guarded, PoolPolicy, Session, SessionRegistry};
use crate::store::{GlobalBudget, MemoryPolicy, RowWindow};
use crate::timer::TimerWheel;
use crate::{read, write};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ProfileId(pub u64);

impl fmt::Display for ProfileId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "profile{}", self.0)
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Profile {
    pub id: ProfileId,
    pub name: Arc<str>,
    pub driver: Arc<str>,
    pub config: ConnectionConfig,
    pub read_only: bool,
    pub safety: SafetyLevel,
}

pub struct CoreApi {
    drivers: Arc<DriverRegistry>,
    timer: Arc<TimerWheel>,
    sessions: Arc<SessionRegistry>,
    queries: Arc<QueryMgr>,
    profiles: RwLock<HashMap<ProfileId, Profile>>,
    next_profile: AtomicU64,
}

impl CoreApi {
    pub fn new() -> Self {
        Self::with_policy(MemoryPolicy::default(), PoolPolicy::default())
    }

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

    pub fn register_driver(
        &self,
        id: impl Into<Arc<str>>,
        ctor: impl Fn() -> Arc<dyn Driver> + Send + Sync + 'static,
    ) {
        self.drivers.register(id, ctor);
    }

    pub fn drivers(&self) -> Vec<Arc<str>> {
        self.drivers.ids()
    }

    // ---- profiles -----------------------------------------------------

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
            read_only: false,
            safety: SafetyLevel::default(),
        };
        write(&self.profiles).insert(id, profile);
        id
    }

    pub async fn add_profile_full(&self, mut profile: Profile) -> ProfileId {
        let id = ProfileId(self.next_profile.fetch_add(1, Ordering::SeqCst));
        profile.id = id;
        write(&self.profiles).insert(id, profile);
        id
    }

    pub async fn profile(&self, id: ProfileId) -> Option<Profile> {
        read(&self.profiles).get(&id).cloned()
    }

    pub async fn profiles(&self) -> Vec<Profile> {
        let mut all: Vec<_> = read(&self.profiles).values().cloned().collect();
        all.sort_by_key(|p| p.id);
        all
    }

    // ---- connections --------------------------------------------------

    pub async fn connect(&self, id: ProfileId) -> Result<Arc<Session>, DbError> {
        let session = self.session(id)?;
        let lease = session.acquire().await?;
        drop(lease);
        Ok(session)
    }

    pub fn session(&self, id: ProfileId) -> Result<Arc<Session>, DbError> {
        let profile = read(&self.profiles)
            .get(&id)
            .cloned()
            .ok_or_else(|| unknown_profile(id))?;
        self.sessions.open(
            id,
            &profile.name,
            &profile.driver,
            profile.config.clone(),
            profile.safety,
        )
    }

    // ---- safety -------------------------------------------------------

    pub fn safety_gate(&self, id: ProfileId) -> Result<Arc<SafetyGate>, DbError> {
        Ok(self.session(id)?.gate().clone())
    }

    pub async fn safety(&self, id: ProfileId) -> Option<SafetyLevel> {
        read(&self.profiles).get(&id).map(|p| p.safety)
    }

    pub async fn set_safety(&self, id: ProfileId, level: SafetyLevel) -> Result<(), DbError> {
        {
            let mut profiles = write(&self.profiles);
            let profile = profiles.get_mut(&id).ok_or_else(|| unknown_profile(id))?;
            profile.safety = level;
        }
        if let Some(session) = self.sessions.get(id) {
            session.gate().set_level(level);
        }
        Ok(())
    }

    pub fn evaluate_safety(&self, id: ProfileId, sql: &str) -> Result<SafetyDecision, DbError> {
        Ok(self.safety_gate(id)?.plan(sql))
    }

    pub fn satisfy_safety(
        &self,
        id: ProfileId,
        challenge: &str,
        attestation: &Attestation,
    ) -> Result<(), DbError> {
        self.safety_gate(id)?.satisfy(challenge, attestation)
    }

    pub async fn capabilities(&self, id: ProfileId) -> Result<Capabilities, DbError> {
        let session = self.session(id)?;
        let lease = session.acquire().await?;
        Ok(lease.capabilities().clone())
    }

    pub async fn disconnect(&self, id: ProfileId) {
        self.sessions.close(id);
    }

    // ---- queries ------------------------------------------------------

    pub async fn run_query(&self, id: ProfileId, req: Request) -> Result<QueryId, DbError> {
        let session = self.session(id)?;
        let lease = session.acquire().await?;
        self.queries.run(lease, req).await
    }

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

    pub async fn get_rows(&self, qid: QueryId, range: Range<u64>) -> Result<RowWindow, DbError> {
        self.queries
            .get_rows(qid, range)
            .await
            .ok_or(DbError::Closed)
    }

    pub async fn cancel(&self, qid: QueryId) -> Result<CancelReport, DbError> {
        self.queries.cancel(qid).ok_or(DbError::Closed)
    }

    pub async fn close_query(&self, qid: QueryId) {
        self.queries.close(qid);
    }

    pub fn subscribe_events(&self) -> broadcast::Receiver<QueryEvent> {
        self.queries.subscribe()
    }

    pub fn queries(&self) -> &Arc<QueryMgr> {
        &self.queries
    }

    // ---- catalog ------------------------------------------------------

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

    pub fn timer(&self) -> &Arc<TimerWheel> {
        &self.timer
    }

    pub fn result_bytes(&self) -> usize {
        self.queries.budget().used()
    }

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

    struct RefusedSink;

    impl crate::export::ExportSink for RefusedSink {
        fn begin(&mut self, _shape: &datagrep_api::shape::Shape) -> Result<(), DbError> {
            panic!("the export reached the driver")
        }

        fn chunk(
            &mut self,
            _batch: datagrep_api::driver::Batch,
        ) -> Result<crate::export::SinkFlow, DbError> {
            panic!("the export reached the driver")
        }
    }

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

    #[tokio::test]
    async fn the_ladder_stops_a_write_before_the_driver_sees_it() {
        let (core, counters) = core(MockPlan::default());
        let id = core.add_profile("prod", mock_config()).await;
        core.set_safety(id, SafetyLevel::AuthWrites)
            .await
            .expect("set the rung");

        core.run_query(id, Request::native("select 1"))
            .await
            .expect("a read is exempt at this rung");
        assert_eq!(counters.executes(), 1);

        let err = core
            .run_query(id, Request::native("delete from users"))
            .await
            .expect_err("the write must be refused");
        let DbError::Safety { challenge, .. } = err else {
            panic!("expected a safety refusal, got {err:?}");
        };
        assert_eq!(counters.executes(), 1, "the write reached the driver");

        core.satisfy_safety(id, &challenge, &Attestation::Acknowledged)
            .expect_err("an acknowledgement is not authentication");
        assert!(core
            .run_query(id, Request::native("delete from users"))
            .await
            .is_err());

        let decision = core
            .evaluate_safety(id, "delete from users")
            .expect("evaluate");
        let challenge = decision.challenge.expect("a challenge to clear");
        core.satisfy_safety(
            id,
            &challenge,
            &Attestation::TypedPhrase {
                typed: "prod".to_string(),
            },
        )
        .expect("the connection name clears the rung");

        core.run_query(id, Request::native("delete from users"))
            .await
            .expect("the cleared statement runs");
        assert_eq!(counters.executes(), 2);
        core.shutdown().await;
    }

    #[tokio::test]
    async fn an_export_is_gated_on_the_same_ladder_as_a_query() {
        let (core, counters) = core(MockPlan::default());
        let id = core.add_profile("prod", mock_config()).await;
        core.set_safety(id, SafetyLevel::WarnAll)
            .await
            .expect("set the rung");

        let mut sink = RefusedSink;
        let err = core
            .run_export(id, Request::native("select 1"), &mut sink)
            .await
            .expect_err("every-query rung gates the export too");
        assert!(matches!(err, DbError::Safety { .. }));
        assert_eq!(counters.executes(), 0);
        core.shutdown().await;
    }

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
