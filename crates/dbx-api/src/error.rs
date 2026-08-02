//! The one error type that crosses the seam. Coarse by design: drivers keep
//! engine-specific detail in `Query { code }` text rather than leaking their
//! own error enums upward.

use crate::config::ConfigError;

/// Error from any driver operation. `is_recoverable` tells the core whether
/// the connection survived — a non-recoverable error poisons and evicts it
/// (design §3.5 isolation).
#[derive(Debug, thiserror::Error)]
pub enum DbError {
    /// Could not establish the connection (DNS, refused, handshake).
    #[error("connect failed: {0}")]
    Connect(String),

    /// Transport failure on an established connection.
    #[error("i/o error: {0}")]
    Io(#[from] std::io::Error),

    /// A deadline elapsed (ours or a server-side one we set — design §3.3).
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

    /// A driver panicked and was caught at the task boundary; the connection
    /// is poisoned and evicted, the app lives (design §3.5, §9.5).
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
}
