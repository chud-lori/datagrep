//! [`MongoCatalog`] (ticket item 5, design §5.1: lazy, on-demand browsing —
//! never an eager whole-catalog index). `database` and `collection` levels
//! are cheap server enumerations (`listDatabases`/`listCollections`); the
//! `field` level has no server-declared schema at all (`SCHEMA_DECLARED` is
//! false), so it is *inferred* by sampling — and always labeled as such.

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use bson::{doc, Bson, Document as BsonDocument};
use tokio::sync::Mutex;

use datagrep_api::catalog::{
    Catalog, Completion, CompletionCtx, Enumeration, FieldTrie, InferredSchema, LevelDef, ListOpts,
    ObjectDetail, ObjectKind, ObjectNode, Page,
};
use datagrep_api::error::DbError;
use datagrep_api::shape::{LogicalType, ObjectPath};

use crate::error::map_mongo_error;
use crate::value::bson_to_value;

/// Default `$sample` size for [`MongoCatalog::infer_shape`] (ticket item 5).
const DEFAULT_SAMPLE_SIZE: u32 = 500;
/// `complete()`'s field-name candidate cap (ticket item 5).
const COMPLETE_LIMIT: usize = 50;

pub struct MongoCatalog {
    client: mongodb::Client,
    default_database: String,
    /// Cache of the most recent [`InferredSchema`] per `(database,
    /// collection)`, seeded by [`MongoCatalog::infer_shape`] and reused by
    /// both the field level of `children()` and `complete()` (ticket item 5:
    /// "field names from cached inference").
    field_cache: Arc<Mutex<HashMap<(String, String), InferredSchema>>>,
}

impl MongoCatalog {
    pub fn new(client: mongodb::Client, default_database: String) -> Self {
        Self {
            client,
            default_database,
            field_cache: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    fn resolve<'a>(&self, path: &'a ObjectPath) -> Result<Vec<&'a Arc<str>>, DbError> {
        Ok(path.parts().iter().collect())
    }

    async fn list_databases(&self, opts: &ListOpts) -> Result<Vec<ObjectNode>, DbError> {
        let specs = self
            .client
            .list_databases()
            .await
            .map_err(map_mongo_error)?;
        let mut names: Vec<String> = specs
            .into_iter()
            .map(|s| s.name)
            .filter(|n| matches_prefix(n, &opts.prefix))
            .collect();
        names.sort();
        names.truncate(opts.limit as usize);
        Ok(names
            .into_iter()
            .map(|name| ObjectNode {
                path: ObjectPath::new(vec![Arc::from(name)]),
                kind: ObjectKind::Database,
                has_children: true,
                comment: None,
            })
            .collect())
    }

    async fn list_collections(
        &self,
        db: &str,
        opts: &ListOpts,
    ) -> Result<Vec<ObjectNode>, DbError> {
        let names = self
            .client
            .database(db)
            .list_collection_names()
            .await
            .map_err(map_mongo_error)?;
        let mut names: Vec<String> = names
            .into_iter()
            .filter(|n| matches_prefix(n, &opts.prefix))
            .collect();
        names.sort();
        names.truncate(opts.limit as usize);
        Ok(names
            .into_iter()
            .map(|name| ObjectNode {
                path: ObjectPath::new(vec![Arc::from(db), Arc::from(name)]),
                kind: ObjectKind::Collection,
                has_children: true,
                comment: None,
            })
            .collect())
    }

    async fn list_fields(
        &self,
        db: &str,
        coll: &str,
        opts: &ListOpts,
    ) -> Result<Vec<ObjectNode>, DbError> {
        let schema = self.inferred(db, coll).await?;
        let mut names: Vec<&str> = schema
            .root
            .iter()
            .map(|(name, _)| name.as_ref())
            .filter(|n| matches_prefix(n, &opts.prefix))
            .collect();
        names.sort();
        names.truncate(opts.limit as usize);
        Ok(names
            .into_iter()
            .map(|name| ObjectNode {
                path: ObjectPath::new(vec![Arc::from(db), Arc::from(coll), Arc::from(name)]),
                kind: ObjectKind::Field,
                has_children: false,
                comment: None,
            })
            .collect())
    }

