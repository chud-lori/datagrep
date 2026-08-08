//! Reusable mock driver stack, behind `#[cfg(any(test, feature = "testing"))]`.
//!
//! The memory contract (design §3.2) can only be tested against a producer
//! that never stops and a consumer that stops reading, so the mocks here are
//! deliberately *hostile*: infinite batch production, a panic-on-execute mode
//! for driver-panic isolation (§3.5), a slow mode for adaptive fetch sizing
//! (§3.2), and a call counter so a test can prove the feeder actually parked
//! rather than merely looking idle.
//!
//! Sibling crates (`datagrep-drv-*`, the spike UI) enable the `testing` feature to
//! reuse this instead of growing their own half-correct copy.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use datagrep_api::caps::{Capabilities, Caps, LanguageId, ParamStyle, SqlDialect};
use datagrep_api::catalog::{
    Catalog, Completion, CompletionCtx, Enumeration, InferredSchema, LevelDef, ListOpts,
    ObjectDetail, ObjectKind, ObjectNode, Page,
};
use datagrep_api::config::{ConfigError, ConfigSchema, ConnectionConfig, ResolvedConfig};
use datagrep_api::driver::{
    Batch, BoxFuture, CancelKind, CancelOutcome, Canceller, ConnectCtx, Connection, Cursor,
    CursorStats, Driver, DriverMeta, Enforcement, FetchHint, Payload, ResumeToken, ServerInfo,
    Transaction, TxOpts,
};
use datagrep_api::error::DbError;
use datagrep_api::request::Request;
use datagrep_api::shape::{FieldDef, FieldFlags, LogicalType, ObjectPath, RowSchema, Shape};
use datagrep_api::value::Value;

/// Every mock call site bumps one of these. Shared (via `Arc`) by the driver
/// and everything it creates, so a test reads one object to see the whole
/// pipeline's behaviour — in particular [`MockCounters::next_batch_calls`],
/// which is how "the feeder parked" is proven rather than assumed.
#[derive(Debug, Default)]
pub struct MockCounters {
    connects: AtomicUsize,
    executes: AtomicUsize,
    next_batch: AtomicUsize,
    cursor_closes: AtomicUsize,
    conn_closes: AtomicUsize,
    cancels: AtomicUsize,
    pings: AtomicUsize,
}

impl MockCounters {
    /// Completed `Driver::connect` calls.
    pub fn connects(&self) -> usize {
        self.connects.load(Ordering::SeqCst)
    }

    /// `Connection::execute` calls (counted before any configured panic).
    pub fn executes(&self) -> usize {
        self.executes.load(Ordering::SeqCst)
    }

    /// `Cursor::next_batch` calls — THE backpressure probe (design §3.2).
    pub fn next_batch_calls(&self) -> usize {
        self.next_batch.load(Ordering::SeqCst)
    }

    /// `Cursor::close` calls; a cancelled query must close its cursor (§3.3).
    pub fn cursor_closes(&self) -> usize {
        self.cursor_closes.load(Ordering::SeqCst)
    }

    /// `Connection::close` calls; an idle-reaped pool entry must close (§3.5).
    pub fn conn_closes(&self) -> usize {
        self.conn_closes.load(Ordering::SeqCst)
    }

    /// `Canceller::cancel` calls — the server half of a stop (§3.3).
    pub fn cancels(&self) -> usize {
        self.cancels.load(Ordering::SeqCst)
    }

    /// `Connection::ping` calls; liveness is lazy, never on a timer (§3.4).
    pub fn pings(&self) -> usize {
        self.pings.load(Ordering::SeqCst)
    }
}

/// What the mock cursor emits per batch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MockPayload {
    /// `Shape::Table` rows — the Arrow lane of the store.
    Rows,
    /// `Shape::Documents` values — the non-Arrow lane (design §3.2).
    Docs,
    /// `Shape::Ack` — a statement acknowledgement (INSERT/DDL). Emits
    /// `Payload::Empty` chunks; the affected count travels in the shape.
    Ack { affected: Option<u64> },
}

