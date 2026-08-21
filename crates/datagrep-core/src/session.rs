use std::collections::HashMap;
use std::fmt;
use std::future::Future;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock, Weak};
use std::time::Duration;

use datagrep_api::caps::Capabilities;
use datagrep_api::catalog::Catalog;
use datagrep_api::config::{ConnectionConfig, ResolvedConfig};
use datagrep_api::driver::{
    CancelFlag, Canceller, ConnectCtx, Connection, Cursor, Driver, Enforcement, ServerInfo,
};
use datagrep_api::error::DbError;
use datagrep_api::request::Request;
use tokio::sync::{mpsc, oneshot, OwnedSemaphorePermit, Semaphore};
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;
use tracing::Instrument;

use crate::api::ProfileId;
use crate::registry::DriverRegistry;
use crate::timer::{TimerKey, TimerWheel};
use crate::{lock, read, write};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PoolPolicy {
    pub max_size: usize,
    pub min_idle: usize,
    pub idle_timeout: Duration,
    pub connect_timeout: Option<Duration>,
}

impl Default for PoolPolicy {
    fn default() -> Self {
        Self {
            max_size: 4,
            min_idle: 0,
            idle_timeout: Duration::from_secs(5 * 60),
            connect_timeout: Some(Duration::from_secs(30)),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ConnId(pub u64);

impl fmt::Display for ConnId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "conn{}", self.0)
    }
}

enum ConnCmd {
    Execute {
        req: Box<Request>,
        reply: oneshot::Sender<Result<Box<dyn Cursor>, DbError>>,
    },
    Ping {
        reply: oneshot::Sender<Result<(), DbError>>,
    },
    SetReadOnly {
        on: bool,
        reply: oneshot::Sender<Result<Enforcement, DbError>>,
    },
}

struct ConnInner {
    id: ConnId,
    caps: Capabilities,
    info: ServerInfo,
    canceller: Arc<dyn Canceller>,
    catalog: Arc<dyn Catalog>,
    cancel: CancellationToken,
    poisoned: Arc<AtomicBool>,
    tx: mpsc::Sender<ConnCmd>,
}

#[derive(Clone)]
pub struct ConnectionHandle {
    inner: Arc<ConnInner>,
}

impl ConnectionHandle {
    pub fn spawn(
        conn: Box<dyn Connection>,
        parent: &CancellationToken,
        id: ConnId,
    ) -> Result<Self, DbError> {
        let facts = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            (
                conn.capabilities(),
                conn.server_info().clone(),
                conn.canceller(),
                conn.catalog(),
            )
        }));
        let (caps, info, canceller, catalog) = match facts {
            Ok(facts) => facts,
            Err(payload) => return Err(DbError::DriverPanic(panic_message(payload))),
        };

        let cancel = parent.child_token();
        let poisoned = Arc::new(AtomicBool::new(false));
        let (tx, rx) = mpsc::channel(1);
        tokio::spawn(
            run_connection(conn, rx, cancel.clone(), poisoned.clone())
                .instrument(tracing::info_span!("connection", id = id.0)),
        );

        Ok(Self {
            inner: Arc::new(ConnInner {
                id,
                caps,
                info,
                canceller,
                catalog,
                cancel,
                poisoned,
                tx,
            }),
        })
    }

    pub fn id(&self) -> ConnId {
        self.inner.id
    }

    pub fn capabilities(&self) -> &Capabilities {
        &self.inner.caps
    }

    pub fn server_info(&self) -> &ServerInfo {
        &self.inner.info
    }

    pub fn canceller(&self) -> Arc<dyn Canceller> {
        self.inner.canceller.clone()
    }

    pub fn catalog(&self) -> Arc<dyn Catalog> {
        self.inner.catalog.clone()
    }

    pub fn cancel_token(&self) -> &CancellationToken {
        &self.inner.cancel
    }

    pub fn is_poisoned(&self) -> bool {
        self.inner.poisoned.load(Ordering::Acquire)
    }

    pub async fn execute(&self, req: Request) -> Result<Box<dyn Cursor>, DbError> {
        self.call(|reply| ConnCmd::Execute {
            req: Box::new(req),
            reply,
        })
        .await
    }

    pub async fn ping(&self) -> Result<(), DbError> {
        self.call(|reply| ConnCmd::Ping { reply }).await
    }

    pub async fn set_read_only(&self, on: bool) -> Result<Enforcement, DbError> {
        self.call(|reply| ConnCmd::SetReadOnly { on, reply }).await
    }

    pub fn close(&self) {
        self.inner.cancel.cancel();
    }

    async fn call<T>(
        &self,
        make: impl FnOnce(oneshot::Sender<Result<T, DbError>>) -> ConnCmd,
    ) -> Result<T, DbError> {
        if self.is_poisoned() {
            return Err(DbError::Closed);
        }
        let (reply, wait) = oneshot::channel();
        tokio::select! {
            biased;
            _ = self.inner.cancel.cancelled() => Err(DbError::Cancelled),
            result = async {
                self.inner.tx.send(make(reply)).await.map_err(|_| DbError::Closed)?;
                match wait.await {
                    Ok(result) => result,
                    Err(_) => Err(DbError::Closed),
                }
            } => result,
        }
    }
}

