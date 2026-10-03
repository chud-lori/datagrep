use std::sync::Arc;

use datagrep_api::caps::{Capabilities, Caps, LanguageId, ParamStyle};
use datagrep_api::catalog::{Enumeration, LevelDef, ObjectKind};
use datagrep_api::config::{ConfigField, ConfigSchema, ConfigValue, FieldKind};

// What the app, not the sidecar, decides about an engine; the handshake must agree with it.
#[derive(Debug)]
pub struct EngineManifest {
    pub id: &'static str,
    pub display_name: &'static str,
    pub program: &'static str,
    pub url_schemes: &'static [&'static str],
    pub language: LanguageId,
    pub flags: Caps,
    pub param_style: ParamStyle,
    pub identifier_quote: char,
    pub default_port: u16,
    // The config key the URL path fills, e.g. Oracle's service name.
    pub path_key: &'static str,
    pub path_label: &'static str,
    pub levels: &'static [(&'static str, ObjectKind)],
    // Extra variables the child may inherit on top of the shared allowlist.
    pub env: &'static [&'static str],
}

pub static ORACLE: EngineManifest = EngineManifest {
    id: "oracle",
    display_name: "Oracle",
    program: "datagrep-sidecar-oracle",
    url_schemes: &["oracle"],
    // No Oracle classifier exists yet, so every statement is gated as a write.
    language: LanguageId::Unclassified,
    flags: Caps::SERVER_CANCEL
        .union(Caps::SCHEMA_DECLARED)
        .union(Caps::POSITIONAL_PARAMS),
    param_style: ParamStyle::ColonNamed,
    identifier_quote: '"',
    default_port: 1521,
    path_key: "service",
    path_label: "Service name",
    levels: &[
        ("schema", ObjectKind::Schema),
        ("table", ObjectKind::Table),
        ("column", ObjectKind::Column),
    ],
    env: &[],
};

pub static ENGINES: &[&EngineManifest] = &[&ORACLE];

pub fn manifest(id: &str) -> Option<&'static EngineManifest> {
    ENGINES.iter().copied().find(|m| m.id == id)
}

pub fn manifest_for_url(url: &str) -> Option<&'static EngineManifest> {
    let scheme = url.split_once("://")?.0;
    ENGINES
        .iter()
        .copied()
        .find(|m| m.url_schemes.contains(&scheme))
}

impl EngineManifest {
    pub fn capabilities(&self) -> Capabilities {
        Capabilities {
            flags: self.flags,
            max_statement_bytes: None,
            default_fetch_rows: 500,
            param_style: self.param_style,
            language: self.language,
            identifier_quote: self.identifier_quote,
            catalog_levels: self.levels.len() as u8,
        }
    }

    pub fn level_defs(&self) -> Vec<LevelDef> {
        self.levels
            .iter()
            .map(|(name, kind)| LevelDef {
                name: Arc::from(*name),
                kind: *kind,
                enumeration: Enumeration::Cheap,
            })
            .collect()
    }

    pub fn config_schema(&self) -> ConfigSchema {
        let field = |key: &str, label: &str, kind, required, default, secret| ConfigField {
            key: Arc::from(key),
            label: Arc::from(label),
            kind,
            required,
            default,
            secret,
        };
        ConfigSchema {
            fields: vec![
                field(
                    "host",
                    "Host",
                    FieldKind::Text,
                    true,
                    Some(ConfigValue::Str("localhost".into())),
                    false,
                ),
                field(
                    "port",
                    "Port",
                    FieldKind::Number,
                    true,
                    Some(ConfigValue::Num(f64::from(self.default_port))),
                    false,
                ),
                field("user", "User", FieldKind::Text, true, None, false),
                field(
                    "password",
                    "Password",
                    FieldKind::Password,
                    false,
                    None,
                    true,
                ),
                field(
                    self.path_key,
                    self.path_label,
                    FieldKind::Text,
                    true,
                    None,
                    false,
                ),
            ],
        }
    }
}
