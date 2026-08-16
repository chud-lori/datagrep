//! [`RedisDriver`]: the `datagrep-api` `Driver` impl.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;

use datagrep_api::caps::{Capabilities, Caps, LanguageId, ParamStyle};
use datagrep_api::config::{
    ConfigError, ConfigField, ConfigSchema, ConfigValue, ConnectionConfig, FieldKind,
    ResolvedConfig,
};
use datagrep_api::driver::{ConnectCtx, Connection, Driver, DriverMeta, ServerInfo};
use datagrep_api::error::DbError;
use redis::IntoConnectionInfo;

use crate::catalog::key_enumeration_from_dbsize;
use crate::connection::RedisConnection;
use crate::error::map_redis_error;

/// Flags this driver's connections report, decided up front and explained
/// per-flag. Callers branch on capabilities, never on the driver's id.
///
/// **Deliberately absent, with reasons** (see the crate-level report for the
/// full write-up):
/// - `TRANSACTIONS` — `MULTI`/`EXEC` is a single optimistic pipeline, not an
///   interactive transaction (no mid-transaction reads of your own writes,
///   no savepoints); `begin()` returns `DbError::Unsupported` so the UI
///   greys the button rather than offering a control that lies.
/// - `DDL` — Redis has no schema to declare.
/// - `SCHEMA_DECLARED` — same reason; the catalog is enumeration-only.
/// - `EXACT_COUNT_CHEAP` — `DBSIZE` is O(1) and exact for the *whole*
///   keyspace, but counting a prefix subset has no O(1) form; see
///   `connection.rs`'s `Op::Count` handling.
/// - `RANDOM_ACCESS_PAGE` — SCAN cursors are the only pagination Redis has.
/// - `READ_ONLY_SESSION` — Redis has no server-side read-only session mode;
///   `set_read_only` always returns `Enforcement::Client`.
/// - `SERVER_CANCEL` — cannot be expressed as a single static bit: almost
///   every command can only be `ClientAbandon`-cancelled, but a *blocking*
///   command (`BLPOP`, `WAIT`, `XREAD BLOCK`, …) genuinely can be killed
///   server-side via a second connection's `CLIENT KILL ID`. Rather than
///   set a capability flag that's true for some commands and a lie for
///   most, this is left unset and `RedisCanceller::kind()` reports the
///   truth dynamically, per cancel, which is exactly what `CancelOutcome`
///   exists for. Flagged as a `datagrep-api` capability-model gap in the
///   crate report.
///
/// `KEY_ENUMERATION` is intentionally *not* included in this constant: it
/// depends on a post-handshake `DBSIZE` probe and is added by
/// [`RedisConnection::capabilities`], never here.
/// `ATOMIC_BATCH`: `execute_mutate` sends the whole batch as one
/// `MULTI`/`EXEC` pipeline (`pipe.atomic()`), so a `MutationBatch` really is
/// all-or-nothing — distinct from `TRANSACTIONS`, which stays off because
/// Redis has no interactive `begin`.
pub const REDIS_CAPS: Caps = Caps::EDITABLE_RESULTS
    .union(Caps::EXPRESSION_FILTER)
    .union(Caps::ATOMIC_BATCH);

/// Baseline (pre-handshake) capabilities. `KEY_ENUMERATION` is optimistically
/// set here — before connecting there is no `DBSIZE` to probe — and is the
/// one flag `RedisConnection::capabilities` may turn back off.
pub fn redis_capabilities_baseline() -> Capabilities {
    Capabilities {
        flags: REDIS_CAPS | Caps::KEY_ENUMERATION,
        max_statement_bytes: None,
        default_fetch_rows: 500,
        param_style: ParamStyle::None,
        language: LanguageId::RedisCli,
        identifier_quote: '"', // unused (no identifiers to quote); kept inert
        catalog_levels: 3,     // db-index -> keyspace-prefix (virtual) -> key
    }
}

/// The Redis driver adapter. Stateless — all per-server state
/// lives in the [`RedisConnection`]s it creates.
#[derive(Debug, Default)]
pub struct RedisDriver;

impl RedisDriver {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl Driver for RedisDriver {
    fn meta(&self) -> DriverMeta {
        DriverMeta {
            id: Arc::from("redis"),
            display_name: Arc::from("Redis"),
            version: Arc::from(env!("CARGO_PKG_VERSION")),
        }
    }

    fn capabilities(&self) -> Capabilities {
        redis_capabilities_baseline()
    }

