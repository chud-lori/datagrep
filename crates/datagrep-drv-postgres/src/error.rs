use datagrep_api::DbError;

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

pub fn map_pg_error(err: tokio_postgres::Error) -> DbError {
    use std::error::Error as _;

    if err.is_closed() {
        return DbError::Closed;
    }
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
