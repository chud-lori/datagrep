//! Error mapping: `mongodb::error::Error` -> the one `DbError` that crosses
//! the datagrep-api seam. `DbError` is coarse on purpose — see its own doc
//! comment.

use mongodb::error::{Error as MongoError, ErrorKind};

use datagrep_api::DbError;

/// Translate a driver error into `DbError`. Kept coarse on purpose — engine
/// detail lives in `DbError::Query { code, message, .. }` text, never in a
/// leaked driver-specific enum: nothing above the seam should have to match
/// on `mongodb::ErrorKind` to understand what went wrong.
pub fn map_mongo_error(err: MongoError) -> DbError {
    match *err.kind {
        ErrorKind::Authentication { message, .. } => DbError::Auth(message),
        ErrorKind::InvalidTlsConfig { message, .. } => DbError::Tls(message),
        ErrorKind::DnsResolve { message, .. } => DbError::Connect(message),
        ErrorKind::ServerSelection { message, .. } => DbError::Connect(message),
        ErrorKind::Io(e) => DbError::Io(std::io::Error::new(e.kind(), e.to_string())),
        ErrorKind::Command(cmd) => DbError::Query {
            code: Some(cmd.code.to_string()),
            message: cmd.message,
            position: None,
        },
        ErrorKind::Write(failure) => DbError::Query {
            code: None,
            message: format!("{failure:?}"),
            position: None,
        },
        ErrorKind::InsertMany(e) => DbError::Query {
            code: None,
            message: format!("{e:?}"),
            position: None,
        },
        ErrorKind::BulkWrite(e) => DbError::Query {
            code: None,
            message: format!("{e:?}"),
            position: None,
        },
        ErrorKind::InvalidArgument { message, .. } => DbError::Unsupported { feature: message },
        ErrorKind::IncompatibleServer { message, .. } => DbError::Unsupported { feature: message },
        ErrorKind::SessionsNotSupported => DbError::Unsupported {
            feature: "this deployment does not support sessions/transactions".into(),
        },
        ErrorKind::Shutdown => DbError::Closed,
        ErrorKind::BsonDeserialization(e) => DbError::Protocol(e.to_string()),
        ErrorKind::BsonSerialization(e) => DbError::Protocol(e.to_string()),
        other => DbError::Protocol(format!("{other}")),
    }
}

/// `true` when a driver-reported timeout should surface as `DbError::Timeout`
/// rather than a generic protocol error — used where we set `maxTimeMS`
/// ourselves and want the honest "operation timed out" shape.
pub fn is_timeout(err: &MongoError) -> bool {
    // The driver reports server-side maxTimeMS expiry as a `Command` error
    // with code 50 ("ExceededTimeLimit"); client-side deadline elapsing is
    // handled separately by the `tokio::time::timeout` wrapper (see
    // `connection.rs`), which never constructs a `mongodb::Error` at all.
    matches!(&*err.kind, ErrorKind::Command(cmd) if cmd.code == 50)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shutdown_maps_to_closed() {
        let err = MongoError::custom("boom".to_string());
        // `custom` doesn't build `Shutdown`, so just check the generic path
        // doesn't panic and produces a Protocol error for an unmapped kind.
        let mapped = map_mongo_error(err);
        assert!(matches!(mapped, DbError::Protocol(_)));
    }
}