    /// Cached inference, populated lazily on first use (design §5.1: never
    /// eager). A cache hit costs nothing; a miss runs one `$sample` at the
    /// default size.
    async fn inferred(&self, db: &str, coll: &str) -> Result<InferredSchema, DbError> {
        let key = (db.to_string(), coll.to_string());
        if let Some(cached) = self.field_cache.lock().await.get(&key) {
            return Ok(cached.clone());
        }
        let schema = self.sample(db, coll, DEFAULT_SAMPLE_SIZE).await?;
        self.field_cache.lock().await.insert(key, schema.clone());
        Ok(schema)
    }

    /// All indexes of one collection via `listIndexes` (the official
    /// driver's typed `list_indexes`), with per-index on-disk sizes taken
    /// from the `collStats` reply the caller already paid for.
    async fn list_index_entries(
        &self,
        db: &str,
        coll: &str,
        index_sizes: Option<&BsonDocument>,
    ) -> Result<Vec<MongoIndexInfo>, DbError> {
        let collection = self.client.database(db).collection::<BsonDocument>(coll);
        let mut cursor = collection.list_indexes().await.map_err(map_mongo_error)?;
        let mut out = Vec::new();
        while cursor.advance().await.map_err(map_mongo_error)? {
            let model = cursor.deserialize_current().map_err(map_mongo_error)?;
            let name = model
                .options
                .as_ref()
                .and_then(|o| o.name.clone())
                .unwrap_or_default();
            let size_bytes = index_sizes
                .and_then(|sizes| sizes.get(&name))
                .and_then(bson_as_i64);
            let opts = model.options.as_ref();
            out.push(MongoIndexInfo {
                // `_id_` is unique by definition even though `listIndexes`
                // does not spell it out, and it is the closest thing a
                // collection has to a primary key.
                unique: opts.and_then(|o| o.unique).unwrap_or(false) || name == "_id_",
                primary: name == "_id_",
                sparse: opts.and_then(|o| o.sparse).unwrap_or(false),
                filter_json: opts
                    .and_then(|o| o.partial_filter_expression.as_ref())
                    .map(document_json),
                expire_after_seconds: opts
                    .and_then(|o| o.expire_after)
                    .map(|d| d.as_secs() as i64),
                keys: model
                    .keys
                    .iter()
                    .map(|(k, v)| (k.clone(), key_order(v)))
                    .collect(),
                name,
                size_bytes,
            });
        }
        Ok(out)
    }

    async fn sample(
        &self,
        db: &str,
        coll: &str,
        sample_size: u32,
    ) -> Result<InferredSchema, DbError> {
        let collection = self.client.database(db).collection::<BsonDocument>(coll);
        let pipeline = vec![doc! { "$sample": { "size": sample_size as i64 } }];
        let mut cursor = collection
            .aggregate(pipeline)
            .await
            .map_err(map_mongo_error)?;
        let mut sampled = 0u64;
        let mut root: Vec<(Arc<str>, FieldTrie)> = Vec::new();
        while cursor.advance().await.map_err(map_mongo_error)? {
            let doc: BsonDocument = cursor.deserialize_current().map_err(map_mongo_error)?;
            sampled += 1;
            record_doc(&mut root, &doc);
        }
        Ok(InferredSchema { sampled, root })
    }
}

/// Record one sampled document's top-level fields into `root`, recursing one
/// level into document-valued fields so nested paths ("address.city") are
/// visible too — a shallow but honest reading of design §3.1's `FieldTrie`
/// (full arbitrary-depth recursion is a grid/view-projection concern above
/// this seam, per the design's `ViewProjection` note).
fn record_doc(root: &mut Vec<(Arc<str>, FieldTrie)>, doc: &BsonDocument) {
    for (k, v) in doc.iter() {
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
        let logical = bson_to_value(v).logical_type().unwrap_or(LogicalType::Null);
        root[idx].1.record(logical);
        if let Bson::Document(nested) = v {
            record_doc(&mut root[idx].1.children, nested);
        }
    }
}

