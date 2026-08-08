//! [`MongoDriver`]: the `Driver` impl (ticket item 1).

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use bson::doc;

use datagrep_api::caps::{Capabilities, Caps, LanguageId, ParamStyle};
use datagrep_api::config::{
    ConfigError, ConfigField, ConfigSchema, ConfigValue, ConnectionConfig, FieldKind,
    ResolvedConfig,
};
use datagrep_api::driver::{ConnectCtx, Connection, Driver, DriverMeta, ServerInfo};
use datagrep_api::error::DbError;

use crate::connection::MongoConnection;
use crate::error::map_mongo_error;

/// Capability flags every Mongo connection reports, independent of the
/// post-handshake `TRANSACTIONS` bit (ticket item 1's flag list, minus the
/// three explicitly-false ones: `EXACT_COUNT_CHEAP`, `RANDOM_ACCESS_PAGE`,
/// `SCHEMA_DECLARED`).
///
/// `SERVER_CANCEL` here means "this engine has server-side cancel machinery
/// in principle" — the *actual*, honestly-degraded strength of a given
/// cancel is reported per-cancellation by [`crate::canceller::MongoCanceller::kind`]:
/// `maxTimeMS` always goes out, but a true `killOp` needs privileges we may
/// lack, in which case the cancel degrades to `ClientAbandon` and says so.
const BASE_CAPS: Caps = Caps::DDL
    .union(Caps::EXPLAIN)
    .union(Caps::SERVER_CANCEL)
    .union(Caps::EDITABLE_RESULTS)
    .union(Caps::EXPRESSION_FILTER)
    .union(Caps::KEY_ENUMERATION);

/// Baseline/post-handshake capabilities, parameterized on whether this
/// server actually supports multi-document transactions. Those need a 4.0+
/// replica set, which is only knowable after `hello` — so the flag is
/// detected post-handshake and reported honestly, never assumed.
pub fn mongo_capabilities(transactions_supported: bool) -> Capabilities {
    let mut flags = BASE_CAPS;
    if transactions_supported {
        flags |= Caps::TRANSACTIONS;
    }
    Capabilities {
        flags,
        max_statement_bytes: Some(16 * 1024 * 1024), // BSON document hard limit
        // Ticket: "default_fetch_rows 101 then 1000" — 101 is the honest
        // single starting value; growth toward ~1000 is datagrep-core's adaptive
        // sizing (`clamp(prev * target_ms / actual_ms, ...)`), not something
        // a single `u32` field can express.
        default_fetch_rows: 101,
        // The engine takes structured commands, not `$1`/`?`-templated text
        // (ticket item 1).
        param_style: ParamStyle::None,
        language: LanguageId::MongoShell,
        // Mongo has no quoted-identifier syntax; field/collection names are
        // never re-quoted into generated text. Kept as the least-surprising
        // placeholder since `Capabilities::identifier_quote` is not optional.
        identifier_quote: '"',
        // database -> collection -> field (ticket item 1's `catalog_levels`).
        catalog_levels: 3,
    }
}

/// The MongoDB driver adapter. Stateless — all per-server state
/// lives in the [`MongoConnection`]s it creates.
#[derive(Debug, Default)]
pub struct MongoDriver;

impl MongoDriver {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl Driver for MongoDriver {
    fn meta(&self) -> DriverMeta {
        DriverMeta {
            id: Arc::from("mongodb"),
            display_name: Arc::from("MongoDB"),
            version: Arc::from(env!("CARGO_PKG_VERSION")),
        }
    }

    fn capabilities(&self) -> Capabilities {
        // Pre-handshake: transactions support is unknown, so the honest
        // baseline reports it unavailable rather than guessing.
        mongo_capabilities(false)
    }

