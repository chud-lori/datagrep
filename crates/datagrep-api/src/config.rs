//! Declarative connection configuration. Drivers describe their form as data
//! (`ConfigSchema`) so every frontend renders it without knowing the engine;
//! secrets are resolved late and zeroized on drop, never stored.

use std::collections::BTreeMap;
use std::fmt;
use std::sync::Arc;

use serde::{Deserialize, Serialize};

/// A driver's connection form, as data — the UI renders it, no per-engine code.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct ConfigSchema {
    pub fields: Vec<ConfigField>,
}

/// One field of the connection form.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ConfigField {
    /// Stable key the value is stored under (e.g. `host`, `port`).
    pub key: Arc<str>,
    /// Human label for the form.
    pub label: Arc<str>,
    pub kind: FieldKind,
    pub required: bool,
    pub default: Option<ConfigValue>,
    /// Secret fields never enter `ConnectionConfig` — they live in the OS
    /// keychain and only ever surface as a `SecretString`.
    pub secret: bool,
}

/// Widget/validation kind of a config field.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum FieldKind {
    Text,
    /// Rendered masked; implies `secret` handling.
    Password,
    Number,
    Bool,
    Select {
        options: Vec<Arc<str>>,
    },
    /// A filesystem path (e.g. an SQLite file), with a picker.
    Path,
}

/// A stored config value. Deliberately tiny — connection profiles are
/// git-committable TOML a human can read and review, not arbitrary JSON.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ConfigValue {
    Str(String),
    Num(f64),
    Bool(bool),
}

/// A connection profile minus its secrets. Safe to persist, export, and diff.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct ConnectionConfig {
    /// Which driver this profile belongs to (registry id, e.g. `postgres`).
    pub driver: Arc<str>,
    pub values: BTreeMap<String, ConfigValue>,
}

/// A profile plus its resolved secrets, built just-in-time for `connect` and
/// dropped (zeroized) as soon as the handshake completes.
#[derive(Debug)]
pub struct ResolvedConfig {
    pub config: ConnectionConfig,
    pub secrets: BTreeMap<String, SecretString>,
}

impl ResolvedConfig {
    /// A resolved config with no secrets (e.g. SQLite, trust auth).
    pub fn without_secrets(config: ConnectionConfig) -> Self {
        Self {
            config,
            secrets: BTreeMap::new(),
        }
    }
}

/// A secret that zeroizes its bytes on drop and redacts itself from `Debug` —
/// never logged, never in crash dumps, never shown to the UI.
pub struct SecretString(String);

impl SecretString {
    pub fn new(value: String) -> Self {
        Self(value)
    }

    /// Deliberately loud name: every call site is a place secret bytes escape.
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for SecretString {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("\"••••\"")
    }
}

impl Drop for SecretString {
    fn drop(&mut self) {
        zeroize_in_place(&mut self.0);
    }
}

/// Overwrite a string's bytes with zeros via volatile writes so the compiler
/// cannot elide the wipe of a buffer that is about to be freed.
fn zeroize_in_place(s: &mut str) {
    // SAFETY: writing 0x00 bytes keeps the buffer valid UTF-8 (NUL is a valid
    // one-byte scalar), so the String invariants hold.
    let bytes = unsafe { s.as_bytes_mut() };
    for b in bytes.iter_mut() {
        // SAFETY: `b` is a valid, aligned, exclusive reference into the buffer.
        unsafe { std::ptr::write_volatile(b, 0) };
    }
    std::sync::atomic::compiler_fence(std::sync::atomic::Ordering::SeqCst);
}

