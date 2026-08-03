//! design §3.8: "profiles store only `secret_ref`, NEVER a secret." This
//! module is the enforcement point — every `create_profile`/`update_profile`
//! call in `Store` runs `config` through here before it ever reaches SQL.
//!
//! The check is a case-insensitive substring match on config *keys*, not
//! values: we cannot know what a driver-specific field name means, but no
//! legitimate non-secret field should ever be named `password`, `token`,
//! etc. False positives (a field genuinely called e.g. `key_prefix`) are the
//! accepted cost of a crate that has no per-driver schema knowledge — the
//! error message tells the caller exactly why and points at `secret_ref`.

use dbx_api::ConnectionConfig;

use crate::error::ProfilesError;

/// Substrings that mark a config key as secret-shaped. Case-insensitive.
const SECRET_PATTERNS: &[&str] = &["password", "secret", "token", "key", "passphrase"];

/// Rejects a `ConnectionConfig` whose keys look like they hold a credential.
///
/// # Errors
/// `ProfilesError::SecretShapedKey` naming the offending key and pattern.
pub fn validate_no_secrets(config: &ConnectionConfig) -> Result<(), ProfilesError> {
    for key in config.values.keys() {
        let lower = key.to_ascii_lowercase();
        if let Some(pattern) = SECRET_PATTERNS.iter().find(|p| lower.contains(**p)) {
            return Err(ProfilesError::SecretShapedKey {
                key: key.clone(),
                pattern,
            });
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use dbx_api::ConfigValue;
    use std::collections::BTreeMap;
    use std::sync::Arc;

    fn config_with(key: &str) -> ConnectionConfig {
        let mut values = BTreeMap::new();
        values.insert(key.to_string(), ConfigValue::Str("x".into()));
        ConnectionConfig {
            driver: Arc::from("postgres"),
            values,
        }
    }

    #[test]
    fn rejects_password_case_insensitively() {
        for key in [
            "password",
            "Password",
            "PASSWORD",
            "db_password",
            "userToken",
            "api_key",
        ] {
            let err = validate_no_secrets(&config_with(key)).unwrap_err();
            assert!(
                matches!(err, ProfilesError::SecretShapedKey { .. }),
                "key {key} should be rejected"
            );
        }
    }

    #[test]
    fn accepts_ordinary_fields() {
        for key in [
            "host",
            "port",
            "database",
            "tls",
            "sslmode",
            "application_name",
        ] {
            assert!(
                validate_no_secrets(&config_with(key)).is_ok(),
                "key {key} should be accepted"
            );
        }
    }
}
