//! [`ElasticsearchDriver`]: the `Driver` impl — capabilities, the connection
//! form, URL parsing, and the handshake that decides which of this engine's
//! two families we are actually talking to.

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

use crate::connection::EsConnection;
use crate::http::{
    choose_page_mode, classify_root, has_async_search, Auth, EsHttp, PageMode, Product,
};

/// The stable registry id. Deliberately `elasticsearch` for both products:
/// nothing above `datagrep-api` may branch on a driver id, and the OpenSearch
/// difference is carried by `ServerInfo` plus degraded capabilities, which is
/// what those mechanisms are for.
pub const DRIVER_ID: &str = "elasticsearch";

/// Baseline capability flags, before the handshake narrows them.
///
/// The four that are deliberately **absent**, each for a stated reason:
///
/// - `TRANSACTIONS` — Elasticsearch has no multi-document transactions.
/// - `DDL` — index and mapping management is not SQL DDL; pretending it is
///   would put a `CREATE TABLE`-shaped control in front of something else.
/// - `EXACT_COUNT_CHEAP` — `hits.total` stops counting at `track_total_hits`
///   (10 000 by default), so the UI must show "≥ N". An exact count is a
///   separate `_count` request, which is a real scan.
/// - `RANDOM_ACCESS_PAGE` — deep `from`+`size` paging costs
///   `from + size` per shard and 400s past `index.max_result_window`. Keyset
///   (`search_after`) only.
/// - `SCHEMA_DECLARED` — mappings exist, but dynamic mapping means they are
///   not exhaustive: a document can carry a field the mapping has never seen.
///   The catalog uses sampling, and says so.
/// - `EDITABLE_RESULTS` — this driver does not generate writes (see the crate
///   report); hits do have a real `_index`/`_id` identity, so this is a
///   scope decision, not an engine limitation.
const BASE_CAPS: Caps = Caps::EXPLAIN
    .union(Caps::SERVER_CANCEL)
    .union(Caps::EXPRESSION_FILTER)
    .union(Caps::KEY_ENUMERATION)
    // `profile: true` runs the search and reports real per-shard timings.
    .union(Caps::EXPLAIN_ANALYZE)
    // The cursor streams page by page and never materializes a result set, so
    // "export all" genuinely is not "load all" (design §5.1).
    .union(Caps::EXPORT_STREAMING);

/// Capabilities for a connection, narrowed by what the handshake found.
pub fn es_capabilities(product: Product, page_mode: PageMode) -> Capabilities {
    let mut flags = BASE_CAPS;
    if matches!(product, Product::OpenSearch) || page_mode == PageMode::Scroll {
        // Without `_async_search` there is no handle to a still-running search,
        // so a cancel degrades to abandoning the channel. The flag comes off
        // rather than the UI being told a stop button will reach the server.
        flags.remove(Caps::SERVER_CANCEL);
    }
    Capabilities {
        flags,
        // `http.max_content_length` defaults to 100 MB.
        max_statement_bytes: Some(100 * 1024 * 1024),
        default_fetch_rows: 500,
        // `$1`-numbered placeholders, bound into the *parsed* body — see
        // `console::bind_params`.
        param_style: ParamStyle::DollarNumbered,
        language: LanguageId::EsDsl,
        // Elasticsearch has no quoted-identifier syntax; index and field names
        // are never re-quoted into generated text. Kept as the least
        // surprising placeholder since the field is not optional.
        identifier_quote: '"',
        // index|alias|datastream -> field.
        catalog_levels: 2,
    }
}

/// The Elasticsearch / OpenSearch driver adapter. Stateless — all per-server
/// state lives in the [`EsConnection`]s it creates.
#[derive(Debug, Default)]
pub struct ElasticsearchDriver;

impl ElasticsearchDriver {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl Driver for ElasticsearchDriver {
    fn meta(&self) -> DriverMeta {
        DriverMeta {
            id: Arc::from(DRIVER_ID),
            display_name: Arc::from("Elasticsearch / OpenSearch"),
            version: Arc::from(env!("CARGO_PKG_VERSION")),
        }
    }

