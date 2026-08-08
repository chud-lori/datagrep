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
}
