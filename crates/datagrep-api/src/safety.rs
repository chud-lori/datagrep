use std::fmt;

use serde::{Deserialize, Serialize};

// The reference's five rungs: Silent, Alert 1 = WarnAll, Alert 2 = WarnWrites, Safe 1 = AuthAll, Safe 2 = AuthWrites.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SafetyLevel {
    #[default]
    Silent,
    WarnAll,
    WarnWrites,
    AuthAll,
    AuthWrites,
}

impl SafetyLevel {
    pub const ALL: [SafetyLevel; 5] = [
        SafetyLevel::Silent,
        SafetyLevel::WarnAll,
        SafetyLevel::WarnWrites,
        SafetyLevel::AuthAll,
        SafetyLevel::AuthWrites,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            SafetyLevel::Silent => "silent",
            SafetyLevel::WarnAll => "warn_all",
            SafetyLevel::WarnWrites => "warn_writes",
            SafetyLevel::AuthAll => "auth_all",
            SafetyLevel::AuthWrites => "auth_writes",
        }
    }

    pub fn parse(s: &str) -> Option<SafetyLevel> {
        SafetyLevel::ALL.into_iter().find(|l| l.as_str() == s)
    }

    // `read` is "datagrep-lang classified this statement Read"; Unknown is not a read, so it is gated.
    pub fn requirement(self, read: bool) -> Requirement {
        match self {
            SafetyLevel::Silent => Requirement::None,
            SafetyLevel::WarnAll => Requirement::Warn,
            SafetyLevel::AuthAll => Requirement::Authenticate,
            SafetyLevel::WarnWrites if read => Requirement::None,
            SafetyLevel::WarnWrites => Requirement::Warn,
            SafetyLevel::AuthWrites if read => Requirement::None,
            SafetyLevel::AuthWrites => Requirement::Authenticate,
        }
    }

    // The `confirm_writes` boolean this enum replaced: true meant "ask before a write".
    pub fn from_confirm_writes(confirm_writes: bool) -> SafetyLevel {
        if confirm_writes {
            SafetyLevel::WarnWrites
        } else {
            SafetyLevel::Silent
        }
    }

    pub fn confirms_writes(self) -> bool {
        self.requirement(false) != Requirement::None
    }
}

impl fmt::Display for SafetyLevel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum Requirement {
    #[default]
    None,
    Warn,
    Authenticate,
}

impl Requirement {
    pub fn as_str(self) -> &'static str {
        match self {
            Requirement::None => "none",
            Requirement::Warn => "warn",
            Requirement::Authenticate => "authenticate",
        }
    }

    pub fn parse(s: &str) -> Option<Requirement> {
        match s {
            "none" => Some(Requirement::None),
            "warn" => Some(Requirement::Warn),
            "authenticate" => Some(Requirement::Authenticate),
            _ => None,
        }
    }
}

impl fmt::Display for Requirement {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Requirement::None => "nothing",
            Requirement::Warn => "a warning",
            Requirement::Authenticate => "authentication",
        })
    }
}

// What a frontend did to clear a rung. The engine judges it; the frontend does not report a verdict.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum Attestation {
    Acknowledged,
    TypedPhrase { typed: String },
    SystemAuth { method: String },
}

impl Attestation {
    pub fn strength(&self) -> Requirement {
        match self {
            Attestation::Acknowledged => Requirement::Warn,
            Attestation::TypedPhrase { .. } | Attestation::SystemAuth { .. } => {
                Requirement::Authenticate
            }
        }
    }

    // `phrase` is the connection name, held by the engine and never sent in a challenge.
    pub fn satisfies(&self, requirement: Requirement, phrase: &str) -> bool {
        if self.strength() < requirement {
            return false;
        }
        match self {
            Attestation::Acknowledged => true,
            Attestation::TypedPhrase { typed } => typed == phrase,
            Attestation::SystemAuth { method } => !method.trim().is_empty(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_ladder_gates_reads_only_on_the_every_query_rungs() {
        assert_eq!(SafetyLevel::Silent.requirement(true), Requirement::None);
        assert_eq!(SafetyLevel::Silent.requirement(false), Requirement::None);
        assert_eq!(SafetyLevel::WarnAll.requirement(true), Requirement::Warn);
        assert_eq!(SafetyLevel::WarnAll.requirement(false), Requirement::Warn);
        assert_eq!(SafetyLevel::WarnWrites.requirement(true), Requirement::None);
        assert_eq!(
            SafetyLevel::WarnWrites.requirement(false),
            Requirement::Warn
        );
        assert_eq!(
            SafetyLevel::AuthAll.requirement(true),
            Requirement::Authenticate
        );
        assert_eq!(SafetyLevel::AuthWrites.requirement(true), Requirement::None);
        assert_eq!(
            SafetyLevel::AuthWrites.requirement(false),
            Requirement::Authenticate
        );
    }

    #[test]
    fn the_legacy_boolean_maps_onto_alert_2_and_silent() {
        assert_eq!(
            SafetyLevel::from_confirm_writes(true),
            SafetyLevel::WarnWrites
        );
        assert_eq!(SafetyLevel::from_confirm_writes(false), SafetyLevel::Silent);
        assert!(SafetyLevel::WarnWrites.confirms_writes());
        assert!(SafetyLevel::AuthAll.confirms_writes());
        assert!(!SafetyLevel::Silent.confirms_writes());
    }

    #[test]
    fn an_acknowledgement_never_clears_an_authenticate_rung() {
        let ack = Attestation::Acknowledged;
        assert!(ack.satisfies(Requirement::Warn, "prod"));
        assert!(!ack.satisfies(Requirement::Authenticate, "prod"));

        let typed = Attestation::TypedPhrase {
            typed: "prod".to_string(),
        };
        assert!(typed.satisfies(Requirement::Authenticate, "prod"));
        assert!(!typed.satisfies(Requirement::Authenticate, "staging"));

        let system = Attestation::SystemAuth {
            method: "touch_id".to_string(),
        };
        assert!(system.satisfies(Requirement::Authenticate, "prod"));
        assert!(!Attestation::SystemAuth {
            method: "  ".to_string()
        }
        .satisfies(Requirement::Authenticate, "prod"));
    }

    #[test]
    fn every_level_round_trips_through_its_wire_name() {
        for level in SafetyLevel::ALL {
            assert_eq!(SafetyLevel::parse(level.as_str()), Some(level));
        }
        assert_eq!(SafetyLevel::parse("safe_2"), None);
    }
}