impl fmt::Debug for ConnectionHandle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ConnectionHandle")
            .field("id", &self.inner.id)
            .field("server", &self.inner.info.product)
            .field("poisoned", &self.is_poisoned())
            .finish()
    }
}

async fn run_connection(
    conn: Box<dyn Connection>,
    mut rx: mpsc::Receiver<ConnCmd>,
    cancel: CancellationToken,
    poisoned: Arc<AtomicBool>,
) {
    // `Arc` so each guarded call can hold the connection for its own task.
    let conn: Arc<dyn Connection> = Arc::from(conn);

    loop {
        let cmd = tokio::select! {
            biased;
            _ = cancel.cancelled() => break,
            received = rx.recv() => match received {
                Some(cmd) => cmd,
                None => break,
            },
        };

        let fatal = match cmd {
            ConnCmd::Execute { req, reply } => {
                let c = conn.clone();
                let out = guarded(async move { c.execute(*req).await }).await;
                let fatal = is_fatal(&out);
                let _ = reply.send(out);
                fatal
            }
            ConnCmd::Ping { reply } => {
                let c = conn.clone();
                let out = guarded(async move { c.ping().await }).await;
                let fatal = is_fatal(&out);
                let _ = reply.send(out);
                fatal
            }
            ConnCmd::SetReadOnly { on, reply } => {
                let c = conn.clone();
                let out = guarded(async move { c.set_read_only(on).await }).await;
                let fatal = is_fatal(&out);
                let _ = reply.send(out);
                fatal
            }
        };

        if fatal {
            poisoned.store(true, Ordering::Release);
            break;
        }
    }

    let c = conn.clone();
    if let Err(err) = guarded(async move { c.close().await }).await {
        tracing::debug!(%err, "closing connection");
    }
    tracing::debug!("connection task exited");
}

fn is_fatal<T>(out: &Result<T, DbError>) -> bool {
    matches!(out, Err(err) if !err.is_recoverable())
}

pub(crate) async fn guarded<T, F>(fut: F) -> Result<T, DbError>
where
    T: Send + 'static,
    F: Future<Output = Result<T, DbError>> + Send + 'static,
{
    match tokio::spawn(fut).await {
        Ok(result) => result,
        Err(err) if err.is_panic() => {
            let message = panic_message(err.into_panic());
            tracing::error!(%message, "driver panicked; poisoning the connection");
            Err(DbError::DriverPanic(message))
        }
        Err(_) => Err(DbError::Cancelled),
    }
}

