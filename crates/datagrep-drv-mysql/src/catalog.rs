//! [`MySqlCatalog`]: lazy browsing — one cheap bounded query per level, never
//! a crawl of the whole catalog. MySQL's namespace is honestly two levels
//! (`database` → `table`) plus columns; there is no schema tier and this
//! catalog does not fake one.

use std::sync::Arc;

use async_trait::async_trait;
use bytes::Bytes;
use mysql_async::prelude::Queryable;
use mysql_async::Conn;
use tokio::sync::Mutex;

use datagrep_api::catalog::{
    Catalog, Completion, CompletionCtx, Enumeration, FieldTrie, InferredSchema, LevelDef, ListOpts,
    ObjectDetail, ObjectKind, ObjectNode, Page,
};
use datagrep_api::driver::ResumeToken;
use datagrep_api::error::DbError;
use datagrep_api::shape::{FieldDef, FieldFlags, Identity, LogicalType, ObjectPath, RowSchema};

use crate::error::map_mysql_error;
use crate::sql::{json_escape, quote_object_path};
use crate::value::{decode_value, logical_type_of_data_type};

pub struct MySqlCatalog {
    conn: Arc<Mutex<Option<Conn>>>,
}

impl MySqlCatalog {
    pub fn new(conn: Arc<Mutex<Option<Conn>>>) -> Self {
        Self { conn }
    }
}

fn resume_str(resume: &Option<ResumeToken>) -> String {
    resume
        .as_ref()
        .and_then(|t| std::str::from_utf8(&t.0).ok())
        .unwrap_or("")
        .to_string()
}

fn next_token(items_len: usize, limit: u32, last: Option<&str>) -> Option<ResumeToken> {
    if items_len < limit as usize {
        return None;
    }
    last.map(|n| ResumeToken(Bytes::copy_from_slice(n.as_bytes())))
}

