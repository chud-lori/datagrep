//! [`PgCatalog`] (ticket item 5, design §5.1: lazy, one-cheap-query-at-a-time
//! browsing — never eager whole-catalog indexing). Every method below issues
//! exactly one parameterized query against `pg_catalog`/`information_schema`
//! equivalents, bounded by `ListOpts::limit`.

use std::sync::Arc;

use async_trait::async_trait;
use bytes::Bytes;
use tokio::sync::Mutex;
use tokio_postgres::Client;

use dbx_api::catalog::{
    Catalog, Completion, CompletionCtx, Enumeration, FieldTrie, InferredSchema, LevelDef, ListOpts,
    ObjectDetail, ObjectKind, ObjectNode, Page,
};
use dbx_api::driver::ResumeToken;
use dbx_api::error::DbError;
use dbx_api::shape::{FieldDef, FieldFlags, Identity, LogicalType, ObjectPath, RowSchema};

use crate::error::map_pg_error;
use crate::value::{decode_binary, logical_type_of};

pub struct PgCatalog {
    client: Arc<Mutex<Option<Client>>>,
}

impl PgCatalog {
    pub fn new(client: Arc<Mutex<Option<Client>>>) -> Self {
        Self { client }
    }

    async fn current_database(&self) -> Result<String, DbError> {
        let guard = self.client.lock().await;
        let client = guard.as_ref().ok_or(DbError::Closed)?;
        let row = client
            .query_one("SELECT current_database()", &[])
            .await
            .map_err(map_pg_error)?;
        Ok(row.get::<_, String>(0))
    }

    async fn require_current_database(&self, db: &str) -> Result<(), DbError> {
        let current = self.current_database().await?;
        if current != db {
            return Err(DbError::Unsupported {
                feature: format!(
                    "browsing database {db:?} over a connection to {current:?} — Postgres has no \
                     cross-database catalog access; open a new connection to {db:?} instead"
                ),
            });
        }
        Ok(())
    }
}

fn resume_prefix(resume: &Option<ResumeToken>) -> Option<String> {
    resume
        .as_ref()
        .and_then(|t| std::str::from_utf8(&t.0).ok().map(str::to_string))
}

fn next_token(items_len: usize, limit: u32, last_name: Option<&str>) -> Option<ResumeToken> {
    if items_len < limit as usize {
        return None;
    }
    last_name.map(|n| ResumeToken(Bytes::copy_from_slice(n.as_bytes())))
}