fn panic_message(payload: Box<dyn std::any::Any + Send>) -> String {
    if let Some(s) = payload.downcast_ref::<&'static str>() {
        (*s).to_string()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        "driver panicked with a non-string payload".to_string()
    }
}

pub struct ConnLease {
    handle: ConnectionHandle,
    session: Option<Weak<Session>>,
    _permit: Option<OwnedSemaphorePermit>,
}

impl ConnLease {
    pub fn detached(handle: ConnectionHandle) -> Self {
        Self {
            handle,
            session: None,
            _permit: None,
        }
    }

    pub fn handle(&self) -> &ConnectionHandle {
        &self.handle
    }
}

impl std::ops::Deref for ConnLease {
    type Target = ConnectionHandle;

    fn deref(&self) -> &Self::Target {
        &self.handle
    }
}

impl Drop for ConnLease {
    fn drop(&mut self) {
        let Some(session) = self.session.as_ref().and_then(Weak::upgrade) else {
            // No pool to go back to (detached, or the session is gone).
            if self.session.is_some() {
                self.handle.close();
            }
            return;
        };
        session.release(self.handle.clone());
    }
}

impl fmt::Debug for ConnLease {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ConnLease")
            .field("handle", &self.handle)
            .field("pooled", &self.session.is_some())
            .finish()
    }
}

#[derive(Debug)]
pub struct PinnedConn {
    lease: ConnLease,
}

impl PinnedConn {
    pub fn handle(&self) -> &ConnectionHandle {
        self.lease.handle()
    }

    pub fn release(self) -> ConnLease {
        self.lease
    }
}

impl std::ops::Deref for PinnedConn {
    type Target = ConnectionHandle;

    fn deref(&self) -> &Self::Target {
        self.lease.handle()
    }
}

struct IdleConn {
    handle: ConnectionHandle,
    reap: Option<TimerKey>,
}

pub struct Session {
    profile: ProfileId,
    driver: Arc<dyn Driver>,
    config: ConnectionConfig,
    policy: PoolPolicy,
    cancel: CancellationToken,
    timer: Arc<TimerWheel>,
    permits: Arc<Semaphore>,
    idle: Mutex<Vec<IdleConn>>,
    next_id: AtomicU64,
    connects: AtomicU64,
    me: Mutex<Weak<Session>>,
}

impl Session {
    pub fn new(
        profile: ProfileId,
        driver: Arc<dyn Driver>,
        config: ConnectionConfig,
        policy: PoolPolicy,
        timer: Arc<TimerWheel>,
        parent: &CancellationToken,
    ) -> Arc<Self> {
        let session = Arc::new(Self {
            profile,
            driver,
            config,
            permits: Arc::new(Semaphore::new(policy.max_size.max(1))),
            policy,
            cancel: parent.child_token(),
            timer,
            idle: Mutex::new(Vec::new()),
            next_id: AtomicU64::new(0),
            connects: AtomicU64::new(0),
            me: Mutex::new(Weak::new()),
        });
        *lock(&session.me) = Arc::downgrade(&session);
        session
    }

    pub fn profile(&self) -> ProfileId {
        self.profile
    }

    pub fn cancel_token(&self) -> &CancellationToken {
        &self.cancel
    }

    pub fn idle_count(&self) -> usize {
        lock(&self.idle).len()
    }

    pub fn connect_count(&self) -> u64 {
        self.connects.load(Ordering::SeqCst)
    }

    pub async fn acquire(self: &Arc<Self>) -> Result<ConnLease, DbError> {
        if self.cancel.is_cancelled() {
            return Err(DbError::Closed);
        }
        let permit = tokio::select! {
            biased;
            _ = self.cancel.cancelled() => return Err(DbError::Closed),
            permit = self.permits.clone().acquire_owned() => {
                permit.map_err(|_| DbError::Closed)?
            }
        };

        while let Some(idle) = self.take_idle() {
            if !idle.is_poisoned() {
                return Ok(ConnLease {
                    handle: idle,
                    session: Some(lock(&self.me).clone()),
                    _permit: Some(permit),
                });
            }
            idle.close();
        }

        let handle = self.connect().await?;
        Ok(ConnLease {
            handle,
            session: Some(lock(&self.me).clone()),
            _permit: Some(permit),
        })
    }

