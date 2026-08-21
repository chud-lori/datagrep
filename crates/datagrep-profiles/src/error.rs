use std::path::PathBuf;

#[derive(Debug, thiserror::Error)]
pub enum ProfilesError {
    #[error("sqlite error: {0}")]
    Sqlite(#[from] rusqlite::Error),

    #[error("i/o error: {0}")]
    Io(#[from] std::io::Error),

    #[error("serialization error: {0}")]
    Json(#[from] serde_json::Error),

    #[error("toml export error: {0}")]
    TomlSer(#[from] toml::ser::Error),

    #[error("toml import error: {0}")]
    TomlDe(#[from] toml::de::Error),

    #[error(
        "config key `{key}` looks like a secret (matches `{pattern}`) — secrets never live in \
         Profile.config; store the credential in the OS keychain and reference it via \
         `secret_ref` instead"
    )]
    SecretShapedKey { key: String, pattern: &'static str },

    #[error(
        "database schema version {found} is newer than this build supports (max {supported}) — \
         upgrade datagrep-profiles"
    )]
    FutureSchema { found: i64, supported: i64 },

    #[error("{what} not found: {id}")]
    NotFound { what: &'static str, id: String },

    #[error("failed to start datagrep-profiles worker thread: {0}")]
    WorkerStart(String),

    #[error("datagrep-profiles worker thread is no longer running")]
    WorkerGone,

    #[error("could not determine a parent directory for database path {0:?}")]
    NoParentDir(PathBuf),
}