#[async_trait]
impl Catalog for PgCatalog {
    fn levels(&self) -> Vec<LevelDef> {
        vec![
            LevelDef {
                name: Arc::from("database"),
                kind: ObjectKind::Database,
                enumeration: Enumeration::Cheap,
            },
            LevelDef {
                name: Arc::from("schema"),
                kind: ObjectKind::Schema,
                enumeration: Enumeration::Cheap,
            },
            LevelDef {
                name: Arc::from("table"),
                kind: ObjectKind::Table,
                enumeration: Enumeration::Cheap,
            },
            LevelDef {
                name: Arc::from("column"),
                kind: ObjectKind::Column,
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
            [] => self.list_databases(opts).await,
            [db] => self.list_schemas(db, opts).await,
            [db, schema] => self.list_relations(db, schema, opts).await,
            [db, schema, table] => self.list_columns(db, schema, table, opts).await,
            _ => Err(DbError::Unsupported {
                feature: "catalog path deeper than column level".into(),
            }),
        }
    }

    async fn describe(&self, path: &ObjectPath) -> Result<ObjectDetail, DbError> {
        match path.parts() {
            [db, schema, table] => self.describe_relation(db, schema, table).await,
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
            [db, schema] => Ok(ObjectDetail {
                node: ObjectNode {
                    path: path.clone(),
                    kind: ObjectKind::Schema,
                    has_children: true,
                    comment: None,
                },
                schema: None,
                extra: vec![
                    (Arc::from("database"), Arc::from(db.as_ref())),
                    (Arc::from("schema"), Arc::from(schema.as_ref())),
                ],
            }),
            _ => Err(DbError::Unsupported {
                feature: "describe() needs a database/schema/table[/column] path".into(),
            }),
        }
    }

    async fn infer_shape(
        &self,
        path: &ObjectPath,
        sample_size: u32,
    ) -> Result<InferredSchema, DbError> {
        let table = crate::sql::quote_object_path(path)?;
        let guard = self.client.lock().await;
        let client = guard.as_ref().ok_or(DbError::Closed)?;
        let sql = format!("SELECT * FROM {table} LIMIT $1");
        let stmt = client.prepare(&sql).await.map_err(map_pg_error)?;
        let rows = client
            .query(&stmt, &[&(sample_size as i64)])
            .await
            .map_err(map_pg_error)?;
        let sampled = rows.len() as u64;
        let mut root: Vec<(Arc<str>, FieldTrie)> = stmt
            .columns()
            .iter()
            .map(|c| (Arc::from(c.name()), FieldTrie::default()))
            .collect();
        for row in &rows {
            for (i, col) in stmt.columns().iter().enumerate() {
                let value = row
                    .try_get::<_, Option<&[u8]>>(i)
                    .ok()
                    .flatten()
                    .map(|raw| decode_binary(col.type_(), raw));
                let logical = match value {
                    Some(v) => v.logical_type().unwrap_or(LogicalType::Null),
                    None => LogicalType::Null,
                };
                root[i].1.record(logical);
            }
        }
        Ok(InferredSchema { sampled, root })
    }

    async fn complete(&self, ctx: CompletionCtx) -> Result<Vec<Completion>, DbError> {
        let prefix = prefix_at_caret(&ctx.text, ctx.offset as usize);
        if prefix.is_empty() {
            return Ok(Vec::new());
        }
        let guard = self.client.lock().await;
        let client = guard.as_ref().ok_or(DbError::Closed)?;

        let mut out = Vec::new();
        let table_rows = client
            .query(
                "SELECT relname, n.nspname FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace \
                 WHERE c.relkind IN ('r','v','m','p') AND c.relname LIKE $1 || '%' \
                 ORDER BY c.relname LIMIT 25",
                &[&prefix],
            )
            .await
            .map_err(map_pg_error)?;
        for row in table_rows {
            let name: String = row.get(0);
            let schema: String = row.get(1);
            out.push(Completion {
                label: Arc::from(name),
                kind: ObjectKind::Table,
                detail: Some(Arc::from(schema)),
            });
        }

        let col_rows = client
            .query(
                "SELECT DISTINCT attname FROM pg_attribute WHERE attnum > 0 AND NOT attisdropped \
                 AND attname LIKE $1 || '%' ORDER BY attname LIMIT 25",
                &[&prefix],
            )
            .await
            .map_err(map_pg_error)?;
        for row in col_rows {
            let name: String = row.get(0);
            out.push(Completion {
                label: Arc::from(name),
                kind: ObjectKind::Column,
                detail: None,
            });
        }

        Ok(out)
    }
}

/// Scan backwards from the caret over identifier characters to find the
/// token being typed — the prefix fed to server-side `LIKE $1 || '%'`
/// completion (design item 5: "never a local index").
fn prefix_at_caret(text: &str, offset: usize) -> String {
    let bytes = text.as_bytes();
    let end = offset.min(bytes.len());
    let mut start = end;
    while start > 0 && matches!(bytes[start - 1], b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'_') {
        start -= 1;
    }
    text[start..end].to_string()
}

impl PgCatalog {
    async fn list_databases(&self, opts: ListOpts) -> Result<Page<ObjectNode>, DbError> {
        let prefix = opts.prefix.as_deref().unwrap_or("");
        let after = resume_prefix(&opts.resume).unwrap_or_default();
        let guard = self.client.lock().await;
        let client = guard.as_ref().ok_or(DbError::Closed)?;
        let rows = client
            .query(
                "SELECT datname FROM pg_database \
                 WHERE datistemplate = false AND datname LIKE $1 || '%' AND datname > $2 \
                 ORDER BY datname LIMIT $3",
                &[&prefix, &after, &(opts.limit as i64)],
            )
            .await
            .map_err(map_pg_error)?;
        let names: Vec<String> = rows.iter().map(|r| r.get(0)).collect();
        let last = names.last().cloned();
        let items = names
            .into_iter()
            .map(|name| ObjectNode {
                path: ObjectPath::new(vec![Arc::from(name)]),
                kind: ObjectKind::Database,
                has_children: true,
                comment: None,
            })
            .collect::<Vec<_>>();
        let next = next_token(items.len(), opts.limit, last.as_deref());
        Ok(Page { items, next })
    }

