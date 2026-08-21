use std::collections::BTreeMap;
use std::fmt;
use std::sync::Arc;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct ConfigSchema {
    pub fields: Vec<ConfigField>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ConfigField {
    pub key: Arc<str>,
    pub label: Arc<str>,
    pub kind: FieldKind,
    pub required: bool,
    pub default: Option<ConfigValue>,
    pub secret: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum FieldKind {
    Text,
    Password,
    Number,
    Bool,
    Select { options: Vec<Arc<str>> },
    Path,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ConfigValue {
    Str(String),
    Num(f64),
    Bool(bool),
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct ConnectionConfig {
    pub driver: Arc<str>,
    pub values: BTreeMap<String, ConfigValue>,
}

#[derive(Debug)]
pub struct ResolvedConfig {
    pub config: ConnectionConfig,
    pub secrets: BTreeMap<String, SecretString>,
}

impl ResolvedConfig {
    pub fn without_secrets(config: ConnectionConfig) -> Self {
        Self {
            config,
            secrets: BTreeMap::new(),
        }
    }
}

pub struct SecretString(String);

impl SecretString {
    pub fn new(value: String) -> Self {
        Self(value)
    }

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

fn zeroize_in_place(s: &mut str) {
    // SAFETY: 0x00 is a one-byte UTF-8 scalar, so the String invariants hold.
    let bytes = unsafe { s.as_bytes_mut() };
    for b in bytes.iter_mut() {
        // SAFETY: `b` is a valid, aligned, exclusive reference into the buffer.
        unsafe { std::ptr::write_volatile(b, 0) };
    }
    std::sync::atomic::compiler_fence(std::sync::atomic::Ordering::SeqCst);
}

pub fn redact_url(url: &str) -> String {
    let Some(scheme_end) = url.find("://") else {
        return url.to_string();
    };
    let authority_start = scheme_end + 3;
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