fn matches_prefix(name: &str, prefix: &Option<Arc<str>>) -> bool {
    match prefix {
        Some(p) => name.starts_with(p.as_ref()),
        None => true,
    }
}

#[async_trait]
impl Catalog for MongoCatalog {
    fn levels(&self) -> Vec<LevelDef> {
        vec![
            LevelDef {
                name: Arc::from("database"),
                kind: ObjectKind::Database,
                enumeration: Enumeration::Cheap,
            },
            LevelDef {
                name: Arc::from("collection"),
                kind: ObjectKind::Collection,
                enumeration: Enumeration::Cheap,
            },
            LevelDef {
                name: Arc::from("field"),
                kind: ObjectKind::Field,
                // Never free: listing fields means sampling documents.
                // `OnDemand` keeps the UI from auto-expanding into a
                // `$sample` aggregation just because a triangle got drawn
                // (design's `Enumeration` doc: "stops a KEYS * ... because
                // someone clicked a triangle" — the Mongo-shaped version of
                // that same mistake is auto-sampling every collection).
                enumeration: Enumeration::OnDemand,
            },
        ]
    }

    async fn children(
        &self,
        parent: &ObjectPath,
        opts: ListOpts,
    ) -> Result<Page<ObjectNode>, DbError> {
        let parts = self.resolve(parent)?;
        let items = match parts.as_slice() {
            [] => self.list_databases(&opts).await?,
            [db] => self.list_collections(db, &opts).await?,
            [db, coll] => self.list_fields(db, coll, &opts).await?,
            _ => {
                return Err(DbError::Unsupported {
                    feature: "catalog path deeper than field level".into(),
                })
            }
        };
        // v1 simplification: one fetch, client-side prefix/limit filtering,
        // no resumable server-side page token — database/collection counts
        // are small enough on virtually every real deployment that this
        // never approaches the cost of Postgres's thousand-table case. See
        // the crate report's deviations.
        Ok(Page { items, next: None })
    }

    async fn describe(&self, path: &ObjectPath) -> Result<ObjectDetail, DbError> {
        let parts = self.resolve(path)?;
        match parts.as_slice() {
            [db] => Ok(ObjectDetail {
                node: ObjectNode {
                    path: path.clone(),
                    kind: ObjectKind::Database,
                    has_children: true,
                    comment: None,
                },
                schema: None,
                extra: vec![(Arc::from("database"), Arc::from(db.as_ref()))],
            }),
            [db, coll] => {
                let result = self
                    .client
                    .database(db)
                    .run_command(doc! { "collStats": coll.as_ref() })
                    .await
                    .map_err(map_mongo_error)?;
                let mut extra = vec![(Arc::from("inferred_schema"), Arc::from("true"))];
                for (key, label) in [
                    ("count", "row_estimate"),
                    ("size", "size_bytes"),
                    ("storageSize", "storage_size_bytes"),
                    ("avgObjSize", "avg_document_size_bytes"),
                ] {
                    if let Some(v) = result.get(key) {
                        extra.push((Arc::from(label), Arc::from(display_number(v))));
                    }
                }
                // Real indexes via `listIndexes`, fetched here and only here —
                // on the explicit `describe()` of this one collection (design
                // §5.1: lazy; never during tree expansion, never on connect).
                // Sizes come from the `collStats` reply already in hand.
                let index_sizes = result.get_document("indexSizes").ok();
                let indexes = self.list_index_entries(db, coll, index_sizes).await?;
                extra.push((Arc::from("indexes"), Arc::from(indexes_json(&indexes))));
                // The field list is *inferred from a sample* (`$sample`,
                // cached per collection) — Mongo declares no schema, and the
                // payload says so explicitly (`inferred_schema`/`sampled_docs`)
                // so no UI can present inference as ground truth.
                let inferred = self.inferred(db, coll).await?;
                extra.push((
                    Arc::from("sampled_docs"),
                    Arc::from(inferred.sampled.to_string()),
                ));
                extra.push((
                    Arc::from("inferred_columns"),
                    Arc::from(inferred_columns_json(&inferred)),
                ));
                Ok(ObjectDetail {
                    node: ObjectNode {
                        path: path.clone(),
                        kind: ObjectKind::Collection,
                        has_children: true,
                        comment: None,
                    },
                    // `SCHEMA_DECLARED` is false: Mongo has no server-declared
                    // schema, so `describe()` never fabricates a `RowSchema`
                    // here — the sampled field list rides in `extra` under
                    // `inferred_columns`, labeled as inference.
                    schema: None,
                    extra,
                })
            }
            [db, coll, field] => {
                let schema = self.inferred(db, coll).await?;
                let trie = schema
                    .root
                    .iter()
                    .find(|(name, _)| name.as_ref() == field.as_ref())
                    .map(|(_, t)| t);
                let mut extra = vec![(Arc::from("inferred_schema"), Arc::from("true"))];
                if let Some(trie) = trie {
                    extra.push((
                        Arc::from("presence_ratio"),
                        Arc::from(format!("{:.3}", trie.presence_ratio(schema.sampled))),
                    ));
                    for (ty, n) in &trie.types {
                        extra.push((Arc::from(format!("type:{ty:?}")), Arc::from(n.to_string())));
                    }
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
                feature: "describe() needs a database/collection[/field] path".into(),
            }),
        }
    }

