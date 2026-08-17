//! The one error type that crosses the seam. Coarse by design: drivers keep
//! engine-specific detail in `Query { code }` text rather than leaking their
//! own error enums upward.

use crate::config::ConfigError;

/// Error from any driver operation. `is_recoverable` tells the core whether
/// the connection survived — a non-recoverable error poisons and evicts it, so
/// one bad connection cannot take the rest of the app with it.
#[derive(Debug, thiserror::Error)]
pub enum DbError {
    /// Could not establish the connection (DNS, refused, handshake).
    #[error("connect failed: {0}")]
    Connect(String),

    /// Transport failure on an established connection.
    #[error("i/o error: {0}")]
    Io(#[from] std::io::Error),

    /// A deadline elapsed (ours or a server-side one we set).
    #[error("operation timed out")]
    Timeout,

    /// The user cancelled; not a failure, and the UI must not dress it as one.
    #[error("cancelled")]
    Cancelled,

    /// The engine cannot do this — should be prevented upstream by a
    /// capability flag; reaching here means a flag is missing.
    #[error("unsupported by this engine: {feature}")]
    Unsupported { feature: String },

    /// Authentication or authorization failed.
    #[error("authentication failed: {0}")]
    Auth(String),

    /// TLS setup or verification failed.
    #[error("tls error: {0}")]
    Tls(String),

    /// The server rejected the statement. `position` is a byte offset into the
    /// statement text when the engine reports one, for editor squiggles.
    #[error("query failed{}: {message}", .code.as_deref().map(|c| format!(" [{c}]")).unwrap_or_default())]
    Query {
        /// Engine-native error code (e.g. SQLSTATE), preserved verbatim.
        code: Option<String>,
        message: String,
        position: Option<u32>,
    },

    /// A concurrent modification beat this write: an optimistic-concurrency
    /// precondition (`Mutation::*::expect`) no longer held, a serialization
    /// failure, a write-write conflict. Recoverable — the caller re-reads and
    /// decides (rebase/discard); the connection itself is fine.
    #[error("conflict{}: {message}", .code.as_deref().map(|c| format!(" [{c}]")).unwrap_or_default())]
    Conflict {
        /// Engine-native code (`version_conflict_engine_exception`, SQLSTATE
        /// `40001`, …), preserved verbatim.
        code: Option<String>,
        message: String,
    },

    /// A driver panicked and was caught at the task boundary; the connection
    /// is poisoned and evicted, the app lives.
    #[error("driver panicked: {0}")]
    DriverPanic(String),

    /// The wire protocol was violated — a bug in us or the server; never retried.
    #[error("protocol error: {0}")]
    Protocol(String),

    /// The connection configuration is invalid.
    #[error("configuration error: {0}")]
    Config(#[from] ConfigError),

    /// A budget or server limit was hit (memory policy, row cap, quota).
    #[error("resource exhausted: {0}")]
    ResourceExhausted(String),

    /// The connection or cursor was already closed.
    #[error("closed")]
    Closed,
}

impl DbError {
    /// Non-fatal marker: `true` means the connection is still usable and the
    /// error stays local to the request; `false` poisons the connection.
    pub fn is_recoverable(&self) -> bool {
        matches!(
            self,
            DbError::Timeout
                | DbError::Cancelled
                | DbError::Unsupported { .. }
                | DbError::Query { .. }
                | DbError::Conflict { .. }
                | DbError::Config(_)
                | DbError::ResourceExhausted(_)
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recoverability_split() {
        assert!(DbError::Cancelled.is_recoverable());
        assert!(DbError::Query {
            code: Some("42P01".into()),
            message: "relation does not exist".into(),
            position: Some(15),
        }
        .is_recoverable());
        assert!(DbError::Conflict {
            code: Some("version_conflict_engine_exception".into()),
            message: "required seqNo [3], current [7]".into(),
        }
        .is_recoverable());
        assert!(!DbError::Protocol("bad frame".into()).is_recoverable());
        assert!(!DbError::DriverPanic("index out of bounds".into()).is_recoverable());
        assert!(!DbError::Closed.is_recoverable());
    }

    #[test]
    fn query_error_display_includes_code() {
        let e = DbError::Query {
            code: Some("42P01".into()),
            message: "no such table".into(),
            position: None,
        };
        assert_eq!(e.to_string(), "query failed [42P01]: no such table");
    }

    #[test]
    fn conflict_display_includes_code() {
        let e = DbError::Conflict {
            code: Some("version_conflict_engine_exception".into()),
            message: "somebody else wrote first".into(),
        };
        assert_eq!(
            e.to_string(),
            "conflict [version_conflict_engine_exception]: somebody else wrote first"
        );
        let bare = DbError::Conflict {
            code: None,
            message: "write-write conflict".into(),
        };
        assert_eq!(bare.to_string(), "conflict: write-write conflict");
    }
}
