//! # dbx-api — THE STABLE SEAM
//!
//! The driver contract for `dbx` (see `dbx-design.md`, §3 and §3.1). Everything
//! above this crate (core, frontends) and everything below it (drivers, the
//! WASM host) meets here, and only here.
//!
//! Ground rules this crate encodes:
//! - **Streaming-first.** A driver is a factory of pull-based batch cursors;
//!   [`Connection::execute`] returns when the server accepts the request and
//!   never buffers a result. Backpressure must reach the database socket.
//! - **Capability flags, not driver checks.** Any `if driver_id == …` above
//!   this crate is a missing [`Caps`] flag.
//! - **Never lose bytes.** Unmappable values ride in [`Value::Unsupported`]
//!   with their raw encoding; JSON stays raw text; decimals stay strings.
//! - **`Absent` is distinct from `Null`** — load-bearing for sparse documents.
//! - **~5 dependencies, no runtime.** No tokio, no Arrow, no reqwest: plugins
//!   and the TUI must not inherit a runtime through this crate.

#![deny(missing_debug_implementations)]
#![warn(rust_2018_idioms)]

pub mod caps;
pub mod catalog;
pub mod config;
pub mod driver;
pub mod error;
pub mod request;
pub mod shape;
pub mod value;

/// Re-exported so driver crates can build `Value::Bytes` / `Value::Unsupported`
/// without taking their own `bytes` dependency — losing raw bytes because a
/// crate could not name the type would defeat design §3.1's "never lose bytes".
pub use bytes::Bytes;

pub use caps::{Capabilities, Caps, LanguageId, ParamStyle, SqlDialect};
pub use catalog::{
    Catalog, Completion, CompletionCtx, Enumeration, FieldTrie, InferredSchema, LevelDef, ListOpts,
    ObjectDetail, ObjectKind, ObjectNode, Page,
};
pub use config::{
    ConfigError, ConfigField, ConfigSchema, ConfigValue, ConnectionConfig, FieldKind,
    ResolvedConfig, SecretString,
};
pub use driver::{
    Batch, BoxFuture, BoxStream, CancelFlag, CancelKind, CancelOutcome, Canceller, ConnectCtx,
    Connection, Cursor, CursorStats, Driver, DriverMeta, Enforcement, FetchHint, IsolationLevel,
    Notice, NoticeSeverity, Payload, ResumeToken, Row, ServerInfo, Transaction, TxOpts,
};
pub use error::DbError;
pub use request::{DdlOp, ExecOpts, Mutation, MutationBatch, Op, Predicate, Request, SortKey};
pub use shape::{
    FieldDef, FieldFlags, GraphChunk, GraphSchema, Identity, LogicalType, ObjectPath, RowSchema,
    SchemaDelta, Shape, ValueKind,
};
pub use value::{Document, FieldPath, Geometry, PathParseError, PathSeg, TzSpec, Value};