    async fn infer_shape(
        &self,
        path: &ObjectPath,
        sample_size: u32,
    ) -> Result<InferredSchema, DbError> {
        let parts = self.resolve(path)?;
        let (db, coll) = match parts.as_slice() {
            [coll] => (self.default_database.as_str(), coll.as_ref()),
            [db, coll] => (db.as_ref(), coll.as_ref()),
            _ => {
                return Err(DbError::Unsupported {
                    feature: "infer_shape() needs a [database, collection] or [collection] path"
                        .into(),
                })
            }
        };
        let schema = self.sample(db, coll, sample_size.max(1)).await?;
        self.field_cache
            .lock()
            .await
            .insert((db.to_string(), coll.to_string()), schema.clone());
        Ok(schema)
    }

    async fn complete(&self, ctx: CompletionCtx) -> Result<Vec<Completion>, DbError> {
        let prefix = prefix_at_caret(&ctx.text, ctx.offset as usize);
        if prefix.is_empty() {
            return Ok(Vec::new());
        }
        match ctx.scope.as_ref().map(|s| s.parts()) {
            Some([db, coll]) => {
                let schema = self.inferred(db, coll).await?;
                let mut out: Vec<Completion> = schema
                    .root
                    .iter()
                    .filter(|(name, _)| name.starts_with(&prefix))
                    .take(COMPLETE_LIMIT)
                    .map(|(name, trie)| Completion {
                        label: name.clone(),
                        kind: ObjectKind::Field,
                        detail: trie
                            .types
                            .first()
                            .map(|(ty, _)| Arc::from(format!("{ty:?}"))),
                    })
                    .collect();
                out.truncate(COMPLETE_LIMIT);
                Ok(out)
            }
            _ => {
                let db = ctx
                    .scope
                    .as_ref()
                    .and_then(|s| s.parts().first())
                    .map(|d| d.to_string())
                    .unwrap_or_else(|| self.default_database.clone());
                let names = self
                    .client
                    .database(&db)
                    .list_collection_names()
                    .await
                    .map_err(map_mongo_error)?;
                Ok(names
                    .into_iter()
                    .filter(|n| n.starts_with(&prefix))
                    .take(COMPLETE_LIMIT)
                    .map(|name| Completion {
                        label: Arc::from(name),
                        kind: ObjectKind::Collection,
                        detail: None,
                    })
                    .collect())
            }
        }
    }
}