/// A connection URL with any inline password masked, for an error message or a
/// log line.
///
/// A pasted `postgres://alice:hunter2@host/db` is the one place a password
/// arrives as ordinary text, before anything has had a chance to split it into
/// a `SecretString` — and "could not parse `<url>`" is precisely the message a
/// user copies into a bug report or a chat channel. So the redaction has to
/// happen at the point of formatting, not at the point of storage.
///
/// The username, host, port and path survive: a redaction that hides those is
/// useless for diagnosing the very error it accompanies, and none of them is
/// the secret. Anything that is not a URL with credentials comes back
/// unchanged.
pub fn redact_url(url: &str) -> String {
    let Some(scheme_end) = url.find("://") else {
        return url.to_string();
    };
    let authority_start = scheme_end + 3;
    // The authority runs to the first `/`, `?` or `#`. Bounding it matters:
    // an `@` in a path or a query string is not a credential separator, and
    // masking up to it would eat the part of the URL worth showing.
    let authority_end = url[authority_start..]
        .find(['/', '?', '#'])
        .map(|i| authority_start + i)
        .unwrap_or(url.len());
    let authority = &url[authority_start..authority_end];
    // Last `@`, not first: a password may legally contain one.
    let Some(at) = authority.rfind('@') else {
        return url.to_string();
    };
    // No `:` means userinfo is a bare username, which is not a secret.
    let Some(colon) = authority[..at].find(':') else {
        return url.to_string();
    };
    format!(
        "{}{}:••••{}",
        &url[..authority_start],
        &authority[..colon],
        &url[authority_start + at..]
    )
}

/// Configuration problems, reported per-field so the form can point at them.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ConfigError {
    #[error("missing required field `{key}`")]
    MissingField { key: String },
    #[error("invalid value for `{key}`: {reason}")]
    InvalidValue { key: String, reason: String },
    #[error("unknown field `{key}`")]
    UnknownField { key: String },
    #[error("could not parse connection url: {reason}")]
    InvalidUrl { reason: String },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secret_string_debug_is_redacted() {
        let s = SecretString::new("hunter2".to_string());
        let dbg = format!("{s:?}");
        assert!(!dbg.contains("hunter2"), "secret leaked into Debug: {dbg}");
        assert_eq!(dbg, "\"••••\"");
        // The value is still reachable on purpose, only through `expose`.
        assert_eq!(s.expose(), "hunter2");
    }

    #[test]
    fn resolved_config_debug_is_redacted() {
        let mut secrets = BTreeMap::new();
        secrets.insert("password".to_string(), SecretString::new("s3cret".into()));
        let rc = ResolvedConfig {
            config: ConnectionConfig::default(),
            secrets,
        };
        assert!(!format!("{rc:?}").contains("s3cret"));
    }

    #[test]
    fn zeroize_overwrites_bytes() {
        let mut s = String::from("correct horse battery staple");
        zeroize_in_place(&mut s);
        assert_eq!(s.len(), 28, "length preserved, contents wiped");
        assert!(s.as_bytes().iter().all(|&b| b == 0));
    }

    #[test]
    fn config_roundtrips_through_serde() {
        let mut values = BTreeMap::new();
        values.insert("host".into(), ConfigValue::Str("localhost".into()));
        values.insert("port".into(), ConfigValue::Num(5432.0));
        values.insert("tls".into(), ConfigValue::Bool(true));
        let cfg = ConnectionConfig {
            driver: Arc::from("postgres"),
            values,
        };
        let json = serde_json::to_string(&cfg).unwrap();
        let back: ConnectionConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(back, cfg);
    }

    /// The redaction has to keep enough of the URL to diagnose the error it is
    /// attached to, and drop exactly the password.
    #[test]
    fn redact_url_masks_the_password_and_nothing_else() {
        assert_eq!(
            redact_url("postgres://alice:hunter2@db.example.com:5432/app?sslmode=require"),
            "postgres://alice:••••@db.example.com:5432/app?sslmode=require"
        );
        // A password containing an `@` — the reason the scan is `rfind`.
        assert_eq!(
            redact_url("mongodb://u:p@ss@host/db"),
            "mongodb://u:••••@host/db"
        );
        // A bare username is not a secret and stays readable.
        assert_eq!(
            redact_url("redis://user@host:6379"),
            "redis://user@host:6379"
        );
        // Nothing to redact, nothing changed.
        assert_eq!(redact_url("sqlite:///tmp/x.db"), "sqlite:///tmp/x.db");
        assert_eq!(redact_url(":memory:"), ":memory:");
        assert_eq!(redact_url("nonsense"), "nonsense");
        assert_eq!(redact_url(""), "");
        // An `@` in the path or query is not a credential separator.
        assert_eq!(
            redact_url("http://host:9200/idx?q=a@b"),
            "http://host:9200/idx?q=a@b"
        );
        // Non-ASCII anywhere must not panic on a byte-index slice.
        assert_eq!(
            redact_url("postgres://ünïcode:pä$$@hö.st/dæta"),
            "postgres://ünïcode:••••@hö.st/dæta"
        );
    }
}
