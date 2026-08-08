//! Error mapping onto `datagrep_api::DbError`, plus the deferred-TLS marker.

use datagrep_api::DbError;

/// TLS posture for a Postgres connection. Only `Disable`/`Prefer`-without-cert
/// are implemented in v1; `require`/`verify-ca`/`verify-full` are accepted by
/// [`crate::driver::PostgresDriver::config_schema`] as selectable values (so
/// the connection form is honest about what the engine supports) but
/// `connect` fails fast with a clear "not yet implemented" error rather than
/// silently downgrading to plaintext — a silent downgrade would be a security
/// regression. datagrep never loses bytes and never lies about them; the TLS
/// analogue is never silently dropping encryption the user asked for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TlsMode {
    #[default]
    Disable,
    Require,
    VerifyCa,
    VerifyFull,
}

impl TlsMode {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "disable" => Some(TlsMode::Disable),
            "require" => Some(TlsMode::Require),
            "verify-ca" => Some(TlsMode::VerifyCa),
            "verify-full" => Some(TlsMode::VerifyFull),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            TlsMode::Disable => "disable",
            TlsMode::Require => "require",
            TlsMode::VerifyCa => "verify-ca",
            TlsMode::VerifyFull => "verify-full",
        }
    }
}

/// Translate a `tokio_postgres::Error` into the one error type that crosses
/// the datagrep-api seam (design: `DbError` doc comment — "coarse by design").
pub fn map_pg_error(err: tokio_postgres::Error) -> DbError {
    use std::error::Error as _;

    if err.is_closed() {
        return DbError::Closed;
    }
    // SQLSTATE-bearing errors are genuine query failures the UI should show
    // inline (squiggles, position) rather than treat as connection loss.
    if let Some(db_err) = err.as_db_error() {
        return DbError::Query {
            code: Some(db_err.code().code().to_string()),
            message: db_err.message().to_string(),
            position: db_err.position().map(|p| match p {
                tokio_postgres::error::ErrorPosition::Original(pos) => *pos,
                tokio_postgres::error::ErrorPosition::Internal { position, .. } => *position,
            }),
        };
    }
    // Authentication failures surface as a generic error with no SQLSTATE;
    // tokio-postgres's `Error::to_string()` mentions "password authentication
    // failed" etc. verbatim, so string-sniffing here is the pragmatic call —
    // there is no structured `is_auth()` API in this crate version.
    let msg = err.to_string();
    if msg.contains("password") || msg.contains("authentication") {
        return DbError::Auth(msg);
    }
    if err
        .source()
        .and_then(|s| s.downcast_ref::<std::io::Error>())
        .is_some()
    {
        return DbError::Io(std::io::Error::other(msg));
    }
    DbError::Protocol(msg)
}