/// Everything configurable about the mock stack. `Default` is a small, finite,
/// well-behaved result; every hostile behaviour is opt-in.
#[derive(Debug, Clone)]
pub struct MockPlan {
    /// Batches before end-of-stream. **`None` means infinite** — the producer
    /// the backpressure test needs.
    pub batches: Option<u64>,
    /// Rows per batch, further bounded by the caller's [`FetchHint::max_rows`]
    /// so adaptive fetch sizing is observable.
    pub rows_per_batch: usize,
    /// Panic inside `Connection::execute`, for driver-panic isolation (§3.5).
    pub panic_on_execute: bool,
    /// Slow mode: sleep this long inside every `next_batch`.
    pub batch_delay: Option<Duration>,
    /// Fail with a recoverable `DbError::Query` after this many batches.
    pub fail_after: Option<u64>,
    /// Distinct values in the `status` column. Low values make the §5.1
    /// dictionary-encoding heuristic fire.
    pub status_cardinality: usize,
    pub payload: MockPayload,
    /// What this engine claims a cancel can do (§3.3).
    pub cancel_kind: CancelKind,
    /// What `Canceller::cancel` reports back.
    pub cancel_outcome: CancelOutcome,
    /// Starting fetch size the connection advertises (§3.2).
    pub default_fetch_rows: u32,
}

impl Default for MockPlan {
    fn default() -> Self {
        Self {
            batches: Some(1),
            rows_per_batch: 8,
            panic_on_execute: false,
            batch_delay: None,
            fail_after: None,
            status_cardinality: 3,
            payload: MockPayload::Rows,
            cancel_kind: CancelKind::ServerSide,
            cancel_outcome: CancelOutcome::ServerCancelled,
            default_fetch_rows: 500,
        }
    }
}

impl MockPlan {
    /// An endless producer — what the memory contract is actually tested with.
    pub fn infinite(rows_per_batch: usize) -> Self {
        Self {
            batches: None,
            rows_per_batch,
            ..Self::default()
        }
    }
}

/// The `status` column's value pool; index modulo
/// [`MockPlan::status_cardinality`] keeps cardinality exactly configurable.
const STATUSES: [&str; 8] = [
    "active", "pending", "closed", "archived", "draft", "failed", "queued", "done",
];

/// The schema every `MockPayload::Rows` cursor announces: one high-cardinality
/// string (`name`), one low-cardinality string (`status`), and an integer key.
pub fn mock_row_schema() -> RowSchema {
    let field = |name: &str, logical: LogicalType, flags: FieldFlags| FieldDef {
        name: Arc::from(name),
        logical,
        flags,
        native_type: None,
    };
    RowSchema {
        fields: vec![
            field("id", LogicalType::I64, FieldFlags::PRIMARY_KEY),
            field("name", LogicalType::Str, FieldFlags::NULLABLE),
            field("status", LogicalType::Str, FieldFlags::NULLABLE),
        ],
        identity: Some(datagrep_api::shape::Identity {
            field_indices: vec![0],
        }),
    }
}

/// A driver that constructs nothing expensive and hands out [`MockConnection`]s
/// wired to the same [`MockCounters`].
#[derive(Debug)]
pub struct MockDriver {
    plan: MockPlan,
    counters: Arc<MockCounters>,
}

impl MockDriver {
    pub fn new() -> Self {
        Self::with_plan(MockPlan::default())
    }

    pub fn with_plan(plan: MockPlan) -> Self {
        Self {
            plan,
            counters: Arc::new(MockCounters::default()),
        }
    }

    /// The counter object every connection/cursor this driver makes will bump.
    pub fn counters(&self) -> Arc<MockCounters> {
        self.counters.clone()
    }

    /// Build a connection directly, skipping the registry/profile path — the
    /// shortcut most core tests want.
    pub fn connection(&self) -> MockConnection {
        MockConnection::new(self.plan.clone(), self.counters.clone())
    }
}

impl Default for MockDriver {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Driver for MockDriver {
    fn meta(&self) -> DriverMeta {
        DriverMeta {
            id: Arc::from("mock"),
            display_name: Arc::from("Mock"),
            version: Arc::from(env!("CARGO_PKG_VERSION")),
        }
    }

    fn capabilities(&self) -> Capabilities {
        mock_capabilities(self.plan.default_fetch_rows)
    }

    fn config_schema(&self) -> ConfigSchema {
        ConfigSchema::default()
    }

    fn parse_url(&self, url: &str) -> Result<ConnectionConfig, ConfigError> {
        if url.starts_with("mock://") {
            Ok(ConnectionConfig {
                driver: Arc::from("mock"),
                values: Default::default(),
            })
        } else {
            Err(ConfigError::InvalidUrl {
                reason: "expected mock:// url".into(),
            })
        }
    }

