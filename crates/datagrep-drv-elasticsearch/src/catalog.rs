//! [`EsCatalog`] — lazy, on-demand namespace browsing: expand per node when
//! the user asks, never an eager whole-catalog index.
//!
//! Two levels: **index / alias / data stream**, then **field**. Both are
//! bounded server-side calls:
//!
//! - the top level is one `_cat/indices` (plus `_alias` and `_data_stream`),
//!   narrowed by an index-expression prefix so the server does the filtering;
//! - the field level is `GET /<that one index>/_mapping` — **never** a
//!   cluster-wide `GET /_mapping`, which on a cluster with a few thousand
//!   indices is tens of megabytes of JSON fetched to answer a question about
//!   one index.
//!
//! # `SCHEMA_DECLARED` is false, and `describe` respects that
//!
//! Elasticsearch mappings are real declarations, but dynamic mapping means
//! they are not exhaustive: a document can introduce a field the mapping has
//! never seen. So [`ObjectDetail::schema`] — whose contract is "declared
//! schema, when the engine has one" — stays `None`, and the mapping is
//! reported through `extra` as a `fields` JSON array clearly labelled with the
//! index's `dynamic` setting. `infer_shape` is the honest, explicitly-labelled
//! substitute, exactly as for Mongo.

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{json, Value as Json};
use tokio::sync::Mutex;

use datagrep_api::catalog::{
    Catalog, Completion, CompletionCtx, Enumeration, FieldTrie, InferredSchema, LevelDef, ListOpts,
    ObjectDetail, ObjectKind, ObjectNode, Page,
};
use datagrep_api::driver::ResumeToken;
use datagrep_api::error::DbError;
use datagrep_api::shape::{LogicalType, ObjectPath};

use crate::http::{EsHttp, Method};
use crate::json::OrderedJson;
use crate::value::{json_to_value, FieldTypes};

/// Default sample size for [`Catalog::infer_shape`].
const DEFAULT_SAMPLE_SIZE: u32 = 500;
/// `index.max_result_window` bounds any single sample.
const MAX_SAMPLE_SIZE: u32 = 10_000;
/// Completion candidate cap: completion is a *server-side* prefix query with
/// a small limit, never a client-side filter over everything.
const COMPLETE_LIMIT: usize = 50;

pub struct EsCatalog {
    http: Arc<EsHttp>,
    /// Per-index mapping cache, populated lazily on first use and shared with
    /// the connection so a scan and the browser never fetch it twice.
    mapping_cache: Arc<Mutex<HashMap<String, Arc<FieldTypes>>>>,
}

impl EsCatalog {
    pub fn new(
        http: Arc<EsHttp>,
        mapping_cache: Arc<Mutex<HashMap<String, Arc<FieldTypes>>>>,
    ) -> Self {
        Self {
            http,
            mapping_cache,
        }
    }

    /// One index's mapping, cached. Never cluster-wide.
    pub async fn mapping(&self, index: &str) -> Result<Arc<FieldTypes>, DbError> {
        if let Some(hit) = self.mapping_cache.lock().await.get(index) {
            return Ok(hit.clone());
        }
        let json = self
            .http
            .request(
                Method::Get,
                &format!("/{}/_mapping", encode_index_expression(index)?),
                &[],
                None,
                None,
                None,
            )
            .await?;
        let types = Arc::new(merge_mappings(&json));
        self.mapping_cache
            .lock()
            .await
            .insert(index.to_string(), types.clone());
        Ok(types)
    }

    async fn list_indices(&self, opts: &ListOpts) -> Result<Vec<CatIndex>, DbError> {
        let expression = match opts.prefix.as_deref() {
            Some(p) if !p.is_empty() => format!("/{}*", encode_index_expression(p)?),
            _ => String::new(),
        };
        let json = self
            .http
            .request(
                Method::Get,
                &format!("/_cat/indices{expression}"),
                &[
                    ("format", "json".to_string()),
                    (
                        "h",
                        "index,health,status,docs.count,store.size,pri,rep".to_string(),
                    ),
                    // Hidden/system indices (`.security`, `.async-search`) are
                    // noise in a data browser and are not listed.
                    ("expand_wildcards", "open".to_string()),
                ],
                None,
                None,
                None,
            )
            .await
            // A prefix matching nothing is a 404 from `_cat`, not an error the
            // user needs to see.
            .or_else(not_found_is_empty_array)?;
        Ok(parse_cat_indices(&json))
    }