    fn config_schema(&self) -> ConfigSchema {
        ConfigSchema {
            fields: vec![
                ConfigField {
                    key: Arc::from("hosts"),
                    label: Arc::from("Host(s)"),
                    kind: FieldKind::Text,
                    required: true,
                    default: Some(ConfigValue::Str("localhost:27017".into())),
                    secret: false,
                },
                ConfigField {
                    key: Arc::from("srv"),
                    label: Arc::from("Use mongodb+srv:// (Atlas / DNS seedlist)"),
                    kind: FieldKind::Bool,
                    required: false,
                    default: Some(ConfigValue::Bool(false)),
                    secret: false,
                },
                ConfigField {
                    key: Arc::from("user"),
                    label: Arc::from("User"),
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
                    key: Arc::from("database"),
                    label: Arc::from("Default database"),
                    kind: FieldKind::Text,
                    required: true,
                    default: Some(ConfigValue::Str("test".into())),
                    secret: false,
                },
                ConfigField {
                    key: Arc::from("auth_source"),
                    label: Arc::from("Auth source"),
                    kind: FieldKind::Text,
                    required: false,
                    default: None,
                    secret: false,
                },
                ConfigField {
                    key: Arc::from("replica_set"),
                    label: Arc::from("Replica set name"),
                    kind: FieldKind::Text,
                    required: false,
                    default: None,
                    secret: false,
                },
                ConfigField {
                    key: Arc::from("tls"),
                    label: Arc::from("TLS"),
                    kind: FieldKind::Bool,
                    required: false,
                    default: Some(ConfigValue::Bool(false)),
                    secret: false,
                },
                ConfigField {
                    key: Arc::from("extra_options"),
                    label: Arc::from("Extra connection string options (raw, e.g. \"retryWrites=true&w=majority\")"),
                    kind: FieldKind::Text,
                    required: false,
                    default: None,
                    secret: false,
                },
            ],
        }
    }

    fn parse_url(&self, url: &str) -> Result<ConnectionConfig, ConfigError> {
        parse_mongo_url(url)
    }