    pub async fn pin(self: &Arc<Self>) -> Result<PinnedConn, DbError> {
        Ok(PinnedConn {
            lease: self.acquire().await?,
        })
    }

    pub fn shutdown(&self) {
        self.cancel.cancel();
        self.permits.close();
        for idle in lock(&self.idle).drain(..) {
            if let Some(key) = idle.reap {
                self.timer.cancel(key);
            }
            idle.handle.close();
        }
    }

    fn take_idle(&self) -> Option<ConnectionHandle> {
        let mut idle = lock(&self.idle);
        let entry = idle.pop()?;
        if let Some(key) = entry.reap {
            self.timer.cancel(key);
        }
        Some(entry.handle)
    }

    fn release(&self, handle: ConnectionHandle) {
        if handle.is_poisoned() || self.cancel.is_cancelled() {
            tracing::debug!(id = %handle.id(), "evicting connection instead of pooling it");
            handle.close();
            return;
        }
        let id = handle.id();
        let me = lock(&self.me).clone();
        let key = self
            .timer
            .schedule(Instant::now() + self.policy.idle_timeout, move || {
                if let Some(session) = me.upgrade() {
                    session.reap(id);
                }
            });
        lock(&self.idle).push(IdleConn {
            handle,
            reap: Some(key),
        });
    }

    fn reap(&self, id: ConnId) {
        let victim = {
            let mut idle = lock(&self.idle);
            idle.iter()
                .position(|c| c.handle.id() == id)
                .map(|pos| idle.remove(pos))
        };
        if let Some(entry) = victim {
            tracing::debug!(%id, "idle timeout; closing socket");
            entry.handle.close();
        }
    }

    async fn connect(self: &Arc<Self>) -> Result<ConnectionHandle, DbError> {
        let id = ConnId(self.next_id.fetch_add(1, Ordering::SeqCst));
        let cfg = ResolvedConfig::without_secrets(self.config.clone());
        let flag = CancelFlag::new();
        let ctx = ConnectCtx {
            cancel: flag.clone(),
            connect_timeout: self.policy.connect_timeout,
            application_name: Some(Arc::from("datagrep")),
        };

        let driver = self.driver.clone();
        let dial = tokio::spawn(async move { driver.connect(&cfg, ctx).await });
        let timeout = self.policy.connect_timeout;

        let dialled = tokio::select! {
            biased;
            _ = self.cancel.cancelled() => {
                flag.cancel();
                return Err(DbError::Cancelled);
            }
            result = with_optional_timeout(timeout, dial) => result,
        };

        let conn = match dialled {
            Ok(Ok(conn)) => conn?,
            Ok(Err(err)) if err.is_panic() => {
                return Err(DbError::DriverPanic(panic_message(err.into_panic())));
            }
            Ok(Err(_)) => return Err(DbError::Cancelled),
            Err(()) => {
                flag.cancel();
                return Err(DbError::Timeout);
            }
        };

        self.connects.fetch_add(1, Ordering::SeqCst);
        tracing::debug!(%id, profile = self.profile.0, "connected");
        ConnectionHandle::spawn(conn, &self.cancel, id)
    }
}

async fn with_optional_timeout<F: Future>(
    timeout: Option<Duration>,
    fut: F,
) -> Result<F::Output, ()> {
    match timeout {
        Some(d) => tokio::time::timeout(d, fut).await.map_err(|_| ()),
        None => Ok(fut.await),
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}

impl fmt::Debug for Session {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Session")
            .field("profile", &self.profile)
            .field("idle", &self.idle_count())
            .field("connects", &self.connect_count())
            .field("max_size", &self.policy.max_size)
            .finish()
    }
}