#[async_trait]
impl Catalog for MySqlCatalog {
    fn levels(&self) -> Vec<LevelDef> {
        // Two organizational levels + columns — MySQL has no database/schema
        // split (`SCHEMA` is a synonym for `DATABASE`), and this driver
        // reports the true two-level shape rather than imitating Postgres.
        vec![
            LevelDef {
                name: Arc::from("database"),
                kind: ObjectKind::Database,
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
            [db] => self.list_tables(db, opts).await,
            [db, table] => self.list_columns(db, table, opts).await,
            _ => Err(DbError::Unsupported {
                feature: "catalog path deeper than column level".into(),
            }),
        }
    }

    async fn describe(&self, path: &ObjectPath) -> Result<ObjectDetail, DbError> {
        match path.parts() {
            [db, table] => self.describe_table(db, table).await,
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
            _ => Err(DbError::Unsupported {
                feature: "describe() needs a database or database.table path".into(),
            }),
        }
    }

    async fn infer_shape(
        &self,
        path: &ObjectPath,
        sample_size: u32,
    ) -> Result<InferredSchema, DbError> {
        let table = quote_object_path(path)?;
        let mut guard = self.conn.lock().await;
        let conn = guard.as_mut().ok_or(DbError::Closed)?;
        // Bounded by `sample_size`; the LIMIT is a bound parameter.
        let sql = format!("SELECT * FROM {table} LIMIT ?");
        let result: Vec<mysql_async::Row> = conn
            .exec(sql, (u64::from(sample_size),))
            .await
            .map_err(map_mysql_error)?;
        let sampled = result.len() as u64;
        let columns: Vec<mysql_async::Column> = result
            .first()
            .map(|r| r.columns().to_vec())
            .unwrap_or_default();
        let mut root: Vec<(Arc<str>, FieldTrie)> = columns
            .iter()
            .map(|c| (Arc::from(c.name_str().as_ref()), FieldTrie::default()))
            .collect();
        for row in result {
            let values = row.unwrap();
            for (i, v) in values.into_iter().enumerate() {
                if let (Some(col), Some(slot)) = (columns.get(i), root.get_mut(i)) {
                    let logical = decode_value(col, v)
                        .logical_type()
                        .unwrap_or(LogicalType::Null);
                    slot.1.record(logical);
                }
            }
        }
        Ok(InferredSchema { sampled, root })
    }

    async fn complete(&self, ctx: CompletionCtx) -> Result<Vec<Completion>, DbError> {
        let prefix = prefix_at_caret(&ctx.text, ctx.offset as usize);
        if prefix.is_empty() {
            return Ok(Vec::new());
        }
        // Scope: an explicit catalog scope wins; otherwise the connection's
        // current database (server-side DATABASE()).
        let scope_db: Option<String> = ctx
            .scope
            .as_ref()
            .and_then(|p| p.parts().first())
            .map(|s| s.to_string());

        let mut guard = self.conn.lock().await;
        let conn = guard.as_mut().ok_or(DbError::Closed)?;

        let mut out = Vec::new();
        // Server-side prefix query, LIMIT 50: matching happens on the server
        // against a bounded slice, so completion never needs a full schema
        // resident in memory.
        let tables: Vec<(String, String)> = conn
            .exec(
                "SELECT table_name, table_type FROM information_schema.tables \
                 WHERE table_schema = COALESCE(?, DATABASE()) \
                 AND table_name LIKE CONCAT(?, '%') \
                 ORDER BY table_name LIMIT 50",
                (scope_db.clone(), prefix.clone()),
            )
            .await
            .map_err(map_mysql_error)?;
        for (name, table_type) in tables {
            out.push(Completion {
                label: Arc::from(name),
                kind: if table_type.contains("VIEW") {
                    ObjectKind::View
                } else {
                    ObjectKind::Table
                },
                detail: None,
            });
        }

        let columns: Vec<(String, String)> = conn
            .exec(
                "SELECT DISTINCT column_name, table_name FROM information_schema.columns \
                 WHERE table_schema = COALESCE(?, DATABASE()) \
                 AND column_name LIKE CONCAT(?, '%') \
                 ORDER BY column_name LIMIT 50",
                (scope_db, prefix),
            )
            .await
            .map_err(map_mysql_error)?;
        for (name, table) in columns {
            out.push(Completion {
                label: Arc::from(name),
                kind: ObjectKind::Column,
                detail: Some(Arc::from(table)),
            });
        }

        Ok(out)
    }
}

/// Scan backwards from the caret over identifier characters to find the
/// token being typed.
fn prefix_at_caret(text: &str, offset: usize) -> String {
    let bytes = text.as_bytes();
    let end = offset.min(bytes.len());
    let mut start = end;
    while start > 0 && matches!(bytes[start - 1], b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'_') {
        start -= 1;
    }
    // Rebuild from the bytes, not by slicing the `str`: the caret `offset` is
    // an editor position, and both it and the backwards scan can stop inside
    // a multi-byte character (type after any non-ASCII text and they do).
    // Slicing a `str` off a char boundary panics; an empty prefix is the
    // right answer for a caret that is not on one.
    std::str::from_utf8(&bytes[start..end])
        .unwrap_or("")
        .to_string()
}

impl MySqlCatalog {
    async fn list_databases(&self, opts: ListOpts) -> Result<Page<ObjectNode>, DbError> {
        let prefix = opts.prefix.as_deref().unwrap_or("").to_string();
        let after = resume_str(&opts.resume);
        let mut guard = self.conn.lock().await;
        let conn = guard.as_mut().ok_or(DbError::Closed)?;
        let names: Vec<String> = conn
            .exec(
                "SELECT schema_name FROM information_schema.schemata \
                 WHERE schema_name LIKE CONCAT(?, '%') AND schema_name > ? \
                 ORDER BY schema_name LIMIT ?",
                (prefix, after, u64::from(opts.limit)),
            )
            .await
            .map_err(map_mysql_error)?;
        let last = names.last().cloned();
        let items: Vec<ObjectNode> = names
            .into_iter()
            .map(|name| ObjectNode {
                path: ObjectPath::new(vec![Arc::from(name)]),
                kind: ObjectKind::Database,
                has_children: true,
                comment: None,
            })
            .collect();
        let next = next_token(items.len(), opts.limit, last.as_deref());
        Ok(Page { items, next })
    }

