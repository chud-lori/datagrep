//! Exit codes (ticket "Requirements"): `0` ok, `1` query error, `2` usage
//! error, `130` cancelled. `CliError` is the one error type `main` matches on
//! to pick a code — every command function returns `Result<(), CliError>`,
//! never panics, and every variant carries a message naming what to fix.

use std::fmt;

/// The three non-zero outcomes a command can report, plus the process exit
/// code each one maps to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExitKind {
    /// A query/connection/driver failure — the command ran, the database (or
    /// the attempt to reach it) said no.
    QueryError,
    /// Bad arguments, an unknown profile, a malformed file — the command
    /// itself could not even be attempted as given.
    UsageError,
    /// The user hit Ctrl-C.
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

/// The one error type every `dbx` subcommand returns. Never a panic, never a
/// bare `anyhow`-style opaque blob: `message` always names what to fix, and
/// `kind` picks the exit code.
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

/// Any driver/core failure becomes a `QueryError` by default — the common
/// case for `dbx_api::DbError` reaching a command function. Commands that
/// need a different mapping (e.g. a connect failure they want to call a
/// usage error) build a `CliError` by hand instead of relying on `?`.
impl From<dbx_api::DbError> for CliError {
    fn from(err: dbx_api::DbError) -> Self {
        CliError::query(err.to_string())
    }
}

impl From<dbx_profiles::ProfilesError> for CliError {
    fn from(err: dbx_profiles::ProfilesError) -> Self {
        CliError::usage(err.to_string())
    }
}

impl From<dbx_secrets::SecretError> for CliError {
    fn from(err: dbx_secrets::SecretError) -> Self {
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
