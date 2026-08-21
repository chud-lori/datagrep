use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use mysql_async::prelude::Queryable;
use mysql_async::{Conn, Opts, OptsBuilder, Pool, PoolConstraints, PoolOpts};

use datagrep_api::{
    caps::{Capabilities, Caps, LanguageId, ParamStyle, SqlDialect},
    config::{
        ConfigError, ConfigField, ConfigSchema, ConfigValue, ConnectionConfig, FieldKind,
        ResolvedConfig,
    },
    driver::{ConnectCtx, Connection, Driver, DriverMeta, ServerInfo},
    error::DbError,
};

use crate::connection::MySqlConnection;
use crate::error::map_mysql_error;
use crate::sql::Flavor;

pub const MYSQL_BASE_CAPS: Caps = Caps::TRANSACTIONS
    .union(Caps::NESTED_TRANSACTIONS)
    .union(Caps::DDL)
    .union(Caps::EXPLAIN)
    .union(Caps::EDITABLE_RESULTS)
    .union(Caps::SERVER_CANCEL)
    .union(Caps::EXACT_COUNT_CHEAP)
    .union(Caps::RANDOM_ACCESS_PAGE)
    .union(Caps::SCHEMA_DECLARED)
    .union(Caps::KEY_ENUMERATION)
    .union(Caps::READ_ONLY_SESSION)
    .union(Caps::MULTI_STATEMENT)
    .union(Caps::POSITIONAL_PARAMS)
    .union(Caps::EXPORT_STREAMING)
    .union(Caps::EXPRESSION_FILTER)
    .union(Caps::ATOMIC_BATCH);

pub fn supports_explain_analyze(flavor: Flavor, version: (u16, u16, u16)) -> bool {
    match flavor {
        Flavor::MySql => version >= (8, 0, 18),
        Flavor::MariaDb => version >= (10, 1, 0),
    }
}

pub fn mysql_capabilities(flavor: Flavor, version: (u16, u16, u16)) -> Capabilities {
    let mut flags = MYSQL_BASE_CAPS;
    if supports_explain_analyze(flavor, version) {
        flags |= Caps::EXPLAIN_ANALYZE;
    }
    Capabilities {
        flags,
        max_statement_bytes: None, // max_allowed_packet varies per server config
        default_fetch_rows: 500,
        param_style: ParamStyle::QuestionMark,
        language: LanguageId::Sql(SqlDialect::Mysql),
        identifier_quote: '`',
        catalog_levels: 3,
    }
}

pub fn parse_server_version(version: &str) -> (Flavor, (u16, u16, u16)) {
    let flavor = if version.to_ascii_lowercase().contains("mariadb") {
        Flavor::MariaDb
    } else {
        Flavor::MySql
    };
    let body = version.strip_prefix("5.5.5-").unwrap_or(version);
    let mut nums = [0u16; 3];
    let mut idx = 0;
    let mut cur: Option<u32> = None;
    for c in body.chars() {
        if let Some(d) = c.to_digit(10) {
            cur = Some(cur.unwrap_or(0) * 10 + d);
        } else if c == '.' && cur.is_some() && idx < 2 {
            nums[idx] = cur.take().unwrap_or(0).min(u32::from(u16::MAX)) as u16;
            idx += 1;
        } else {
            break;
        }
    }
    if let Some(v) = cur {
        nums[idx] = v.min(u32::from(u16::MAX)) as u16;
    }
    (flavor, (nums[0], nums[1], nums[2]))
}

#[derive(Debug, Default)]
pub struct MySqlDriver;

impl MySqlDriver {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl Driver for MySqlDriver {
    fn meta(&self) -> DriverMeta {
        DriverMeta {
            id: Arc::from("mysql"),
            display_name: Arc::from("MySQL / MariaDB"),
            version: Arc::from(env!("CARGO_PKG_VERSION")),
        }
    }

