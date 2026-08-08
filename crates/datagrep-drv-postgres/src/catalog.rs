//! [`PgCatalog`]: lazy, one-cheap-query-at-a-time browsing — never eager
//! whole-catalog indexing, which would make connecting to a big server cost
//! more than the user asked for. Every method below issues exactly one
//! parameterized query against `pg_catalog`/`information_schema` equivalents,
//! bounded by `ListOpts::limit`.
//!
//! # Two rules this file learned the hard way
//!
//! 1. **It borrows a session from [`crate::pool::PgPool`], never the
//!    connection's one pinned client.** Browsing the schema tree while a
//!    result grid is open is the single most common thing a user does; when
//!    the catalog shared the cursor's socket, that froze the driver forever
//!    (TEST-REPORT.md F2). Each method acquires one session, does its one
//!    query on it, and gives it straight back.
//! 2. **Every `pg_catalog` column whose Postgres type is not plainly `text`
//!    is cast in SQL.** `pg_class.relkind` is the 1-byte `"char"` type
//!    (OID 18), which `tokio_postgres` decodes as `i8` and *panics* on if you
//!    ask for a `String` — which is precisely what happened on every table
//!    listing and every `describe` against a real server (TEST-REPORT.md F3).
//!    `::text` at the query site is deliberate: it is visible next to the
//!    column, and it survives someone reordering the select list later.
//!    (`name`-typed columns — `relname`, `nspname`, `attname`, `datname`,
//!    `amname` — *are* decodable as `String`; `oid` as `u32`; those are left
//!    alone.)

use std::sync::Arc;

use async_trait::async_trait;
use bytes::Bytes;
use tokio_postgres::Client;

use datagrep_api::catalog::{
    Catalog, Completion, CompletionCtx, Enumeration, FieldTrie, InferredSchema, LevelDef, ListOpts,
    ObjectDetail, ObjectKind, ObjectNode, Page,
};
use datagrep_api::driver::ResumeToken;
use datagrep_api::error::DbError;
use datagrep_api::shape::{FieldDef, FieldFlags, Identity, LogicalType, ObjectPath, RowSchema};

use crate::error::map_pg_error;
use crate::pool::{PgPool, PooledClient};
use crate::value::{decode_binary, logical_type_of};

pub struct PgCatalog {
    pool: Arc<PgPool>,
}

impl PgCatalog {
    pub fn new(pool: Arc<PgPool>) -> Self {
        Self { pool }
    }

    /// Borrow a session for one catalog query. Never the session a cursor or
    /// transaction has pinned — see the module docs.
    async fn session(&self) -> Result<PooledClient, DbError> {
        self.pool.acquire().await
    }
}

