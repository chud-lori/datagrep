#![deny(missing_debug_implementations)]
#![warn(rust_2018_idioms)]

pub mod caps;
pub mod catalog;
pub mod config;
pub mod driver;
pub mod error;
pub mod request;
pub mod safety;
pub mod shape;
pub mod value;

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
pub use safety::{Attestation, Requirement, SafetyLevel};
pub use shape::{
    FieldDef, FieldFlags, GraphChunk, GraphSchema, Identity, LogicalType, ObjectPath, RowSchema,
    SchemaDelta, Shape, ValueKind,
};
pub use value::{Document, FieldPath, Geometry, PathParseError, PathSeg, TzSpec, Value};