    async fn list_tables(
        &self,
        db: &Arc<str>,
        opts: ListOpts,
    ) -> Result<Page<ObjectNode>, DbError> {
        let prefix = opts.prefix.as_deref().unwrap_or("").to_string();
        let after = resume_str(&opts.resume);
        let mut guard = self.conn.lock().await;
        let conn = guard.as_mut().ok_or(DbError::Closed)?;
        let rows: Vec<(String, String)> = conn
            .exec(
                "SELECT table_name, table_type FROM information_schema.tables \
                 WHERE table_schema = ? AND table_name LIKE CONCAT(?, '%') AND table_name > ? \
                 ORDER BY table_name LIMIT ?",
                (db.to_string(), prefix, after, u64::from(opts.limit)),
            )
            .await
            .map_err(map_mysql_error)?;
        let last = rows.last().map(|(n, _)| n.clone());
        let items: Vec<ObjectNode> = rows
            .into_iter()
            .map(|(name, table_type)| ObjectNode {
                path: ObjectPath::new(vec![db.clone(), Arc::from(name)]),
                // information_schema.tables.table_type: BASE TABLE, VIEW,
                // SYSTEM VIEW (information_schema itself).
                kind: if table_type.contains("VIEW") {
                    ObjectKind::View
                } else {
                    ObjectKind::Table
                },
                has_children: true,
                comment: None,
            })
            .collect();
        let next = next_token(items.len(), opts.limit, last.as_deref());
        Ok(Page { items, next })
    }

    async fn list_columns(
        &self,
        db: &Arc<str>,
        table: &Arc<str>,
        opts: ListOpts,
    ) -> Result<Page<ObjectNode>, DbError> {
        let prefix = opts.prefix.as_deref().unwrap_or("").to_string();
        // Columns page in declared (ordinal) order; the resume token is the
        // last ordinal position.
        let after: u64 = resume_str(&opts.resume).parse().unwrap_or(0);
        let mut guard = self.conn.lock().await;
        let conn = guard.as_mut().ok_or(DbError::Closed)?;
        let rows: Vec<(String, u64, Option<String>)> = conn
            .exec(
                "SELECT column_name, ordinal_position, column_comment \
                 FROM information_schema.columns \
                 WHERE table_schema = ? AND table_name = ? \
                 AND column_name LIKE CONCAT(?, '%') AND ordinal_position > ? \
                 ORDER BY ordinal_position LIMIT ?",
                (
                    db.to_string(),
                    table.to_string(),
                    prefix,
                    after,
                    u64::from(opts.limit),
                ),
            )
            .await
            .map_err(map_mysql_error)?;
        let last = rows.last().map(|(_, ord, _)| ord.to_string());
        let items: Vec<ObjectNode> = rows
            .into_iter()
            .map(|(name, _, comment)| ObjectNode {
                path: ObjectPath::new(vec![db.clone(), table.clone(), Arc::from(name)]),
                kind: ObjectKind::Column,
                has_children: false,
                comment: comment.filter(|c| !c.is_empty()).map(Arc::from),
            })
            .collect();
        let next = next_token(items.len(), opts.limit, last.as_deref());
        Ok(Page { items, next })
    }

