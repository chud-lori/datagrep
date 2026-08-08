//! [`SqliteDriver`]: the stateless entry point (design §3.1 — "Stateless:
//! all per-server state lives in the Connections it creates").
//!
//! ## datagrep-api gaps this driver works around (do not fix by editing datagrep-api)
//!
//! 1. **No `NESTED_TRANSACTIONS`, `MULTI_STATEMENT`, `POSITIONAL_PARAMS`, or
//!    `EXPRESSION_FILTER` bits in [`Caps`].** All four are genuinely true of
//!    this driver (savepoints via plain SQL — see `transaction.rs`; `?`
//!    positional params; `Predicate` compiles to a real `WHERE`), but
//!    `datagrep_api::Caps` only defines the flags listed below. Reported upward
//!    rather than silently invented.
//! 2. **(Resolved.)** `Mutation::Update`/`Delete` now carry their row
//!    identity as named `(FieldPath, Value)` pairs, so `connection.rs`
//!    compiles the WHERE clause directly from the mutation — the old
//!    positional `PRAGMA table_info` primary-key convention is gone.
//! 3. **`Catalog::describe` takes no `ListOpts`**, so there is no per-call
//!    signal for "include the expensive row count." Resolved conservatively:
//!    `describe()` never runs `COUNT(*)` at all (see `catalog.rs`); a caller
//!    that wants an exact count uses `Op::Count`.
//! 4. **`Op::Scan` has no `offset` field**, only `resume`/keyset — so
//!    `Caps::RANDOM_ACCESS_PAGE` (true; SQLite genuinely supports `OFFSET`)
//!    is not currently reachable through the structured `Op` surface, only
//!    via `Request::Native` text.

use std::collections::BTreeMap;
use std::sync::Arc;

use async_trait::async_trait;
use datagrep_api::{
    Capabilities, Caps, ConfigError, ConfigField, ConfigSchema, ConfigValue, ConnectCtx,
    Connection, ConnectionConfig, DbError, Driver, DriverMeta, FieldKind, LanguageId, ParamStyle,
    ResolvedConfig, SqlDialect,
};

use crate::connection::SqliteConnection;

/// The SQLite driver. Zero fields: every stateful thing it produces
/// ([`SqliteConnection`]) owns its own worker thread and rusqlite handle.
#[derive(Debug, Default, Clone, Copy)]
pub struct SqliteDriver;

impl SqliteDriver {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl Driver for SqliteDriver {
    fn meta(&self) -> DriverMeta {
        DriverMeta {
            id: Arc::from("sqlite"),
            display_name: Arc::from("SQLite"),
            version: Arc::from(env!("CARGO_PKG_VERSION")),
        }
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            flags: Caps::TRANSACTIONS
                | Caps::DDL
                | Caps::EXPLAIN
                | Caps::EDITABLE_RESULTS
                | Caps::SERVER_CANCEL
                | Caps::EXACT_COUNT_CHEAP
                | Caps::RANDOM_ACCESS_PAGE
                | Caps::SCHEMA_DECLARED
                | Caps::READ_ONLY_SESSION,
            max_statement_bytes: None,
            default_fetch_rows: 2000,
            param_style: ParamStyle::QuestionMark,
            language: LanguageId::Sql(SqlDialect::Sqlite),
            identifier_quote: '"',
            catalog_levels: 3, // database (main/attached) -> table|view -> column
        }
    }

    fn config_schema(&self) -> ConfigSchema {
        ConfigSchema {
            fields: vec![
                ConfigField {
                    key: Arc::from("path"),
                    label: Arc::from("Database file"),
                    kind: FieldKind::Path,
                    required: true,
                    default: None,
                    secret: false,
                },
                ConfigField {
                    key: Arc::from("read_only"),
                    label: Arc::from("Read-only"),
                    kind: FieldKind::Bool,
                    required: false,
                    default: Some(ConfigValue::Bool(false)),
                    secret: false,
                },
            ],
        }
    }

