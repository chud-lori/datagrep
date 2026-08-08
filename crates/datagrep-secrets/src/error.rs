//! Error type for secret resolution.
//!
//! Security invariant: no variant's `Display` may ever contain
//! secret material. References, env var *names*, exit statuses, and captured
//! **stderr** are allowed; stdout (the secret channel of `exec:`) and resolved
//! values are not. The exec command line is also kept out of every variant so
//! a command can never be logged alongside any of its output.

use std::time::Duration;

/// Why a secret could not be resolved (or stored/deleted).
#[derive(Debug, thiserror::Error)]
pub enum SecretError {
    /// The ref is `prompt:` — the frontend must ask the user. Deliberately an
    /// error, not a blocking read: this crate never touches a TTY — the UI
    /// gets a masked field, and we only ever receive what it collected.
    #[error("secret requires an interactive prompt (ref `{reference}`)")]
    NeedsPrompt {
        /// The offending ref's string form (refs are not secrets).
        reference: String,
    },

    /// The reference string didn't parse.
    #[error("invalid secret reference `{input}`: {reason}")]
    Parse { input: String, reason: String },

    /// `env:` variable unset or not valid unicode. Carries the name only.
    #[error("environment variable `{var}` is {problem}")]
    Env {
        var: String,
        /// "not set" | "not valid unicode".
        problem: &'static str,
    },

    /// OS keychain failure, from the `keyring` crate.
    /// `keyring::Error` messages describe the entry and the platform error;
    /// on the read path there is no secret to leak, and `set_password`
    /// failures do not echo the password.
    #[error("keychain error for service `{service}`, account `{account}`: {source}")]
    Keychain {
        service: String,
        account: String,
        #[source]
        source: keyring::Error,
    },

    /// `exec:` command could not be spawned at all.
    #[error("failed to spawn secret command: {source}")]
    ExecSpawn {
        #[source]
        source: std::io::Error,
    },

    /// `exec:` command exited non-zero. stderr is captured (it is the
    /// command's diagnostic channel); stdout — the secret channel — and the
    /// command line are deliberately NOT included, so neither can ever be
    /// logged.
    #[error("secret command failed with {status}; stderr: {stderr}")]
    ExecFailed {
        /// e.g. "exit status: 1" or "signal: 9".
        status: String,
        /// Captured stderr, truncated to a sane length.
        stderr: String,
    },

    /// `exec:` command ran past the timeout and was killed.
    #[error("secret command timed out after {timeout:?}")]
    ExecTimeout { timeout: Duration },

    /// `exec:` produced stdout that is not valid UTF-8. The bytes are dropped,
    /// never shown.
    #[error("secret command produced non-UTF-8 output")]
    ExecNotUtf8,

    /// `exec:` succeeded but printed nothing (after trimming). Almost always a
    /// misconfigured command; surfaced instead of storing an empty password.
    #[error("secret command produced no output")]
    ExecEmpty,

    /// `store`/`delete` on a ref that isn't a keychain entry.
    #[error(
        "secret ref `{reference}` is read-only: {operation} is only supported for keychain refs"
    )]
    ReadOnly {
        reference: String,
        /// "store" | "delete".
        operation: &'static str,
    },
}

impl SecretError {
    pub(crate) fn parse(input: &str, reason: &str) -> Self {
        SecretError::Parse {
            input: input.to_owned(),
            reason: reason.to_owned(),
        }
    }
}
