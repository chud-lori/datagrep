use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use base64::Engine as _;
use serde_json::Value as Json;

use datagrep_api::error::DbError;

use crate::error::{map_reqwest_error, map_status_error};
use crate::json::OrderedJson;

pub const OPAQUE_ID_HEADER: &str = "X-Opaque-Id";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Product {
    Elasticsearch,
    OpenSearch,
}

impl Product {
    pub fn display_name(self) -> &'static str {
        match self {
            Product::Elasticsearch => "Elasticsearch",
            Product::OpenSearch => "OpenSearch",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PageMode {
    Pit,
    Scroll,
}

impl PageMode {
    pub fn as_str(self) -> &'static str {
        match self {
            PageMode::Pit => "pit+search_after",
            PageMode::Scroll => "scroll",
        }
    }
}

#[derive(Clone)]
pub enum Auth {
    None,
    Basic(Arc<str>),
    ApiKey(Arc<str>),
    Bearer(Arc<str>),
}

impl Auth {
    pub fn basic(user: &str, password: &str) -> Self {
        let raw = format!("{user}:{password}");
        Auth::Basic(Arc::from(
            base64::engine::general_purpose::STANDARD
                .encode(raw)
                .as_str(),
        ))
    }

    pub fn api_key(value: &str) -> Self {
        let encoded = if value.contains(':') {
            base64::engine::general_purpose::STANDARD.encode(value)
        } else {
            value.to_string()
        };
        Auth::ApiKey(Arc::from(encoded.as_str()))
    }

    pub fn bearer(token: &str) -> Self {
        Auth::Bearer(Arc::from(token))
    }

    fn header_value(&self) -> Option<String> {
        match self {
            Auth::None => None,
            Auth::Basic(b) => Some(format!("Basic {b}")),
            Auth::ApiKey(k) => Some(format!("ApiKey {k}")),
            Auth::Bearer(t) => Some(format!("Bearer {t}")),
        }
    }

    pub fn scheme(&self) -> &'static str {
        match self {
            Auth::None => "none",
            Auth::Basic(_) => "basic",
            Auth::ApiKey(_) => "api_key",
            Auth::Bearer(_) => "bearer",
        }
    }
}

impl fmt::Debug for Auth {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Auth::{}(\"••••\")", self.scheme())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Method {
    Get,
    Post,
    Put,
    Delete,
    Head,
}

impl Method {
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s.to_ascii_uppercase().as_str() {
            "GET" => Method::Get,
            "POST" => Method::Post,
            "PUT" => Method::Put,
            "DELETE" => Method::Delete,
            "HEAD" => Method::Head,
            _ => return None,
        })
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Method::Get => "GET",
            Method::Post => "POST",
            Method::Put => "PUT",
            Method::Delete => "DELETE",
            Method::Head => "HEAD",
        }
    }

    fn to_reqwest(self) -> reqwest::Method {
        match self {
            Method::Get => reqwest::Method::GET,
            Method::Post => reqwest::Method::POST,
            Method::Put => reqwest::Method::PUT,
            Method::Delete => reqwest::Method::DELETE,
            Method::Head => reqwest::Method::HEAD,
        }
    }

    pub fn is_read(self) -> bool {
        matches!(self, Method::Get | Method::Head)
    }
}

pub struct EsHttp {
    client: reqwest::Client,
    base: String,
    auth: Auth,
    request_timeout: Duration,
}

impl fmt::Debug for EsHttp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("EsHttp")
            .field("base", &self.base)
            .field("auth", &self.auth)
            .finish()
    }
}

impl EsHttp {
    pub fn new(
        base: String,
        auth: Auth,
        request_timeout: Duration,
        accept_invalid_certs: bool,
    ) -> Result<Self, DbError> {
        let mut builder = reqwest::Client::builder()
            .use_rustls_tls()
            .connect_timeout(request_timeout)
            .pool_idle_timeout(Duration::from_secs(30))
            .user_agent(concat!(
                "datagrep-drv-elasticsearch/",
                env!("CARGO_PKG_VERSION")
            ));
        if accept_invalid_certs {
            builder = builder.danger_accept_invalid_certs(true);
        }
        let client = builder.build().map_err(|e| DbError::Tls(e.to_string()))?;
        Ok(Self {
            client,
            base: base.trim_end_matches('/').to_string(),
            auth,
            request_timeout,
        })
    }

    pub fn base(&self) -> &str {
        &self.base
    }

