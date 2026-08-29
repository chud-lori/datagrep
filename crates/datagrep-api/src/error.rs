use crate::config::ConfigError;
use crate::safety::Requirement;

#[derive(Debug, thiserror::Error)]
pub enum DbError {
    #[error("connect failed: {0}")]
    Connect(String),

    #[error("i/o error: {0}")]
    Io(#[from] std::io::Error),

    #[error("operation timed out")]
    Timeout,

    #[error("cancelled")]
    Cancelled,

    #[error("unsupported by this engine: {feature}")]
    Unsupported { feature: String },

    #[error("authentication failed: {0}")]
    Auth(String),

    #[error("tls error: {0}")]
    Tls(String),

    #[error("query failed{}: {message}", .code.as_deref().map(|c| format!(" [{c}]")).unwrap_or_default())]
    Query {
        code: Option<String>,
        message: String,
        position: Option<u32>,
    },

    #[error("conflict{}: {message}", .code.as_deref().map(|c| format!(" [{c}]")).unwrap_or_default())]
    Conflict {
        code: Option<String>,
        message: String,
    },

    #[error("driver panicked: {0}")]
    DriverPanic(String),

    #[error("protocol error: {0}")]
    Protocol(String),

    #[error("configuration error: {0}")]
    Config(#[from] ConfigError),

    #[error("resource exhausted: {0}")]
    ResourceExhausted(String),

    #[error("`{profile}` is in safe mode: this statement requires {requirement} first (challenge {challenge})")]
    Safety {
        profile: String,
        requirement: Requirement,
        challenge: String,
    },

    #[error("closed")]
    Closed,
}

impl DbError {
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
                | DbError::Safety { .. }
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
        assert!(
            DbError::Safety {
                profile: "prod".into(),
                requirement: Requirement::Authenticate,
                challenge: "c1".into(),
            }
            .is_recoverable(),
            "a safety refusal must not poison the connection it never used"
        );
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