    async fn connect(
        &self,
        _cfg: &ResolvedConfig,
        _ctx: ConnectCtx,
    ) -> Result<Box<dyn Connection>, DbError> {
        self.counters.connects.fetch_add(1, Ordering::SeqCst);
        Ok(Box::new(self.connection()))
    }
}

fn mock_capabilities(default_fetch_rows: u32) -> Capabilities {
    Capabilities {
        flags: Caps::TRANSACTIONS | Caps::DDL | Caps::SERVER_CANCEL | Caps::SCHEMA_DECLARED,
        max_statement_bytes: None,
        default_fetch_rows,
        param_style: ParamStyle::DollarNumbered,
        language: LanguageId::Sql(SqlDialect::Postgres),
        identifier_quote: '"',
        catalog_levels: 2,
    }
}

/// One mock connection. `execute` either panics (isolation fixture) or hands
/// back a [`MockCursor`] built from the same plan.
#[derive(Debug)]
pub struct MockConnection {
    plan: MockPlan,
    counters: Arc<MockCounters>,
    info: ServerInfo,
    canceller: Arc<MockCanceller>,
    catalog: Arc<MockCatalog>,
}

impl MockConnection {
    pub fn new(plan: MockPlan, counters: Arc<MockCounters>) -> Self {
        let canceller = Arc::new(MockCanceller {
            kind: plan.cancel_kind,
            outcome: plan.cancel_outcome,
            counters: counters.clone(),
        });
        Self {
            info: ServerInfo {
                product: Arc::from("mockdb"),
                version: Arc::from("0.0.0"),
                details: Vec::new(),
            },
            catalog: Arc::new(MockCatalog),
            plan,
            counters,
            canceller,
        }
    }

    /// A standalone connection with fresh counters.
    pub fn standalone(plan: MockPlan) -> (Self, Arc<MockCounters>) {
        let counters = Arc::new(MockCounters::default());
        (Self::new(plan, counters.clone()), counters)
    }
}

#[async_trait]
impl Connection for MockConnection {
    fn capabilities(&self) -> Capabilities {
        mock_capabilities(self.plan.default_fetch_rows)
    }

    fn server_info(&self) -> &ServerInfo {
        &self.info
    }

    async fn ping(&self) -> Result<(), DbError> {
        self.counters.pings.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }

    async fn execute(&self, _req: Request) -> Result<Box<dyn Cursor>, DbError> {
        self.counters.executes.fetch_add(1, Ordering::SeqCst);
        assert!(
            !self.plan.panic_on_execute,
            "mock driver panic (panic_on_execute)"
        );
        Ok(Box::new(MockCursor::new(
            self.plan.clone(),
            self.counters.clone(),
        )))
    }

    fn canceller(&self) -> Arc<dyn Canceller> {
        self.canceller.clone()
    }

    fn catalog(&self) -> Arc<dyn Catalog> {
        self.catalog.clone()
    }

    async fn begin(&self, _opts: TxOpts) -> Result<Box<dyn Transaction>, DbError> {
        Err(DbError::Unsupported {
            feature: "transactions".into(),
        })
    }

    async fn set_read_only(&self, _on: bool) -> Result<Enforcement, DbError> {
        Ok(Enforcement::Client)
    }

    async fn close(&self) -> Result<(), DbError> {
        self.counters.conn_closes.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

/// Cancel side of the mock, honest about its configured [`CancelKind`].
#[derive(Debug)]
struct MockCanceller {
    kind: CancelKind,
    outcome: CancelOutcome,
    counters: Arc<MockCounters>,
}

impl Canceller for MockCanceller {
    fn kind(&self) -> CancelKind {
        self.kind
    }

    fn cancel(&self) -> BoxFuture<'_, Result<CancelOutcome, DbError>> {
        Box::pin(async move {
            self.counters.cancels.fetch_add(1, Ordering::SeqCst);
            Ok(self.outcome)
        })
    }
}

/// The hostile producer. Emits up to [`MockPlan::batches`] chunks (or forever),
/// honouring the caller's row hint so adaptive sizing is observable.
#[derive(Debug)]
pub struct MockCursor {
    plan: MockPlan,
    counters: Arc<MockCounters>,
    shape: Shape,
    seq: u64,
    rows_emitted: u64,
    stats: CursorStats,
    closed: bool,
}

impl MockCursor {
    pub fn new(plan: MockPlan, counters: Arc<MockCounters>) -> Self {
        let shape = match plan.payload {
            MockPayload::Rows => Shape::Table(Arc::new(mock_row_schema())),
            MockPayload::Docs => Shape::Documents { root_hint: None },
            MockPayload::Ack { affected } => Shape::Ack {
                affected,
                message: None,
            },
        };
        Self {
            plan,
            counters,
            shape,
            seq: 0,
            rows_emitted: 0,
            stats: CursorStats::default(),
            closed: false,
        }
    }

    /// A cursor with its own counters, for tests that drive the feeder alone.
    pub fn standalone(plan: MockPlan) -> (Self, Arc<MockCounters>) {
        let counters = Arc::new(MockCounters::default());
        (Self::new(plan, counters.clone()), counters)
    }

