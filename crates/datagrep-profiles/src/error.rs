//! The one error type this crate returns. Coarse by design, mirroring
//! `datagrep_api::DbError`'s philosophy: callers match on a handful of
//! variants, not per-driver detail.

use std::path::PathBuf;

/// Everything that can go wrong opening, migrating, or using a profile store.
#[derive(Debug, thiserror::Error)]
pub enum ProfilesError {
    /// A `rusqlite`/SQLite failure (constraint violation, syntax, I/O via
    /// SQLite's own VFS, etc.).
    #[error("sqlite error: {0}")]
    Sqlite(#[from] rusqlite::Error),

    /// A plain filesystem failure (creating the parent directory, copying the
    /// `.bak` snapshot, etc.) — kept distinct from `Sqlite` so callers can
    /// tell "disk problem" from "database problem".
    #[error("i/o error: {0}")]
    Io(#[from] std::io::Error),

    /// `config_json` failed to (de)serialize.
    #[error("serialization error: {0}")]
    Json(#[from] serde_json::Error),

    /// TOML export failed to serialize.
    #[error("toml export error: {0}")]
    TomlSer(#[from] toml::ser::Error),

    /// TOML import failed to parse.
    #[error("toml import error: {0}")]
    TomlDe(#[from] toml::de::Error),

    /// `Profile.config` may never contain a secret-shaped key. Point the
    /// caller at `secret_ref` instead.
    #[error(
        "config key `{key}` looks like a secret (matches `{pattern}`) — secrets never live in \
         Profile.config; store the credential in the OS keychain and reference it via \
         `secret_ref` instead"
    )]
    SecretShapedKey { key: String, pattern: &'static str },

    /// The on-disk schema is newer than this build knows how to read.
    /// Migrations are forward-only; we refuse to guess.
    #[error(
        "database schema version {found} is newer than this build supports (max {supported}) — \
         upgrade datagrep-profiles"
    )]
    FutureSchema { found: i64, supported: i64 },

    /// No row with that id.
    #[error("{what} not found: {id}")]
    NotFound { what: &'static str, id: String },


    /// The store's worker thread could not be started.
    #[error("failed to start datagrep-profiles worker thread: {0}")]
    WorkerStart(String),

    /// The worker thread is gone (panicked or already shut down) so a
    /// request could not be completed.
    #[error("datagrep-profiles worker thread is no longer running")]
    WorkerGone,

    /// A parent directory for the database file could not be determined.
    #[error("could not determine a parent directory for database path {0:?}")]
    NoParentDir(PathBuf),
}
