use serde::Deserialize;

use crate::model::SafetyLevel;

#[derive(Debug, Clone, Default, Deserialize)]
pub struct Profile {
    pub name: String,
    #[serde(default)]
    pub driver: String,
    #[serde(default)]
    pub read_only: bool,
    #[serde(default)]
    pub safety: SafetyLevel,
    #[serde(default)]
    pub color: Option<String>,
    #[serde(default)]
    pub has_secret: bool,
}

impl Profile {
    pub fn parse_list(json: &str) -> Result<Vec<Self>, String> {
        serde_json::from_str(json).map_err(|e| format!("the profile list did not decode: {e}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_safety_settings_survive_the_decode() {
        let profiles = Profile::parse_list(
            r#"[{"name":"prod","driver":"postgres","read_only":true,"safety":"auth_writes",
                 "confirm_writes":true,"color":"red","has_secret":true}]"#,
        )
        .expect("valid list");
        assert!(profiles[0].read_only);
        assert_eq!(profiles[0].safety, SafetyLevel::AuthWrites);
        assert_eq!(profiles[0].color.as_deref(), Some("red"));
    }

    #[test]
    fn a_missing_safety_key_reads_as_silent_and_an_unknown_one_fails_closed() {
        let profiles = Profile::parse_list(
            r#"[{"name":"old","driver":"sqlite"},
                 {"name":"new","driver":"sqlite","safety":"safe_9"}]"#,
        )
        .expect("valid list");
        assert_eq!(profiles[0].safety, SafetyLevel::Silent);
        assert_eq!(profiles[1].safety, SafetyLevel::AuthAll);
    }

    #[test]
    fn a_guardrail_key_this_build_does_not_know_is_not_fatal() {
        let profiles = Profile::parse_list(r#"[{"name":"dev","driver":"sqlite","future":1}]"#)
            .expect("valid list");
        assert!(!profiles[0].read_only);
        assert!(!profiles[0].has_secret);
    }
}