pub struct SessionRegistry {
    drivers: Arc<DriverRegistry>,
    timer: Arc<TimerWheel>,
    policy: PoolPolicy,
    cancel: CancellationToken,
    sessions: RwLock<HashMap<ProfileId, Arc<Session>>>,
}

impl SessionRegistry {
    pub fn new(drivers: Arc<DriverRegistry>, timer: Arc<TimerWheel>) -> Self {
        Self::with_policy(drivers, timer, PoolPolicy::default())
    }

    pub fn with_policy(
        drivers: Arc<DriverRegistry>,
        timer: Arc<TimerWheel>,
        policy: PoolPolicy,
    ) -> Self {
        Self {
            drivers,
            timer,
            policy,
            cancel: CancellationToken::new(),
            sessions: RwLock::new(HashMap::new()),
        }
    }

    pub fn cancel_token(&self) -> &CancellationToken {
        &self.cancel
    }

    pub fn open(
        &self,
        profile: ProfileId,
        driver_id: &str,
        config: ConnectionConfig,
    ) -> Result<Arc<Session>, DbError> {
        if let Some(session) = read(&self.sessions).get(&profile) {
            return Ok(session.clone());
        }
        let driver = self.drivers.get(driver_id).ok_or_else(|| {
            DbError::Config(datagrep_api::config::ConfigError::InvalidValue {
                key: "driver".into(),
                reason: format!("no driver registered as `{driver_id}`"),
            })
        })?;

        let mut sessions = write(&self.sessions);
        // Another task may have raced us between the read and the write.
        if let Some(session) = sessions.get(&profile) {
            return Ok(session.clone());
        }
        let session = Session::new(
            profile,
            driver,
            config,
            self.policy,
            self.timer.clone(),
            &self.cancel,
        );
        sessions.insert(profile, session.clone());
        Ok(session)
    }

    pub fn get(&self, profile: ProfileId) -> Option<Arc<Session>> {
        read(&self.sessions).get(&profile).cloned()
    }

    pub fn close(&self, profile: ProfileId) {
        if let Some(session) = write(&self.sessions).remove(&profile) {
            session.shutdown();
        }
    }

    pub fn len(&self) -> usize {
        read(&self.sessions).len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn shutdown(&self) {
        for (_, session) in write(&self.sessions).drain() {
            session.shutdown();
        }
        self.cancel.cancel();
    }
}

impl fmt::Debug for SessionRegistry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SessionRegistry")
            .field("sessions", &self.len())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::{MockDriver, MockPlan};
    use datagrep_api::driver::CancelKind;
    use std::time::Instant as StdInstant;