    pub fn auth_scheme(&self) -> &'static str {
        self.auth.scheme()
    }

    fn url(&self, path: &str) -> String {
        if path.starts_with('/') {
            format!("{}{}", self.base, path)
        } else {
            format!("{}/{}", self.base, path)
        }
    }

    pub async fn request(
        &self,
        method: Method,
        path: &str,
        query: &[(&str, String)],
        body: Option<&Json>,
        opaque_id: Option<&str>,
        timeout: Option<Duration>,
    ) -> Result<Json, DbError> {
        self.request_sized(method, path, query, body, opaque_id, timeout)
            .await
            .map(|(json, _)| json)
    }

    pub async fn request_sized(
        &self,
        method: Method,
        path: &str,
        query: &[(&str, String)],
        body: Option<&Json>,
        opaque_id: Option<&str>,
        timeout: Option<Duration>,
    ) -> Result<(Json, usize), DbError> {
        let (text, size) = self
            .send(method, path, query, body, opaque_id, timeout)
            .await?;
        if text.trim().is_empty() {
            return Ok((Json::Null, size));
        }
        let json = serde_json::from_str(&text)
            .map_err(|e| DbError::Protocol(format!("response was not valid JSON: {e}")))?;
        Ok((json, size))
    }

    pub async fn request_ordered(
        &self,
        method: Method,
        path: &str,
        query: &[(&str, String)],
        body: Option<&Json>,
        opaque_id: Option<&str>,
        timeout: Option<Duration>,
    ) -> Result<(OrderedJson, usize), DbError> {
        let (text, size) = self
            .send(method, path, query, body, opaque_id, timeout)
            .await?;
        if text.trim().is_empty() {
            return Ok((OrderedJson::Null, size));
        }
        let json = OrderedJson::parse(&text)
            .map_err(|e| DbError::Protocol(format!("response was not valid JSON: {e}")))?;
        Ok((json, size))
    }

    pub async fn request_ndjson(
        &self,
        method: Method,
        path: &str,
        query: &[(&str, String)],
        body: &str,
        opaque_id: Option<&str>,
        timeout: Option<Duration>,
    ) -> Result<Json, DbError> {
        let (text, _size) = self
            .send_raw(
                method,
                path,
                query,
                body,
                "application/x-ndjson",
                opaque_id,
                timeout,
            )
            .await?;
        if text.trim().is_empty() {
            return Ok(Json::Null);
        }
        serde_json::from_str(&text)
            .map_err(|e| DbError::Protocol(format!("response was not valid JSON: {e}")))
    }

    async fn send(
        &self,
        method: Method,
        path: &str,
        query: &[(&str, String)],
        body: Option<&Json>,
        opaque_id: Option<&str>,
        timeout: Option<Duration>,
    ) -> Result<(String, usize), DbError> {
        let url = self.url(path);
        let mut req = self
            .client
            .request(method.to_reqwest(), &url)
            .timeout(timeout.unwrap_or(self.request_timeout));
        if !query.is_empty() {
            req = req.query(query);
        }
        if let Some(v) = self.auth.header_value() {
            req = req.header(reqwest::header::AUTHORIZATION, v);
        }
        if let Some(id) = opaque_id {
            req = req.header(OPAQUE_ID_HEADER, id);
        }
        if let Some(body) = body {
            req = req.json(body);
        }

        tracing::debug!(method = method.as_str(), path, "elasticsearch request");

        let resp = req.send().await.map_err(map_reqwest_error)?;
        let status = resp.status();
        let text = resp.text().await.map_err(map_reqwest_error)?;
        let size = text.len();
        if !status.is_success() {
            return Err(map_status_error(status.as_u16(), &text));
        }
        Ok((text, size))
    }

    #[allow(clippy::too_many_arguments)] // mirrors `send` plus an explicit content type
    async fn send_raw(
        &self,
        method: Method,
        path: &str,
        query: &[(&str, String)],
        body: &str,
        content_type: &'static str,
        opaque_id: Option<&str>,
        timeout: Option<Duration>,
    ) -> Result<(String, usize), DbError> {
        let url = self.url(path);
        let mut req = self
            .client
            .request(method.to_reqwest(), &url)
            .timeout(timeout.unwrap_or(self.request_timeout))
            .header(reqwest::header::CONTENT_TYPE, content_type)
            .body(body.to_string());
        if !query.is_empty() {
            req = req.query(query);
        }
        if let Some(v) = self.auth.header_value() {
            req = req.header(reqwest::header::AUTHORIZATION, v);
        }
        if let Some(id) = opaque_id {
            req = req.header(OPAQUE_ID_HEADER, id);
        }

        // Path only, never the body: an NDJSON body can carry user documents.
        tracing::debug!(method = method.as_str(), path, "elasticsearch raw request");

        let resp = req.send().await.map_err(map_reqwest_error)?;
        let status = resp.status();
        let text = resp.text().await.map_err(map_reqwest_error)?;
        let size = text.len();
        if !status.is_success() {
            return Err(map_status_error(status.as_u16(), &text));
        }
        Ok((text, size))
    }

    pub async fn root_info(&self) -> Result<Json, DbError> {
        self.request(Method::Get, "/", &[], None, None, None).await
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RootInfo {
    pub product: Product,
    pub version: String,
    pub details: Vec<(Arc<str>, Arc<str>)>,
}

pub fn classify_root(root: &Json) -> RootInfo {
    let version = root.get("version");
    let distribution = version
        .and_then(|v| v.get("distribution"))
        .and_then(Json::as_str);
    let product = match distribution {
        Some(d) if d.eq_ignore_ascii_case("opensearch") => Product::OpenSearch,
        _ => Product::Elasticsearch,
    };
    let number = version
        .and_then(|v| v.get("number"))
        .and_then(Json::as_str)
        .unwrap_or("unknown")
        .to_string();

    let mut details: Vec<(Arc<str>, Arc<str>)> = Vec::new();
    let mut push = |k: &str, v: &str| details.push((Arc::from(k), Arc::from(v)));
    push("distribution", distribution.unwrap_or("elasticsearch"));
    if let Some(c) = root.get("cluster_name").and_then(Json::as_str) {
        push("cluster_name", c);
    }
    if let Some(c) = root.get("cluster_uuid").and_then(Json::as_str) {
        push("cluster_uuid", c);
    }
    for key in ["lucene_version", "build_flavor", "build_type"] {
        if let Some(v) = version.and_then(|v| v.get(key)).and_then(Json::as_str) {
            push(key, v);
        }
    }
    RootInfo {
        product,
        version: number,
        details,
    }
}

pub fn version_pair(version: &str) -> (u32, u32) {
    let mut it = version
        .split(|c: char| !c.is_ascii_digit())
        .filter(|s| !s.is_empty());
    let major = it.next().and_then(|s| s.parse().ok()).unwrap_or(0);
    let minor = it.next().and_then(|s| s.parse().ok()).unwrap_or(0);
    (major, minor)
}

pub fn choose_page_mode(product: Product, version: &str) -> PageMode {
    let (major, minor) = version_pair(version);
    match product {
        Product::Elasticsearch if major > 7 || (major == 7 && minor >= 12) => PageMode::Pit,
        _ => PageMode::Scroll,
    }
}

pub fn has_async_search(product: Product, version: &str) -> bool {
    let (major, minor) = version_pair(version);
    matches!(product, Product::Elasticsearch) && (major > 7 || (major == 7 && minor >= 7))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn es_root() -> Json {
        serde_json::json!({
            "name": "node-1",
            "cluster_name": "docker-cluster",
            "cluster_uuid": "abc",
            "version": {
                "number": "8.15.0",
                "build_flavor": "default",
                "build_type": "docker",
                "lucene_version": "9.11.1"
            },
            "tagline": "You Know, for Search"
        })
    }

    fn opensearch_root() -> Json {
        serde_json::json!({
            "name": "node-1",
            "cluster_name": "opensearch-cluster",
            "version": {
                "distribution": "opensearch",
                "number": "2.11.0",
                "build_type": "tar",
                "lucene_version": "9.7.0",
                "minimum_wire_compatibility_version": "7.10.0"
            },
            "tagline": "The OpenSearch Project"
        })
    }

    #[test]
    fn classify_distinguishes_products_by_distribution_not_version_number() {
        let RootInfo {
            product: p,
            version: v,
            details,
        } = classify_root(&es_root());
        assert_eq!(p, Product::Elasticsearch);
        assert_eq!(v, "8.15.0");
        assert!(details
            .iter()
            .any(|(k, v)| &**k == "distribution" && &**v == "elasticsearch"));

        let RootInfo {
            product: p,
            version: v,
            details,
        } = classify_root(&opensearch_root());
        assert_eq!(p, Product::OpenSearch);
        assert_eq!(v, "2.11.0");
        assert!(details
            .iter()
            .any(|(k, v)| &**k == "distribution" && &**v == "opensearch"));
        assert!(details.iter().any(|(k, _)| &**k == "lucene_version"));
    }

    #[test]
    fn classify_survives_a_root_document_it_has_never_seen() {
        let info = classify_root(&serde_json::json!({}));
        assert_eq!(
            info.product,
            Product::Elasticsearch,
            "no distribution field => ES"
        );
        assert_eq!(info.version, "unknown");
    }

    #[test]
    fn version_pair_tolerates_suffixes_and_garbage() {
        assert_eq!(version_pair("8.15.0"), (8, 15));
        assert_eq!(version_pair("7.10.2"), (7, 10));
        assert_eq!(version_pair("9.0.0-SNAPSHOT"), (9, 0));
        assert_eq!(version_pair("unknown"), (0, 0));
    }

    #[test]
    fn page_mode_is_pit_only_where_shard_doc_actually_exists() {
        assert_eq!(
            choose_page_mode(Product::Elasticsearch, "8.15.0"),
            PageMode::Pit
        );
        assert_eq!(
            choose_page_mode(Product::Elasticsearch, "7.12.0"),
            PageMode::Pit
        );
        assert_eq!(
            choose_page_mode(Product::Elasticsearch, "7.10.2"),
            PageMode::Scroll
        );
        // OpenSearch: different PIT endpoint shape entirely.
        assert_eq!(
            choose_page_mode(Product::OpenSearch, "2.11.0"),
            PageMode::Scroll
        );
        assert_eq!(PageMode::Pit.as_str(), "pit+search_after");
    }

    #[test]
    fn async_search_is_only_claimed_for_elasticsearch() {
        assert!(has_async_search(Product::Elasticsearch, "8.15.0"));
        assert!(!has_async_search(Product::Elasticsearch, "7.6.0"));
        assert!(!has_async_search(Product::OpenSearch, "2.11.0"));
    }

    #[test]
    fn auth_debug_never_leaks_the_credential() {
        let a = Auth::basic("elastic", "hunter2");
        let dbg = format!("{a:?}");
        assert!(!dbg.contains("hunter2"), "credential leaked: {dbg}");
        assert!(!dbg.contains("ZWxhc3RpYzpodW50ZXIy"), "b64 leaked: {dbg}");
        assert_eq!(dbg, "Auth::basic(\"••••\")");

        for a in [Auth::api_key("id:key"), Auth::bearer("tok"), Auth::None] {
            let dbg = format!("{a:?}");
            assert!(!dbg.contains("key") || dbg.contains("api_key"));
            assert!(dbg.contains("••••"));
        }
    }

    #[test]
    fn auth_header_values_are_correctly_encoded() {
        assert_eq!(
            Auth::basic("elastic", "hunter2").header_value().unwrap(),
            "Basic ZWxhc3RpYzpodW50ZXIy"
        );
        // Raw `id:api_key` gets encoded…
        assert_eq!(
            Auth::api_key("abc:def").header_value().unwrap(),
            format!(
                "ApiKey {}",
                base64::engine::general_purpose::STANDARD.encode("abc:def")
            )
        );
        // …an already-encoded key is passed through untouched.
        assert_eq!(
            Auth::api_key("YWJjOmRlZg==").header_value().unwrap(),
            "ApiKey YWJjOmRlZg=="
        );
        assert_eq!(Auth::bearer("t0k").header_value().unwrap(), "Bearer t0k");
        assert!(Auth::None.header_value().is_none());
    }

    #[test]
    fn method_parsing_and_read_classification() {
        assert_eq!(Method::parse("get"), Some(Method::Get));
        assert_eq!(Method::parse("DELETE"), Some(Method::Delete));
        assert_eq!(Method::parse("PATCH"), None);
        assert!(Method::Get.is_read());
        assert!(Method::Head.is_read());
        assert!(!Method::Post.is_read());
        assert!(!Method::Delete.is_read());
    }

    #[test]
    fn base_url_trailing_slash_is_normalised() {
        let h = EsHttp::new(
            "http://localhost:9200/".into(),
            Auth::None,
            Duration::from_secs(5),
            false,
        )
        .unwrap();
        assert_eq!(h.base(), "http://localhost:9200");
        assert_eq!(h.url("/_search"), "http://localhost:9200/_search");
        assert_eq!(h.url("_search"), "http://localhost:9200/_search");
    }

    #[test]
    fn http_debug_does_not_include_the_credential() {
        let h = EsHttp::new(
            "http://localhost:9200".into(),
            Auth::basic("u", "p"),
            Duration::from_secs(5),
            false,
        )
        .unwrap();
        let dbg = format!("{h:?}");
        assert!(!dbg.contains('p') || !dbg.contains("dTpw"));
        assert!(dbg.contains("••••"));
    }
}
