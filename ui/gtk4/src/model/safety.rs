use serde::Deserialize;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(from = "String")]
pub enum SafetyLevel {
    #[default]
    Silent,
    WarnAll,
    WarnWrites,
    AuthAll,
    AuthWrites,
}

impl From<String> for SafetyLevel {
    fn from(name: String) -> Self {
        // A level this build does not know fails closed, exactly as the engine reads it back.
        SafetyLevel::parse(&name).unwrap_or(SafetyLevel::AuthAll)
    }
}

impl SafetyLevel {
    pub const ALL: [SafetyLevel; 5] = [
        SafetyLevel::Silent,
        SafetyLevel::WarnAll,
        SafetyLevel::WarnWrites,
        SafetyLevel::AuthAll,
        SafetyLevel::AuthWrites,
    ];

    pub fn parse(name: &str) -> Option<SafetyLevel> {
        match name {
            "silent" => Some(SafetyLevel::Silent),
            "warn_all" => Some(SafetyLevel::WarnAll),
            "warn_writes" => Some(SafetyLevel::WarnWrites),
            "auth_all" => Some(SafetyLevel::AuthAll),
            "auth_writes" => Some(SafetyLevel::AuthWrites),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            SafetyLevel::Silent => "silent",
            SafetyLevel::WarnAll => "warn_all",
            SafetyLevel::WarnWrites => "warn_writes",
            SafetyLevel::AuthAll => "auth_all",
            SafetyLevel::AuthWrites => "auth_writes",
        }
    }

