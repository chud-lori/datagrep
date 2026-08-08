//! Error mapping and the deferred-TLS marker (design §3.1 seam).
//!
//! TLS posture mirrors `datagrep-drv-postgres`'s documented deviation: the ticket
//! asks for `redis://`/`rediss://` parsing so the connection form is honest
//! about what the engine supports, but the required dependency list
//! (`redis` with only `tokio-comp`/`connection-manager`) does not include a
//! TLS backend (`tls-native-tls` / `tls-rustls`). Rather than silently
//! downgrading a `rediss://` request to plaintext — a security regression —
//! `connect` fails fast with a clear "not yet implemented" error. See
//! `driver.rs` module docs for the full gap note.

use datagrep_api::DbError;

/// Translate a `redis::RedisError` into the one error type that crosses the
/// datagrep-api seam (`DbError` doc comment: "coarse by design").
pub fn map_redis_error(err: redis::RedisError) -> DbError {
    use redis::ErrorKind;

    if err.is_io_error() {
        return DbError::Io(std::io::Error::other(err.to_string()));
    }
    if err.is_timeout() {
        return DbError::Timeout;
    }
    if err.is_connection_dropped() {
        return DbError::Closed;
    }
    match err.kind() {
        ErrorKind::AuthenticationFailed => DbError::Auth(err.to_string()),
        ErrorKind::InvalidClientConfig => {
            DbError::Config(datagrep_api::ConfigError::InvalidValue {
                key: "url".into(),
                reason: err.to_string(),
            })
        }
        ErrorKind::Io => DbError::Io(std::io::Error::other(err.to_string())),
        _ => {
            // Server-reported errors (WRONGTYPE, syntax error, …) carry a
            // `code()` (the RESP error's leading word, e.g. "WRONGTYPE")
            // when the server sent one — preserved verbatim rather than
            // folded into the message, matching `DbError::Query`'s contract.
            DbError::Query {
                code: err.code().map(str::to_string),
                message: err.to_string(),
                position: None,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generic_server_error_becomes_query_error() {
        let e = redis::RedisError::from((redis::ErrorKind::Client, "boom"));
        let mapped = map_redis_error(e);
        assert!(matches!(mapped, DbError::Query { .. }));
    }
}