/// Postgres has no cross-database catalog access, so browsing a database
/// other than the one this session is connected to is refused rather than
/// silently answered from the wrong database.
async fn require_current_database(client: &Client, db: &str) -> Result<(), DbError> {
    let row = client
        .query_one("SELECT current_database()::text", &[])
        .await
        .map_err(map_pg_error)?;
    let current: String = row.get(0);
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

/// Map `pg_class.relkind` (already cast to `text` at the query site) onto the
/// engine-independent [`ObjectKind`]. `v` = view, `m` = materialized view;
/// ordinary/partitioned/foreign tables (`r`/`p`/`f`) are all tables.
fn object_kind_of(relkind: &str) -> ObjectKind {
    match relkind {
        "v" | "m" => ObjectKind::View,
        _ => ObjectKind::Table,
    }
}

/// `row.get()` **panics** on a type mismatch, which crashed the whole process
/// on the `relkind` bug rather than surfacing a `DbError`. Catalog reads whose
/// Postgres type we had to reason about go through here instead, so a future
/// mismatch is a recoverable protocol error with the column index in it.
fn try_get_text(row: &tokio_postgres::Row, idx: usize) -> Result<String, DbError> {
    row.try_get::<_, String>(idx).map_err(|e| {
        DbError::Protocol(format!(
            "catalog query column {idx} did not decode as text ({e}) — a pg_catalog column is \
             probably missing its ::text cast"
        ))
    })
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
        let session = self.session().await?;
        let client = &*session;
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
        let session = self.session().await?;
        let client = &*session;

        let mut out = Vec::new();
        let table_rows = client
            .query(
                "SELECT c.relname::text, n.nspname::text \
                 FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace \
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
                "SELECT DISTINCT attname::text FROM pg_attribute \
                 WHERE attnum > 0 AND NOT attisdropped \
                 AND attname LIKE $1 || '%' ORDER BY 1 LIMIT 25",
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
/// completion — matching happens on the server, never against a locally
/// built index of the catalog.
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
        let session = self.session().await?;
        let rows = session
            .query(
                "SELECT datname::text FROM pg_database \
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
        let prefix = opts.prefix.as_deref().unwrap_or("");
        let after = resume_prefix(&opts.resume).unwrap_or_default();
        let session = self.session().await?;
        require_current_database(&session, db).await?;
        let rows = session
            .query(
                "SELECT nspname::text FROM pg_namespace \
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
        let prefix = opts.prefix.as_deref().unwrap_or("");
        let after = resume_prefix(&opts.resume).unwrap_or_default();
        let session = self.session().await?;
        require_current_database(&session, db).await?;
        let rows = session
            .query(
                // Cast to text, never selected bare: relkind is the
                // 1-byte `"char"` type and decoding it as `String` panics
                // (TEST-REPORT.md F3 — every table listing, against every
                // real server).
                "SELECT c.relname::text, c.relkind::text \
                 FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace \
                 WHERE n.nspname = $1 AND c.relkind IN ('r','v','m','p','f') \
                 AND c.relname LIKE $2 || '%' AND c.relname > $3 \
                 ORDER BY c.relname LIMIT $4",
                &[&schema.as_ref(), &prefix, &after, &(opts.limit as i64)],
            )
            .await
            .map_err(map_pg_error)?;
        let mut names_kinds: Vec<(String, String)> = Vec::with_capacity(rows.len());
        for r in &rows {
            names_kinds.push((try_get_text(r, 0)?, try_get_text(r, 1)?));
        }
        let last = names_kinds.last().map(|(n, _)| n.clone());
        let items = names_kinds
            .into_iter()
            .map(|(name, relkind)| ObjectNode {
                path: ObjectPath::new(vec![db.clone(), schema.clone(), Arc::from(name)]),
                kind: object_kind_of(&relkind),
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
        let prefix = opts.prefix.as_deref().unwrap_or("");
        let after = resume_prefix(&opts.resume).unwrap_or_default();
        let session = self.session().await?;
        require_current_database(&session, db).await?;
        let rows = session
            .query(
                "SELECT a.attname::text FROM pg_attribute a \
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
        let session = self.session().await?;
        let client = &*session;
        require_current_database(client, db).await?;

        // One cheap query for size facts — the `reltuples` estimate and
        // `pg_relation_size`, never a `COUNT(*)` that would scan the table —
        // plus the table comment. `relkind::text` for
        // the same reason as in `list_relations` — bare `relkind` is `"char"`
        // and panicked here on every `--describe` (TEST-REPORT.md F3).
        let size_row = client
            .query_opt(
                "SELECT c.reltuples::float8, c.relkind::text, pg_relation_size(c.oid), \
                 obj_description(c.oid, 'pg_class') \
                 FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace \
                 WHERE n.nspname = $1 AND c.relname = $2",
                &[&schema.as_ref(), &table.as_ref()],
            )
            .await
            .map_err(map_pg_error)?;
        let mut extra = Vec::new();
        let mut relkind = "r".to_string();
        let mut comment: Option<Arc<str>> = None;
        if let Some(row) = &size_row {
            let reltuples: f64 = row.get(0);
            relkind = try_get_text(row, 1)?;
            let size_bytes: i64 = row.get(2);
            comment = row.get::<_, Option<String>>(3).map(Arc::from);
            extra.push((
                Arc::from("row_estimate"),
                Arc::from(format!("{}", reltuples.max(0.0) as i64)),
            ));
            extra.push((Arc::from("size_bytes"), Arc::from(size_bytes.to_string())));
        }

        // Indexes — fetched here and only here, on the explicit `describe()`
        // of this one relation — never during tree expansion, never on
        // connect. One catalog-only query.
        let indexes = self.list_indexes(client, schema, table).await?;
        extra.push((Arc::from("indexes"), Arc::from(indexes_json(&indexes))));

        // Columns + types + default expressions, for the declared RowSchema
        // (`SCHEMA_DECLARED`).
        let col_rows = client
            .query(
                "SELECT a.attname::text, a.atttypid, a.attnotnull, \
                 pg_get_expr(d.adbin, d.adrelid) \
                 FROM pg_attribute a \
                 JOIN pg_class c ON c.oid = a.attrelid \
                 JOIN pg_namespace n ON n.oid = c.relnamespace \
                 LEFT JOIN pg_attrdef d ON d.adrelid = a.attrelid AND d.adnum = a.attnum \
                 WHERE n.nspname = $1 AND c.relname = $2 AND a.attnum > 0 AND NOT a.attisdropped \
                 ORDER BY a.attnum",
                &[&schema.as_ref(), &table.as_ref()],
            )
            .await
            .map_err(map_pg_error)?;
        let mut fields = Vec::with_capacity(col_rows.len());
        let mut defaults: Vec<(String, String)> = Vec::new();
        for row in &col_rows {
            let name: String = row.get(0);
            let oid: u32 = row.get(1);
            let not_null: bool = row.get(2);
            if let Some(default_expr) = row.get::<_, Option<String>>(3) {
                defaults.push((name.clone(), default_expr));
            }
            let ty = tokio_postgres::types::Type::from_oid(oid);
            let logical = ty
                .as_ref()
                .map(logical_type_of)
                .unwrap_or(LogicalType::Unknown);
            let mut flags = FieldFlags::empty();
            if !not_null {
                flags |= FieldFlags::NULLABLE;
            }
            // Index-derived flags, from the same single index query above:
            // `INDEXED` = leading key column of some index (the position a
            // lookup can use); `UNIQUE` = single-column unique index;
            // `PRIMARY_KEY` = member of the primary-key index.
            let leading =
                |ix: &PgIndexInfo| ix.columns.first().is_some_and(|(col, _)| col == &name);
            if indexes.iter().any(leading) {
                flags |= FieldFlags::INDEXED;
            }
            if indexes
                .iter()
                .any(|ix| ix.unique && ix.columns.len() == 1 && leading(ix))
            {
                flags |= FieldFlags::UNIQUE;
            }
            if indexes
                .iter()
                .any(|ix| ix.primary && ix.columns.iter().any(|(col, _)| col == &name))
            {
                flags |= FieldFlags::PRIMARY_KEY;
            }
            fields.push(FieldDef {
                name: Arc::from(name),
                logical,
                flags,
                native_type: ty.as_ref().map(|t| Arc::from(t.name())),
            });
        }
        if let Some(json) = column_defaults_json(&defaults) {
            extra.push((Arc::from("column_defaults"), Arc::from(json)));
        }

        let pk_row = client
            .query(
                "SELECT a.attname::text FROM pg_index i \
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
                kind: object_kind_of(&relkind),
                has_children: true,
                comment,
            },
            schema: Some(RowSchema { fields, identity }),
            extra,
        })
    }

    /// All indexes of one relation: `pg_index` joined to `pg_class`/`pg_am`,
    /// one row per key column (`generate_series` over `indnkeyatts`), grouped
    /// client-side. `pg_get_indexdef(oid)` gives the whole definition,
    /// `pg_get_indexdef(oid, n, true)` the nth key column's text (which also
    /// covers expression indexes), `pg_get_expr(indpred, …)` the partial
    /// predicate, and `pg_relation_size` the on-disk size — all catalog-only,
    /// never touching the table's rows.
    async fn list_indexes(
        &self,
        client: &Client,
        schema: &Arc<str>,
        table: &Arc<str>,
    ) -> Result<Vec<PgIndexInfo>, DbError> {
        let rows = client
            .query(
                "SELECT ci.relname::text, i.indisunique, i.indisprimary, am.amname::text, \
                 pg_get_expr(i.indpred, i.indrelid), \
                 pg_relation_size(i.indexrelid), \
                 pg_get_indexdef(i.indexrelid), \
                 pg_get_indexdef(i.indexrelid, g.n, true), \
                 (i.indoption[g.n - 1] & 1) <> 0 \
                 FROM pg_index i \
                 JOIN pg_class c ON c.oid = i.indrelid \
                 JOIN pg_namespace ns ON ns.oid = c.relnamespace \
                 JOIN pg_class ci ON ci.oid = i.indexrelid \
                 JOIN pg_am am ON am.oid = ci.relam \
                 CROSS JOIN LATERAL generate_series(1, i.indnkeyatts::int4) AS g(n) \
                 WHERE ns.nspname = $1 AND c.relname = $2 \
                 ORDER BY ci.relname, g.n",
                &[&schema.as_ref(), &table.as_ref()],
            )
            .await
            .map_err(map_pg_error)?;
        let mut out: Vec<PgIndexInfo> = Vec::new();
        for row in &rows {
            let name: String = row.get(0);
            let column: String = row.get(7);
            let descending: bool = row.get(8);
            match out.last_mut() {
                Some(last) if last.name == name => last.columns.push((column, descending)),
                _ => out.push(PgIndexInfo {
                    name,
                    unique: row.get(1),
                    primary: row.get(2),
                    method: row.get(3),
                    filter: row.get(4),
                    size_bytes: row.get(5),
                    definition: row.get(6),
                    columns: vec![(column, descending)],
                }),
            }
        }
        Ok(out)
    }
}

/// One index of a relation, grouped from the per-key-column query in
/// [`PgCatalog::list_indexes`]. `columns` is `(text, descending)` in key
/// order; `descending` comes from `indoption` bit 0 and is only meaningful
/// for b-tree — [`indexes_json`] nulls it out for other access methods.
struct PgIndexInfo {
    name: String,
    unique: bool,
    primary: bool,
    method: String,
    filter: Option<String>,
    size_bytes: i64,
    definition: String,
    columns: Vec<(String, bool)>,
}

/// The engine-independent index JSON shape (see the datagrep-ffi describe
/// contract): `[{name, columns:[{name, order}], unique, primary, type,
/// partial, filter, size_bytes, definition, sparse, expire_after_seconds}]`.
fn indexes_json(indexes: &[PgIndexInfo]) -> String {
    let entries: Vec<String> = indexes
        .iter()
        .map(|ix| {
            let ordered = ix.method == "btree";
            let cols: Vec<String> = ix
                .columns
                .iter()
                .map(|(name, desc)| {
                    let order = match (ordered, desc) {
                        (false, _) => "null",
                        (true, true) => "\"desc\"",
                        (true, false) => "\"asc\"",
                    };
                    format!("{{\"name\":{},\"order\":{}}}", json_str(name), order)
                })
                .collect();
            format!(
                "{{\"name\":{},\"columns\":[{}],\"unique\":{},\"primary\":{},\
                 \"type\":{},\"partial\":{},\"filter\":{},\"size_bytes\":{},\
                 \"definition\":{},\"sparse\":false,\"expire_after_seconds\":null}}",
                json_str(&ix.name),
                cols.join(","),
                ix.unique,
                ix.primary,
                json_str(&ix.method),
                ix.filter.is_some(),
                json_opt_str(ix.filter.as_deref()),
                ix.size_bytes,
                json_str(&ix.definition),
            )
        })
        .collect();
    format!("[{}]", entries.join(","))
}

/// `{"col": "<default expression>"}`; `None` when no column has a default.
fn column_defaults_json(defaults: &[(String, String)]) -> Option<String> {
    if defaults.is_empty() {
        return None;
    }
    let entries: Vec<String> = defaults
        .iter()
        .map(|(name, expr)| format!("{}:{}", json_str(name), json_str(expr)))
        .collect();
    Some(format!("{{{}}}", entries.join(",")))
}

/// Minimal JSON string encoding. Hand-rolled on purpose: this crate's
/// dependency policy keeps `serde_json` out of drivers (see the `Cargo.toml`
/// note on `tokio-postgres` features), and the catalog only ever needs to
/// *emit* a handful of strings/bools.
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

fn json_opt_str(s: Option<&str>) -> String {
    match s {
        Some(s) => json_str(s),
        None => "null".to_string(),
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

    /// The index JSON shape, pinned by parsing (serde_json is dev-only):
    /// cross-engine keys all present, b-tree key order/direction kept,
    /// non-btree methods get `order: null`, and partial predicates ride in
    /// `filter`.
    #[test]
    fn indexes_json_has_the_cross_engine_shape() {
        let indexes = vec![
            PgIndexInfo {
                name: "users_pkey".into(),
                unique: true,
                primary: true,
                method: "btree".into(),
                filter: None,
                size_bytes: 16384,
                definition: "CREATE UNIQUE INDEX users_pkey ON public.users USING btree (id)"
                    .into(),
                columns: vec![("id".into(), false)],
            },
            PgIndexInfo {
                name: "idx_users_active_email".into(),
                unique: false,
                primary: false,
                method: "btree".into(),
                filter: Some("(deleted_at IS NULL)".into()),
                size_bytes: 8192,
                definition: "CREATE INDEX idx_users_active_email ON public.users \
                             USING btree (email DESC, created_at) WHERE (deleted_at IS NULL)"
                    .into(),
                columns: vec![("email".into(), true), ("created_at".into(), false)],
            },
            PgIndexInfo {
                name: "idx_users_tags".into(),
                unique: false,
                primary: false,
                method: "gin".into(),
                filter: None,
                size_bytes: 4096,
                definition: "CREATE INDEX idx_users_tags ON public.users USING gin (tags)".into(),
                columns: vec![("tags".into(), false)],
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
        assert_eq!(list[0]["unique"], true);
        assert_eq!(list[0]["columns"][0]["order"], "asc");
        assert_eq!(list[1]["partial"], true);
        assert_eq!(list[1]["filter"], "(deleted_at IS NULL)");
        assert_eq!(list[1]["columns"][0]["name"], "email");
        assert_eq!(list[1]["columns"][0]["order"], "desc");
        assert_eq!(list[1]["columns"][1]["order"], "asc");
        assert_eq!(list[2]["type"], "gin");
        assert_eq!(
            list[2]["columns"][0]["order"],
            serde_json::Value::Null,
            "direction is a btree concept; inventing ASC for GIN would be wrong"
        );
        assert_eq!(indexes_json(&[]), "[]");
    }

    #[test]
    fn column_defaults_json_is_none_when_empty_and_escapes_when_not() {
        assert_eq!(column_defaults_json(&[]), None);
        let json = column_defaults_json(&[
            ("id".into(), "nextval('users_id_seq'::regclass)".into()),
            ("note".into(), "'say \"hi\"'::text".into()),
        ])
        .expect("two defaults");
        let parsed: serde_json::Value = serde_json::from_str(&json).expect("valid JSON");
        assert_eq!(parsed["id"], "nextval('users_id_seq'::regclass)");
        assert_eq!(parsed["note"], "'say \"hi\"'::text");
    }

    /// `relkind` is a single character, and only `v`/`m` are views. Pinned
    /// because the mapping used to be an inline `match` duplicated at two
    /// sites — both of which also decoded the column with the wrong Rust
    /// type (TEST-REPORT.md F3).
    #[test]
    fn relkind_maps_onto_object_kind() {
        assert_eq!(object_kind_of("v"), ObjectKind::View);
        assert_eq!(object_kind_of("m"), ObjectKind::View);
        assert_eq!(object_kind_of("r"), ObjectKind::Table);
        assert_eq!(object_kind_of("p"), ObjectKind::Table);
        assert_eq!(object_kind_of("f"), ObjectKind::Table);
    }

    /// Every `pg_catalog` column this file reads as a Rust `String` must be
    /// either a `name` (which `tokio_postgres` does decode as `String`) or
    /// explicitly `::text`-cast. `relkind` is the 1-byte `"char"` type, so a
    /// bare qualified reference to it in a select list is the exact panic
    /// reported in TEST-REPORT.md F3. Every SQL mention in this file is
    /// qualified (`c.…`), so scanning for that is enough to catch a
    /// reintroduction — and cheaper than another live server round trip.
    #[test]
    fn relkind_is_never_selected_without_a_text_cast() {
        // Built at runtime, not written literally: a literal needle would
        // match this very test and make the scan self-referential.
        let needle = format!("c.{}", "relkind");
        let src = include_str!("catalog.rs");
        let mut from = 0usize;
        let mut checked = 0usize;
        while let Some(pos) = src[from..].find(&needle) {
            let at = from + pos;
            let rest = &src[at + needle.len()..];
            assert!(
                rest.starts_with("::text") || rest.starts_with(" IN"),
                "byte {at}: relkind must be selected as `::text` — it is Postgres's 1-byte \
                 \"char\" type and decoding it as a Rust String panics. Context: {:?}",
                &src[at..(at + 60).min(src.len())]
            );
            checked += 1;
            from = at + needle.len();
        }
        assert!(
            checked >= 3,
            "expected the relations query, the describe query and the relkind filter to be \
             scanned; found {checked} — did the queries move?"
        );
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
