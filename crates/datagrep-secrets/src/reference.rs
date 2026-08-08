//! [`SecretRef`] — the parsed form of a profile's secret reference string.
//!
//! References are **not** secrets: they name where a secret lives, and are
//! deliberately safe to store in git-committable profiles and to show in
//! UI/errors. The referenced *value* only ever exists as a
//! [`datagrep_api::SecretString`].

use std::fmt;
use std::str::FromStr;

use crate::SecretError;

/// A parsed secret reference.
///
/// String forms: `keychain:<service>:<account>` · `env:<VAR>` ·
/// `exec:<command line>` · `prompt:`. `Display` round-trips through
/// [`SecretRef::from_str`].
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum SecretRef {
    /// OS keychain entry, addressed as service + account (keyring crate terms).
    Keychain { service: String, account: String },
    /// Process environment variable. Read-only.
    Env { var: String },
    /// Shell command line run via `sh -c`; trimmed stdout is the secret.
    /// Read-only. Covers `op read …`, `aws rds generate-db-auth-token …`, etc.
    Exec { command: String },
    /// Ask the user. Resolution always returns [`SecretError::NeedsPrompt`];
    /// prompting is the frontend's job (this crate never reads a TTY).
    Prompt,
}

impl SecretRef {
    /// Whether [`store`](crate::SecretResolver::store) /
    /// [`delete`](crate::SecretResolver::delete) can work on this ref.
    /// Only keychain refs are writable; env/exec/prompt are read-only sources.
    pub fn is_writable(&self) -> bool {
        matches!(self, SecretRef::Keychain { .. })
    }

    /// Short scheme name, for tracing and error text. Never secret material.
    pub fn scheme(&self) -> &'static str {
        match self {
            SecretRef::Keychain { .. } => "keychain",
            SecretRef::Env { .. } => "env",
            SecretRef::Exec { .. } => "exec",
            SecretRef::Prompt => "prompt",
        }
    }
}

impl FromStr for SecretRef {
    type Err = SecretError;

    fn from_str(input: &str) -> Result<Self, Self::Err> {
        let raw = input.trim();
        let (scheme, rest) = match raw.split_once(':') {
            Some((s, r)) => (s, r),
            // Bare `prompt` is accepted as a convenience alias of `prompt:`.
            None if raw == "prompt" => ("prompt", ""),
            None => {
                return Err(SecretError::parse(
                    raw,
                    "expected `<scheme>:<rest>` with scheme one of \
                     keychain | env | exec | prompt",
                ));
            }
        };
        match scheme {
            "keychain" => {
                let (service, account) = rest.split_once(':').ok_or_else(|| {
                    SecretError::parse(raw, "keychain ref needs `keychain:<service>:<account>`")
                })?;
                if service.is_empty() || account.is_empty() {
                    return Err(SecretError::parse(
                        raw,
                        "keychain service and account must be non-empty",
                    ));
                }
                Ok(SecretRef::Keychain {
                    service: service.to_owned(),
                    account: account.to_owned(),
                })
            }
            "env" => {
                if rest.is_empty() || rest.contains(char::is_whitespace) {
                    return Err(SecretError::parse(
                        raw,
                        "env ref needs `env:<VAR>` with a non-empty variable name",
                    ));
                }
                Ok(SecretRef::Env {
                    var: rest.to_owned(),
                })
            }
            "exec" => {
                let command = rest.trim();
                if command.is_empty() {
                    return Err(SecretError::parse(
                        raw,
                        "exec ref needs `exec:<command line>` with a non-empty command",
                    ));
                }
                Ok(SecretRef::Exec {
                    command: command.to_owned(),
                })
            }
            "prompt" => {
                if rest.is_empty() {
                    Ok(SecretRef::Prompt)
                } else {
                    Err(SecretError::parse(
                        raw,
                        "prompt ref takes no argument: `prompt:`",
                    ))
                }
            }
            other => Err(SecretError::parse(
                raw,
                &format!("unknown scheme `{other}` (expected keychain | env | exec | prompt)"),
            )),
        }
    }
}

impl fmt::Display for SecretRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SecretRef::Keychain { service, account } => {
                write!(f, "keychain:{service}:{account}")
            }
            SecretRef::Env { var } => write!(f, "env:{var}"),
            SecretRef::Exec { command } => write!(f, "exec:{command}"),
            SecretRef::Prompt => f.write_str("prompt:"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_keychain() {
        let r: SecretRef = "keychain:datagrep:prod-pg/admin".parse().unwrap();
        assert_eq!(
            r,
            SecretRef::Keychain {
                service: "datagrep".into(),
                account: "prod-pg/admin".into()
            }
        );
        assert!(r.is_writable());
    }

    #[test]
    fn keychain_account_may_contain_colons() {
        // Account is "everything after the second colon" — colons inside are data.
        let r: SecretRef = "keychain:datagrep:acct:with:colons".parse().unwrap();
        assert_eq!(
            r,
            SecretRef::Keychain {
                service: "datagrep".into(),
                account: "acct:with:colons".into()
            }
        );
    }

    #[test]
    fn parses_env_exec_prompt() {
        assert_eq!(
            "env:PGPASSWORD".parse::<SecretRef>().unwrap(),
            SecretRef::Env {
                var: "PGPASSWORD".into()
            }
        );
        assert_eq!(
            "exec:op read op://vault/pg/password"
                .parse::<SecretRef>()
                .unwrap(),
            SecretRef::Exec {
                command: "op read op://vault/pg/password".into()
            }
        );
        assert_eq!("prompt:".parse::<SecretRef>().unwrap(), SecretRef::Prompt);
        assert_eq!("prompt".parse::<SecretRef>().unwrap(), SecretRef::Prompt);
    }

    #[test]
    fn display_round_trips() {
        for s in [
            "keychain:datagrep:staging",
            "env:DB_PASS",
            "exec:aws rds generate-db-auth-token --hostname h",
            "prompt:",
        ] {
            let r: SecretRef = s.parse().unwrap();
            assert_eq!(r.to_string(), s);
            assert_eq!(r.to_string().parse::<SecretRef>().unwrap(), r);
        }
    }

    #[test]
    fn rejects_malformed_refs() {
        for bad in [
            "",
            "swordfish",             // no scheme
            "vault:foo",             // unknown scheme
            "keychain:only-service", // missing account
            "keychain::acct",        // empty service
            "keychain:svc:",         // empty account
            "env:",                  // empty var
            "env:HAS SPACE",         // whitespace in var
            "exec:",                 // empty command
            "exec:   ",              // blank command
            "prompt:extra",          // prompt takes no argument
        ] {
            let err = bad.parse::<SecretRef>().unwrap_err();
            assert!(
                matches!(err, SecretError::Parse { .. }),
                "`{bad}` should be a parse error, got: {err}"
            );
        }
    }

    #[test]
    fn only_keychain_is_writable() {
        assert!(!"env:X".parse::<SecretRef>().unwrap().is_writable());
        assert!(!"exec:true".parse::<SecretRef>().unwrap().is_writable());
        assert!(!"prompt:".parse::<SecretRef>().unwrap().is_writable());
    }
}