    fn capabilities(&self) -> Capabilities {
        // Pre-handshake we do not know the product, so the honest baseline is
        // the weaker one: no server cancel until an async-search-capable
        // Elasticsearch has actually answered.
        es_capabilities(Product::OpenSearch, PageMode::Scroll)
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
                    default: Some(ConfigValue::Num(9200.0)),
                    secret: false,
                },
                ConfigField {
                    key: Arc::from("tls"),
                    label: Arc::from("TLS (https)"),
                    kind: FieldKind::Bool,
                    required: false,
                    default: Some(ConfigValue::Bool(false)),
                    secret: false,
                },
                ConfigField {
                    key: Arc::from("auth"),
                    label: Arc::from("Authentication"),
                    kind: FieldKind::Select {
                        options: vec![
                            Arc::from("none"),
                            Arc::from("basic"),
                            Arc::from("api_key"),
                            Arc::from("bearer"),
                        ],
                    },
                    required: false,
                    default: Some(ConfigValue::Str("none".into())),
                    secret: false,
                },
                ConfigField {
                    key: Arc::from("user"),
                    label: Arc::from("User (basic auth)"),
                    kind: FieldKind::Text,
                    required: false,
                    default: None,
                    secret: false,
                },
                ConfigField {
                    key: Arc::from("password"),
                    label: Arc::from("Password (basic auth)"),
                    kind: FieldKind::Password,
                    required: false,
                    default: None,
                    secret: true,
                },
                ConfigField {
                    key: Arc::from("api_key"),
                    label: Arc::from("API key (`id:api_key`, or the encoded form)"),
                    kind: FieldKind::Password,
                    required: false,
                    default: None,
                    secret: true,
                },
                ConfigField {
                    key: Arc::from("bearer_token"),
                    label: Arc::from("Bearer token"),
                    kind: FieldKind::Password,
                    required: false,
                    default: None,
                    secret: true,
                },
                ConfigField {
                    key: Arc::from("index"),
                    label: Arc::from("Default index / alias / data stream"),
                    kind: FieldKind::Text,
                    required: false,
                    default: None,
                    secret: false,
                },
                ConfigField {
                    key: Arc::from("path_prefix"),
                    label: Arc::from("Path prefix (when behind a reverse proxy)"),
                    kind: FieldKind::Text,
                    required: false,
                    default: None,
                    secret: false,
                },
                ConfigField {
                    key: Arc::from("async_search"),
                    label: Arc::from(
                        "Submit searches as async searches (enables a true server-side cancel)",
                    ),
                    kind: FieldKind::Bool,
                    required: false,
                    default: Some(ConfigValue::Bool(true)),
                    secret: false,
                },
                ConfigField {
                    key: Arc::from("accept_invalid_certs"),
                    label: Arc::from("Skip TLS certificate verification (insecure)"),
                    kind: FieldKind::Bool,
                    required: false,
                    default: Some(ConfigValue::Bool(false)),
                    secret: false,
                },
            ],
        }
    }

    fn parse_url(&self, url: &str) -> Result<ConnectionConfig, ConfigError> {
        parse_es_url(url)
    }

    #[tracing::instrument(skip(self, cfg, ctx))]
    async fn connect(
        &self,
        cfg: &ResolvedConfig,
        ctx: ConnectCtx,
    ) -> Result<Box<dyn Connection>, DbError> {
        let timeout = ctx.connect_timeout.unwrap_or(Duration::from_secs(15));
        let base = build_base_url(cfg)?;
        let auth = build_auth(cfg)?;
        let accept_invalid_certs = bool_field(cfg, "accept_invalid_certs")?.unwrap_or(false);
        let want_async = bool_field(cfg, "async_search")?.unwrap_or(true);
        let default_index = str_field(cfg, "index")?
            .filter(|s| !s.is_empty())
            .map(|s| Arc::from(s.as_str()));

        let http = Arc::new(EsHttp::new(base, auth, timeout, accept_invalid_certs)?);

        if ctx.cancel.is_cancelled() {
            return Err(DbError::Cancelled);
        }

        // Exactly one cheap request on connect (design §5.1: "on connect issue
        // exactly one cheap query"). `GET /` is a few hundred bytes and is the
        // only reliable way to tell the two products apart.
        let root = tokio::time::timeout(timeout, http.root_info())
            .await
            .map_err(|_| DbError::Timeout)??;

        let (product, version, mut details) = classify_root(&root);
        let page_mode = choose_page_mode(product, &version);
        let async_search = want_async && has_async_search(product, &version);

        details.push((Arc::from("pagination"), Arc::from(page_mode.as_str())));
        details.push((
            Arc::from("cancellation"),
            Arc::from(if async_search {
                "async search + tasks API (server-side)"
            } else {
                "client abandon (no async search on this cluster)"
            }),
        ));
        details.push((Arc::from("auth"), Arc::from(http.auth_scheme())));
        if accept_invalid_certs {
            // A connection that skipped verification always says so.
            details.push((
                Arc::from("tls_verification"),
                Arc::from("DISABLED (accept_invalid_certs)"),
            ));
        }

        let server_info = ServerInfo {
            product: Arc::from(product.display_name()),
            version: Arc::from(version.as_str()),
            details,
        };
        let caps = es_capabilities(product, page_mode);

        Ok(Box::new(EsConnection::new(
            http,
            server_info,
            caps,
            page_mode,
            async_search,
            default_index,
            ctx.application_name.clone(),
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

fn num_field(cfg: &ResolvedConfig, key: &str) -> Result<Option<f64>, DbError> {
    match cfg.config.values.get(key) {
        None => Ok(None),
        Some(ConfigValue::Num(n)) => Ok(Some(*n)),
        Some(ConfigValue::Str(s)) => s.parse().map(Some).map_err(|_| {
            DbError::Config(ConfigError::InvalidValue {
                key: key.into(),
                reason: format!("expected a number, got {s:?}"),
            })
        }),
        Some(other) => Err(DbError::Config(ConfigError::InvalidValue {
            key: key.into(),
            reason: format!("expected a number, got {other:?}"),
        })),
    }
}

/// A secret, preferring the resolved keychain value and falling back to a
/// value that is still in the config (which happens between `parse_url` and
/// the caller routing it into the keychain).
fn secret(cfg: &ResolvedConfig, key: &str) -> Option<String> {
    cfg.secrets
        .get(key)
        .map(|s| s.expose().to_string())
        .or_else(|| str_field(cfg, key).ok().flatten())
        .filter(|s| !s.is_empty())
}

pub fn build_base_url(cfg: &ResolvedConfig) -> Result<String, DbError> {
    let host = str_field(cfg, "host")?
        .filter(|h| !h.is_empty())
        .ok_or_else(|| {
            DbError::Config(ConfigError::MissingField {
                key: "host".into(),
            })
        })?;
    let port = num_field(cfg, "port")?.unwrap_or(9200.0) as u16;
    let tls = bool_field(cfg, "tls")?.unwrap_or(false);
    let scheme = if tls { "https" } else { "http" };
    let prefix = str_field(cfg, "path_prefix")?.unwrap_or_default();
    let prefix = prefix.trim_matches('/');
    let mut url = format!("{scheme}://{host}:{port}");
    if !prefix.is_empty() {
        url.push('/');
        url.push_str(prefix);
    }
    Ok(url)
}

/// Build the credential from the resolved config. The `auth` selector is
/// authoritative; when it is absent the first credential actually present
/// wins, so a pasted URL with a password just works.
pub fn build_auth(cfg: &ResolvedConfig) -> Result<Auth, DbError> {
    let mode = str_field(cfg, "auth")?.unwrap_or_default();
    match mode.as_str() {
        "basic" => {
            let user = str_field(cfg, "user")?.unwrap_or_default();
            let password = secret(cfg, "password").unwrap_or_default();
            Ok(Auth::basic(&user, &password))
        }
        "api_key" => secret(cfg, "api_key")
            .map(|k| Auth::api_key(&k))
            .ok_or_else(|| {
                DbError::Config(ConfigError::MissingField {
                    key: "api_key".into(),
                })
            }),
        "bearer" => secret(cfg, "bearer_token")
            .map(|t| Auth::bearer(&t))
            .ok_or_else(|| {
                DbError::Config(ConfigError::MissingField {
                    key: "bearer_token".into(),
                })
            }),
        "none" => Ok(Auth::None),
        // Unset: infer from whichever credential is present.
        _ => {
            if let Some(key) = secret(cfg, "api_key") {
                return Ok(Auth::api_key(&key));
            }
            if let Some(token) = secret(cfg, "bearer_token") {
                return Ok(Auth::bearer(&token));
            }
            match str_field(cfg, "user")?.filter(|u| !u.is_empty()) {
                Some(user) => Ok(Auth::basic(
                    &user,
                    &secret(cfg, "password").unwrap_or_default(),
                )),
                None => Ok(Auth::None),
            }
        }
    }
}

/// Split a pasted `http(s)://[user:pass@]host[:port][/index]` URL into config
/// fields. Hand-rolled and network-free: `parse_url` is synchronous and must
/// never resolve anything.
fn parse_es_url(url: &str) -> Result<ConnectionConfig, ConfigError> {
    let (tls, rest) = if let Some(r) = url.strip_prefix("https://") {
        (true, r)
    } else if let Some(r) = url.strip_prefix("http://") {
        (false, r)
    } else if let Some(r) = url.strip_prefix("elasticsearch://") {
        // The scheme some tools use for a profile URL; plain HTTP transport.
        (false, r)
    } else {
        return Err(ConfigError::InvalidUrl {
            reason: "expected an http:// or https:// url".into(),
        });
    };

    let (before_query, query) = match rest.split_once('?') {
        Some((b, q)) => (b, Some(q)),
        None => (rest, None),
    };
    let (authority, path) = match before_query.split_once('/') {
        Some((a, p)) => (a, Some(p)),
        None => (before_query, None),
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
    // IPv6 literals arrive as `[::1]:9200`.
    let (host, port) = if let Some(rest) = hostport.strip_prefix('[') {
        let (h, tail) = rest.split_once(']').ok_or_else(|| ConfigError::InvalidUrl {
            reason: "unterminated IPv6 literal".into(),
        })?;
        (h.to_string(), tail.strip_prefix(':').map(str::to_string))
    } else {
        match hostport.split_once(':') {
            Some((h, p)) => (h.to_string(), Some(p.to_string())),
            None => (hostport.to_string(), None),
        }
    };
    if host.is_empty() {
        return Err(ConfigError::InvalidUrl {
            reason: "missing host".into(),
        });
    }
    let port: f64 = match port {
        Some(p) => p.parse().map_err(|_| ConfigError::InvalidUrl {
            reason: format!("port {p:?} is not a number"),
        })?,
        None => {
            if tls {
                9243.0
            } else {
                9200.0
            }
        }
    };

    let mut values = BTreeMap::new();
    values.insert("host".to_string(), ConfigValue::Str(host));
    values.insert("port".to_string(), ConfigValue::Num(port));
    values.insert("tls".to_string(), ConfigValue::Bool(tls));

    if let Some(userinfo) = userinfo.filter(|u| !u.is_empty()) {
        values.insert("auth".to_string(), ConfigValue::Str("basic".into()));
        match userinfo.split_once(':') {
            Some((u, p)) => {
                values.insert("user".to_string(), ConfigValue::Str(percent_decode(u)));
                if !p.is_empty() {
                    // The caller routes this into the keychain and zeroizes
                    // the source string (design §3.8).
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

    if let Some(path) = path.map(|p| p.trim_matches('/')).filter(|p| !p.is_empty()) {
        // A single trailing segment is the default index; anything deeper is a
        // reverse-proxy path prefix.
        if path.contains('/') {
            values.insert("path_prefix".to_string(), ConfigValue::Str(path.to_string()));
        } else {
            values.insert("index".to_string(), ConfigValue::Str(path.to_string()));
        }
    }

    if let Some(query) = query {
        for pair in query.split('&').filter(|p| !p.is_empty()) {
            let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
            match k {
                "index" => {
                    values.insert("index".to_string(), ConfigValue::Str(percent_decode(v)));
                }
                "api_key" => {
                    values.insert("auth".to_string(), ConfigValue::Str("api_key".into()));
                    values.insert("api_key".to_string(), ConfigValue::Str(percent_decode(v)));
                }
                "path_prefix" => {
                    values.insert(
                        "path_prefix".to_string(),
                        ConfigValue::Str(percent_decode(v)),
                    );
                }
                unknown => {
                    return Err(ConfigError::UnknownField {
                        key: unknown.to_string(),
                    })
                }
            }
        }
    }

    Ok(ConnectionConfig {
        driver: Arc::from(DRIVER_ID),
        values,
    })
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Ok(hex) = u8::from_str_radix(&s[i + 1..i + 3], 16) {
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

#[cfg(test)]
mod tests {
    use super::*;

    fn resolved(pairs: &[(&str, ConfigValue)]) -> ResolvedConfig {
        let mut values = BTreeMap::new();
        for (k, v) in pairs {
            values.insert(k.to_string(), v.clone());
        }
        ResolvedConfig::without_secrets(ConnectionConfig {
            driver: Arc::from(DRIVER_ID),
            values,
        })
    }

    #[test]
    fn capabilities_never_claim_the_five_things_elasticsearch_cannot_do() {
        let caps = es_capabilities(Product::Elasticsearch, PageMode::Pit);
        assert!(!caps.flags.contains(Caps::TRANSACTIONS));
        assert!(!caps.flags.contains(Caps::DDL));
        assert!(!caps.flags.contains(Caps::EXACT_COUNT_CHEAP));
        assert!(!caps.flags.contains(Caps::RANDOM_ACCESS_PAGE));
        assert!(!caps.flags.contains(Caps::SCHEMA_DECLARED));
        assert!(!caps.flags.contains(Caps::EDITABLE_RESULTS));
        assert!(!caps.flags.contains(Caps::READ_ONLY_SESSION));

        assert!(caps.flags.contains(Caps::EXPLAIN));
        assert!(caps.flags.contains(Caps::SERVER_CANCEL));
        assert!(caps.flags.contains(Caps::EXPRESSION_FILTER));
        assert!(caps.flags.contains(Caps::KEY_ENUMERATION));

        assert_eq!(caps.default_fetch_rows, 500);
        assert_eq!(caps.language, LanguageId::EsDsl);
        assert_eq!(caps.catalog_levels, 2);
    }

    #[test]
    fn server_cancel_is_dropped_where_there_is_no_async_search_handle() {
        let os = es_capabilities(Product::OpenSearch, PageMode::Scroll);
        assert!(
            !os.flags.contains(Caps::SERVER_CANCEL),
            "OpenSearch's asynchronous search is a different plugin endpoint; \
             claiming a server cancel here would be a lie"
        );
        let old_es = es_capabilities(Product::Elasticsearch, PageMode::Scroll);
        assert!(!old_es.flags.contains(Caps::SERVER_CANCEL));
        // The pre-handshake baseline is the weaker one.
        assert!(!ElasticsearchDriver::new()
            .capabilities()
            .flags
            .contains(Caps::SERVER_CANCEL));
    }

    #[test]
    fn driver_meta_is_the_registry_id_the_ffi_registers() {
        let meta = ElasticsearchDriver::new().meta();
        assert_eq!(&*meta.id, "elasticsearch");
        assert_eq!(&*meta.display_name, "Elasticsearch / OpenSearch");
    }

    #[test]
    fn parse_url_splits_host_port_tls_credentials_and_index() {
        let cfg = ElasticsearchDriver::new()
            .parse_url("https://elastic:hunter2@es.example.com:9243/my-index")
            .unwrap();
        assert_eq!(&*cfg.driver, "elasticsearch");
        assert_eq!(
            cfg.values.get("host"),
            Some(&ConfigValue::Str("es.example.com".into()))
        );
        assert_eq!(cfg.values.get("port"), Some(&ConfigValue::Num(9243.0)));
        assert_eq!(cfg.values.get("tls"), Some(&ConfigValue::Bool(true)));
        assert_eq!(
            cfg.values.get("auth"),
            Some(&ConfigValue::Str("basic".into()))
        );
        assert_eq!(
            cfg.values.get("user"),
            Some(&ConfigValue::Str("elastic".into()))
        );
        assert_eq!(
            cfg.values.get("password"),
            Some(&ConfigValue::Str("hunter2".into()))
        );
        assert_eq!(
            cfg.values.get("index"),
            Some(&ConfigValue::Str("my-index".into()))
        );
    }

    #[test]
    fn parse_url_defaults_ports_and_handles_ipv6_and_percent_encoding() {
        let d = ElasticsearchDriver::new();
        let cfg = d.parse_url("http://localhost").unwrap();
        assert_eq!(cfg.values.get("port"), Some(&ConfigValue::Num(9200.0)));
        assert_eq!(cfg.values.get("tls"), Some(&ConfigValue::Bool(false)));
        assert!(!cfg.values.contains_key("user"));

        let cfg = d.parse_url("https://cloud.example.com").unwrap();
        assert_eq!(cfg.values.get("port"), Some(&ConfigValue::Num(9243.0)));

        let cfg = d.parse_url("http://[::1]:9201/logs").unwrap();
        assert_eq!(cfg.values.get("host"), Some(&ConfigValue::Str("::1".into())));
        assert_eq!(cfg.values.get("port"), Some(&ConfigValue::Num(9201.0)));

        // A password with reserved characters survives the round trip.
        let cfg = d
            .parse_url("http://u:p%40ss%3Aword@h:9200")
            .unwrap();
        assert_eq!(
            cfg.values.get("password"),
            Some(&ConfigValue::Str("p@ss:word".into()))
        );
    }

    #[test]
    fn parse_url_treats_a_deep_path_as_a_proxy_prefix_not_an_index() {
        let cfg = ElasticsearchDriver::new()
            .parse_url("https://gateway.example.com/team/es")
            .unwrap();
        assert_eq!(
            cfg.values.get("path_prefix"),
            Some(&ConfigValue::Str("team/es".into()))
        );
        assert!(!cfg.values.contains_key("index"));
    }

    #[test]
    fn parse_url_accepts_index_and_api_key_query_parameters() {
        let cfg = ElasticsearchDriver::new()
            .parse_url("https://h:9243?index=logs&api_key=abc%3Adef")
            .unwrap();
        assert_eq!(
            cfg.values.get("index"),
            Some(&ConfigValue::Str("logs".into()))
        );
        assert_eq!(
            cfg.values.get("auth"),
            Some(&ConfigValue::Str("api_key".into()))
        );
        assert_eq!(
            cfg.values.get("api_key"),
            Some(&ConfigValue::Str("abc:def".into()))
        );
    }

    #[test]
    fn parse_url_rejects_what_it_cannot_honestly_parse() {
        let d = ElasticsearchDriver::new();
        assert!(d.parse_url("postgres://localhost/db").is_err());
        assert!(d.parse_url("http://").is_err());
        assert!(d.parse_url("http://h:notaport").is_err());
        assert!(d.parse_url("http://[::1:9200").is_err());
        // An unknown query parameter is refused rather than silently dropped.
        assert!(d.parse_url("http://h:9200?sniff=true").is_err());
    }

    #[test]
    fn base_url_composition_covers_tls_port_and_proxy_prefix() {
        let cfg = resolved(&[
            ("host", ConfigValue::Str("es.example.com".into())),
            ("port", ConfigValue::Num(9243.0)),
            ("tls", ConfigValue::Bool(true)),
            ("path_prefix", ConfigValue::Str("/team/es/".into())),
        ]);
        assert_eq!(
            build_base_url(&cfg).unwrap(),
            "https://es.example.com:9243/team/es"
        );

        let cfg = resolved(&[("host", ConfigValue::Str("localhost".into()))]);
        assert_eq!(build_base_url(&cfg).unwrap(), "http://localhost:9200");

        // A missing host is a per-field config error the form can point at.
        let err = build_base_url(&resolved(&[])).unwrap_err();
        assert!(matches!(
            err,
            DbError::Config(ConfigError::MissingField { .. })
        ));
    }

    #[test]
    fn auth_selection_follows_the_selector_then_falls_back_to_what_is_present() {
        assert_eq!(
            build_auth(&resolved(&[
                ("auth", ConfigValue::Str("basic".into())),
                ("user", ConfigValue::Str("elastic".into())),
                ("password", ConfigValue::Str("hunter2".into())),
            ]))
            .unwrap()
            .scheme(),
            "basic"
        );
        assert_eq!(
            build_auth(&resolved(&[
                ("auth", ConfigValue::Str("api_key".into())),
                ("api_key", ConfigValue::Str("id:key".into())),
            ]))
            .unwrap()
            .scheme(),
            "api_key"
        );
        assert_eq!(
            build_auth(&resolved(&[
                ("auth", ConfigValue::Str("bearer".into())),
                ("bearer_token", ConfigValue::Str("t".into())),
            ]))
            .unwrap()
            .scheme(),
            "bearer"
        );
        assert_eq!(
            build_auth(&resolved(&[("auth", ConfigValue::Str("none".into()))]))
                .unwrap()
                .scheme(),
            "none"
        );
        // No selector: infer.
        assert_eq!(
            build_auth(&resolved(&[("user", ConfigValue::Str("u".into()))]))
                .unwrap()
                .scheme(),
            "basic"
        );
        assert_eq!(build_auth(&resolved(&[])).unwrap().scheme(), "none");
        // A selected mode with no credential is a per-field error, not a
        // silent downgrade to anonymous.
        assert!(build_auth(&resolved(&[(
            "auth",
            ConfigValue::Str("api_key".into())
        )]))
        .is_err());
    }

    #[test]
    fn auth_prefers_the_keychain_secret_over_a_config_value() {
        let mut cfg = resolved(&[
            ("auth", ConfigValue::Str("basic".into())),
            ("user", ConfigValue::Str("elastic".into())),
            ("password", ConfigValue::Str("stale-from-config".into())),
        ]);
        cfg.secrets.insert(
            "password".to_string(),
            datagrep_api::config::SecretString::new("from-keychain".into()),
        );
        let auth = build_auth(&cfg).unwrap();
        // Only the header value can prove which one was used, and it is not
        // exposed — so assert through the encoding directly.
        let expected = Auth::basic("elastic", "from-keychain");
        assert_eq!(format!("{auth:?}"), format!("{expected:?}"));
        assert_eq!(auth.scheme(), "basic");
    }

    #[test]
    fn config_schema_marks_every_credential_field_secret() {
        let schema = ElasticsearchDriver::new().config_schema();
        for key in ["password", "api_key", "bearer_token"] {
            let field = schema
                .fields
                .iter()
                .find(|f| &*f.key == key)
                .unwrap_or_else(|| panic!("{key} missing from the form"));
            assert!(field.secret, "{key} must never enter ConnectionConfig");
        }
        assert!(schema.fields.iter().any(|f| &*f.key == "host" && f.required));
    }

    #[test]
    fn a_non_numeric_port_in_config_is_a_field_error_not_a_panic() {
        let cfg = resolved(&[
            ("host", ConfigValue::Str("h".into())),
            ("port", ConfigValue::Str("nine thousand".into())),
        ]);
        assert!(matches!(
            build_base_url(&cfg),
            Err(DbError::Config(ConfigError::InvalidValue { .. }))
        ));
    }
}
