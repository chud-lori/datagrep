//! [`PostgresDriver`]: the `Driver` impl.

use std::str::FromStr as _;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use tokio_postgres::config::{Host, SslMode};

use datagrep_api::{
    caps::{Capabilities, Caps, LanguageId, ParamStyle, SqlDialect},
    config::{
        ConfigError, ConfigField, ConfigSchema, ConfigValue, ConnectionConfig, FieldKind,
        ResolvedConfig,
    },
    driver::{ConnectCtx, Connection, Driver, DriverMeta, ServerInfo},
    error::DbError,
};

use crate::connection::PgConnection;
use crate::error::TlsMode;
use crate::pool::PgPool;

/// Capability flags this driver's baseline (pre-handshake) [`Capabilities`]
/// reports. Note: six capabilities Postgres genuinely has
/// (`NESTED_TRANSACTIONS`, `EXPLAIN_ANALYZE`, `MULTI_STATEMENT`,
/// `POSITIONAL_PARAMS`, `EXPORT_STREAMING`, `EXPRESSION_FILTER`) have no bit
/// on `datagrep_api::Caps` (`crates/datagrep-api/src/caps.rs` defines only the
/// ten below). Since `datagrep-api` is the frozen seam this driver must not
/// modify, we set every flag that *does* exist and applies to Postgres, and
/// leave the gap documented rather than inventing bits that wouldn't compile
/// against the real `Caps` type.
pub const PG_CAPS: Caps = Caps::TRANSACTIONS
    .union(Caps::DDL)
    .union(Caps::EXPLAIN)
    .union(Caps::EDITABLE_RESULTS)
    .union(Caps::SERVER_CANCEL)
    .union(Caps::EXACT_COUNT_CHEAP)
    .union(Caps::RANDOM_ACCESS_PAGE)
    .union(Caps::SCHEMA_DECLARED)
    .union(Caps::KEY_ENUMERATION)
    .union(Caps::READ_ONLY_SESSION);

/// Baseline capabilities shared by [`Driver::capabilities`] (pre-handshake)
/// and [`PgConnection::capabilities`](crate::connection::PgConnection)
/// (post-handshake) — Postgres has no version-dependent flags worth gating
/// on in v1, so the two are identical, but they're kept as one function to
/// avoid the two literals drifting apart.
pub fn pg_capabilities() -> Capabilities {
    Capabilities {
        flags: PG_CAPS,
        max_statement_bytes: None,
        default_fetch_rows: 500,
        param_style: ParamStyle::DollarNumbered,
        language: LanguageId::Sql(SqlDialect::Postgres),
        identifier_quote: '"',
        catalog_levels: 4, // database -> schema -> table|view|matview -> column
    }
}

/// The Postgres driver adapter. Stateless — all per-server state lives in the
/// [`PgConnection`]s it creates.
#[derive(Debug, Default)]
pub struct PostgresDriver;

impl PostgresDriver {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl Driver for PostgresDriver {
    fn meta(&self) -> DriverMeta {
        DriverMeta {
            id: Arc::from("postgres"),
            display_name: Arc::from("PostgreSQL"),
            version: Arc::from(env!("CARGO_PKG_VERSION")),
        }
    }