    /// The picker's row title.
    pub fn title(self) -> &'static str {
        match self {
            SafetyLevel::Silent => "Silent",
            SafetyLevel::WarnAll => "Warn on everything",
            SafetyLevel::WarnWrites => "Warn on writes",
            SafetyLevel::AuthAll => "Authenticate everything",
            SafetyLevel::AuthWrites => "Authenticate writes",
        }
    }

    /// The line under the picker — short enough to leave the selected level its room.
    pub fn blurb(self) -> &'static str {
        match self {
            SafetyLevel::Silent => "Sends every statement without asking",
            SafetyLevel::WarnAll => "Confirms every statement, reads included",
            SafetyLevel::WarnWrites => "Confirms anything that is not a read",
            SafetyLevel::AuthAll => "Authenticates everything, reads included",
            SafetyLevel::AuthWrites => "Authenticates anything that is not a read",
        }
    }

    /// "`prod` …" — the connection as the subject of the sentence.
    pub fn phrase(self) -> &'static str {
        match self {
            SafetyLevel::Silent => "sends every statement without asking",
            SafetyLevel::WarnAll => "warns before every statement",
            SafetyLevel::WarnWrites => "warns before anything that is not a read",
            SafetyLevel::AuthAll => "requires authentication before every statement",
            SafetyLevel::AuthWrites => "requires authentication before anything that is not a read",
        }
    }

    /// The two or three words the chrome and tooltips carry.
    pub fn badge(self) -> &'static str {
        match self {
            SafetyLevel::Silent => "",
            SafetyLevel::WarnAll => "warns on everything",
            SafetyLevel::WarnWrites => "warns on writes",
            SafetyLevel::AuthAll => "authenticates everything",
            SafetyLevel::AuthWrites => "authenticates writes",
        }
    }

    pub fn gates(self) -> bool {
        self != SafetyLevel::Silent
    }

    pub fn authenticates(self) -> bool {
        matches!(self, SafetyLevel::AuthAll | SafetyLevel::AuthWrites)
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Requirement {
    #[default]
    None,
    Warn,
    Authenticate,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SafetyStatement {
    pub text: String,
    #[serde(default)]
    pub class: String,
    #[serde(default)]
    pub requires: Requirement,
}

/// What `datagrep_safety_evaluate_json` / `_pending_json` answered: the rung, and the challenge that clears it.
#[derive(Debug, Clone, Deserialize)]
pub struct SafetyDecision {
    pub profile: String,
    #[serde(default)]
    pub level: SafetyLevel,
    #[serde(default)]
    pub requires: Requirement,
    #[serde(default)]
    pub challenge: Option<String>,
    #[serde(default)]
    pub statements: Vec<SafetyStatement>,
}

fn class_noun(class: &str) -> &'static str {
    match class {
        "read" => "a read",
        "write" => "a write",
        "ddl" => "a DDL statement",
        "tcl" => "a transaction statement",
        "admin" => "an admin statement",
        _ => "a statement datagrep cannot classify",
    }
}

fn class_counts(statements: &[SafetyStatement]) -> String {
    let plural = |n: usize, one: &str, many: &str| {
        if n == 1 {
            format!("1 {one}")
        } else {
            format!("{n} {many}")
        }
    };
    let mut parts = Vec::new();
    for (class, one, many) in [
        ("read", "read", "reads"),
        ("write", "write", "writes"),
        ("ddl", "DDL statement", "DDL statements"),
        ("tcl", "transaction statement", "transaction statements"),
        ("admin", "admin statement", "admin statements"),
        (
            "unknown",
            "unclassifiable statement",
            "unclassifiable statements",
        ),
    ] {
        let n = statements.iter().filter(|s| s.class == class).count();
        if n > 0 {
            parts.push(plural(n, one, many));
        }
    }
    parts.join(", ")
}

impl SafetyDecision {
    pub fn parse(json: &str) -> Option<SafetyDecision> {
        serde_json::from_str(json).ok()
    }

    pub fn parse_pending(json: &str) -> Vec<SafetyDecision> {
        serde_json::from_str(json).unwrap_or_default()
    }

    pub fn heading(&self) -> String {
        let what = match self.statements.as_slice() {
            [only] => class_noun(&only.class).to_string(),
            many => format!("{} statements", many.len()),
        };
        match self.requires {
            Requirement::Authenticate => {
                format!("Authenticate to run {what} on “{}”", self.profile)
            }
            _ => format!("Run {what} on “{}”?", self.profile),
        }
    }

    pub fn body(&self) -> String {
        let mut body = format!("“{}” {}.", self.profile, self.level.phrase());
        match self.statements.as_slice() {
            [] => {}
            [only] => {
                body.push_str(&format!(" This statement is {}", class_noun(&only.class)));
                body.push_str(if only.class == "unknown" {
                    ", so it counts as a write."
                } else {
                    "."
                });
            }
            many => {
                body.push_str(&format!(" This script is {}.", class_counts(many)));
                if many.iter().any(|s| s.class == "unknown") {
                    body.push_str(" Anything unclassifiable counts as a write.");
                }
            }
        }
        body.push_str(" Nothing has been sent.");
        body
    }
}

/// The challenge id a synchronous refusal names in its error text.
pub fn challenge_in_error(error: &str) -> Option<&str> {
    let rest = error.rsplit_once("(challenge ")?.1;
    let id = &rest[..rest.find(')')?];
    (!id.is_empty()).then_some(id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_level_round_trips_and_an_unknown_one_fails_closed() {
        for level in SafetyLevel::ALL {
            assert_eq!(SafetyLevel::parse(level.as_str()), Some(level));
        }
        assert_eq!(
            SafetyLevel::from("safe_2".to_string()),
            SafetyLevel::AuthAll
        );
        assert_eq!(SafetyLevel::from("silent".to_string()), SafetyLevel::Silent);
    }

    #[test]
    fn only_silent_neither_gates_nor_carries_a_badge() {
        for level in SafetyLevel::ALL {
            assert_eq!(level.gates(), level != SafetyLevel::Silent);
            assert_eq!(level.badge().is_empty(), !level.gates());
        }
        assert!(SafetyLevel::AuthWrites.authenticates());
        assert!(!SafetyLevel::WarnWrites.authenticates());
    }

    #[test]
    fn a_decision_decodes_and_names_the_connection_and_the_class() {
        let d = SafetyDecision::parse(
            r#"{"profile":"prod","level":"warn_writes","requires":"warn",
                "challenge":"c-1","statements":[{"text":"drop table t","class":"ddl","requires":"warn"}]}"#,
        )
        .expect("a decision");
        assert_eq!(d.level, SafetyLevel::WarnWrites);
        assert_eq!(d.requires, Requirement::Warn);
        assert_eq!(d.heading(), "Run a DDL statement on “prod”?");
        assert!(d
            .body()
            .starts_with("“prod” warns before anything that is not a read."));
        assert!(d.body().ends_with("Nothing has been sent."));
    }

    #[test]
    fn an_authenticate_decision_says_so_in_the_heading() {
        let d = SafetyDecision::parse(
            r#"{"profile":"prod","level":"auth_all","requires":"authenticate",
                "challenge":"c-2","statements":[{"text":"select 1","class":"read","requires":"authenticate"}]}"#,
        )
        .expect("a decision");
        assert_eq!(d.heading(), "Authenticate to run a read on “prod”");
    }

    #[test]
    fn a_script_is_summarised_by_class_and_unknown_counts_as_a_write() {
        let d = SafetyDecision::parse(
            r#"{"profile":"prod","level":"warn_all","requires":"warn","challenge":"c-3",
                "statements":[{"text":"select 1","class":"read","requires":"warn"},
                              {"text":"update t set a=1","class":"write","requires":"warn"},
                              {"text":"frobnicate","class":"unknown","requires":"warn"}]}"#,
        )
        .expect("a decision");
        assert_eq!(d.heading(), "Run 3 statements on “prod”?");
        let body = d.body();
        assert!(
            body.contains("1 read, 1 write, 1 unclassifiable statement"),
            "{body}"
        );
        assert!(
            body.contains("Anything unclassifiable counts as a write."),
            "{body}"
        );
    }

    #[test]
    fn the_challenge_id_is_recovered_from_a_synchronous_refusal() {
        let error =
            "`prod` is in safe mode: this statement requires a warning first (challenge ch-42)";
        assert_eq!(challenge_in_error(error), Some("ch-42"));
        assert_eq!(challenge_in_error("`prod` refused: syntax error"), None);
        assert_eq!(challenge_in_error("(challenge )"), None);
    }
}
