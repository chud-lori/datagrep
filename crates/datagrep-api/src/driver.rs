//! The driver contract. A driver is a pure stream-of-batches factory: it never
//! sees the result store, Arrow, or the UI, and it MUST NOT buffer results —
//! backpressure has to reach the database socket.

use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use serde::{Deserialize, Serialize};

use crate::caps::Capabilities;
use crate::catalog::Catalog;
use crate::config::{ConfigError, ConfigSchema, ConnectionConfig, ResolvedConfig};
use crate::error::DbError;
use crate::request::Request;
use crate::shape::{GraphChunk, SchemaDelta, Shape};
use crate::value::Value;

/// Boxed future for object-safe non-`async_trait` methods ([`Canceller`]).
/// Defined here because `futures-core` does not ship one and we refuse a
/// `futures-util` dependency for a type alias.
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Boxed stream, for adapters above this crate that want a `Stream` view of a
/// cursor without inheriting a runtime.
pub type BoxStream<'a, T> = Pin<Box<dyn futures_core::Stream<Item = T> + Send + 'a>>;

/// Minimal runtime-free cancel token: cooperative, cloneable, checkable from
/// any thread. Drivers poll it at await points during connect.
#[derive(Debug, Clone, Default)]
pub struct CancelFlag(Arc<AtomicBool>);

impl CancelFlag {
    pub fn new() -> Self {
        Self::default()
    }

    /// Request cancellation. Idempotent; observed at the next check.
    pub fn cancel(&self) {
        self.0.store(true, Ordering::Release);
    }

    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::Acquire)
    }
}

/// A compiled-in (or WASM-hosted) engine adapter. Stateless: all per-server
/// state lives in the [`Connection`]s it creates.
#[async_trait]
pub trait Driver: Send + Sync {
    fn meta(&self) -> DriverMeta;

    /// Baseline capabilities before any handshake — the connection's
    /// post-handshake [`Connection::capabilities`] is the authoritative one.
    fn capabilities(&self) -> Capabilities;

    /// The connection form, as data — no per-engine UI code.
    fn config_schema(&self) -> ConfigSchema;

    /// Split a pasted URL into config fields; the caller routes any password
    /// into the keychain and zeroizes the source string — a pasted URL must not
    /// leave a password sitting in a config file or in process memory.
    fn parse_url(&self, url: &str) -> Result<ConnectionConfig, ConfigError>;

    async fn connect(
        &self,
        cfg: &ResolvedConfig,
        ctx: ConnectCtx,
    ) -> Result<Box<dyn Connection>, DbError>;
}

/// One live connection. Owned by a single task; a panic inside is caught at
/// the task boundary and becomes `DbError::DriverPanic` — one misbehaving
/// driver must not take the process down with it.
#[async_trait]
pub trait Connection: Send + Sync {
    /// Post-handshake, version-aware capabilities.
    fn capabilities(&self) -> Capabilities;

    fn server_info(&self) -> &ServerInfo;

    /// Cheap liveness check, used lazily on next use — never on a timer.
    async fn ping(&self) -> Result<(), DbError>;

    /// THE method. Returns as soon as the server accepts the request. It MUST
    /// NOT wait for or buffer the full result.
    async fn execute(&self, req: Request) -> Result<Box<dyn Cursor>, DbError>;

    /// Cloneable, `'static`, usable from another task while `execute` is in
    /// flight — the only sane way to model cancellation.
    fn canceller(&self) -> Arc<dyn Canceller>;

    fn catalog(&self) -> Arc<dyn Catalog>;

    async fn begin(&self, opts: TxOpts) -> Result<Box<dyn Transaction>, DbError>;

    /// Returns HOW STRONGLY read-only was enforced, so the UI can say so
    /// honestly — a client-only badge must admit it.
    async fn set_read_only(&self, on: bool) -> Result<Enforcement, DbError>;

    /// Graceful shutdown; idempotent. After this every call returns `Closed`.
    async fn close(&self) -> Result<(), DbError>;
}