    async fn list_schemas(
        &self,
        db: &Arc<str>,
        opts: ListOpts,
    ) -> Result<Page<ObjectNode>, DbError> {
        self.require_current_database(db).await?;
        let prefix = opts.prefix.as_deref().unwrap_or("");
        let after = resume_prefix(&opts.resume).unwrap_or_default();
        let guard = self.client.lock().await;
        let client = guard.as_ref().ok_or(DbError::Closed)?;
        let rows = client
            .query(
                "SELECT nspname FROM pg_namespace \
                 WHERE nspname LIKE $1 || '%' AND nspname > $2 ORDER BY nspname LIMIT $3",
                &[&prefix, &after, &(opts.limit as i64)],
            )
            .await
            .map_err(map_pg_error)?;
        let names: Vec<String> = rows.iter().map(|r| r.get(0)).collect();
        let last = names.last().cloned();
        let items = names
            .into_iter()
            .map(|name| ObjectNode {
                path: ObjectPath::new(vec![db.clone(), Arc::from(name)]),
                kind: ObjectKind::Schema,
                has_children: true,
                comment: None,
            })
            .collect::<Vec<_>>();
        let next = next_token(items.len(), opts.limit, last.as_deref());
        Ok(Page { items, next })
    }

    async fn list_relations(
        &self,
        db: &Arc<str>,
        schema: &Arc<str>,
        opts: ListOpts,
    ) -> Result<Page<ObjectNode>, DbError> {
        self.require_current_database(db).await?;
        let prefix = opts.prefix.as_deref().unwrap_or("");
        let after = resume_prefix(&opts.resume).unwrap_or_default();
        let guard = self.client.lock().await;
        let client = guard.as_ref().ok_or(DbError::Closed)?;
        let rows = client
            .query(
                "SELECT c.relname, c.relkind FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace \
                 WHERE n.nspname = $1 AND c.relkind IN ('r','v','m','p','f') \
                 AND c.relname LIKE $2 || '%' AND c.relname > $3 \
                 ORDER BY c.relname LIMIT $4",
                &[&schema.as_ref(), &prefix, &after, &(opts.limit as i64)],
            )
            .await
            .map_err(map_pg_error)?;
        let mut names_kinds: Vec<(String, String)> = Vec::with_capacity(rows.len());
        for r in &rows {
            names_kinds.push((r.get(0), r.get(1)));
        }
        let last = names_kinds.last().map(|(n, _)| n.clone());
        let items = names_kinds
            .into_iter()
            .map(|(name, relkind)| ObjectNode {
                path: ObjectPath::new(vec![db.clone(), schema.clone(), Arc::from(name)]),
                kind: match relkind.as_str() {
                    "v" | "m" => ObjectKind::View,
                    _ => ObjectKind::Table,
                },
                has_children: true,
                comment: None,
            })
            .collect::<Vec<_>>();
        let next = next_token(items.len(), opts.limit, last.as_deref());
        Ok(Page { items, next })
    }

    async fn list_columns(
        &self,
        db: &Arc<str>,
        schema: &Arc<str>,
        table: &Arc<str>,
        opts: ListOpts,
    ) -> Result<Page<ObjectNode>, DbError> {
        self.require_current_database(db).await?;
        let prefix = opts.prefix.as_deref().unwrap_or("");
        let after = resume_prefix(&opts.resume).unwrap_or_default();
        let guard = self.client.lock().await;
        let client = guard.as_ref().ok_or(DbError::Closed)?;
        let rows = client
            .query(
                "SELECT a.attname FROM pg_attribute a \
                 JOIN pg_class c ON c.oid = a.attrelid \
                 JOIN pg_namespace n ON n.oid = c.relnamespace \
                 WHERE n.nspname = $1 AND c.relname = $2 AND a.attnum > 0 AND NOT a.attisdropped \
                 AND a.attname LIKE $3 || '%' AND a.attname > $4 \
                 ORDER BY a.attnum LIMIT $5",
                &[
                    &schema.as_ref(),
                    &table.as_ref(),
                    &prefix,
                    &after,
                    &(opts.limit as i64),
                ],
            )
            .await
            .map_err(map_pg_error)?;
        let names: Vec<String> = rows.iter().map(|r| r.get(0)).collect();
        let last = names.last().cloned();
        let items = names
            .into_iter()
            .map(|name| ObjectNode {
                path: ObjectPath::new(vec![
                    db.clone(),
                    schema.clone(),
                    table.clone(),
                    Arc::from(name),
                ]),
                kind: ObjectKind::Column,
                has_children: false,
                comment: None,
            })
            .collect::<Vec<_>>();
        let next = next_token(items.len(), opts.limit, last.as_deref());
        Ok(Page { items, next })
    }