/// One index of a collection, flattened from the driver's `IndexModel` for
/// [`indexes_json`] — kept as a plain struct so the JSON shape is unit
/// testable without a server.
struct MongoIndexInfo {
    name: String,
    /// Key order as declared. `MongoKeyOrder::Other` carries the non-b-tree
    /// key kinds (`"text"`, `"2dsphere"`, `"hashed"`, …).
    keys: Vec<(String, MongoKeyOrder)>,
    unique: bool,
    primary: bool,
    sparse: bool,
    /// `partialFilterExpression`, already rendered to JSON text.
    filter_json: Option<String>,
    /// TTL (`expireAfterSeconds`).
    expire_after_seconds: Option<i64>,
    size_bytes: Option<i64>,
}

enum MongoKeyOrder {
    Ascending,
    Descending,
    Other(String),
}

fn key_order(v: &Bson) -> MongoKeyOrder {
    let numeric = match v {
        Bson::Int32(n) => Some(f64::from(*n)),
        Bson::Int64(n) => Some(*n as f64),
        Bson::Double(f) => Some(*f),
        _ => None,
    };
    match (numeric, v) {
        (Some(n), _) if n < 0.0 => MongoKeyOrder::Descending,
        (Some(_), _) => MongoKeyOrder::Ascending,
        (None, Bson::String(s)) => MongoKeyOrder::Other(s.clone()),
        (None, other) => MongoKeyOrder::Other(other.to_string()),
    }
}

/// The engine-independent index JSON shape (see the datagrep-ffi describe
/// contract): `[{name, columns:[{name, order}], unique, primary, type,
/// partial, filter, size_bytes, definition, sparse, expire_after_seconds}]`.
///
/// Mongo specifics: a special key kind (`text`, `2dsphere`, `hashed`)
/// becomes the index's `type` and that key's `order` is `null` (direction is
/// a b-tree concept); everything else is `"btree"`, which is what a regular
/// Mongo index is. `definition` is `null` — Mongo has no DDL text to show.
fn indexes_json(indexes: &[MongoIndexInfo]) -> String {
    let entries: Vec<String> = indexes
        .iter()
        .map(|ix| {
            let cols: Vec<String> = ix
                .keys
                .iter()
                .map(|(name, order)| {
                    let order = match order {
                        MongoKeyOrder::Ascending => "\"asc\"".to_string(),
                        MongoKeyOrder::Descending => "\"desc\"".to_string(),
                        MongoKeyOrder::Other(_) => "null".to_string(),
                    };
                    format!("{{\"name\":{},\"order\":{}}}", json_str(name), order)
                })
                .collect();
            let ty = ix
                .keys
                .iter()
                .find_map(|(_, order)| match order {
                    MongoKeyOrder::Other(kind) => Some(kind.as_str()),
                    _ => None,
                })
                .unwrap_or("btree");
            format!(
                "{{\"name\":{},\"columns\":[{}],\"unique\":{},\"primary\":{},\
                 \"type\":{},\"partial\":{},\"filter\":{},\"size_bytes\":{},\
                 \"definition\":null,\"sparse\":{},\"expire_after_seconds\":{}}}",
                json_str(&ix.name),
                cols.join(","),
                ix.unique,
                ix.primary,
                json_str(ty),
                ix.filter_json.is_some(),
                ix.filter_json.as_deref().unwrap_or("null"),
                ix.size_bytes
                    .map_or_else(|| "null".to_string(), |n| n.to_string()),
                ix.sparse,
                ix.expire_after_seconds
                    .map_or_else(|| "null".to_string(), |n| n.to_string()),
            )
        })
        .collect();
    format!("[{}]", entries.join(","))
}