    async fn list_aliases(&self, opts: &ListOpts) -> Result<Vec<String>, DbError> {
        let expression = match opts.prefix.as_deref() {
            Some(p) if !p.is_empty() => format!("/{}*", encode_index_expression(p)?),
            _ => String::new(),
        };
        let json = self
            .http
            .request(
                Method::Get,
                &format!("/_alias{expression}"),
                &[],
                None,
                None,
                None,
            )
            .await
            .or_else(not_found_is_empty_object)?;
        Ok(parse_aliases(&json))
    }

    async fn list_data_streams(&self, opts: &ListOpts) -> Result<Vec<String>, DbError> {
        let expression = match opts.prefix.as_deref() {
            Some(p) if !p.is_empty() => format!("/{}*", encode_index_expression(p)?),
            _ => String::new(),
        };
        // Data streams landed in ES 7.9 / OpenSearch 2.0; on anything older
        // (or with the feature disabled) this 400s or 404s, which is not an
        // error the browser should surface.
        let json = match self
            .http
            .request(
                Method::Get,
                &format!("/_data_stream{expression}"),
                &[],
                None,
                None,
                None,
            )
            .await
        {
            Ok(json) => json,
            Err(_) => return Ok(Vec::new()),
        };
        Ok(json
            .get("data_streams")
            .and_then(Json::as_array)
            .map(|streams| {
                streams
                    .iter()
                    .filter_map(|s| s.get("name").and_then(Json::as_str))
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default())
    }

    async fn top_level(&self, opts: &ListOpts) -> Result<Page<ObjectNode>, DbError> {
        let indices = self.list_indices(opts).await?;
        let aliases = self.list_aliases(opts).await?;
        let streams = self.list_data_streams(opts).await?;

        let mut nodes: Vec<ObjectNode> = Vec::new();
        for idx in indices {
            nodes.push(ObjectNode {
                path: ObjectPath::new(vec![Arc::from(idx.name.as_str())]),
                kind: ObjectKind::Collection,
                has_children: true,
                comment: Some(Arc::from(
                    format!(
                        "index · {} docs · {} · health {}",
                        idx.docs.as_deref().unwrap_or("?"),
                        idx.store.as_deref().unwrap_or("?"),
                        idx.health.as_deref().unwrap_or("?")
                    )
                    .as_str(),
                )),
            });
        }
        for alias in aliases {
            nodes.push(ObjectNode {
                path: ObjectPath::new(vec![Arc::from(alias.as_str())]),
                kind: ObjectKind::View,
                has_children: true,
                comment: Some(Arc::from("alias")),
            });
        }
        for stream in streams {
            nodes.push(ObjectNode {
                path: ObjectPath::new(vec![Arc::from(stream.as_str())]),
                kind: ObjectKind::Collection,
                has_children: true,
                comment: Some(Arc::from("data stream")),
            });
        }

        Ok(paginate_by_name(nodes, opts))
    }

    async fn list_fields(&self, index: &str, opts: &ListOpts) -> Result<Page<ObjectNode>, DbError> {
        let types = self.mapping(index).await?;
        let mut nodes: Vec<ObjectNode> = types
            .paths()
            .filter(|(path, _, _)| match opts.prefix.as_deref() {
                Some(p) => path.starts_with(p),
                None => true,
            })
            .map(|(path, _, native)| ObjectNode {
                path: ObjectPath::new(vec![Arc::from(index), Arc::from(path)]),
                kind: ObjectKind::Field,
                has_children: false,
                comment: Some(native.clone()),
            })
            .collect();
        nodes.sort_by(|a, b| a.path.to_string().cmp(&b.path.to_string()));
        Ok(paginate_by_name(nodes, opts))
    }

    async fn index_stats(&self, index: &str) -> Result<Json, DbError> {
        self.http
            .request(
                Method::Get,
                &format!("/{}/_stats/store,docs", encode_index_expression(index)?),
                &[],
                None,
                None,
                None,
            )
            .await
    }

    async fn sample(&self, index: &str, sample_size: u32) -> Result<InferredSchema, DbError> {
        let size = sample_size.clamp(1, MAX_SAMPLE_SIZE);
        let types = self.mapping(index).await.unwrap_or_default();
        // `random_score` is Elasticsearch's `$sample`: a cheap randomised
        // ordering so inference is not biased toward whatever happens to sit
        // at the head of the index.
        let body = json!({
            "size": size,
            "track_total_hits": false,
            "query": { "function_score": { "query": { "match_all": {} }, "random_score": {} } }
        });
        let (response, _) = self
            .http
            .request_ordered(
                Method::Post,
                &format!("/{}/_search", encode_index_expression(index)?),
                &[],
                Some(&body),
                None,
                None,
            )
            .await?;

        let hits: Vec<OrderedJson> = response
            .get("hits")
            .and_then(|h| h.get("hits"))
            .and_then(OrderedJson::as_array)
            .map(<[OrderedJson]>::to_vec)
            .unwrap_or_default();
        let mut sampled = 0u64;
        let mut root: Vec<(Arc<str>, FieldTrie)> = Vec::new();
        for hit in &hits {
            let Some(source) = hit.get("_source") else {
                continue;
            };
            sampled += 1;
            record_source(&mut root, source, "", &types);
        }
        Ok(InferredSchema { sampled, root })
    }
}

/// One row of `_cat/indices?format=json`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CatIndex {
    pub name: String,
    pub health: Option<String>,
    pub status: Option<String>,
    pub docs: Option<String>,
    pub store: Option<String>,
}

pub fn parse_cat_indices(json: &Json) -> Vec<CatIndex> {
    let Some(rows) = json.as_array() else {
        return Vec::new();
    };
    let mut out: Vec<CatIndex> = rows
        .iter()
        .filter_map(|row| {
            let name = row.get("index").and_then(Json::as_str)?;
            Some(CatIndex {
                name: name.to_string(),
                health: row.get("health").and_then(Json::as_str).map(str::to_string),
                status: row.get("status").and_then(Json::as_str).map(str::to_string),
                docs: row
                    .get("docs.count")
                    .and_then(Json::as_str)
                    .map(str::to_string),
                store: row
                    .get("store.size")
                    .and_then(Json::as_str)
                    .map(str::to_string),
            })
        })
        .collect();
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

/// `GET /_alias` -> `{ "<index>": { "aliases": { "<alias>": {…} } } }`.
pub fn parse_aliases(json: &Json) -> Vec<String> {
    let Some(map) = json.as_object() else {
        return Vec::new();
    };
    let mut names: Vec<String> = map
        .values()
        .filter_map(|v| v.get("aliases").and_then(Json::as_object))
        .flat_map(|aliases| aliases.keys().cloned())
        .collect();
    names.sort();
    names.dedup();
    names
}

/// Merge every concrete index's `mappings.properties` in a `_mapping`
/// response. A wildcard or an alias resolves to several indices, and a field
/// present in any of them is a field the caller can query.
pub fn merge_mappings(json: &Json) -> FieldTypes {
    let mut merged = FieldTypes::new();
    let Some(map) = json.as_object() else {
        return merged;
    };
    for index in map.values() {
        let Some(props) = index.get("mappings").and_then(|m| m.get("properties")) else {
            continue;
        };
        let types = FieldTypes::from_properties(props);
        for (path, _, native) in types.paths() {
            merged.insert(path, native);
        }
    }
    merged
}

/// Record one `_source` into a [`FieldTrie`], recursing one level into
/// object-valued fields — the same shallow-but-honest `FieldTrie` the Mongo
/// driver's `record_doc` builds.
fn record_source(
    root: &mut Vec<(Arc<str>, FieldTrie)>,
    source: &OrderedJson,
    prefix: &str,
    types: &FieldTypes,
) {
    let Some(fields) = source.as_object() else {
        return;
    };
    for (k, v) in fields {
        let path = if prefix.is_empty() {
            k.clone()
        } else {
            format!("{prefix}.{k}")
        };
        let idx = match root
            .iter()
            .position(|(name, _)| name.as_ref() == k.as_str())
        {
            Some(i) => i,
            None => {
                root.push((Arc::from(k.as_str()), FieldTrie::default()));
                root.len() - 1
            }
        };
        let logical = json_to_value(v, &path, types)
            .logical_type()
            .unwrap_or(LogicalType::Unknown);
        root[idx].1.record(logical);
        if v.is_object() {
            record_source(&mut root[idx].1.children, v, &path, types);
        }
    }
}

/// Client-side keyset pagination over an already-sorted node list.
///
/// `_cat/indices` has no server-side paging, so a very wide cluster is paged
/// here — by *name*, not by offset, so a concurrently-created index cannot
/// make a page skip or repeat an entry.
fn paginate_by_name(mut nodes: Vec<ObjectNode>, opts: &ListOpts) -> Page<ObjectNode> {
    nodes.sort_by(|a, b| a.path.to_string().cmp(&b.path.to_string()));
    nodes.dedup_by(|a, b| a.path == b.path);
    if let Some(after) = opts.resume.as_ref().and_then(decode_name_token) {
        nodes.retain(|n| n.path.to_string() > after);
    }
    let limit = opts.limit.max(1) as usize;
    let truncated = nodes.len() > limit;
    nodes.truncate(limit);
    let next = if truncated {
        nodes
            .last()
            .map(|n| ResumeToken(n.path.to_string().into_bytes().into()))
    } else {
        None
    };
    Page { items: nodes, next }
}

fn decode_name_token(token: &ResumeToken) -> Option<String> {
    String::from_utf8(token.0.to_vec()).ok()
}

/// Percent-encode anything outside Elasticsearch's own legal index-name
/// character set, plus the `*`/`,`/`-`/`+` an index *expression* legitimately
/// uses.
///
/// Introspection requests get built from server-returned names: a catalog
/// path or a user-typed prefix reaches this function and then becomes part of
/// a URL path, so a `/` or a `..` in it must not be able to retarget the
/// request at a different endpoint.
pub fn encode_index_expression(expr: &str) -> Result<String, DbError> {
    if expr.is_empty() {
        return Err(DbError::Unsupported {
            feature: "an empty index expression".into(),
        });
    }
    let mut out = String::with_capacity(expr.len());
    for ch in expr.chars() {
        match ch {
            'a'..='z' | 'A'..='Z' | '0'..='9' | '-' | '_' | '.' | '+' | '*' | ',' => out.push(ch),
            other => {
                let mut buf = [0u8; 4];
                for b in other.encode_utf8(&mut buf).as_bytes() {
                    out.push('%');
                    out.push_str(&format!("{b:02X}"));
                }
            }
        }
    }
    Ok(out)
}

fn not_found_is_empty_array(err: DbError) -> Result<Json, DbError> {
    match &err {
        DbError::Query { code, .. } if code.as_deref() == Some("index_not_found_exception") => {
            Ok(Json::Array(Vec::new()))
        }
        _ => Err(err),
    }
}

fn not_found_is_empty_object(err: DbError) -> Result<Json, DbError> {
    match &err {
        DbError::Query { code, .. }
            if code.as_deref() == Some("index_not_found_exception")
                || code.as_deref() == Some("aliases_not_found_exception") =>
        {
            Ok(Json::Object(serde_json::Map::new()))
        }
        _ => Err(err),
    }
}

/// Build the `fields` and `indexes` JSON arrays `describe()` reports through
/// `ObjectDetail::extra`.
///
/// `indexes` follows the shape the other drivers are standardising on — one
/// object per index with `name`/`kind`/`fields`/`unique`/`primary`/
/// `definition`. For Elasticsearch it is necessarily sparse and a little
/// metaphorical: there is no secondary-index object to list, because *every
/// mapped searchable field is its own inverted index*. So the array reports
/// the document id as the primary key plus one entry per searchable field,
/// which is the closest true statement about what this engine can seek on.
pub fn describe_arrays(types: &FieldTypes) -> (Json, Json) {
    let mut fields: Vec<Json> = types
        .paths()
        .map(|(path, _, native)| json!({ "name": path, "type": native.as_ref() }))
        .collect();
    fields.sort_by(|a, b| a["name"].as_str().cmp(&b["name"].as_str()));

    let mut indexes: Vec<Json> = vec![json!({
        "name": "_id",
        "kind": "primary",
        "fields": ["_id"],
        "unique": true,
        "primary": true,
        "definition": "document id (Elasticsearch has no user-defined primary key)"
    })];
    let mut searchable: Vec<Json> = types
        .paths()
        // Container types are not themselves searchable.
        .filter(|(_, _, native)| !matches!(native.as_ref(), "object" | "nested"))
        .map(|(path, _, native)| {
            json!({
                "name": path,
                "kind": "inverted",
                "fields": [path],
                "unique": false,
                "primary": false,
                "definition": format!("mapped as {native}")
            })
        })
        .collect();
    searchable.sort_by(|a, b| a["name"].as_str().cmp(&b["name"].as_str()));
    indexes.extend(searchable);

    (Json::Array(fields), Json::Array(indexes))
}

fn stat_number(stats: &Json, path: &[&str]) -> Option<i64> {
    let mut cur = stats;
    for seg in path {
        cur = cur.get(seg)?;
    }
    cur.as_i64()
}

#[async_trait]
impl Catalog for EsCatalog {
    fn levels(&self) -> Vec<LevelDef> {
        vec![
            LevelDef {
                name: Arc::from("index"),
                kind: ObjectKind::Collection,
                // One `_cat/indices` call, narrowable by prefix server-side.
                enumeration: Enumeration::Cheap,
            },
            LevelDef {
                name: Arc::from("field"),
                kind: ObjectKind::Field,
                // One `_mapping` call scoped to a single index — cheap, and
                // crucially never the cluster-wide mapping.
                enumeration: Enumeration::Cheap,
            },
        ]
    }

    async fn children(
        &self,
        parent: &ObjectPath,
        opts: ListOpts,
    ) -> Result<Page<ObjectNode>, DbError> {
        match parent.parts() {
            [] => self.top_level(&opts).await,
            [index] => self.list_fields(index, &opts).await,
            _ => Err(DbError::Unsupported {
                feature: "catalog path deeper than index/field".into(),
            }),
        }
    }

    async fn describe(&self, path: &ObjectPath) -> Result<ObjectDetail, DbError> {
        match path.parts() {
            [index] => {
                let types = self.mapping(index).await?;
                let stats = self.index_stats(index).await.unwrap_or(Json::Null);
                let (fields, indexes) = describe_arrays(&types);

                let mut extra: Vec<(Arc<str>, Arc<str>)> = Vec::new();
                let mut push =
                    |k: &str, v: String| extra.push((Arc::from(k), Arc::from(v.as_str())));
                push("field_count", types.len().to_string());
                if let Some(n) = stat_number(&stats, &["_all", "primaries", "docs", "count"]) {
                    push("document_count", n.to_string());
                }
                if let Some(n) = stat_number(&stats, &["_all", "primaries", "docs", "deleted"]) {
                    push("documents_deleted", n.to_string());
                }
                if let Some(n) =
                    stat_number(&stats, &["_all", "primaries", "store", "size_in_bytes"])
                {
                    push("store_size_bytes", n.to_string());
                }
                if let Some(n) = stat_number(&stats, &["_all", "total", "store", "size_in_bytes"]) {
                    push("store_size_bytes_with_replicas", n.to_string());
                }
                // The honesty label that goes with `SCHEMA_DECLARED = false`.
                push(
                    "schema_source",
                    "_mapping (declared, but dynamic: documents may carry unmapped fields)".into(),
                );
                push("fields", fields.to_string());
                push("indexes", indexes.to_string());

                Ok(ObjectDetail {
                    node: ObjectNode {
                        path: path.clone(),
                        kind: ObjectKind::Collection,
                        has_children: true,
                        comment: None,
                    },
                    // See the module doc: a mapping is declared but not
                    // exhaustive, and `SCHEMA_DECLARED` is false, so this
                    // never fabricates a complete `RowSchema`.
                    schema: None,
                    extra,
                })
            }
            [index, field] => {
                let types = self.mapping(index).await?;
                let native = types.native(field);
                let mut extra: Vec<(Arc<str>, Arc<str>)> = vec![(
                    Arc::from("mapped"),
                    Arc::from(if native.is_some() { "true" } else { "false" }),
                )];
                if let Some(native) = native {
                    extra.push((Arc::from("type"), native));
                } else {
                    extra.push((
                        Arc::from("note"),
                        Arc::from(
                            "not present in this index's mapping; it may still exist in \
                             documents under a dynamic mapping",
                        ),
                    ));
                }
                Ok(ObjectDetail {
                    node: ObjectNode {
                        path: path.clone(),
                        kind: ObjectKind::Field,
                        has_children: false,
                        comment: None,
                    },
                    schema: None,
                    extra,
                })
            }
            _ => Err(DbError::Unsupported {
                feature: "describe() needs an [index] or [index, field] path".into(),
            }),
        }
    }

    async fn infer_shape(
        &self,
        path: &ObjectPath,
        sample_size: u32,
    ) -> Result<InferredSchema, DbError> {
        let [index] = path.parts() else {
            return Err(DbError::Unsupported {
                feature: "infer_shape() needs an [index] path".into(),
            });
        };
        let size = if sample_size == 0 {
            DEFAULT_SAMPLE_SIZE
        } else {
            sample_size
        };
        self.sample(index, size).await
    }

    async fn complete(&self, ctx: CompletionCtx) -> Result<Vec<Completion>, DbError> {
        let prefix = prefix_at_caret(&ctx.text, ctx.offset as usize);
        if prefix.is_empty() {
            return Ok(Vec::new());
        }
        match ctx.scope.as_ref().map(ObjectPath::parts) {
            // Inside an index: complete field names from that index's mapping.
            Some([index]) => {
                let types = self.mapping(index).await?;
                let mut out: Vec<Completion> = types
                    .paths()
                    .filter(|(path, _, _)| path.starts_with(&prefix))
                    .map(|(path, _, native)| Completion {
                        label: Arc::from(path),
                        kind: ObjectKind::Field,
                        detail: Some(native.clone()),
                    })
                    .collect();
                out.sort_by(|a, b| a.label.cmp(&b.label));
                out.truncate(COMPLETE_LIMIT);
                Ok(out)
            }
            // Otherwise: a bounded, server-side index-name prefix query.
            _ => {
                let indices = self
                    .list_indices(&ListOpts {
                        prefix: Some(Arc::from(prefix.as_str())),
                        limit: COMPLETE_LIMIT as u32,
                        resume: None,
                    })
                    .await?;
                Ok(indices
                    .into_iter()
                    .take(COMPLETE_LIMIT)
                    .map(|idx| Completion {
                        label: Arc::from(idx.name.as_str()),
                        kind: ObjectKind::Collection,
                        detail: idx.docs.map(|d| Arc::from(format!("{d} docs").as_str())),
                    })
                    .collect())
            }
        }
    }
}

/// Scan backwards from the caret over identifier characters (same convention
/// as the Postgres and Mongo drivers; `.`, `-` and `*` are included because
/// they are legal inside an index expression and a field path).
fn prefix_at_caret(text: &str, offset: usize) -> String {
    let bytes = text.as_bytes();
    let end = offset.min(bytes.len());
    let mut start = end;
    while start > 0
        && matches!(
            bytes[start - 1],
            b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'_' | b'-' | b'.' | b'*'
        )
    {
        start -= 1;
    }
    text[start..end].to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ordered(text: &str) -> OrderedJson {
        OrderedJson::parse(text).expect(text)
    }

    #[test]
    fn cat_indices_rows_are_parsed_and_sorted() {
        let json = json!([
            { "index": "logs-2026.08", "health": "green", "status": "open",
              "docs.count": "100000", "store.size": "12.3mb" },
            { "index": "events", "health": "yellow", "status": "open",
              "docs.count": "7", "store.size": "5.1kb" },
            { "not-an-index": true }
        ]);
        let parsed = parse_cat_indices(&json);
        assert_eq!(parsed.len(), 2, "malformed rows are skipped, not fatal");
        assert_eq!(parsed[0].name, "events");
        assert_eq!(parsed[1].name, "logs-2026.08");
        assert_eq!(parsed[1].docs.as_deref(), Some("100000"));
        assert_eq!(parsed[0].health.as_deref(), Some("yellow"));
    }

    #[test]
    fn aliases_are_collected_across_every_backing_index() {
        let json = json!({
            "logs-000001": { "aliases": { "logs": {}, "logs-write": {} } },
            "logs-000002": { "aliases": { "logs": {} } }
        });
        assert_eq!(parse_aliases(&json), vec!["logs", "logs-write"]);
        assert!(parse_aliases(&json!({})).is_empty());
    }

    #[test]
    fn mappings_from_several_concrete_indices_merge_into_one_field_set() {
        let json = json!({
            "logs-000001": { "mappings": { "properties": {
                "ts": { "type": "date" }, "msg": { "type": "text" } } } },
            "logs-000002": { "mappings": { "properties": {
                "ts": { "type": "date" }, "level": { "type": "keyword" } } } }
        });
        let merged = merge_mappings(&json);
        assert_eq!(merged.len(), 3);
        assert_eq!(merged.native("level").as_deref(), Some("keyword"));
        assert_eq!(merged.native("msg").as_deref(), Some("text"));
        assert!(merge_mappings(&json!(null)).is_empty());
    }

    /// A name that came back from the server (or was typed by the user)
    /// becomes part of a URL path, so it must not be able to retarget the
    /// request.
    #[test]
    fn index_expressions_cannot_escape_their_path_segment() {
        assert_eq!(
            encode_index_expression("logs-2026.08").unwrap(),
            "logs-2026.08"
        );
        assert_eq!(encode_index_expression("logs*").unwrap(), "logs*");
        assert_eq!(encode_index_expression("a,b").unwrap(), "a,b");
        assert_eq!(
            encode_index_expression("../_cluster/settings").unwrap(),
            "..%2F_cluster%2Fsettings",
            "a traversal attempt must stay inside one path segment"
        );
        assert_eq!(
            encode_index_expression("x/_delete_by_query").unwrap(),
            "x%2F_delete_by_query"
        );
        assert_eq!(encode_index_expression("a b").unwrap(), "a%20b");
        assert_eq!(encode_index_expression("a?q=1").unwrap(), "a%3Fq%3D1");
        assert!(encode_index_expression("").is_err());
    }

    fn node(name: &str) -> ObjectNode {
        ObjectNode {
            path: ObjectPath::new(vec![Arc::from(name)]),
            kind: ObjectKind::Collection,
            has_children: true,
            comment: None,
        }
    }

    #[test]
    fn listings_page_by_name_so_a_new_index_cannot_skip_an_entry() {
        let nodes: Vec<ObjectNode> = ["e", "a", "c", "b", "d"].iter().map(|n| node(n)).collect();
        let opts = ListOpts {
            prefix: None,
            limit: 2,
            resume: None,
        };
        let page = paginate_by_name(nodes.clone(), &opts);
        assert_eq!(
            page.items
                .iter()
                .map(|n| n.path.to_string())
                .collect::<Vec<_>>(),
            vec!["a", "b"]
        );
        let token = page.next.expect("more pages");
        assert_eq!(decode_name_token(&token).as_deref(), Some("b"));

        let page2 = paginate_by_name(
            nodes.clone(),
            &ListOpts {
                resume: Some(token),
                ..opts.clone()
            },
        );
        assert_eq!(
            page2
                .items
                .iter()
                .map(|n| n.path.to_string())
                .collect::<Vec<_>>(),
            vec!["c", "d"]
        );

        // The final page reports no continuation.
        let last = paginate_by_name(nodes, &ListOpts { limit: 100, ..opts });
        assert_eq!(last.items.len(), 5);
        assert!(last.next.is_none());
    }

    #[test]
    fn listings_deduplicate_an_alias_that_shares_a_name_with_nothing_else() {
        let nodes = vec![node("a"), node("a"), node("b")];
        let page = paginate_by_name(
            nodes,
            &ListOpts {
                prefix: None,
                limit: 10,
                resume: None,
            },
        );
        assert_eq!(page.items.len(), 2);
    }

    #[test]
    fn describe_arrays_report_fields_and_a_sparse_but_true_indexes_array() {
        let types = FieldTypes::from_properties(&json!({
            "ts": { "type": "date" },
            "addr": { "properties": { "city": { "type": "keyword" } } }
        }));
        let (fields, indexes) = describe_arrays(&types);
        let names: Vec<&str> = fields
            .as_array()
            .unwrap()
            .iter()
            .map(|f| f["name"].as_str().unwrap())
            .collect();
        assert_eq!(names, vec!["addr", "addr.city", "ts"]);

        let idx = indexes.as_array().unwrap();
        assert_eq!(idx[0]["name"], json!("_id"));
        assert_eq!(idx[0]["primary"], json!(true));
        assert_eq!(idx[0]["unique"], json!(true));
        // The `object` container is not itself searchable and is excluded.
        let idx_names: Vec<&str> = idx.iter().map(|i| i["name"].as_str().unwrap()).collect();
        assert_eq!(idx_names, vec!["_id", "addr.city", "ts"]);
        assert_eq!(idx[1]["kind"], json!("inverted"));
        assert!(idx[1]["definition"].as_str().unwrap().contains("keyword"));
    }

    #[test]
    fn field_trie_inference_keeps_heterogeneous_types_and_true_presence() {
        let types = FieldTypes::from_properties(&json!({ "age": { "type": "long" } }));
        let mut root: Vec<(Arc<str>, FieldTrie)> = Vec::new();
        record_source(&mut root, &ordered(r#"{"name":"a","age":30}"#), "", &types);
        record_source(&mut root, &ordered(r#"{"name":"b"}"#), "", &types);
        record_source(
            &mut root,
            &ordered(r#"{"name":"c","age":"thirty"}"#),
            "",
            &types,
        );

        let name = &root.iter().find(|(n, _)| n.as_ref() == "name").unwrap().1;
        let age = &root.iter().find(|(n, _)| n.as_ref() == "age").unwrap().1;
        assert_eq!(name.present, 3);
        assert_eq!(age.present, 2, "absent in doc 2 — absence is not a type");
        assert!((age.presence_ratio(3) - 2.0 / 3.0).abs() < 1e-9);
        assert_eq!(
            age.types,
            vec![(LogicalType::I64, 1), (LogicalType::Str, 1)],
            "a heterogeneous field stays visible, not coerced to a majority type"
        );
    }

    #[test]
    fn field_trie_inference_recurses_one_level_into_objects() {
        let types = FieldTypes::default();
        let mut root: Vec<(Arc<str>, FieldTrie)> = Vec::new();
        record_source(
            &mut root,
            &ordered(r#"{"address":{"city":"sg","zip":"000000"}}"#),
            "",
            &types,
        );
        let addr = &root
            .iter()
            .find(|(n, _)| n.as_ref() == "address")
            .unwrap()
            .1;
        assert_eq!(addr.types, vec![(LogicalType::Document, 1)]);
        assert_eq!(addr.children.len(), 2);
        assert_eq!(&*addr.children[0].0, "city");
    }

    #[test]
    fn a_missing_prefix_match_is_an_empty_listing_not_an_error() {
        let err = DbError::Query {
            code: Some("index_not_found_exception".into()),
            message: "no such index".into(),
            position: None,
        };
        assert_eq!(not_found_is_empty_array(err).unwrap(), json!([]));

        let other = DbError::Query {
            code: Some("security_exception".into()),
            message: "denied".into(),
            position: None,
        };
        assert!(not_found_is_empty_array(other).is_err());
    }

    #[test]
    fn prefix_at_caret_understands_index_and_field_characters() {
        assert_eq!(prefix_at_caret("GET /logs-2026", 14), "logs-2026");
        let text = "{\"query\":{\"term\":{\"addr.ci";
        assert_eq!(prefix_at_caret(text, text.len()), "addr.ci");
        assert_eq!(prefix_at_caret("", 0), "");
        assert_eq!(prefix_at_caret("x ", 2), "");
    }

    #[test]
    fn levels_are_two_deep_and_both_bounded() {
        let http = Arc::new(
            EsHttp::new(
                "http://127.0.0.1:9200".into(),
                crate::http::Auth::None,
                std::time::Duration::from_secs(1),
                false,
            )
            .unwrap(),
        );
        let catalog = EsCatalog::new(http, Arc::new(Mutex::new(HashMap::new())));
        let levels = catalog.levels();
        assert_eq!(levels.len(), 2);
        assert_eq!(&*levels[0].name, "index");
        assert_eq!(&*levels[1].name, "field");
        assert!(levels.iter().all(|l| l.enumeration == Enumeration::Cheap));
    }
}