    fn row(&self, i: u64) -> Vec<Value> {
        let card = self.plan.status_cardinality.clamp(1, STATUSES.len());
        vec![
            Value::I64(i as i64),
            Value::Str(Arc::from(format!("name-{i}"))),
            Value::Str(Arc::from(STATUSES[(i as usize) % card])),
        ]
    }

    fn doc(&self, i: u64) -> Value {
        let card = self.plan.status_cardinality.clamp(1, STATUSES.len());
        Value::Document(Arc::new(datagrep_api::value::Document::from_fields(vec![
            (Arc::from("id"), Value::I64(i as i64)),
            (
                Arc::from("status"),
                Value::Str(Arc::from(STATUSES[(i as usize) % card])),
            ),
        ])))
    }
}

#[async_trait]
impl Cursor for MockCursor {
    fn shape(&self) -> &Shape {
        &self.shape
    }

    async fn next_batch(&mut self, hint: FetchHint) -> Result<Option<Batch>, DbError> {
        self.counters.next_batch.fetch_add(1, Ordering::SeqCst);

        if let Some(delay) = self.plan.batch_delay {
            tokio::time::sleep(delay).await;
        }
        if let Some(fail_after) = self.plan.fail_after {
            if self.seq >= fail_after {
                return Err(DbError::Query {
                    code: Some("MOCK01".into()),
                    message: "mock failure".into(),
                    position: None,
                });
            }
        }
        if let Some(limit) = self.plan.batches {
            if self.seq >= limit {
                return Ok(None);
            }
        }

        let n = match self.plan.payload {
            // An acknowledgement chunk carries no rows.
            MockPayload::Ack { .. } => 0,
            _ => self.plan.rows_per_batch.min(hint.max_rows.max(1) as usize),
        };
        let start = self.rows_emitted;
        let payload = match self.plan.payload {
            MockPayload::Rows => {
                Payload::Rows((0..n as u64).map(|k| self.row(start + k)).collect())
            }
            MockPayload::Docs => {
                Payload::Docs((0..n as u64).map(|k| self.doc(start + k)).collect())
            }
            MockPayload::Ack { .. } => Payload::Empty,
        };

        let batch = Batch {
            seq: self.seq,
            payload,
            schema_delta: Vec::new(),
            notices: Vec::new(),
        };
        self.seq += 1;
        self.rows_emitted += n as u64;
        self.stats.rows = self.rows_emitted;
        self.stats.batches = self.seq;
        // ~24 bytes/row is close enough for byte-ceiling arithmetic.
        self.stats.bytes = self.rows_emitted * 24;
        Ok(Some(batch))
    }

    fn resume_token(&self) -> Option<ResumeToken> {
        Some(ResumeToken(Bytes::from(
            self.rows_emitted.to_be_bytes().to_vec(),
        )))
    }

    fn stats(&self) -> CursorStats {
        self.stats
    }

    async fn close(&mut self) -> Result<(), DbError> {
        if !self.closed {
            self.closed = true;
            self.counters.cursor_closes.fetch_add(1, Ordering::SeqCst);
        }
        Ok(())
    }
}

/// Minimal catalog: two tables under the root, cheap to enumerate.
#[derive(Debug)]
struct MockCatalog;

#[async_trait]
impl Catalog for MockCatalog {
    fn levels(&self) -> Vec<LevelDef> {
        vec![LevelDef {
            name: Arc::from("table"),
            kind: ObjectKind::Table,
            enumeration: Enumeration::Cheap,
        }]
    }

    async fn children(
        &self,
        parent: &ObjectPath,
        _opts: ListOpts,
    ) -> Result<Page<ObjectNode>, DbError> {
        let items = ["users", "events"]
            .into_iter()
            .map(|name| ObjectNode {
                path: parent.child(name),
                kind: ObjectKind::Table,
                has_children: false,
                comment: None,
            })
            .collect();
        Ok(Page { items, next: None })
    }

    async fn describe(&self, path: &ObjectPath) -> Result<ObjectDetail, DbError> {
        Ok(ObjectDetail {
            node: ObjectNode {
                path: path.clone(),
                kind: ObjectKind::Table,
                has_children: false,
                comment: None,
            },
            schema: Some(mock_row_schema()),
            extra: Vec::new(),
        })
    }

    async fn infer_shape(
        &self,
        _path: &ObjectPath,
        _sample_size: u32,
    ) -> Result<InferredSchema, DbError> {
        Ok(InferredSchema::default())
    }

    async fn complete(&self, _ctx: CompletionCtx) -> Result<Vec<Completion>, DbError> {
        Ok(Vec::new())
    }
}