    #[tracing::instrument(skip(self, cfg, ctx))]
    async fn connect(
        &self,
        cfg: &ResolvedConfig,
        ctx: ConnectCtx,
    ) -> Result<Box<dyn Connection>, DbError> {
        let database = str_field(cfg, "database")?.ok_or_else(|| {
            DbError::Config(ConfigError::MissingField {
                key: "database".into(),
            })
        })?;
        let uri = build_uri(cfg)?;
        let timeout = ctx.connect_timeout.unwrap_or(Duration::from_secs(15));

        let (client, server_info, transactions_supported) = tokio::time::timeout(timeout, async {
            let mut opts = mongodb::options::ClientOptions::parse(&uri)
                .await
                .map_err(map_mongo_error)?;
            if let Some(app) = ctx.application_name.as_deref() {
                opts.app_name = Some(app.to_string());
            }
            opts.connect_timeout.get_or_insert(timeout);
            let client = mongodb::Client::with_options(opts).map_err(map_mongo_error)?;

            if ctx.cancel.is_cancelled() {
                return Err(DbError::Cancelled);
            }

            let admin = client.database("admin");
            let hello = admin
                .run_command(doc! { "hello": 1 })
                .await
                .map_err(map_mongo_error)?;
            let build_info = admin
                .run_command(doc! { "buildInfo": 1 })
                .await
                .map_err(map_mongo_error)?;

            let max_wire_version = hello.get_i32("maxWireVersion").unwrap_or(0);
            let set_name = hello.get_str("setName").ok().map(str::to_string);
            let is_mongos = hello.get_str("msg").ok() == Some("isdbgrid");
            let version = build_info
                .get_str("version")
                .unwrap_or("unknown")
                .to_string();

            // Multi-document transactions: replica sets from wire version 7
            // (MongoDB 4.0), sharded clusters from wire version 8 (4.2).
            let transactions_supported = if is_mongos {
                max_wire_version >= 8
            } else {
                set_name.is_some() && max_wire_version >= 7
            };

            let topology = if is_mongos {
                "sharded"
            } else if set_name.is_some() {
                "replica-set"
            } else {
                "standalone"
            };
            let mut details = vec![
                (Arc::from("topology"), Arc::from(topology)),
                (
                    Arc::from("maxWireVersion"),
                    Arc::from(max_wire_version.to_string().as_str()),
                ),
            ];
            if let Some(name) = &set_name {
                details.push((Arc::from("replicaSet"), Arc::from(name.as_str())));
            }
            let server_info = ServerInfo {
                product: Arc::from("MongoDB"),
                version: Arc::from(version.as_str()),
                details,
            };
            Ok((client, server_info, transactions_supported))
        })
        .await
        .map_err(|_| DbError::Timeout)??;

        Ok(Box::new(MongoConnection::new(
            client,
            database,
            server_info,
            transactions_supported,
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

/// Reassemble a `mongodb://`/`mongodb+srv://` connection string from the
/// resolved config fields. The password lives in the keychain as a
/// [`datagrep_api::config::SecretString`], never in `ConnectionConfig`, and is
/// only ever pulled back in here at connect time — so a serialized config,
/// a log line, or a debug dump can never carry it.
fn build_uri(cfg: &ResolvedConfig) -> Result<String, DbError> {
    let hosts = str_field(cfg, "hosts")?.ok_or_else(|| {
        DbError::Config(ConfigError::MissingField {
            key: "hosts".into(),
        })
    })?;
    let srv = bool_field(cfg, "srv")?.unwrap_or(false);
    let user = str_field(cfg, "user")?;
    let password = cfg
        .secrets
        .get("password")
        .map(|s| s.expose().to_string())
        .or_else(|| str_field(cfg, "password").ok().flatten());
    let database = str_field(cfg, "database")?.unwrap_or_default();
    let auth_source = str_field(cfg, "auth_source")?;
    let replica_set = str_field(cfg, "replica_set")?;
    let tls = bool_field(cfg, "tls")?.unwrap_or(false);
    let extra = str_field(cfg, "extra_options")?;

    let scheme = if srv { "mongodb+srv" } else { "mongodb" };
    let mut uri = format!("{scheme}://");
    if let Some(u) = &user {
        uri.push_str(&percent_encode(u));
        if let Some(p) = &password {
            uri.push(':');
            uri.push_str(&percent_encode(p));
        }
        uri.push('@');
    }
    uri.push_str(&hosts);
    uri.push('/');
    uri.push_str(&database);

    let mut query: Vec<String> = Vec::new();
    if let Some(src) = &auth_source {
        query.push(format!("authSource={src}"));
    }
    if let Some(rs) = &replica_set {
        query.push(format!("replicaSet={rs}"));
    }
    if tls {
        query.push("tls=true".to_string());
    }
    if let Some(extra) = &extra {
        if !extra.is_empty() {
            query.push(extra.clone());
        }
    }
    if !query.is_empty() {
        uri.push('?');
        uri.push_str(&query.join("&"));
    }
    Ok(uri)
}

/// Minimal RFC-3986 `userinfo` percent-encoding for the handful of reserved
/// characters that can appear in a username/password and would otherwise be
/// misparsed as URI delimiters (`:`, `@`, `/`, `?`, `#`, `%`).
fn percent_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b':' | b'@' | b'/' | b'?' | b'#' | b'%' => {
                out.push('%');
                out.push_str(&format!("{b:02X}"));
            }
            _ => out.push(b as char),
        }
    }
    out
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            // Read the pair out of the byte slice rather than by slicing the
            // `&str`. `i + 1..i + 3` lands inside a multi-byte character
            // whenever a `%` is followed by non-ASCII, and slicing a `str` off
            // a char boundary panics — from a *pasted connection URL*, which is
            // untrusted text that reaches here before anything dials.
            let pair = std::str::from_utf8(&bytes[i + 1..i + 3]).ok();
            if let Some(hex) = pair.and_then(|p| u8::from_str_radix(p, 16).ok()) {
                out.push(hex);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Decompose a pasted `mongodb://`/`mongodb+srv://` URL into config fields
/// (ticket item 1). Deliberately hand-rolled rather than
/// `mongodb::options::ClientOptions::parse` (which is `async` and performs
/// SRV/TXT DNS lookups for `mongodb+srv://` — wrong shape for a synchronous,
/// no-network `Driver::parse_url`); this only splits the string, it never
/// resolves anything.
fn parse_mongo_url(url: &str) -> Result<ConnectionConfig, ConfigError> {
    let (srv, rest) = if let Some(r) = url.strip_prefix("mongodb+srv://") {
        (true, r)
    } else if let Some(r) = url.strip_prefix("mongodb://") {
        (false, r)
    } else {
        return Err(ConfigError::InvalidUrl {
            reason: "expected a mongodb:// or mongodb+srv:// url".into(),
        });
    };

    // Split off the query string first, then the path, then userinfo@hosts.
    let (before_query, query) = match rest.split_once('?') {
        Some((b, q)) => (b, Some(q)),
        None => (rest, None),
    };
    let (before_path, path) = match before_query.split_once('/') {
        Some((b, p)) => (b, Some(p)),
        None => (before_query, None),
    };
    let (userinfo, hosts) = match before_path.rsplit_once('@') {
        Some((u, h)) => (Some(u), h),
        None => (None, before_path),
    };
    if hosts.is_empty() {
        return Err(ConfigError::InvalidUrl {
            reason: "missing host".into(),
        });
    }

    let mut values = BTreeMap::new();
    values.insert("hosts".to_string(), ConfigValue::Str(hosts.to_string()));
    values.insert("srv".to_string(), ConfigValue::Bool(srv));

    if let Some(userinfo) = userinfo {
        match userinfo.split_once(':') {
            Some((u, p)) => {
                values.insert("user".to_string(), ConfigValue::Str(percent_decode(u)));
                if !p.is_empty() {
                    values.insert("password".to_string(), ConfigValue::Str(percent_decode(p)));
                }
            }
            None => {
                values.insert(
                    "user".to_string(),
                    ConfigValue::Str(percent_decode(userinfo)),
                );
            }
        }
    }

    if let Some(db) = path.filter(|p| !p.is_empty()) {
        values.insert("database".to_string(), ConfigValue::Str(db.to_string()));
    }

    if let Some(query) = query {
        let mut leftover = Vec::new();
        for pair in query.split('&').filter(|p| !p.is_empty()) {
            let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
            match k {
                "authSource" => {
                    values.insert(
                        "auth_source".to_string(),
                        ConfigValue::Str(percent_decode(v)),
                    );
                }
                "replicaSet" => {
                    values.insert(
                        "replica_set".to_string(),
                        ConfigValue::Str(percent_decode(v)),
                    );
                }
                "tls" | "ssl" => {
                    values.insert(
                        "tls".to_string(),
                        ConfigValue::Bool(v.eq_ignore_ascii_case("true")),
                    );
                }
                _ => leftover.push(pair.to_string()),
            }
        }
        if !leftover.is_empty() {
            values.insert(
                "extra_options".to_string(),
                ConfigValue::Str(leftover.join("&")),
            );
        }
    }

    Ok(ConnectionConfig {
        driver: Arc::from("mongodb"),
        values,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_url_extracts_fields() {
        let driver = MongoDriver::new();
        let cfg = driver
            .parse_url("mongodb://alice:hunter2@db.example.com:27017/appdb?authSource=admin&replicaSet=rs0")
            .unwrap();
        assert_eq!(&*cfg.driver, "mongodb");
        assert_eq!(
            cfg.values.get("hosts"),
            Some(&ConfigValue::Str("db.example.com:27017".into()))
        );
        assert_eq!(cfg.values.get("srv"), Some(&ConfigValue::Bool(false)));
        assert_eq!(
            cfg.values.get("user"),
            Some(&ConfigValue::Str("alice".into()))
        );
        assert_eq!(
            cfg.values.get("password"),
            Some(&ConfigValue::Str("hunter2".into()))
        );
        assert_eq!(
            cfg.values.get("database"),
            Some(&ConfigValue::Str("appdb".into()))
        );
        assert_eq!(
            cfg.values.get("auth_source"),
            Some(&ConfigValue::Str("admin".into()))
        );
        assert_eq!(
            cfg.values.get("replica_set"),
            Some(&ConfigValue::Str("rs0".into()))
        );
    }

    #[test]
    fn parse_url_accepts_srv_scheme() {
        let driver = MongoDriver::new();
        let cfg = driver
            .parse_url("mongodb+srv://user@cluster0.example.mongodb.net/mydb")
            .unwrap();
        assert_eq!(cfg.values.get("srv"), Some(&ConfigValue::Bool(true)));
        assert_eq!(
            cfg.values.get("hosts"),
            Some(&ConfigValue::Str("cluster0.example.mongodb.net".into()))
        );
    }

    #[test]
    fn parse_url_no_auth_no_db() {
        let driver = MongoDriver::new();
        let cfg = driver.parse_url("mongodb://localhost:27017").unwrap();
        assert_eq!(
            cfg.values.get("hosts"),
            Some(&ConfigValue::Str("localhost:27017".into()))
        );
        assert!(!cfg.values.contains_key("user"));
        assert!(!cfg.values.contains_key("database"));
    }

    #[test]
    fn parse_url_rejects_non_mongo_scheme() {
        let driver = MongoDriver::new();
        assert!(driver.parse_url("postgres://localhost/db").is_err());
    }

    #[test]
    fn parse_url_rejects_missing_host() {
        let driver = MongoDriver::new();
        assert!(driver.parse_url("mongodb://").is_err());
    }

    #[test]
    fn capabilities_never_claim_exact_count_random_access_or_declared_schema() {
        let caps = mongo_capabilities(true);
        assert!(!caps.flags.contains(Caps::EXACT_COUNT_CHEAP));
        assert!(!caps.flags.contains(Caps::RANDOM_ACCESS_PAGE));
        assert!(!caps.flags.contains(Caps::SCHEMA_DECLARED));
        assert!(caps.flags.contains(Caps::TRANSACTIONS));
        assert_eq!(caps.param_style, ParamStyle::None);
        assert_eq!(caps.default_fetch_rows, 101);
        assert_eq!(caps.catalog_levels, 3);
    }

    #[test]
    fn capabilities_drop_transactions_when_not_detected() {
        let caps = mongo_capabilities(false);
        assert!(!caps.flags.contains(Caps::TRANSACTIONS));
        assert!(caps.flags.contains(Caps::DDL));
        assert!(caps.flags.contains(Caps::EXPLAIN));
        assert!(caps.flags.contains(Caps::KEY_ENUMERATION));
    }

    #[test]
    fn build_uri_reassembles_from_fields() {
        let mut values = BTreeMap::new();
        values.insert(
            "hosts".to_string(),
            ConfigValue::Str("localhost:27017".into()),
        );
        values.insert("database".to_string(), ConfigValue::Str("appdb".into()));
        let cfg = ResolvedConfig::without_secrets(ConnectionConfig {
            driver: Arc::from("mongodb"),
            values,
        });
        let uri = build_uri(&cfg).unwrap();
        assert_eq!(uri, "mongodb://localhost:27017/appdb");
    }

    /// `percent_decode` indexed the `&str` by byte offset, so a `%` followed by
    /// a multi-byte character sliced off a char boundary and panicked. The
    /// input is a *pasted connection URL* — untrusted text that reaches
    /// `parse_url` before anything dials — so this was one paste from taking
    /// the process down. Decoding now reads the byte pair directly and leaves a
    /// `%` that is not followed by two hex ASCII digits alone.
    #[test]
    fn a_percent_escape_before_a_multibyte_char_does_not_panic() {
        for tail in [
            "%\u{20ac}",
            "%\u{20ac}\u{20ac}",
            "\u{20ac}%",
            "%",
            "%A",
            "%\u{e9}9",
            "100%",
        ] {
            assert!(
                !percent_decode(tail).is_empty() || tail.is_empty(),
                "{tail:?}"
            );
        }
        // A real escape still decodes, and the surrounding text survives.
        assert_eq!(percent_decode("a%20b"), "a b");
        assert_eq!(percent_decode("%41%42"), "AB");
        // A lone `%` before non-ASCII is passed through, not swallowed.
        assert_eq!(percent_decode("%\u{20ac}"), "%\u{20ac}");
    }
}