/// A pull-based chunk stream. Pull-only is the backpressure story: nobody
/// calls `next_batch`, nothing is read off the socket.
#[async_trait]
pub trait Cursor: Send {
    fn shape(&self) -> &Shape;

    /// Pull exactly one chunk; `None` = end of stream. The driver picks the
    /// real size, bounded by the hint (PG portal `max_rows`, Mongo batchSize).
    async fn next_batch(&mut self, hint: FetchHint) -> Result<Option<Batch>, DbError>;

    /// Opaque serializable continuation (ES `search_after`+PIT, Redis SCAN
    /// cursor, SQL keyset). Lets the core close a server cursor on idle and
    /// resume later — what makes dropping to zero open connections on idle safe.
    fn resume_token(&self) -> Option<ResumeToken>;

    fn stats(&self) -> CursorStats;

    /// Release server resources (portal, cursor) early; idempotent.
    async fn close(&mut self) -> Result<(), DbError>;
}

/// An open transaction, pinned to its connection's socket — a pool that moves
/// a `BEGIN` between sockets is a correctness bug.
#[async_trait]
pub trait Transaction: Send {
    async fn execute(&self, req: Request) -> Result<Box<dyn Cursor>, DbError>;

    async fn commit(self: Box<Self>) -> Result<(), DbError>;

    async fn rollback(self: Box<Self>) -> Result<(), DbError>;
}

/// Out-of-band cancellation, honest about how strong it is per engine.
/// Plain trait (not `async_trait`) so `cancel` can be called through
/// `Arc<dyn Canceller>` from any task.
pub trait Canceller: Send + Sync {
    /// What cancelling actually does on this engine — surfaced verbatim in
    /// the UI so "stopped" never silently means "server still burning".
    fn kind(&self) -> CancelKind;

    fn cancel(&self) -> BoxFuture<'_, Result<CancelOutcome, DbError>>;
}

/// Identity of a driver, for the registry and the connection form.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DriverMeta {
    /// Stable registry id (`postgres`, `sqlite`, …). Never branched on above
    /// datagrep-api — that is what capability flags are for.
    pub id: Arc<str>,
    pub display_name: Arc<str>,
    /// Driver (not server) version.
    pub version: Arc<str>,
}

/// What the server told us at handshake; shown in the UI and used for
/// version-aware capabilities.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServerInfo {
    pub product: Arc<str>,
    pub version: Arc<str>,
    /// Extra engine-reported facts, as display pairs (never branched on).
    pub details: Vec<(Arc<str>, Arc<str>)>,
}

/// Ambient context for `connect`: cancellation and bounds, no runtime types.
#[derive(Debug, Clone, Default)]
pub struct ConnectCtx {
    pub cancel: CancelFlag,
    pub connect_timeout: Option<Duration>,
    /// Reported to the server (`application_name`) so DBAs can see who we are.
    pub application_name: Option<Arc<str>>,
}

/// Transaction options; drivers reject combinations they cannot honor rather
/// than silently downgrading.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct TxOpts {
    pub isolation: Option<IsolationLevel>,
    pub read_only: bool,
}

/// Standard isolation levels; a driver maps to its engine's nearest honest
/// equivalent or errors — never a silent downgrade.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum IsolationLevel {
    ReadUncommitted,
    ReadCommitted,
    RepeatableRead,
    Serializable,
}

/// How strongly a read-only request is actually enforced — the UI states
/// which, because a client-side-only badge is a different promise.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Enforcement {
    /// The server itself refuses writes on this session.
    Server,
    /// Only our client-side classifier stands in the way.
    Client,
    /// Nothing enforces it; the UI must say so.
    None,
}

/// What a cancel can actually do on this engine.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum CancelKind {
    /// A real server-side kill exists (PG CancelRequest, `KILL QUERY`, …).
    ServerSide,
    /// We can only stop consuming; the server may keep executing.
    ClientAbandon,
    /// Only a pre-set server-side deadline bounds the work.
    DeadlineOnly,
}

/// What a cancel actually achieved — shown to the user, never embellished.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum CancelOutcome {
    /// The server acknowledged killing the operation.
    ServerCancelled,
    /// The cancel was sent but the protocol gives no ack (PG's race by design).
    Requested,
    /// We stopped consuming; the server may still be executing.
    ClientAbandoned,
}