/// The sampled field list in the same column shape declared-schema engines
/// use, so the UI needs one renderer — with `presence_ratio` carrying the
/// honesty (a field seen in 60% of sampled documents is not a column).
fn inferred_columns_json(schema: &InferredSchema) -> String {
    let entries: Vec<String> = schema
        .root
        .iter()
        .enumerate()
        .map(|(ordinal, (name, trie))| {
            let mut types: Vec<(LogicalType, u64)> = trie.types.clone();
            types.sort_by(|a, b| b.1.cmp(&a.1));
            let non_null: Vec<String> = types
                .iter()
                .filter(|(ty, _)| *ty != LogicalType::Null)
                .map(|(ty, _)| format!("{ty:?}"))
                .collect();
            let logical = if non_null.is_empty() {
                "Null".to_string()
            } else {
                non_null.join(" | ")
            };
            let nullable = trie.present < schema.sampled
                || types.iter().any(|(ty, _)| *ty == LogicalType::Null);
            let is_id = name.as_ref() == "_id";
            format!(
                "{{\"name\":{},\"native_type\":null,\"logical_type\":{},\
                 \"nullable\":{},\"default\":null,\"primary_key\":{},\"unique\":{},\
                 \"indexed\":{},\"auto_generated\":false,\"ordinal\":{},\
                 \"presence_ratio\":{:.4}}}",
                json_str(name),
                json_str(&logical),
                nullable,
                is_id,
                is_id,
                is_id,
                ordinal,
                trie.presence_ratio(schema.sampled),
            )
        })
        .collect();
    format!("[{}]", entries.join(","))
}

/// A BSON document as JSON text, covering the types a
/// `partialFilterExpression` realistically contains; anything exotic
/// degrades to its display string rather than lying or failing.
fn document_json(doc: &BsonDocument) -> String {
    let entries: Vec<String> = doc
        .iter()
        .map(|(k, v)| format!("{}:{}", json_str(k), bson_json(v)))
        .collect();
    format!("{{{}}}", entries.join(","))
}

fn bson_json(v: &Bson) -> String {
    match v {
        Bson::Null => "null".to_string(),
        Bson::Boolean(b) => b.to_string(),
        Bson::Int32(n) => n.to_string(),
        Bson::Int64(n) => n.to_string(),
        Bson::Double(f) if f.is_finite() => format!("{f}"),
        Bson::String(s) => json_str(s),
        Bson::Array(items) => {
            let inner: Vec<String> = items.iter().map(bson_json).collect();
            format!("[{}]", inner.join(","))
        }
        Bson::Document(d) => document_json(d),
        other => json_str(&other.to_string()),
    }
}

fn bson_as_i64(v: &Bson) -> Option<i64> {
    match v {
        Bson::Int32(n) => Some(i64::from(*n)),
        Bson::Int64(n) => Some(*n),
        Bson::Double(f) => Some(*f as i64),
        _ => None,
    }
}

