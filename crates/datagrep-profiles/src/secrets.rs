use datagrep_api::ConnectionConfig;

use crate::error::ProfilesError;

const SECRET_PATTERNS: &[&str] = &["password", "secret", "token", "key", "passphrase"];

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
    use datagrep_api::ConfigValue;
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