    async fn describe_table(
        &self,
        db: &Arc<str>,
        table: &Arc<str>,
    ) -> Result<ObjectDetail, DbError> {
        let mut guard = self.conn.lock().await;
        let conn = guard.as_mut().ok_or(DbError::Closed)?;

        // Table-level facts (cheap estimates — table_rows is the storage
        // engine's estimate, surfaced as such).
        type TableFacts = (String, Option<String>, Option<u64>, Option<u64>, String);
        let fact_row: Option<TableFacts> = conn
            .exec_first(
                "SELECT table_type, engine, table_rows, data_length, table_comment \
                 FROM information_schema.tables WHERE table_schema = ? AND table_name = ?",
                (db.to_string(), table.to_string()),
            )
            .await
            .map_err(map_mysql_error)?;

        // Column detail, one query.
        let col_rows: Vec<(String, String, String, String, String, String)> = conn
            .exec(
                "SELECT column_name, data_type, column_type, is_nullable, column_key, extra \
                 FROM information_schema.columns \
                 WHERE table_schema = ? AND table_name = ? ORDER BY ordinal_position",
                (db.to_string(), table.to_string()),
            )
            .await
            .map_err(map_mysql_error)?;

        // Index detail, one query (name, ordered columns, uniqueness).
        let idx_rows: Vec<(String, i64, String)> = conn
            .exec(
                "SELECT index_name, non_unique, column_name \
                 FROM information_schema.statistics \
                 WHERE table_schema = ? AND table_name = ? \
                 ORDER BY index_name, seq_in_index",
                (db.to_string(), table.to_string()),
            )
            .await
            .map_err(map_mysql_error)?;
        drop(guard);

        if fact_row.is_none() && col_rows.is_empty() {
            return Err(DbError::Query {
                code: None,
                message: format!("no such table: {db}.{table}"),
                position: None,
            });
        }

        let mut fields = Vec::with_capacity(col_rows.len());
        let mut pk_indices = Vec::new();
        for (i, (name, data_type, column_type, is_nullable, column_key, extra)) in
            col_rows.iter().enumerate()
        {
            let mut flags = FieldFlags::empty();
            if is_nullable == "YES" {
                flags |= FieldFlags::NULLABLE;
            }
            match column_key.as_str() {
                "PRI" => {
                    flags |= FieldFlags::PRIMARY_KEY | FieldFlags::INDEXED;
                    pk_indices.push(i as u32);
                }
                "UNI" => flags |= FieldFlags::UNIQUE | FieldFlags::INDEXED,
                "MUL" => flags |= FieldFlags::INDEXED,
                _ => {}
            }
            let extra_lc = extra.to_ascii_lowercase();
            if extra_lc.contains("auto_increment") || extra_lc.contains("generated") {
                flags |= FieldFlags::AUTO_GENERATED;
            }
            fields.push(FieldDef {
                name: Arc::from(name.as_str()),
                logical: logical_type_of_data_type(data_type, column_type),
                flags,
                native_type: Some(Arc::from(column_type.as_str())),
            });
        }
        let identity = if pk_indices.is_empty() {
            None
        } else {
            Some(Identity {
                field_indices: pk_indices,
            })
        };

        let mut extra = Vec::new();
        let mut kind = ObjectKind::Table;
        if let Some((table_type, engine, table_rows, data_length, comment)) = fact_row {
            if table_type.contains("VIEW") {
                kind = ObjectKind::View;
            }
            if let Some(engine) = engine {
                extra.push((Arc::from("engine"), Arc::from(engine)));
            }
            if let Some(rows) = table_rows {
                extra.push((Arc::from("estimated_rows"), Arc::from(rows.to_string())));
            }
            if let Some(bytes) = data_length {
                extra.push((Arc::from("data_length"), Arc::from(bytes.to_string())));
            }
            if !comment.is_empty() {
                extra.push((Arc::from("comment"), Arc::from(comment)));
            }
        }
        extra.push((Arc::from("indexes"), Arc::from(indexes_json(&idx_rows))));

        Ok(ObjectDetail {
            node: ObjectNode {
                path: ObjectPath::new(vec![db.clone(), table.clone()]),
                kind,
                has_children: true,
                comment: None,
            },
            schema: Some(RowSchema { fields, identity }),
            extra,
        })
    }
}

/// Render `information_schema.statistics` rows (already ordered by
/// index_name, seq_in_index) as the cross-driver `indexes` JSON array:
/// `[{"name": …, "columns": […], "unique": bool, "primary": bool}, …]`.
fn indexes_json(rows: &[(String, i64, String)]) -> String {
    let mut out = String::from("[");
    let mut current: Option<(&str, bool, Vec<&str>)> = None;
    let mut first = true;
    let flush = |out: &mut String, first: &mut bool, idx: (&str, bool, Vec<&str>)| {
        if !*first {
            out.push(',');
        }
        *first = false;
        let (name, unique, columns) = idx;
        out.push_str(&format!(
            "{{\"name\":\"{}\",\"columns\":[{}],\"unique\":{},\"primary\":{}}}",
            json_escape(name),
            columns
                .iter()
                .map(|c| format!("\"{}\"", json_escape(c)))
                .collect::<Vec<_>>()
                .join(","),
            unique,
            name == "PRIMARY"
        ));
    };
    for (name, non_unique, column) in rows {
        match &mut current {
            Some((cur_name, _, cols)) if *cur_name == name.as_str() => cols.push(column),
            _ => {
                if let Some(done) = current.take() {
                    flush(&mut out, &mut first, done);
                }
                current = Some((name.as_str(), *non_unique == 0, vec![column.as_str()]));
            }
        }
    }
    if let Some(done) = current.take() {
        flush(&mut out, &mut first, done);
    }
    out.push(']');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prefix_at_caret_finds_identifier_before_cursor() {
        assert_eq!(prefix_at_caret("SELECT * FROM use", 17), "use");
        assert_eq!(prefix_at_caret("SELECT foo.b", 12), "b");
        assert_eq!(prefix_at_caret("SELECT ", 7), "");
        assert_eq!(prefix_at_caret("", 0), "");
    }

    #[test]
    fn next_token_only_when_page_is_full() {
        assert_eq!(next_token(3, 10, Some("x")), None);
        assert!(next_token(10, 10, Some("x")).is_some());
        assert_eq!(next_token(10, 10, None), None);
    }

    #[test]
    fn indexes_json_groups_ordered_columns_and_marks_primary() {
        let rows = vec![
            ("PRIMARY".to_string(), 0, "tenant".to_string()),
            ("PRIMARY".to_string(), 0, "id".to_string()),
            ("idx_email".to_string(), 0, "email".to_string()),
            ("idx_name".to_string(), 1, "last".to_string()),
            ("idx_name".to_string(), 1, "first".to_string()),
        ];
        let json = indexes_json(&rows);
        assert_eq!(
            json,
            r#"[{"name":"PRIMARY","columns":["tenant","id"],"unique":true,"primary":true},{"name":"idx_email","columns":["email"],"unique":true,"primary":false},{"name":"idx_name","columns":["last","first"],"unique":false,"primary":false}]"#
        );
    }

    #[test]
    fn indexes_json_escapes_hostile_names() {
        let rows = vec![("we\"ird".to_string(), 1, "c\\ol".to_string())];
        let json = indexes_json(&rows);
        assert_eq!(
            json,
            r#"[{"name":"we\"ird","columns":["c\\ol"],"unique":false,"primary":false}]"#
        );
    }

    #[test]
    fn indexes_json_empty() {
        assert_eq!(indexes_json(&[]), "[]");
    }

    #[test]
    fn levels_are_two_organizational_plus_columns() {
        let cat = MySqlCatalog::new(Arc::new(Mutex::new(None)));
        let levels = cat.levels();
        assert_eq!(
            levels.len(),
            3,
            "database → table → column; no fake schema tier"
        );
        assert_eq!(&*levels[0].name, "database");
        assert_eq!(&*levels[1].name, "table");
        assert_eq!(&*levels[2].name, "column");
    }
}