    fn capabilities(&self) -> Capabilities {
        pg_capabilities()
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
                    default: Some(ConfigValue::Num(5432.0)),
                    secret: false,
                },
                ConfigField {
                    key: Arc::from("user"),
                    label: Arc::from("User"),
                    kind: FieldKind::Text,
                    required: true,
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
                    key: Arc::from("database"),
                    label: Arc::from("Database"),
                    kind: FieldKind::Text,
                    required: true,
                    default: None,
                    secret: false,
                },
                ConfigField {
                    key: Arc::from("tls"),
                    label: Arc::from("TLS mode"),
                    kind: FieldKind::Select {
                        options: vec![
                            Arc::from("disable"),
                            Arc::from("require"),
                            Arc::from("verify-ca"),
                            Arc::from("verify-full"),
                        ],
                    },
                    required: true,
                    default: Some(ConfigValue::Str("disable".into())),
                    secret: false,
                },
            ],
        }
    }

    fn parse_url(&self, url: &str) -> Result<ConnectionConfig, ConfigError> {
        // Delegates to tokio-postgres's own libpq/URL parser (it accepts both
        // `postgres://user@host:port/db` and `key=value` DSNs) rather than
        // hand-rolling a second parser that could disagree with the one we
        // actually connect with.
        let cfg = tokio_postgres::Config::from_str(url).map_err(|e| ConfigError::InvalidUrl {
            reason: e.to_string(),
        })?;

        let mut values = std::collections::BTreeMap::new();
        if let Some(user) = cfg.get_user() {
            values.insert("user".to_string(), ConfigValue::Str(user.to_string()));
        }
        if let Some(pw) = cfg.get_password() {
            let pw = String::from_utf8_lossy(pw).into_owned();
            values.insert("password".to_string(), ConfigValue::Str(pw));
        }
        if let Some(db) = cfg.get_dbname() {
            values.insert("database".to_string(), ConfigValue::Str(db.to_string()));
        }
        if let Some(Host::Tcp(host)) = cfg.get_hosts().first() {
            values.insert("host".to_string(), ConfigValue::Str(host.clone()));
        }
        if let Some(&port) = cfg.get_ports().first() {
            values.insert("port".to_string(), ConfigValue::Num(port as f64));
        }
        // `sslmode` used to be dropped on the floor here, so
        // `postgres://…?sslmode=require` produced `tls=disable` and then
        // connected in plaintext without a word. That is the exact shape of a
        // silent downgrade: the user stated a requirement and got the opposite,
        // with nothing on screen to say so. Carry the mode through instead —
        // `connect` refuses anything but `disable` while TLS is unimplemented,
        // so a `require` URL now fails loudly rather than succeeding wrongly.
        //
        // `prefer` maps to `disable` and that is not a downgrade: `prefer`
        // states no requirement, and it is what libpq itself does when TLS is
        // unavailable. The catch-all leans the other way — `SslMode` is
        // `#[non_exhaustive]`, and a mode this build has not heard of must fail
        // closed, not open.
        let tls = match cfg.get_ssl_mode() {
            SslMode::Disable | SslMode::Prefer => TlsMode::Disable,
            SslMode::Require => TlsMode::Require,
            _ => TlsMode::Require,
        };
        values.insert(
            "tls".to_string(),
            ConfigValue::Str(tls.as_str().to_string()),
        );

        Ok(ConnectionConfig {
            driver: Arc::from("postgres"),
            values,
        })
    }

    #[tracing::instrument(skip(self, cfg, ctx))]
    async fn connect(
        &self,
        cfg: &ResolvedConfig,
        ctx: ConnectCtx,
    ) -> Result<Box<dyn Connection>, DbError> {
        let host = str_field(cfg, "host")?.unwrap_or_else(|| "localhost".to_string());
        let port = num_field(cfg, "port")?.unwrap_or(5432.0) as u16;
        tracing::info!(%host, port, "connecting to postgres");
        let user = str_field(cfg, "user")?
            .ok_or_else(|| DbError::Config(ConfigError::MissingField { key: "user".into() }))?;
        let database = str_field(cfg, "database")?.ok_or_else(|| {
            DbError::Config(ConfigError::MissingField {
                key: "database".into(),
            })
        })?;
        let password = cfg
            .secrets
            .get("password")
            .map(|s| s.expose().to_string())
            .or_else(|| str_field(cfg, "password").ok().flatten());
        let tls_str = str_field(cfg, "tls")?.unwrap_or_else(|| "disable".to_string());
        let tls_mode = TlsMode::parse(&tls_str).ok_or_else(|| {
            DbError::Config(ConfigError::InvalidValue {
                key: "tls".into(),
                reason: format!("unknown tls mode {tls_str:?}"),
            })
        })?;
        if !matches!(tls_mode, TlsMode::Disable) {
            // TLS is not implemented yet: fail fast and honestly rather than
            // silently connecting in plaintext under a "require" label.
            return Err(DbError::Tls(format!(
                "TLS not yet implemented (mode {tls_str:?}); use tls=disable for now"
            )));
        }

        let mut pg_cfg = tokio_postgres::Config::new();
        pg_cfg.host(host).port(port).user(user).dbname(database);
        if let Some(pw) = password.as_deref() {
            pg_cfg.password(pw);
        }
        if let Some(app) = ctx.application_name.as_deref() {
            pg_cfg.application_name(app);
        }
        let timeout = ctx.connect_timeout.unwrap_or(Duration::from_secs(15));
        pg_cfg.connect_timeout(timeout);

        let connect_fut = pg_cfg.connect(tokio_postgres::NoTls);
        let (client, connection) = tokio::time::timeout(timeout, connect_fut)
            .await
            .map_err(|_| DbError::Timeout)?
            .map_err(|e| DbError::Connect(e.to_string()))?;

        // A driver panic is caught at the task boundary and becomes
        // `DbError::DriverPanic`; here that boundary is this spawned
        // connection-driving task. If it errors (socket drop, protocol
        // violation) there is nothing to report it *to* by this point since
        // `connect` has already returned — matching how tokio-postgres's own
        // examples run the connection future, we trace and drop it.
        tokio::spawn(async move {
            if let Err(e) = connection.await {
                tracing::warn!(error = %e, "postgres connection task ended with an error");
            }
        });

        let server_info = ServerInfo {
            product: Arc::from("PostgreSQL"),
            version: Arc::from("unknown"), // refined below via `server_version()`
            details: Vec::new(),
        };

        // The config is kept alongside the primary session so the connection
        // can dial an *additional* identical session on demand: a cursor or
        // an interactive transaction pins the socket it runs on, and catalog
        // browsing or the next query must not queue behind it (see
        // `pool.rs`). Same host/user/database/application_name, so a pooled
        // session is indistinguishable from the first one.
        let pool = PgPool::with_primary(pg_cfg, timeout, client);
        Ok(Box::new(PgConnection::new(pool, server_info)))
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

#[cfg(test)]
mod tests {
    use super::*;
    use datagrep_api::config::ConnectionConfig;

    #[test]
    fn parse_url_extracts_fields() {
        let driver = PostgresDriver::new();
        let cfg: ConnectionConfig = driver
            .parse_url("postgres://alice@db.example.com:6543/appdb")
            .unwrap();
        assert_eq!(&*cfg.driver, "postgres");
        assert_eq!(
            cfg.values.get("user"),
            Some(&ConfigValue::Str("alice".into()))
        );
        assert_eq!(
            cfg.values.get("host"),
            Some(&ConfigValue::Str("db.example.com".into()))
        );
        assert_eq!(cfg.values.get("port"), Some(&ConfigValue::Num(6543.0)));
        assert_eq!(
            cfg.values.get("database"),
            Some(&ConfigValue::Str("appdb".into()))
        );
    }

    #[test]
    fn parse_url_with_password() {
        let driver = PostgresDriver::new();
        let cfg = driver
            .parse_url("postgres://bob:hunter2@localhost/db")
            .unwrap();
        assert_eq!(
            cfg.values.get("password"),
            Some(&ConfigValue::Str("hunter2".into()))
        );
        // tokio-postgres's `Config` fills in the standard default (5432) even
        // when the URL didn't specify one — same default `config_schema()`
        // advertises, so this is a faithful "unspecified" reading, not a lie.
        assert_eq!(cfg.values.get("port"), Some(&ConfigValue::Num(5432.0)));
    }

    #[test]
    fn parse_url_rejects_garbage() {
        let driver = PostgresDriver::new();
        assert!(driver.parse_url("not a url at all \0").is_err());
    }

    #[test]
    fn capabilities_match_ticket_intersection_with_real_caps() {
        let driver = PostgresDriver::new();
        let caps = driver.capabilities();
        assert!(caps.flags.contains(Caps::TRANSACTIONS));
        assert!(caps.flags.contains(Caps::DDL));
        assert!(caps.flags.contains(Caps::EXPLAIN));
        assert!(caps.flags.contains(Caps::SERVER_CANCEL));
        assert!(caps.flags.contains(Caps::EXACT_COUNT_CHEAP));
        assert!(caps.flags.contains(Caps::RANDOM_ACCESS_PAGE));
        assert!(caps.flags.contains(Caps::SCHEMA_DECLARED));
        assert!(caps.flags.contains(Caps::KEY_ENUMERATION));
        assert!(caps.flags.contains(Caps::READ_ONLY_SESSION));
        assert!(caps.flags.contains(Caps::EDITABLE_RESULTS));
        assert_eq!(caps.param_style, ParamStyle::DollarNumbered);
        assert_eq!(caps.default_fetch_rows, 500);
        assert_eq!(caps.catalog_levels, 4);
    }

    #[test]
    fn config_schema_flags_password_as_secret() {
        let driver = PostgresDriver::new();
        let schema = driver.config_schema();
        let pw = schema
            .fields
            .iter()
            .find(|f| &*f.key == "password")
            .unwrap();
        assert!(pw.secret);
    }
}
