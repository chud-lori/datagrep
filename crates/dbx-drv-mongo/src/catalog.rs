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

use dbx_api::catalog::{
    Catalog, Completion, CompletionCtx, Enumeration, FieldTrie, InferredSchema, LevelDef, ListOpts,
    ObjectDetail, ObjectKind, ObjectNode, Page,
};
use dbx_api::error::DbError;
use dbx_api::shape::{LogicalType, ObjectPath};

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
                    ("count", "document_count"),
                    ("size", "size_bytes"),
                    ("storageSize", "storage_size_bytes"),
                    ("avgObjSize", "avg_document_size_bytes"),
                    ("nindexes", "index_count"),
                ] {
                    if let Some(v) = result.get(key) {
                        extra.push((Arc::from(label), Arc::from(display_number(v))));
                    }
                }
                Ok(ObjectDetail {
                    node: ObjectNode {
                        path: path.clone(),
                        kind: ObjectKind::Collection,
                        has_children: true,
                        comment: None,
                    },
                    // `SCHEMA_DECLARED` is false: Mongo has no server-declared
                    // schema, so `describe()` never fabricates a `RowSchema`
                    // here — inference is a separate, explicitly-labeled call.
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

fn display_number(b: &Bson) -> String {
    match b {
        Bson::Int32(n) => n.to_string(),
        Bson::Int64(n) => n.to_string(),
        Bson::Double(f) => format!("{f}"),
        other => format!("{other}"),
    }
}

/// Scan backwards from the caret over identifier characters (same convention
/// as `dbx-drv-postgres::catalog::prefix_at_caret`).
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
    use dbx_api::Value;

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
        let _ = Value::I64(0); // keep dbx_api::Value import used in this module
    }
}
