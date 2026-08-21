use datagrep_api::DbError;

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