    /// Accepts `sqlite:///path/to.db` (`path` = everything after the third
    /// slash) and the literal `:memory:`.
    fn parse_url(&self, url: &str) -> Result<ConnectionConfig, ConfigError> {
        let path = if url == ":memory:" {
            ":memory:".to_string()
        } else if let Some(rest) = url.strip_prefix("sqlite://") {
            if rest.is_empty() {
                return Err(ConfigError::InvalidUrl {
                    reason: "missing path after `sqlite://`".to_string(),
                });
            }
            rest.to_string()
        } else {
            return Err(ConfigError::InvalidUrl {
                reason: format!(
                    "unrecognized SQLite url: {url:?} (expected `sqlite:///path` or `:memory:`)"
                ),
            });
        };
        let mut values = BTreeMap::new();
        values.insert("path".to_string(), ConfigValue::Str(path));
        Ok(ConnectionConfig {
            driver: Arc::from("sqlite"),
            values,
        })
    }

    async fn connect(
        &self,
        cfg: &ResolvedConfig,
        ctx: ConnectCtx,
    ) -> Result<Box<dyn Connection>, DbError> {
        let path = extract_path(&cfg.config)?;
        let read_only = extract_read_only(&cfg.config);
        let conn = SqliteConnection::open(path, read_only, &ctx, self.capabilities()).await?;
        Ok(Box::new(conn))
    }
}

fn extract_path(cfg: &ConnectionConfig) -> Result<String, DbError> {
    match cfg.values.get("path") {
        Some(ConfigValue::Str(s)) if !s.is_empty() => Ok(s.clone()),
        Some(_) => Err(DbError::Config(ConfigError::InvalidValue {
            key: "path".to_string(),
            reason: "expected a non-empty string".to_string(),
        })),
        None => Err(DbError::Config(ConfigError::MissingField {
            key: "path".to_string(),
        })),
    }
}

fn extract_read_only(cfg: &ConnectionConfig) -> bool {
    matches!(cfg.values.get("read_only"), Some(ConfigValue::Bool(true)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn meta_id_is_sqlite() {
        assert_eq!(&*SqliteDriver.meta().id, "sqlite");
    }

    #[test]
    fn capabilities_match_the_design() {
        let caps = SqliteDriver.capabilities();
        assert!(caps.flags.contains(Caps::TRANSACTIONS));
        assert!(caps.flags.contains(Caps::DDL));
        assert!(caps.flags.contains(Caps::EXPLAIN));
        assert!(caps.flags.contains(Caps::EXACT_COUNT_CHEAP));
        assert!(caps.flags.contains(Caps::SERVER_CANCEL));
        assert!(caps.flags.contains(Caps::SCHEMA_DECLARED));
        assert_eq!(caps.default_fetch_rows, 2000);
        assert_eq!(caps.param_style, ParamStyle::QuestionMark);
        assert_eq!(caps.identifier_quote, '"');
        assert_eq!(caps.catalog_levels, 3);
        assert_eq!(caps.language, LanguageId::Sql(SqlDialect::Sqlite));
    }

    #[test]
    fn parse_url_memory() {
        let cfg = SqliteDriver.parse_url(":memory:").unwrap();
        assert_eq!(
            cfg.values.get("path"),
            Some(&ConfigValue::Str(":memory:".to_string()))
        );
    }

    #[test]
    fn parse_url_file() {
        let cfg = SqliteDriver.parse_url("sqlite:///path/to.db").unwrap();
        assert_eq!(
            cfg.values.get("path"),
            Some(&ConfigValue::Str("/path/to.db".to_string()))
        );
    }

    #[test]
    fn parse_url_rejects_garbage() {
        assert!(SqliteDriver.parse_url("postgres://localhost/db").is_err());
        assert!(SqliteDriver.parse_url("sqlite://").is_err());
    }

    #[test]
    fn extract_path_requires_the_field() {
        let cfg = ConnectionConfig {
            driver: Arc::from("sqlite"),
            values: BTreeMap::new(),
        };
        assert!(matches!(
            extract_path(&cfg),
            Err(DbError::Config(ConfigError::MissingField { .. }))
        ));
    }
}
