use std::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExitKind {
    QueryError,
    UsageError,
    Cancelled,
}

impl ExitKind {
    pub fn code(self) -> u8 {
        match self {
            ExitKind::QueryError => 1,
            ExitKind::UsageError => 2,
            ExitKind::Cancelled => 130,
        }
    }
}

#[derive(Debug)]
pub struct CliError {
    pub kind: ExitKind,
    pub message: String,
}

impl CliError {
    pub fn usage(message: impl Into<String>) -> Self {
        Self {
            kind: ExitKind::UsageError,
            message: message.into(),
        }
    }

    pub fn query(message: impl Into<String>) -> Self {
        Self {
            kind: ExitKind::QueryError,
            message: message.into(),
        }
    }

    pub fn cancelled(message: impl Into<String>) -> Self {
        Self {
            kind: ExitKind::Cancelled,
            message: message.into(),
        }
    }
}

impl fmt::Display for CliError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for CliError {}

impl From<datagrep_api::DbError> for CliError {
    fn from(err: datagrep_api::DbError) -> Self {
        CliError::query(err.to_string())
    }
}

impl From<datagrep_profiles::ProfilesError> for CliError {
    fn from(err: datagrep_profiles::ProfilesError) -> Self {
        CliError::usage(err.to_string())
    }
}

impl From<datagrep_secrets::SecretError> for CliError {
    fn from(err: datagrep_secrets::SecretError) -> Self {
        CliError::usage(err.to_string())
    }
}

impl From<std::io::Error> for CliError {
    fn from(err: std::io::Error) -> Self {
        CliError::usage(format!("i/o error: {err}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exit_codes_match_the_ticket() {
        assert_eq!(ExitKind::QueryError.code(), 1);
        assert_eq!(ExitKind::UsageError.code(), 2);
        assert_eq!(ExitKind::Cancelled.code(), 130);
    }
}
