use std::time::Duration;

#[derive(Debug, thiserror::Error)]
pub enum SecretError {
    #[error("secret requires an interactive prompt (ref `{reference}`)")]
    NeedsPrompt {
        reference: String,
    },

    #[error("invalid secret reference `{input}`: {reason}")]
    Parse { input: String, reason: String },

    #[error("environment variable `{var}` is {problem}")]
    Env {
        var: String,
        problem: &'static str,
    },

    #[error("keychain error for service `{service}`, account `{account}`: {source}")]
    Keychain {
        service: String,
        account: String,
        #[source]
        source: keyring::Error,
    },

    #[error("failed to spawn secret command: {source}")]
    ExecSpawn {
        #[source]
        source: std::io::Error,
    },

    #[error("secret command failed with {status}; stderr: {stderr}")]
    ExecFailed {
        status: String,
        stderr: String,
    },

    #[error("secret command timed out after {timeout:?}")]
    ExecTimeout { timeout: Duration },

    #[error("secret command produced non-UTF-8 output")]
    ExecNotUtf8,

    #[error("secret command produced no output")]
    ExecEmpty,

    #[error(
        "secret ref `{reference}` is read-only: {operation} is only supported for keychain refs"
    )]
    ReadOnly {
        reference: String,
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