    fn capabilities(&self) -> Capabilities {
        mysql_capabilities(Flavor::MySql, (8, 0, 18))
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
                    default: Some(ConfigValue::Num(3306.0)),
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
                    required: false, // MySQL allows connecting with no default database
                    default: None,
                    secret: false,
                },
            ],
        }
    }

    fn parse_url(&self, url: &str) -> Result<ConnectionConfig, ConfigError> {
        let normalized = url
            .strip_prefix("mariadb://")
            .map(|rest| format!("mysql://{rest}"))
            .unwrap_or_else(|| url.to_string());
        let opts = Opts::from_url(&normalized).map_err(|e| ConfigError::InvalidUrl {
            reason: e.to_string(),
        })?;

        let mut values = BTreeMap::new();
        values.insert(
            "host".to_string(),
            ConfigValue::Str(opts.ip_or_hostname().to_string()),
        );
        values.insert(
            "port".to_string(),
            ConfigValue::Num(f64::from(opts.tcp_port())),
        );
        if let Some(user) = opts.user() {
            values.insert("user".to_string(), ConfigValue::Str(user.to_string()));
        }
        if let Some(pass) = opts.pass() {
            values.insert("password".to_string(), ConfigValue::Str(pass.to_string()));
        }
        if let Some(db) = opts.db_name() {
            values.insert("database".to_string(), ConfigValue::Str(db.to_string()));
        }

        Ok(ConnectionConfig {
            driver: Arc::from("mysql"),
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
        let port = num_field(cfg, "port")?.unwrap_or(3306.0) as u16;
        tracing::info!(%host, port, "connecting to mysql/mariadb");
        let user = str_field(cfg, "user")?
            .ok_or_else(|| DbError::Config(ConfigError::MissingField { key: "user".into() }))?;
        let database = str_field(cfg, "database")?;
        let password = cfg
            .secrets
            .get("password")
            .map(|s| s.expose().to_string())
            .or_else(|| str_field(cfg, "password").ok().flatten());

        let builder = OptsBuilder::default()
            .ip_or_hostname(host)
            .tcp_port(port)
            .user(Some(user))
            .pass(password)
            .db_name(database)
            .prefer_socket(false)
            .setup(vec!["SET time_zone = '+00:00'".to_string()]);
        let opts = Opts::from(builder);

        let timeout = ctx.connect_timeout.unwrap_or(Duration::from_secs(15));
        let mut conn = tokio::time::timeout(timeout, Conn::new(opts.clone()))
            .await
            .map_err(|_| DbError::Timeout)?
            .map_err(|e| match map_mysql_error(e) {
                e @ (DbError::Auth(_) | DbError::Config(_)) => e,
                other => DbError::Connect(other.to_string()),
            })?;

        if ctx.cancel.is_cancelled() {
            let _ = conn.disconnect().await;
            return Err(DbError::Cancelled);
        }

        let (version, version_comment): (String, String) = conn
            .query_first("SELECT @@version, @@version_comment")
            .await
            .map_err(map_mysql_error)?
            .unwrap_or_default();
        let (flavor, parsed_version) = parse_server_version(&version);
        let conn_id = conn.id();

        let server_info = ServerInfo {
            product: Arc::from(match flavor {
                Flavor::MySql => "MySQL",
                Flavor::MariaDb => "MariaDB",
            }),
            version: Arc::from(version.as_str()),
            details: vec![
                (Arc::from("version_comment"), Arc::from(version_comment)),
                (Arc::from("connection_id"), Arc::from(conn_id.to_string())),
            ],
        };
        let caps = mysql_capabilities(flavor, parsed_version);

        let kill_opts =
            OptsBuilder::from_opts(opts).pool_opts(PoolOpts::default().with_constraints(
                PoolConstraints::new(0, 2).expect("0 <= 2 is a valid pool constraint"),
            ));
        let kill_pool = Pool::new(kill_opts);

        tracing::info!(
            product = %server_info.product,
            version = %server_info.version,
            conn_id,
            "connected"
        );
        Ok(Box::new(MySqlConnection::new(
            conn,
            server_info,
            caps,
            flavor,
            kill_pool,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_url_extracts_fields() {
        let driver = MySqlDriver::new();
        let cfg = driver
            .parse_url("mysql://alice@db.example.com:3307/appdb")
            .unwrap();
        assert_eq!(&*cfg.driver, "mysql");
        assert_eq!(
            cfg.values.get("user"),
            Some(&ConfigValue::Str("alice".into()))
        );
        assert_eq!(
            cfg.values.get("host"),
            Some(&ConfigValue::Str("db.example.com".into()))
        );
        assert_eq!(cfg.values.get("port"), Some(&ConfigValue::Num(3307.0)));
        assert_eq!(
            cfg.values.get("database"),
            Some(&ConfigValue::Str("appdb".into()))
        );
        assert_eq!(cfg.values.get("password"), None);
    }

    #[test]
    fn parse_url_with_password_and_default_port() {
        let driver = MySqlDriver::new();
        let cfg = driver
            .parse_url("mysql://bob:hunter2@localhost/db")
            .unwrap();
        assert_eq!(
            cfg.values.get("password"),
            Some(&ConfigValue::Str("hunter2".into()))
        );
        assert_eq!(cfg.values.get("port"), Some(&ConfigValue::Num(3306.0)));
    }

    #[test]
    fn parse_url_accepts_mariadb_scheme() {
        let driver = MySqlDriver::new();
        let cfg = driver
            .parse_url("mariadb://u@maria.example.com:3306/db")
            .unwrap();
        assert_eq!(&*cfg.driver, "mysql");
        assert_eq!(
            cfg.values.get("host"),
            Some(&ConfigValue::Str("maria.example.com".into()))
        );
    }

    #[test]
    fn parse_url_rejects_garbage() {
        let driver = MySqlDriver::new();
        assert!(driver.parse_url("not a url \0").is_err());
        assert!(driver.parse_url("postgres://u@h/db").is_err());
    }

    #[test]
    fn version_parse_detects_flavor_and_numbers() {
        assert_eq!(parse_server_version("8.0.36"), (Flavor::MySql, (8, 0, 36)));
        assert_eq!(
            parse_server_version("8.4.0-commercial"),
            (Flavor::MySql, (8, 4, 0))
        );
        assert_eq!(
            parse_server_version("10.11.6-MariaDB-1:10.11.6+maria~ubu2204"),
            (Flavor::MariaDb, (10, 11, 6))
        );
        // Replication-compat prefix some MariaDB builds report.
        assert_eq!(
            parse_server_version("5.5.5-10.11.6-MariaDB"),
            (Flavor::MariaDb, (10, 11, 6))
        );
        assert_eq!(parse_server_version(""), (Flavor::MySql, (0, 0, 0)));
    }

    #[test]
    fn explain_analyze_gating_is_honest() {
        assert!(supports_explain_analyze(Flavor::MySql, (8, 0, 18)));
        assert!(supports_explain_analyze(Flavor::MySql, (8, 4, 0)));
        assert!(!supports_explain_analyze(Flavor::MySql, (8, 0, 17)));
        assert!(!supports_explain_analyze(Flavor::MySql, (5, 7, 44)));
        assert!(supports_explain_analyze(Flavor::MariaDb, (10, 11, 6)));
        assert!(!supports_explain_analyze(Flavor::MariaDb, (10, 0, 38)));

        let caps = mysql_capabilities(Flavor::MySql, (8, 0, 17));
        assert!(!caps.flags.contains(Caps::EXPLAIN_ANALYZE));
        assert!(caps.flags.contains(Caps::EXPLAIN));
        let caps = mysql_capabilities(Flavor::MySql, (8, 0, 18));
        assert!(caps.flags.contains(Caps::EXPLAIN_ANALYZE));
    }

    #[test]
    fn capabilities_match_ticket() {
        let caps = MySqlDriver::new().capabilities();
        for flag in [
            Caps::TRANSACTIONS,
            Caps::NESTED_TRANSACTIONS,
            Caps::DDL,
            Caps::EXPLAIN,
            Caps::EXPLAIN_ANALYZE,
            Caps::SERVER_CANCEL,
            Caps::MULTI_STATEMENT,
            Caps::POSITIONAL_PARAMS,
            Caps::EXACT_COUNT_CHEAP,
            Caps::RANDOM_ACCESS_PAGE,
            Caps::EXPORT_STREAMING,
            Caps::EXPRESSION_FILTER,
            Caps::SCHEMA_DECLARED,
            Caps::KEY_ENUMERATION,
            Caps::READ_ONLY_SESSION,
            Caps::EDITABLE_RESULTS,
        ] {
            assert!(caps.flags.contains(flag), "missing {flag:?}");
        }
        assert!(!caps.flags.contains(Caps::NAMED_PARAMS));
        assert_eq!(caps.param_style, ParamStyle::QuestionMark);
        assert_eq!(caps.language, LanguageId::Sql(SqlDialect::Mysql));
        assert_eq!(caps.default_fetch_rows, 500);
        assert_eq!(caps.identifier_quote, '`');
        assert_eq!(caps.catalog_levels, 3);
    }

    #[test]
    fn config_schema_flags_password_as_secret() {
        let schema = MySqlDriver::new().config_schema();
        let pw = schema
            .fields
            .iter()
            .find(|f| &*f.key == "password")
            .unwrap();
        assert!(pw.secret);
        assert!(matches!(pw.kind, FieldKind::Password));
        let db = schema
            .fields
            .iter()
            .find(|f| &*f.key == "database")
            .unwrap();
        assert!(!db.required, "MySQL connects fine with no default database");
    }
}