/// Per-pull bounds. The driver picks the real chunk size within these; the
/// core adapts them per batch toward a wall-clock window.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct FetchHint {
    pub max_rows: u32,
    pub max_bytes: u32,
    /// Wall-clock target for one pull; feeds adaptive fetch sizing.
    pub target_ms: u32,
}

impl Default for FetchHint {
    /// Starting point before adaptive sizing takes over: conservative rows, a
    /// 4 MB ceiling, and 80 ms — the middle of the 40–120 ms pull window that
    /// keeps the UI responsive without paying a round trip per handful of rows.
    fn default() -> Self {
        Self {
            max_rows: 500,
            max_bytes: 4 * 1024 * 1024,
            target_ms: 80,
        }
    }
}

/// One pulled chunk: payload plus anything the driver learned mid-stream.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Batch {
    /// Monotonic sequence number within the cursor, starting at 0.
    pub seq: u64,
    pub payload: Payload,
    /// Schema evolution discovered in this chunk (append-only for the grid).
    pub schema_delta: Vec<SchemaDelta>,
    /// Server notices/warnings that arrived with this chunk.
    pub notices: Vec<Notice>,
}

/// Chunk contents, matching the cursor's [`Shape`]. No Arrow here — columnar
/// conversion happens above the seam, in datagrep-core.
#[derive(Debug, Clone, PartialEq, Default)]
pub enum Payload {
    Rows(Vec<Row>),
    Docs(Vec<Value>),
    Pairs(Vec<(Value, Value)>),
    Graph(GraphChunk),
    /// No data in this chunk (e.g. an Ack-shaped result).
    #[default]
    Empty,
}

/// One row of a `Table`-shaped result, in schema field order.
pub type Row = Vec<Value>;

/// A non-fatal message from the server (PG NOTICE, MySQL warning) — surfaced,
/// never swallowed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Notice {
    pub severity: NoticeSeverity,
    pub code: Option<Arc<str>>,
    pub message: Arc<str>,
}

/// Severity of a [`Notice`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum NoticeSeverity {
    Info,
    Warning,
}

/// Running totals for the status line; cheap enough to read on every batch.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct CursorStats {
    pub rows: u64,
    pub bytes: u64,
    pub batches: u64,
    /// Server-reported execution time, when the protocol carries one.
    pub server_elapsed_micros: Option<u64>,
}

/// Opaque serializable continuation for resuming a scan after the server-side
/// cursor is gone. Contents are driver-private; the core only stores it.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ResumeToken(pub Bytes);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cancel_flag_is_shared_and_sticky() {
        let a = CancelFlag::new();
        let b = a.clone();
        assert!(!a.is_cancelled());
        b.cancel();
        assert!(a.is_cancelled(), "clones share one flag");
        b.cancel(); // idempotent
        assert!(b.is_cancelled());
    }

    #[test]
    fn fetch_hint_default_matches_design() {
        let h = FetchHint::default();
        assert_eq!(h.max_rows, 500);
        assert_eq!(h.max_bytes, 4 * 1024 * 1024);
        assert!(
            h.target_ms >= 40 && h.target_ms <= 120,
            "inside the 40-120 ms pull window"
        );
    }

    #[test]
    fn resume_token_round_trips_through_serde() {
        let tok = ResumeToken(Bytes::from_static(b"\x00\x01scan-cursor-42"));
        let json = serde_json::to_string(&tok).unwrap();
        let back: ResumeToken = serde_json::from_str(&json).unwrap();
        assert_eq!(back, tok);
    }

    // Compile-time proof the traits are object-safe — the whole point of the
    // seam is `Box<dyn Connection>` / `Box<dyn Cursor>` across crates.
    #[allow(dead_code)]
    fn object_safety(
        _: &dyn Driver,
        _: &dyn Connection,
        _: &mut dyn Cursor,
        _: &dyn Transaction,
        _: &dyn Canceller,
    ) {
    }
}