    fn config_schema(&self) -> ConfigSchema {
        ConfigSchema {
            fields: vec![
                ConfigField {
                    key: Arc::from("host"),
                    label: Arc::from("Host"),
                    kind: FieldKind::Text,
                    required: true,
                    default: Some(ConfigValue::Str("localhost".into())),
                    secret: false,
                },
                ConfigField {
                    key: Arc::from("port"),
                    label: Arc::from("Port"),
                    kind: FieldKind::Number,
                    required: true,
                    default: Some(ConfigValue::Num(6379.0)),
                    secret: false,
                },
                ConfigField {
                    key: Arc::from("username"),
                    label: Arc::from("Username"),
                    kind: FieldKind::Text,
                    required: false,
                    default: None,
                    secret: false,
                },
                ConfigField {
                    key: Arc::from("password"),
                    label: Arc::from("Password"),
                    kind: FieldKind::Password,
                    required: false,
                    default: None,
                    secret: true,
                },
                ConfigField {
                    key: Arc::from("db"),
                    label: Arc::from("Database index"),
                    kind: FieldKind::Number,
                    required: true,
                    default: Some(ConfigValue::Num(0.0)),
                    secret: false,
                },
                ConfigField {
                    key: Arc::from("tls"),
                    label: Arc::from("TLS"),
                    kind: FieldKind::Bool,
                    required: true,
                    default: Some(ConfigValue::Bool(false)),
                    secret: false,
                },
            ],
        }
    }

    fn parse_url(&self, url: &str) -> Result<ConnectionConfig, ConfigError> {
        parse_redis_url(url)
    }

