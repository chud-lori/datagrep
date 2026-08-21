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

pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

pub type BoxStream<'a, T> = Pin<Box<dyn futures_core::Stream<Item = T> + Send + 'a>>;

#[derive(Debug, Clone, Default)]
pub struct CancelFlag(Arc<AtomicBool>);

impl CancelFlag {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn cancel(&self) {
        self.0.store(true, Ordering::Release);
    }

    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::Acquire)
    }
}

#[async_trait]
pub trait Driver: Send + Sync {
    fn meta(&self) -> DriverMeta;

    fn capabilities(&self) -> Capabilities;

    fn config_schema(&self) -> ConfigSchema;

    fn parse_url(&self, url: &str) -> Result<ConnectionConfig, ConfigError>;

    async fn connect(
        &self,
        cfg: &ResolvedConfig,
        ctx: ConnectCtx,
    ) -> Result<Box<dyn Connection>, DbError>;
}

#[async_trait]
pub trait Connection: Send + Sync {
    fn capabilities(&self) -> Capabilities;

    fn server_info(&self) -> &ServerInfo;

    async fn ping(&self) -> Result<(), DbError>;

    async fn execute(&self, req: Request) -> Result<Box<dyn Cursor>, DbError>;

    fn canceller(&self) -> Arc<dyn Canceller>;

    fn catalog(&self) -> Arc<dyn Catalog>;

    async fn begin(&self, opts: TxOpts) -> Result<Box<dyn Transaction>, DbError>;

    async fn set_read_only(&self, on: bool) -> Result<Enforcement, DbError>;

    async fn close(&self) -> Result<(), DbError>;
}

#[async_trait]
pub trait Cursor: Send {
    fn shape(&self) -> &Shape;

    async fn next_batch(&mut self, hint: FetchHint) -> Result<Option<Batch>, DbError>;

    fn resume_token(&self) -> Option<ResumeToken>;

    fn stats(&self) -> CursorStats;

    async fn close(&mut self) -> Result<(), DbError>;
}

#[async_trait]
pub trait Transaction: Send {
    async fn execute(&self, req: Request) -> Result<Box<dyn Cursor>, DbError>;

    async fn commit(self: Box<Self>) -> Result<(), DbError>;

    async fn rollback(self: Box<Self>) -> Result<(), DbError>;
}

pub trait Canceller: Send + Sync {
    fn kind(&self) -> CancelKind;

    fn cancel(&self) -> BoxFuture<'_, Result<CancelOutcome, DbError>>;
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DriverMeta {
    pub id: Arc<str>,
    pub display_name: Arc<str>,
    pub version: Arc<str>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServerInfo {
    pub product: Arc<str>,
    pub version: Arc<str>,
    pub details: Vec<(Arc<str>, Arc<str>)>,
}

#[derive(Debug, Clone, Default)]
pub struct ConnectCtx {
    pub cancel: CancelFlag,
    pub connect_timeout: Option<Duration>,
    pub application_name: Option<Arc<str>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct TxOpts {
    pub isolation: Option<IsolationLevel>,
    pub read_only: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum IsolationLevel {
    ReadUncommitted,
    ReadCommitted,
    RepeatableRead,
    Serializable,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Enforcement {
    Server,
    Client,
    None,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum CancelKind {
    ServerSide,
    ClientAbandon,
    DeadlineOnly,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum CancelOutcome {
    ServerCancelled,
    Requested,
    ClientAbandoned,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct FetchHint {
    pub max_rows: u32,
    pub max_bytes: u32,
    pub target_ms: u32,
}

impl Default for FetchHint {
    fn default() -> Self {
        Self {
            max_rows: 500,
            max_bytes: 4 * 1024 * 1024,
            target_ms: 80,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct Batch {
    pub seq: u64,
    pub payload: Payload,
    pub schema_delta: Vec<SchemaDelta>,
    pub notices: Vec<Notice>,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub enum Payload {
    Rows(Vec<Row>),
    Docs(Vec<Value>),
    Pairs(Vec<(Value, Value)>),
    Graph(GraphChunk),
    #[default]
    Empty,
}

pub type Row = Vec<Value>;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Notice {
    pub severity: NoticeSeverity,
    pub code: Option<Arc<str>>,
    pub message: Arc<str>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum NoticeSeverity {
    Info,
    Warning,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct CursorStats {
    pub rows: u64,
    pub bytes: u64,
    pub batches: u64,
    pub server_elapsed_micros: Option<u64>,
}

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