    async fn describe_relation(
        &self,
        db: &Arc<str>,
        schema: &Arc<str>,
        table: &Arc<str>,
    ) -> Result<ObjectDetail, DbError> {
        self.require_current_database(db).await?;
        let guard = self.client.lock().await;
        let client = guard.as_ref().ok_or(DbError::Closed)?;

        // One cheap query for size facts (design item 5: "reltuples estimate
        // + pg_relation_size").
        let size_row = client
            .query_opt(
                "SELECT c.reltuples::float8, c.relkind, pg_relation_size(c.oid) \
                 FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace \
                 WHERE n.nspname = $1 AND c.relname = $2",
                &[&schema.as_ref(), &table.as_ref()],
            )
            .await
            .map_err(map_pg_error)?;
        let mut extra = Vec::new();
        let mut relkind = "r".to_string();
        if let Some(row) = &size_row {
            let reltuples: f64 = row.get(0);
            relkind = row.get(1);
            let size_bytes: i64 = row.get(2);
            extra.push((
                Arc::from("estimated_rows"),
                Arc::from(format!("{}", reltuples.max(0.0) as i64)),
            ));
            extra.push((Arc::from("size_bytes"), Arc::from(size_bytes.to_string())));
        }

        // Columns + types, for the declared RowSchema (`SCHEMA_DECLARED`).
        let col_rows = client
            .query(
                "SELECT a.attname, a.atttypid, a.attnotnull FROM pg_attribute a \
                 JOIN pg_class c ON c.oid = a.attrelid \
                 JOIN pg_namespace n ON n.oid = c.relnamespace \
                 WHERE n.nspname = $1 AND c.relname = $2 AND a.attnum > 0 AND NOT a.attisdropped \
                 ORDER BY a.attnum",
                &[&schema.as_ref(), &table.as_ref()],
            )
            .await
            .map_err(map_pg_error)?;
        let mut fields = Vec::with_capacity(col_rows.len());
        for row in &col_rows {
            let name: String = row.get(0);
            let oid: u32 = row.get(1);
            let not_null: bool = row.get(2);
            let ty = tokio_postgres::types::Type::from_oid(oid);
            let logical = ty
                .as_ref()
                .map(logical_type_of)
                .unwrap_or(LogicalType::Unknown);
            let mut flags = FieldFlags::empty();
            if !not_null {
                flags |= FieldFlags::NULLABLE;
            }
            fields.push(FieldDef {
                name: Arc::from(name),
                logical,
                flags,
                native_type: ty.as_ref().map(|t| Arc::from(t.name())),
            });
        }

        let pk_row = client
            .query(
                "SELECT a.attname FROM pg_index i \
                 JOIN pg_attribute a ON a.attrelid = i.indrelid AND a.attnum = ANY(i.indkey) \
                 JOIN pg_class c ON c.oid = i.indrelid \
                 JOIN pg_namespace n ON n.oid = c.relnamespace \
                 WHERE n.nspname = $1 AND c.relname = $2 AND i.indisprimary \
                 ORDER BY array_position(i.indkey, a.attnum)",
                &[&schema.as_ref(), &table.as_ref()],
            )
            .await
            .map_err(map_pg_error)?;
        let identity = if pk_row.is_empty() {
            None
        } else {
            let mut idx = Vec::with_capacity(pk_row.len());
            let mut ok = true;
            for r in &pk_row {
                let name: String = r.get(0);
                match fields.iter().position(|f| *f.name == name) {
                    Some(i) => idx.push(i as u32),
                    None => {
                        ok = false;
                        break;
                    }
                }
            }
            if ok {
                Some(Identity { field_indices: idx })
            } else {
                None
            }
        };

        Ok(ObjectDetail {
            node: ObjectNode {
                path: ObjectPath::new(vec![db.clone(), schema.clone(), table.clone()]),
                kind: match relkind.as_str() {
                    "v" | "m" => ObjectKind::View,
                    _ => ObjectKind::Table,
                },
                has_children: true,
                comment: None,
            },
            schema: Some(RowSchema { fields, identity }),
            extra,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prefix_at_caret_finds_identifier_before_cursor() {
        assert_eq!(prefix_at_caret("SELECT * FROM use", 18), "use");
        assert_eq!(prefix_at_caret("SELECT foo.b", 12), "b");
        assert_eq!(prefix_at_caret("SELECT ", 7), "");
        assert_eq!(prefix_at_caret("", 0), "");
    }

    #[test]
    fn next_token_only_when_page_is_full() {
        assert_eq!(
            next_token(3, 10, Some("x")),
            None,
            "short page means no more data"
        );
        assert!(next_token(10, 10, Some("x")).is_some());
        assert_eq!(next_token(10, 10, None), None, "no name to resume from");
    }
}