    #[tracing::instrument(skip(self, cfg, ctx))]
    async fn connect(
        &self,
        cfg: &ResolvedConfig,
        ctx: ConnectCtx,
    ) -> Result<Box<dyn Connection>, DbError> {
        let host = str_field(cfg, "host")?.unwrap_or_else(|| "localhost".to_string());
        let port = num_field(cfg, "port")?.unwrap_or(6379.0) as u16;
        let db = num_field(cfg, "db")?.unwrap_or(0.0) as i64;
        let tls = bool_field(cfg, "tls")?.unwrap_or(false);
        let username = str_field(cfg, "username")?;
        let password = cfg
            .secrets
            .get("password")
            .map(|s| s.expose().to_string())
            .or_else(|| str_field(cfg, "password").ok().flatten());

        if tls {
            // TLS deferred: the required dependency list (`redis` with only
            // `tokio-comp`/`connection-manager`) carries no TLS backend
            // (`tls-native-tls`/`tls-rustls`). `config_schema`/`parse_url`
            // stay honest about the field existing; silently downgrading a
            // `rediss://` request to plaintext would be a security
            // regression, so this fails fast instead (same call as
            // `datagrep-drv-postgres`'s documented TLS deviation).
            return Err(DbError::Tls(
                "TLS not yet implemented for the Redis driver; use tls=false for now".into(),
            ));
        }

        tracing::info!(%host, port, db, "connecting to redis");

        let redis_info = {
            let mut info = redis::RedisConnectionInfo::default().set_db(db);
            if let Some(u) = &username {
                info = info.set_username(u);
            }
            if let Some(p) = &password {
                info = info.set_password(p);
            }
            // RESP3 unlocks the richer reply types `value.rs` maps
            // (Double, Boolean, Map, BigNumber, …); under RESP2 everything
            // downgrades to bulk strings/arrays and the mapping in
            // `value.rs` never actually exercises those arms against a real
            // server.
            info.set_protocol(redis::ProtocolVersion::RESP3)
        };
        let conn_info = redis::ConnectionAddr::Tcp(host.clone(), port)
            .into_connection_info()
            .map_err(map_redis_error)?
            .set_redis_settings(redis_info);
        let client = redis::Client::open(conn_info).map_err(map_redis_error)?;

        let timeout = ctx.connect_timeout.unwrap_or(Duration::from_secs(15));
        let manager = tokio::time::timeout(timeout, client.get_connection_manager())
            .await
            .map_err(|_| DbError::Timeout)?
            .map_err(map_redis_error)?;

        let mut probe = manager.clone();
        let dbsize: i64 = redis::cmd("DBSIZE")
            .query_async(&mut probe)
            .await
            .map_err(map_redis_error)?;
        let key_enumeration = key_enumeration_from_dbsize(dbsize);

        let server_info = ServerInfo {
            product: Arc::from("Redis"),
            version: Arc::from("unknown"),
            details: vec![
                (Arc::from("db"), Arc::from(db.to_string())),
                (
                    Arc::from("dbsize_at_connect"),
                    Arc::from(dbsize.to_string()),
                ),
            ],
        };

        Ok(Box::new(RedisConnection::new(
            manager,
            client,
            server_info,
            key_enumeration,
        )))
    }
}

fn str_field(cfg: &ResolvedConfig, key: &str) -> Result<Option<String>, DbError> {
    match cfg.config.values.get(key) {
        None => Ok(None),
        Some(ConfigValue::Str(s)) => Ok(Some(s.clone())),
        Some(other) => Err(DbError::Config(ConfigError::InvalidValue {
            key: key.into(),
            reason: format!("expected a string, got {other:?}"),
        })),
    }
}

fn num_field(cfg: &ResolvedConfig, key: &str) -> Result<Option<f64>, DbError> {
    match cfg.config.values.get(key) {
        None => Ok(None),
        Some(ConfigValue::Num(n)) => Ok(Some(*n)),
        Some(other) => Err(DbError::Config(ConfigError::InvalidValue {
            key: key.into(),
            reason: format!("expected a number, got {other:?}"),
        })),
    }
}

fn bool_field(cfg: &ResolvedConfig, key: &str) -> Result<Option<bool>, DbError> {
    match cfg.config.values.get(key) {
        None => Ok(None),
        Some(ConfigValue::Bool(b)) => Ok(Some(*b)),
        Some(other) => Err(DbError::Config(ConfigError::InvalidValue {
            key: key.into(),
            reason: format!("expected a bool, got {other:?}"),
        })),
    }
}

/// Hand-rolled `redis://`/`rediss://` splitter.
///
/// Grammar, matching the `redis` crate's own documented format
/// (`redis-1.5.0/src/connection.rs`, `IntoConnectionInfo for &str`):
/// `{redis|rediss}://[username][:password]@]host[:port][/db]`. Deliberately
/// does not depend on the `redis` crate's own URL parser: that parser
/// builds a private `ConnectionInfo` whose fields (`addr`, `redis`) have no
/// public constructor from parts other than round-tripping through another
/// URL string — reconstructing one from `ConnectionConfig`'s discrete
/// fields, only to pass it back through `connect`, would be a pointless
/// second serialization. Query strings and fragments (`?protocol=resp3`,
/// `#insecure`) are accepted but ignored — this driver decides RESP3 and
/// TLS posture itself (`connect`), never takes them from a pasted URL.
fn parse_redis_url(url: &str) -> Result<ConnectionConfig, ConfigError> {
    let (scheme, rest) = url
        .split_once("://")
        .ok_or_else(|| ConfigError::InvalidUrl {
            reason: "missing scheme (expected redis:// or rediss://)".into(),
        })?;
    let tls = match scheme {
        "redis" => false,
        "rediss" => true,
        other => {
            return Err(ConfigError::InvalidUrl {
                reason: format!("unsupported scheme {other:?}, expected redis:// or rediss://"),
            })
        }
    };

    let rest = rest.split(['?', '#']).next().unwrap_or(rest);
    let (authority, path) = match rest.split_once('/') {
        Some((a, p)) => (a, Some(p)),
        None => (rest, None),
    };
    let (userinfo, hostport) = match authority.rsplit_once('@') {
        Some((u, h)) => (Some(u), h),
        None => (None, authority),
    };
    if hostport.is_empty() {
        return Err(ConfigError::InvalidUrl {
            reason: "missing host".into(),
        });
    }
    let (host, port) = match hostport.rsplit_once(':') {
        Some((h, p)) if !h.is_empty() => {
            let port: f64 = p.parse().map_err(|_| ConfigError::InvalidUrl {
                reason: format!("invalid port {p:?}"),
            })?;
            (h.to_string(), Some(port))
        }
        _ => (hostport.to_string(), None),
    };

    let mut values = BTreeMap::new();
    values.insert("host".to_string(), ConfigValue::Str(host));
    values.insert("port".to_string(), ConfigValue::Num(port.unwrap_or(6379.0)));
    values.insert("tls".to_string(), ConfigValue::Bool(tls));

    if let Some(userinfo) = userinfo {
        let (user, pass) = match userinfo.split_once(':') {
            Some((u, p)) => (u, Some(p)),
            None => (userinfo, None),
        };
        if !user.is_empty() {
            values.insert("username".to_string(), ConfigValue::Str(user.to_string()));
        }
        if let Some(pass) = pass {
            values.insert("password".to_string(), ConfigValue::Str(pass.to_string()));
        }
    }

    if let Some(path) = path {
        let db_str = path.trim_end_matches('/');
        if !db_str.is_empty() {
            let db: f64 = db_str.parse().map_err(|_| ConfigError::InvalidUrl {
                reason: format!("invalid database index {db_str:?}"),
            })?;
            values.insert("db".to_string(), ConfigValue::Num(db));
        }
    }
    values
        .entry("db".to_string())
        .or_insert(ConfigValue::Num(0.0));

    Ok(ConnectionConfig {
        driver: Arc::from("redis"),
        values,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_url_extracts_host_port_db() {
        let cfg = parse_redis_url("redis://cache.example.com:6380/3").unwrap();
        assert_eq!(&*cfg.driver, "redis");
        assert_eq!(
            cfg.values.get("host"),
            Some(&ConfigValue::Str("cache.example.com".into()))
        );
        assert_eq!(cfg.values.get("port"), Some(&ConfigValue::Num(6380.0)));
        assert_eq!(cfg.values.get("db"), Some(&ConfigValue::Num(3.0)));
        assert_eq!(cfg.values.get("tls"), Some(&ConfigValue::Bool(false)));
    }

    #[test]
    fn parse_url_rediss_sets_tls() {
        let cfg = parse_redis_url("rediss://localhost:6379").unwrap();
        assert_eq!(cfg.values.get("tls"), Some(&ConfigValue::Bool(true)));
    }

    #[test]
    fn parse_url_extracts_username_and_password() {
        let cfg = parse_redis_url("redis://alice:hunter2@localhost:6379/0").unwrap();
        assert_eq!(
            cfg.values.get("username"),
            Some(&ConfigValue::Str("alice".into()))
        );
        assert_eq!(
            cfg.values.get("password"),
            Some(&ConfigValue::Str("hunter2".into()))
        );
    }

    #[test]
    fn parse_url_password_only_form() {
        let cfg = parse_redis_url("redis://:secret@localhost").unwrap();
        assert_eq!(cfg.values.get("username"), None);
        assert_eq!(
            cfg.values.get("password"),
            Some(&ConfigValue::Str("secret".into()))
        );
        // No explicit db in the URL: defaults to 0 rather than leaving the
        // field unset (config_schema declares `db` required).
        assert_eq!(cfg.values.get("db"), Some(&ConfigValue::Num(0.0)));
    }

    #[test]
    fn parse_url_rejects_bad_scheme_and_missing_host() {
        assert!(parse_redis_url("http://localhost").is_err());
        assert!(parse_redis_url("redis://").is_err());
        assert!(parse_redis_url("not a url").is_err());
    }

    #[test]
    fn parse_url_ignores_query_string() {
        let cfg = parse_redis_url("redis://localhost:6379/0?protocol=resp3").unwrap();
        assert_eq!(cfg.values.get("port"), Some(&ConfigValue::Num(6379.0)));
    }

    #[test]
    fn baseline_capabilities_match_the_ticket() {
        let driver = RedisDriver::new();
        let caps = driver.capabilities();
        assert!(caps.flags.contains(Caps::EDITABLE_RESULTS));
        assert!(caps.flags.contains(Caps::EXPRESSION_FILTER));
        assert!(!caps.flags.contains(Caps::TRANSACTIONS));
        assert!(!caps.flags.contains(Caps::DDL));
        assert!(!caps.flags.contains(Caps::SCHEMA_DECLARED));
        assert!(!caps.flags.contains(Caps::EXACT_COUNT_CHEAP));
        assert!(!caps.flags.contains(Caps::RANDOM_ACCESS_PAGE));
        assert!(!caps.flags.contains(Caps::READ_ONLY_SESSION));
        assert_eq!(caps.param_style, ParamStyle::None);
        assert_eq!(caps.language, LanguageId::RedisCli);
        assert_eq!(caps.default_fetch_rows, 500);
        assert_eq!(caps.catalog_levels, 3);
    }

    #[test]
    fn config_schema_flags_password_as_secret() {
        let driver = RedisDriver::new();
        let schema = driver.config_schema();
        let pw = schema
            .fields
            .iter()
            .find(|f| &*f.key == "password")
            .unwrap();
        assert!(pw.secret);
        let tls = schema.fields.iter().find(|f| &*f.key == "tls").unwrap();
        assert_eq!(tls.kind, FieldKind::Bool);
    }
}