/// Minimal JSON string encoding. Hand-rolled on purpose: drivers keep
/// `serde_json` out of their runtime dependency set (see the workspace
/// dependency notes), and the catalog only ever needs to *emit* JSON here.
fn json_str(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => {
                out.push_str(&format!("\\u{:04x}", c as u32));
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

fn display_number(b: &Bson) -> String {
    match b {
        Bson::Int32(n) => n.to_string(),
        Bson::Int64(n) => n.to_string(),
        Bson::Double(f) => format!("{f}"),
        other => format!("{other}"),
    }
}

/// Scan backwards from the caret over identifier characters (same convention
/// as `datagrep-drv-postgres::catalog::prefix_at_caret`).
fn prefix_at_caret(text: &str, offset: usize) -> String {
    let bytes = text.as_bytes();
    let end = offset.min(bytes.len());
    let mut start = end;
    while start > 0
        && matches!(bytes[start - 1], b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'_' | b'$')
    {
        start -= 1;
    }
    text[start..end].to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use datagrep_api::Value;

    #[test]
    fn prefix_at_caret_finds_identifier_before_cursor() {
        assert_eq!(prefix_at_caret("db.users.find({na", 18), "na");
        assert_eq!(prefix_at_caret("", 0), "");
    }

    /// FieldTrie inference over a synthetic heterogeneous document set,
    /// exercising exactly the shape ticket item 5/`record_doc` builds:
    /// presence ratios and mixed types stay visible rather than collapsing
    /// to a majority type (design §3.1).
    #[test]
    fn record_doc_builds_heterogeneous_field_trie_with_correct_presence() {
        let mut root: Vec<(Arc<str>, FieldTrie)> = Vec::new();
        // doc 1: {name: "a", age: 30}
        record_doc(&mut root, &doc! { "name": "a", "age": 30_i32 });
        // doc 2: {name: "b"} — no `age` at all
        record_doc(&mut root, &doc! { "name": "b" });
        // doc 3: {name: "c", age: "thirty"} — `age` heterogeneous: I64 vs Str
        record_doc(&mut root, &doc! { "name": "c", "age": "thirty" });

        let name_trie = &root.iter().find(|(n, _)| n.as_ref() == "name").unwrap().1;
        let age_trie = &root.iter().find(|(n, _)| n.as_ref() == "age").unwrap().1;

        assert_eq!(name_trie.present, 3);
        assert!((name_trie.presence_ratio(3) - 1.0).abs() < f64::EPSILON);

        assert_eq!(age_trie.present, 2, "age missing from doc 2");
        assert!((age_trie.presence_ratio(3) - (2.0 / 3.0)).abs() < 1e-9);
        assert_eq!(
            age_trie.types,
            vec![(LogicalType::I64, 1), (LogicalType::Str, 1)],
            "heterogeneous types both stay visible, not coerced to a majority"
        );
    }

    #[test]
    fn record_doc_recurses_one_level_into_nested_documents() {
        let mut root: Vec<(Arc<str>, FieldTrie)> = Vec::new();
        record_doc(
            &mut root,
            &doc! { "address": { "city": "sg", "zip": "000000" } },
        );
        let address = &root
            .iter()
            .find(|(n, _)| n.as_ref() == "address")
            .unwrap()
            .1;
        assert_eq!(address.types, vec![(LogicalType::Document, 1)]);
        let city = address
            .children
            .iter()
            .find(|(n, _)| n.as_ref() == "city")
            .unwrap();
        assert_eq!(city.1.types, vec![(LogicalType::Str, 1)]);
    }

    #[test]
    fn matches_prefix_filters_correctly() {
        assert!(matches_prefix("users", &Some(Arc::from("us"))));
        assert!(!matches_prefix("orders", &Some(Arc::from("us"))));
        assert!(matches_prefix("anything", &None));
    }

    #[test]
    fn display_number_handles_bson_numeric_variants() {
        assert_eq!(display_number(&Bson::Int32(5)), "5");
        assert_eq!(display_number(&Bson::Int64(9)), "9");
        let _ = Value::I64(0); // keep datagrep_api::Value import used in this module
    }

    /// The index JSON shape, pinned by parsing (serde_json is dev-only):
    /// compound key order and 1/-1 directions survive, TTL rides in
    /// `expire_after_seconds`, partial filters ride in `filter`, and special
    /// key kinds become the index `type` with `order: null`.
    #[test]
    fn indexes_json_has_the_cross_engine_shape() {
        let indexes = vec![
            MongoIndexInfo {
                name: "_id_".into(),
                keys: vec![("_id".into(), key_order(&Bson::Int32(1)))],
                unique: true,
                primary: true,
                sparse: false,
                filter_json: None,
                expire_after_seconds: None,
                size_bytes: Some(20480),
            },
            MongoIndexInfo {
                name: "user_created".into(),
                keys: vec![
                    ("user_id".into(), key_order(&Bson::Int32(1))),
                    ("created_at".into(), key_order(&Bson::Int32(-1))),
                ],
                unique: false,
                primary: false,
                sparse: true,
                filter_json: Some(document_json(
                    &doc! { "status": "active", "retries": { "$gt": 3 } },
                )),
                expire_after_seconds: Some(3600),
                size_bytes: None,
            },
            MongoIndexInfo {
                name: "title_text".into(),
                keys: vec![("title".into(), key_order(&Bson::String("text".into())))],
                unique: false,
                primary: false,
                sparse: false,
                filter_json: None,
                expire_after_seconds: None,
                size_bytes: Some(4096),
            },
        ];
        let parsed: serde_json::Value =
            serde_json::from_str(&indexes_json(&indexes)).expect("indexes_json emits valid JSON");
        let list = parsed.as_array().expect("a JSON array");
        assert_eq!(list.len(), 3);
        for entry in list {
            for key in [
                "name",
                "columns",
                "unique",
                "primary",
                "type",
                "partial",
                "filter",
                "size_bytes",
                "definition",
                "sparse",
                "expire_after_seconds",
            ] {
                assert!(entry.get(key).is_some(), "missing {key}: {entry}");
            }
        }
        assert_eq!(list[0]["primary"], true);
        assert_eq!(list[0]["unique"], true, "_id_ is unique by definition");
        assert_eq!(list[0]["size_bytes"], 20480);

        let compound = &list[1];
        assert_eq!(compound["columns"][0]["name"], "user_id");
        assert_eq!(compound["columns"][0]["order"], "asc");
        assert_eq!(compound["columns"][1]["name"], "created_at");
        assert_eq!(compound["columns"][1]["order"], "desc");
        assert_eq!(compound["expire_after_seconds"], 3600);
        assert_eq!(compound["sparse"], true);
        assert_eq!(compound["partial"], true);
        assert_eq!(compound["filter"]["status"], "active");
        assert_eq!(compound["filter"]["retries"]["$gt"], 3);

        assert_eq!(list[2]["type"], "text");
        assert_eq!(
            list[2]["columns"][0]["order"],
            serde_json::Value::Null,
            "a text key has no direction; inventing one would be wrong"
        );
        assert_eq!(indexes_json(&[]), "[]");
    }

    /// The inferred field list keeps the declared-column JSON shape but says
    /// it is inference: heterogeneous types stay visible and the presence
    /// ratio rides along.
    #[test]
    fn inferred_columns_json_labels_inference_honestly() {
        let mut root: Vec<(Arc<str>, FieldTrie)> = Vec::new();
        record_doc(
            &mut root,
            &doc! { "_id": 1_i32, "name": "a", "age": 30_i32 },
        );
        record_doc(&mut root, &doc! { "_id": 2_i32, "name": "b" });
        record_doc(
            &mut root,
            &doc! { "_id": 3_i32, "name": "c", "age": "thirty" },
        );
        let schema = InferredSchema { sampled: 3, root };
        let parsed: serde_json::Value = serde_json::from_str(&inferred_columns_json(&schema))
            .expect("inferred_columns_json emits valid JSON");
        let list = parsed.as_array().expect("a JSON array");
        assert_eq!(list.len(), 3);

        let id = &list[0];
        assert_eq!(id["name"], "_id");
        assert_eq!(id["primary_key"], true);
        assert_eq!(id["nullable"], false);
        assert_eq!(id["ordinal"], 0);
        assert_eq!(id["presence_ratio"], 1.0);

        let age = list
            .iter()
            .find(|c| c["name"] == "age")
            .expect("age column");
        assert_eq!(
            age["logical_type"], "I64 | Str",
            "heterogeneous types stay visible, not coerced to a majority"
        );
        assert_eq!(age["nullable"], true, "absent in one sampled doc");
        assert!((age["presence_ratio"].as_f64().unwrap() - 2.0 / 3.0).abs() < 1e-3);
    }

    #[test]
    fn bson_json_escapes_and_nests() {
        assert_eq!(bson_json(&Bson::String("a\"b".into())), r#""a\"b""#);
        assert_eq!(bson_json(&Bson::Null), "null");
        assert_eq!(bson_json(&Bson::Boolean(true)), "true");
        let doc = doc! { "arr": [1_i32, "two"], "f": 1.5 };
        let parsed: serde_json::Value =
            serde_json::from_str(&bson_json(&Bson::Document(doc))).expect("valid JSON");
        assert_eq!(parsed["arr"][0], 1);
        assert_eq!(parsed["arr"][1], "two");
        assert_eq!(parsed["f"], 1.5);
    }
}