    async fn until(deadline_ms: u64, mut cond: impl FnMut() -> bool) -> bool {
        let start = StdInstant::now();
        while start.elapsed() < Duration::from_millis(deadline_ms) {
            if cond() {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
        cond()
    }

    struct Harness {
        registry: SessionRegistry,
        _timer: Arc<TimerWheel>,
        drivers: Arc<DriverRegistry>,
    }

    fn harness(policy: PoolPolicy) -> Harness {
        let drivers = Arc::new(DriverRegistry::new());
        let timer = Arc::new(TimerWheel::new());
        Harness {
            registry: SessionRegistry::with_policy(drivers.clone(), timer.clone(), policy),
            _timer: timer,
            drivers,
        }
    }

    fn register(
        h: &Harness,
        id: &'static str,
        plan: MockPlan,
    ) -> Arc<crate::testing::MockCounters> {
        let driver = Arc::new(MockDriver::with_plan(plan));
        let counters = driver.counters();
        h.drivers.register(id, move || driver.clone());
        counters
    }

    fn config(driver: &str) -> ConnectionConfig {
        ConnectionConfig {
            driver: Arc::from(driver),
            values: Default::default(),
        }
    }

    #[tokio::test]
    async fn opening_a_session_connects_to_nothing() {
        let h = harness(PoolPolicy::default());
        let counters = register(&h, "mock", MockPlan::default());
        let session = h
            .registry
            .open(ProfileId(1), "mock", config("mock"))
            .expect("open");

        assert_eq!(session.connect_count(), 0, "lazy connect violated");
        assert_eq!(counters.connects(), 0);
        assert_eq!(session.idle_count(), 0, "min_idle is 0 — no warm sockets");

        let lease = session.acquire().await.expect("acquire");
        assert_eq!(counters.connects(), 1, "the first use dials");
        drop(lease);
        assert_eq!(session.idle_count(), 1, "returned to the pool");

        // Reuse, not re-dial.
        let lease = session.acquire().await.expect("acquire");
        assert_eq!(counters.connects(), 1, "a pooled socket was re-dialled");
        drop(lease);
        h.registry.shutdown();
    }

    #[tokio::test]
    async fn idle_sockets_are_reaped_to_zero_on_the_timer_wheel() {
        let h = harness(PoolPolicy {
            idle_timeout: Duration::from_millis(40),
            ..PoolPolicy::default()
        });
        let counters = register(&h, "mock", MockPlan::default());
        let session = h
            .registry
            .open(ProfileId(1), "mock", config("mock"))
            .expect("open");

        {
            let _a = session.acquire().await.expect("acquire");
            let _b = session.acquire().await.expect("acquire");
        }
        assert_eq!(session.idle_count(), 2);
        assert_eq!(counters.connects(), 2);

        assert!(
            until(2_000, || session.idle_count() == 0).await,
            "idle sockets were not reaped to zero"
        );
        assert!(
            until(2_000, || counters.conn_closes() == 2).await,
            "reaped sockets were not actually closed"
        );
        h.registry.shutdown();
    }

    #[tokio::test]
    async fn pool_is_capped_at_max_size() {
        let h = harness(PoolPolicy {
            max_size: 2,
            ..PoolPolicy::default()
        });
        let counters = register(&h, "mock", MockPlan::default());
        let session = h
            .registry
            .open(ProfileId(1), "mock", config("mock"))
            .expect("open");

        let a = session.acquire().await.expect("a");
        let b = session.acquire().await.expect("b");
        assert_eq!(counters.connects(), 2);

        let waiting = {
            let session = session.clone();
            tokio::spawn(async move { session.acquire().await.map(|l| l.id()) })
        };
        tokio::time::sleep(Duration::from_millis(40)).await;
        assert!(!waiting.is_finished(), "the pool exceeded max_size");
        assert_eq!(counters.connects(), 2);

        let id = a.id();
        drop(a);
        let got = waiting.await.expect("join").expect("acquire");
        assert_eq!(got, id, "the freed socket was handed to the waiter");
        assert_eq!(counters.connects(), 2, "no extra socket was opened");
        drop(b);
        h.registry.shutdown();
    }

    #[tokio::test]
    async fn a_driver_panic_is_contained_to_its_own_connection() {
        let h = harness(PoolPolicy::default());
        register(
            &h,
            "boom",
            MockPlan {
                panic_on_execute: true,
                ..MockPlan::default()
            },
        );
        let healthy = register(&h, "mock", MockPlan::default());

        let bad = h
            .registry
            .open(ProfileId(1), "boom", config("boom"))
            .expect("open");
        let good = h
            .registry
            .open(ProfileId(2), "mock", config("mock"))
            .expect("open");

        // A sibling connection, open and working before the panic.
        let sibling = good.acquire().await.expect("sibling");
        assert!(
            sibling.execute(Request::native("select 1")).await.is_ok(),
            "sibling works before"
        );

        let lease = bad.acquire().await.expect("acquire");
        let err = match lease.execute(Request::native("select 1")).await {
            Err(err) => err,
            Ok(_) => panic!("the driver panicked, so this cannot succeed"),
        };
        assert!(
            matches!(err, DbError::DriverPanic(ref m) if m.contains("mock driver panic")),
            "expected DriverPanic, got {err:?}"
        );
        assert!(!err.is_recoverable());
        assert!(lease.is_poisoned(), "the connection must be poisoned");

        // Evicted, not pooled.
        drop(lease);
        assert_eq!(
            bad.idle_count(),
            0,
            "a poisoned socket went back in the pool"
        );

        // The app lives: the same session can still open a fresh socket…
        let again = bad.acquire().await.expect("session survived the panic");
        assert!(!again.is_poisoned(), "a fresh socket starts clean");
        drop(again);

        // …and the sibling connection is completely unaffected.
        assert!(
            sibling.execute(Request::native("select 1")).await.is_ok(),
            "sibling still works after a sibling driver panicked"
        );
        assert_eq!(healthy.executes(), 2);
        h.registry.shutdown();
    }

    #[tokio::test]
    async fn cancelling_a_session_cancels_its_connections() {
        let h = harness(PoolPolicy::default());
        let counters = register(&h, "mock", MockPlan::default());
        let session = h
            .registry
            .open(ProfileId(1), "mock", config("mock"))
            .expect("open");
        let lease = session.acquire().await.expect("acquire");
        let conn_token = lease.cancel_token().clone();
        assert!(!conn_token.is_cancelled());

        session.shutdown();
        assert!(conn_token.is_cancelled(), "child token was not cancelled");
        assert!(
            until(2_000, || counters.conn_closes() == 1).await,
            "the socket was not closed"
        );
        assert!(matches!(
            lease.execute(Request::native("x")).await,
            Err(DbError::Cancelled)
        ));
    }

    #[tokio::test]
    async fn a_pinned_connection_leaves_the_pool() {
        let h = harness(PoolPolicy {
            max_size: 1,
            ..PoolPolicy::default()
        });
        register(&h, "mock", MockPlan::default());
        let session = h
            .registry
            .open(ProfileId(1), "mock", config("mock"))
            .expect("open");

        let pinned = session.pin().await.expect("pin");
        let id = pinned.id();
        let racer = {
            let session = session.clone();
            tokio::spawn(async move { session.acquire().await.map(|l| l.id()) })
        };
        tokio::time::sleep(Duration::from_millis(40)).await;
        assert!(
            !racer.is_finished(),
            "a pinned socket was handed to another statement"
        );

        drop(pinned.release());
        let got = racer.await.expect("join").expect("acquire");
        assert_eq!(got, id, "the socket returns to the pool when unpinned");
        h.registry.shutdown();
    }

    #[tokio::test]
    async fn connection_facts_are_cached_at_spawn() {
        let h = harness(PoolPolicy::default());
        register(&h, "mock", MockPlan::default());
        let session = h
            .registry
            .open(ProfileId(1), "mock", config("mock"))
            .expect("open");
        let lease = session.acquire().await.expect("acquire");

        assert_eq!(lease.capabilities().default_fetch_rows, 500);
        assert_eq!(&*lease.server_info().product, "mockdb");
        assert_eq!(lease.canceller().kind(), CancelKind::ServerSide);
        assert_eq!(lease.catalog().levels().len(), 1);
        h.registry.shutdown();
    }

    #[tokio::test]
    async fn opening_an_unregistered_driver_is_a_config_error() {
        let drivers = Arc::new(DriverRegistry::new());
        let registry = SessionRegistry::new(drivers, Arc::new(TimerWheel::new()));
        let err = registry
            .open(ProfileId(1), "nope", config("nope"))
            .expect_err("unregistered");
        assert!(matches!(err, DbError::Config(_)));
    }
}
