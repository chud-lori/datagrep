//! Lazy, incremental catalog over `sqlite_master` / `PRAGMA` introspection.
//! Every listing is paged and bounded — never a whole-catalog dump on
//! connect.
//!
//! The catalog talks to the connection's dedicated worker thread exactly
//! like `Connection`/`Cursor` do: every method here sends a
//! [`crate::connection::WorkerMsg::Catalog`] job closure-free command and
//! awaits the reply, so `rusqlite::Connection` is still only ever touched
//! from its one owning thread.

use std::sync::Arc;

use async_trait::async_trait;
use datagrep_api::{
    Catalog, Completion, CompletionCtx, DbError, Enumeration, FieldDef, FieldFlags, FieldTrie,
    Identity, InferredSchema, LevelDef, ListOpts, ObjectDetail, ObjectKind, ObjectNode, ObjectPath,
    Page, ResumeToken, RowSchema, Value,
};

use crate::error::map_sqlite_err;
use crate::value::{quote_ident, sqlite_value_to_datagrep, SqlParam};

pub struct SqliteCatalog {
    pub(crate) jobs: crate::connection::JobSender,
}

/// One row of `PRAGMA table_info`, in declaration order.
pub(crate) struct ColumnInfo {
    pub name: String,
    pub decl_type: Option<String>,
    pub not_null: bool,
    /// 0 = not part of the primary key; otherwise its 1-based position
    /// within a composite primary key, per SQLite's own `pk` column.
    pub pk_position: i64,
    /// The default expression's SQL text (`dflt_value`), verbatim.
    pub default_value: Option<String>,
}

/// Shared with `connection.rs`'s `Op::Mutate`/identity-detection path so
/// both agree on exactly one source of truth for "what is this table's
/// primary key" (see the datagrep-api gap noted in `driver.rs`).
pub(crate) fn table_info(
    conn: &rusqlite::Connection,
    path: &ObjectPath,
) -> Result<Vec<ColumnInfo>, DbError> {
    let sql = match path.parts() {
        [table] => format!("PRAGMA table_info({})", quote_ident(table)?),
        [db, table] => format!(
            "PRAGMA {}.table_info({})",
            quote_ident(db)?,
            quote_ident(table)?
        ),
        _ => {
            return Err(DbError::Query {
                code: None,
                message: format!("`{path}` is not a table path (expected `table` or `db.table`)"),
                position: None,
            })
        }
    };
    let mut stmt = conn.prepare(&sql).map_err(map_sqlite_err)?;
    let mut rows = stmt.query([]).map_err(map_sqlite_err)?;
    let mut out = Vec::new();
    while let Some(row) = rows.next().map_err(map_sqlite_err)? {
        // table_info columns: cid, name, type, notnull, dflt_value, pk
        let name: String = row.get(1).map_err(map_sqlite_err)?;
        let decl_type: Option<String> = row.get(2).map_err(map_sqlite_err)?;
        let not_null: i64 = row.get(3).map_err(map_sqlite_err)?;
        let default_value: Option<String> = row.get(4).map_err(map_sqlite_err)?;
        let pk_position: i64 = row.get(5).map_err(map_sqlite_err)?;
        out.push(ColumnInfo {
            name,
            decl_type: decl_type.filter(|s| !s.is_empty()),
            not_null: not_null != 0,
            pk_position,
            default_value,
        });
    }
    Ok(out)
}

/// Primary-key columns of `path`, in composite-key order — used by
/// `connection.rs::detect_identity` to report `RowSchema::identity` for a
/// browsed table (mutation WHERE clauses now come from the mutation's own
/// named key, not from this lookup).
pub(crate) fn primary_key_columns(
    conn: &rusqlite::Connection,
    path: &ObjectPath,
) -> Result<Vec<String>, DbError> {
    let mut cols = table_info(conn, path)?;
    cols.retain(|c| c.pk_position > 0);
    cols.sort_by_key(|c| c.pk_position);
    Ok(cols.into_iter().map(|c| c.name).collect())
}

fn row_schema_from_table_info(columns: &[ColumnInfo], indexes: &[IndexInfo]) -> RowSchema {
    let pk_indices: Vec<u32> = columns
        .iter()
        .enumerate()
        .filter(|(_, c)| c.pk_position > 0)
        .map(|(i, _)| i as u32)
        .collect();
    let fields = columns
        .iter()
        .map(|c| {
            let mut flags = FieldFlags::empty();
            if !c.not_null {
                flags |= FieldFlags::NULLABLE;
            }
            if c.pk_position > 0 {
                flags |= FieldFlags::PRIMARY_KEY;
            }
            // `INDEXED` = leading column of some index (the only position a
            // lookup can actually use); `UNIQUE` = a single-column unique
            // index on exactly this column. Both derived from the same
            // `PRAGMA index_list`/`index_xinfo` pass `describe()` already
            // paid for — no extra queries.
            let leading = |ix: &IndexInfo| {
                ix.columns
                    .first()
                    .is_some_and(|col| col.name.as_deref() == Some(c.name.as_str()))
            };
            if indexes.iter().any(leading) {
                flags |= FieldFlags::INDEXED;
            }
            if indexes
                .iter()
                .any(|ix| ix.unique && ix.columns.len() == 1 && leading(ix))
            {
                flags |= FieldFlags::UNIQUE;
            }
            FieldDef {
                name: Arc::from(c.name.as_str()),
                logical: crate::value::logical_type_for_decl(c.decl_type.as_deref()),
                flags,
                native_type: c.decl_type.as_deref().map(Arc::from),
            }
        })
        .collect();
    RowSchema {
        fields,
        identity: if pk_indices.is_empty() {
            None
        } else {
            Some(Identity {
                field_indices: pk_indices,
            })
        },
    }
}

/// Apply `opts` (name prefix already applied server-side where possible;
/// this re-slices for `limit`/`resume`) to an in-memory candidate list. Used
/// for `PRAGMA`-backed levels (`database_list`, `table_info`) where the
/// result set is inherently small and SQL-side pagination isn't available.
fn paginate<T: Clone>(items: Vec<T>, key: impl Fn(&T) -> &str, opts: &ListOpts) -> Page<T> {
    let start = match &opts.resume {
        Some(ResumeToken(bytes)) => {
            let after = String::from_utf8_lossy(bytes).into_owned();
            items
                .iter()
                .position(|it| key(it) > after.as_str())
                .unwrap_or(items.len())
        }
        None => 0,
    };
    let limit = opts.limit.max(1) as usize;
    let end = (start + limit).min(items.len());
    let page_items: Vec<T> = items[start..end].to_vec();
    let next = if end < items.len() {
        page_items
            .last()
            .map(|it| ResumeToken(bytes::Bytes::copy_from_slice(key(it).as_bytes())))
    } else {
        None
    };
    Page {
        items: page_items,
        next,
    }
}

impl SqliteCatalog {
    async fn run<F, T>(&self, f: F) -> Result<T, DbError>
    where
        F: FnOnce(&rusqlite::Connection) -> Result<T, DbError> + Send + 'static,
        T: Send + 'static,
    {
        self.jobs.run_catalog_job(f).await
    }
}

#[async_trait]
impl Catalog for SqliteCatalog {
    fn levels(&self) -> Vec<LevelDef> {
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
        let parent = parent.clone();
        self.run(move |conn| match parent.parts() {
            [] => list_databases(conn, &opts),
            [db] => list_tables(conn, db, &opts),
            [db, table] => list_columns(conn, db, table, &opts),
            _ => Ok(Page {
                items: Vec::new(),
                next: None,
            }),
        })
        .await
    }

    async fn describe(&self, path: &ObjectPath) -> Result<ObjectDetail, DbError> {
        let path = path.clone();
        self.run(move |conn| describe_path(conn, &path)).await
    }

    /// SQLite has `SCHEMA_DECLARED` — this exists mainly for driver-contract
    /// completeness. Rather than parroting the declared schema back as a
    /// trivially-"sampled" trie, it does a real (bounded) sample: SQLite's
    /// type affinity means a declared `INTEGER` column can still hold TEXT,
    /// so sampling actual storage classes is more honest than the decl type
    /// alone. Never lie about a value.
    async fn infer_shape(
        &self,
        path: &ObjectPath,
        sample_size: u32,
    ) -> Result<InferredSchema, DbError> {
        let path = path.clone();
        self.run(move |conn| infer_shape_impl(conn, &path, sample_size))
            .await
    }

    /// Bounded `LIKE`-prefix completion over `sqlite_master`: a bounded
    /// server-side prefix query, so the whole schema never has to be
    /// resident just to complete a 3-character prefix.
    async fn complete(&self, ctx: CompletionCtx) -> Result<Vec<Completion>, DbError> {
        let prefix = current_word(&ctx.text, ctx.offset as usize).to_string();
        self.run(move |conn| complete_impl(conn, &prefix)).await
    }
}

fn list_databases(
    conn: &rusqlite::Connection,
    opts: &ListOpts,
) -> Result<Page<ObjectNode>, DbError> {
    let mut stmt = conn
        .prepare("PRAGMA database_list")
        .map_err(map_sqlite_err)?;
    let mut rows = stmt.query([]).map_err(map_sqlite_err)?;
    let mut names = Vec::new();
    while let Some(row) = rows.next().map_err(map_sqlite_err)? {
        let name: String = row.get(1).map_err(map_sqlite_err)?;
        names.push(name);
    }
    if let Some(prefix) = &opts.prefix {
        names.retain(|n| n.starts_with(prefix.as_ref()));
    }
    names.sort();
    let nodes: Vec<ObjectNode> = names
        .into_iter()
        .map(|name| ObjectNode {
            path: ObjectPath::root().child(name),
            kind: ObjectKind::Database,
            has_children: true,
            comment: None,
        })
        .collect();
    Ok(paginate(
        nodes,
        |n| n.path.parts().last().map_or("", |p| p),
        opts,
    ))
}

fn list_tables(
    conn: &rusqlite::Connection,
    db: &str,
    opts: &ListOpts,
) -> Result<Page<ObjectNode>, DbError> {
    let master = format!("{}.sqlite_master", quote_ident(db)?);
    let like_pattern = format!("{}%", opts.prefix.as_deref().unwrap_or(""));
    let mut sql = format!(
        "SELECT name, type FROM {master} WHERE type IN ('table','view') \
         AND name NOT LIKE 'sqlite\\_%' ESCAPE '\\' AND name LIKE ?1"
    );
    let mut params: Vec<Value> = vec![Value::Str(Arc::from(like_pattern.as_str()))];
    if let Some(ResumeToken(bytes)) = &opts.resume {
        let after = String::from_utf8_lossy(bytes).into_owned();
        sql.push_str(" AND name > ?2");
        params.push(Value::Str(Arc::from(after.as_str())));
    }
    sql.push_str(" ORDER BY name LIMIT ?");
    params.push(Value::I64(i64::from(opts.limit.max(1))));

    let mut stmt = conn.prepare(&sql).map_err(map_sqlite_err)?;
    let bound: Vec<SqlParam<'_>> = params.iter().map(SqlParam).collect();
    let mut rows = stmt
        .query(rusqlite::params_from_iter(bound))
        .map_err(map_sqlite_err)?;
    let mut items = Vec::new();
    while let Some(row) = rows.next().map_err(map_sqlite_err)? {
        let name: String = row.get(0).map_err(map_sqlite_err)?;
        let ty: String = row.get(1).map_err(map_sqlite_err)?;
        items.push(ObjectNode {
            path: ObjectPath::new(vec![Arc::from(db), Arc::from(name.as_str())]),
            kind: if ty == "view" {
                ObjectKind::View
            } else {
                ObjectKind::Table
            },
            has_children: true,
            comment: None,
        });
    }
    let next = items.last().map(|n| {
        let last_name = n
            .path
            .parts()
            .last()
            .map_or(String::new(), |p| p.to_string());
        ResumeToken(bytes::Bytes::from(last_name.into_bytes()))
    });
    let next = if items.len() as u32 >= opts.limit.max(1) {
        next
    } else {
        None
    };
    Ok(Page { items, next })
}

fn list_columns(
    conn: &rusqlite::Connection,
    db: &str,
    table: &str,
    opts: &ListOpts,
) -> Result<Page<ObjectNode>, DbError> {
    let path = ObjectPath::new(vec![Arc::from(db), Arc::from(table)]);
    let mut cols = table_info(conn, &path)?;
    if let Some(prefix) = &opts.prefix {
        cols.retain(|c| c.name.starts_with(prefix.as_ref()));
    }
    let nodes: Vec<ObjectNode> = cols
        .into_iter()
        .map(|c| ObjectNode {
            path: ObjectPath::new(vec![
                Arc::from(db),
                Arc::from(table),
                Arc::from(c.name.as_str()),
            ]),
            kind: ObjectKind::Column,
            has_children: false,
            comment: c.decl_type.map(|t| Arc::from(t.as_str())),
        })
        .collect();
    Ok(paginate(
        nodes,
        |n| n.path.parts().last().map_or("", |p| p),
        opts,
    ))
}

fn describe_path(conn: &rusqlite::Connection, path: &ObjectPath) -> Result<ObjectDetail, DbError> {
    match path.parts() {
        [db] => {
            let mut stmt = conn
                .prepare("PRAGMA database_list")
                .map_err(map_sqlite_err)?;
            let mut rows = stmt.query([]).map_err(map_sqlite_err)?;
            while let Some(row) = rows.next().map_err(map_sqlite_err)? {
                let name: String = row.get(1).map_err(map_sqlite_err)?;
                if name.as_str() == db.as_ref() {
                    let file: String = row.get(2).map_err(map_sqlite_err)?;
                    return Ok(ObjectDetail {
                        node: ObjectNode {
                            path: path.clone(),
                            kind: ObjectKind::Database,
                            has_children: true,
                            comment: None,
                        },
                        schema: None,
                        extra: vec![(
                            Arc::from("file"),
                            Arc::from(if file.is_empty() { ":memory:" } else { &file }),
                        )],
                    });
                }
            }
            Err(DbError::Query {
                code: None,
                message: format!("no such database: {db}"),
                position: None,
            })
        }
        [db, table] => {
            let cols = table_info(conn, path)?;
            if cols.is_empty() {
                return Err(DbError::Query {
                    code: None,
                    message: format!("no such table: {db}.{table}"),
                    position: None,
                });
            }
            let ty = object_kind(conn, db, table)?;
            // Indexes are listed here and only here — on an explicit
            // `describe()` of this one table — lazily, never on tree
            // expansion and never on connect. `PRAGMA index_list` +
            // `index_xinfo` are metadata-only and O(indexes), not O(rows).
            let indexes = list_indexes(conn, db, table)?;
            let mut extra = vec![(Arc::from("indexes"), Arc::from(indexes_json(&indexes)))];
            let defaults = column_defaults_json(&cols);
            if let Some(defaults) = defaults {
                extra.push((Arc::from("column_defaults"), Arc::from(defaults)));
            }
            // Row count is deliberately NOT included here: `COUNT(*)` on
            // SQLite has no cheap estimate (no trustworthy `reltuples`
            // equivalent) and can be O(table size) — running it on every
            // `describe()` would violate the "never eager, never slow on
            // the happy path" catalog philosophy. A caller
            // that wants an exact count should ask for it explicitly via
            // `Op::Count`, which is what that request exists for.
            Ok(ObjectDetail {
                node: ObjectNode {
                    path: path.clone(),
                    kind: ty,
                    has_children: true,
                    comment: None,
                },
                schema: Some(row_schema_from_table_info(&cols, &indexes)),
                extra,
            })
        }
        _ => Err(DbError::Query {
            code: None,
            message: format!("`{path}` is not a describable object"),
            position: None,
        }),
    }
}

fn object_kind(conn: &rusqlite::Connection, db: &str, table: &str) -> Result<ObjectKind, DbError> {
    let master = format!("{}.sqlite_master", quote_ident(db)?);
    let sql = format!("SELECT type FROM {master} WHERE name = ?1");
    let ty: Option<String> = conn
        .query_row(&sql, [table], |r| r.get(0))
        .map_err(map_sqlite_err)?;
    Ok(match ty.as_deref() {
        Some("view") => ObjectKind::View,
        _ => ObjectKind::Table,
    })
}

/// One key column of an index, from `PRAGMA index_xinfo` (`key = 1` rows).
struct IndexColumn {
    /// `None` for the rowid (`cid = -1`) and for expression columns
    /// (`cid = -2`) — SQLite has no cheap name for either; the `definition`
    /// SQL is where the reader sees the expression text.
    name: Option<String>,
    descending: bool,
}

/// One index of a table, from `PRAGMA index_list` + `index_xinfo` +
/// `sqlite_master.sql`.
struct IndexInfo {
    name: String,
    unique: bool,
    /// `index_list.origin`: `"c"` = CREATE INDEX, `"u"` = UNIQUE constraint,
    /// `"pk"` = PRIMARY KEY constraint.
    origin: String,
    partial: bool,
    columns: Vec<IndexColumn>,
    /// The verbatim `CREATE INDEX …` statement; `None` for indexes SQLite
    /// created implicitly (constraint-backed ones have no stored SQL).
    definition: Option<String>,
}

/// All indexes of `db.table`, one `PRAGMA index_list` plus one
/// `PRAGMA index_xinfo` per index. `PRAGMA` cannot bind parameters, so every
/// interpolated identifier goes through [`quote_ident`], which rejects
/// NUL-embedded (silently truncating) names outright.
fn list_indexes(
    conn: &rusqlite::Connection,
    db: &str,
    table: &str,
) -> Result<Vec<IndexInfo>, DbError> {
    let qdb = quote_ident(db)?;
    let sql = format!("PRAGMA {qdb}.index_list({})", quote_ident(table)?);
    let mut stmt = conn.prepare(&sql).map_err(map_sqlite_err)?;
    let mut rows = stmt.query([]).map_err(map_sqlite_err)?;
    let mut heads: Vec<(String, bool, String, bool)> = Vec::new();
    while let Some(row) = rows.next().map_err(map_sqlite_err)? {
        // index_list columns: seq, name, unique, origin, partial
        let name: String = row.get("name").map_err(map_sqlite_err)?;
        let unique: i64 = row.get("unique").map_err(map_sqlite_err)?;
        let origin: String = row.get("origin").map_err(map_sqlite_err)?;
        let partial: i64 = row.get("partial").map_err(map_sqlite_err)?;
        heads.push((name, unique != 0, origin, partial != 0));
    }
    drop(rows);
    drop(stmt);

    let mut out = Vec::with_capacity(heads.len());
    for (name, unique, origin, partial) in heads {
        let xsql = format!("PRAGMA {qdb}.index_xinfo({})", quote_ident(&name)?);
        let mut xstmt = conn.prepare(&xsql).map_err(map_sqlite_err)?;
        let mut xrows = xstmt.query([]).map_err(map_sqlite_err)?;
        let mut columns = Vec::new();
        while let Some(row) = xrows.next().map_err(map_sqlite_err)? {
            // index_xinfo columns: seqno, cid, name, desc, coll, key
            let key: i64 = row.get("key").map_err(map_sqlite_err)?;
            if key == 0 {
                continue; // trailing rowid/aux columns, not index keys
            }
            let col_name: Option<String> = row.get("name").map_err(map_sqlite_err)?;
            let descending: i64 = row.get("desc").map_err(map_sqlite_err)?;
            columns.push(IndexColumn {
                name: col_name,
                descending: descending != 0,
            });
        }
        drop(xrows);
        drop(xstmt);

        // Auto-created constraint indexes (`sqlite_autoindex_*`) sit in
        // `sqlite_master` with a NULL `sql`; a missing row is treated the
        // same rather than failing the whole describe.
        let dsql =
            format!("SELECT sql FROM {qdb}.sqlite_master WHERE type = 'index' AND name = ?1");
        let definition: Option<String> = {
            use rusqlite::OptionalExtension;
            conn.query_row(&dsql, [name.as_str()], |r| r.get::<_, Option<String>>(0))
                .optional()
                .map_err(map_sqlite_err)?
                .flatten()
        };
        out.push(IndexInfo {
            name,
            unique,
            origin,
            partial,
            columns,
            definition,
        });
    }
    Ok(out)
}

/// The engine-independent index JSON shape (see the datagrep-ffi describe
/// contract): `[{name, columns:[{name, order}], unique, primary, type,
/// partial, filter, size_bytes, definition, sparse, expire_after_seconds}]`.
///
/// SQLite specifics, stated honestly: every SQLite index is a b-tree;
/// per-index size needs the optional `dbstat` vtab, so `size_bytes` is
/// `null`; the partial predicate has no PRAGMA of its own, so `partial` is a
/// boolean and the `WHERE` clause is visible only in `definition`.
fn indexes_json(indexes: &[IndexInfo]) -> String {
    let entries: Vec<String> = indexes
        .iter()
        .map(|ix| {
            let cols: Vec<String> = ix
                .columns
                .iter()
                .map(|c| {
                    format!(
                        "{{\"name\":{},\"order\":{}}}",
                        json_opt_str(c.name.as_deref()),
                        if c.descending { "\"desc\"" } else { "\"asc\"" },
                    )
                })
                .collect();
            format!(
                "{{\"name\":{},\"columns\":[{}],\"unique\":{},\"primary\":{},\
                 \"type\":\"btree\",\"partial\":{},\"filter\":null,\"size_bytes\":null,\
                 \"definition\":{},\"sparse\":false,\"expire_after_seconds\":null}}",
                json_str(&ix.name),
                cols.join(","),
                ix.unique,
                ix.origin == "pk",
                ix.partial,
                json_opt_str(ix.definition.as_deref()),
            )
        })
        .collect();
    format!("[{}]", entries.join(","))
}

/// `{"col": "<default SQL text>"}` for every column that has one; `None`
/// when no column does (so `describe()` doesn't emit an empty `{}` pair).
fn column_defaults_json(columns: &[ColumnInfo]) -> Option<String> {
    let entries: Vec<String> = columns
        .iter()
        .filter_map(|c| {
            c.default_value
                .as_deref()
                .map(|d| format!("{}:{}", json_str(&c.name), json_str(d)))
        })
        .collect();
    if entries.is_empty() {
        None
    } else {
        Some(format!("{{{}}}", entries.join(",")))
    }
}

/// Minimal JSON string encoding. Hand-rolled on purpose: this crate's
/// dependency policy keeps `serde_json` out of drivers (see `Cargo.toml`),
/// and the catalog only ever needs to *emit* a handful of strings/bools.
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

fn infer_shape_impl(
    conn: &rusqlite::Connection,
    path: &ObjectPath,
    sample_size: u32,
) -> Result<InferredSchema, DbError> {
    let table = crate::compile::compile_object_path(path)?;
    let sql = format!("SELECT * FROM {table} LIMIT ?1");
    let mut stmt = conn.prepare(&sql).map_err(map_sqlite_err)?;
    let col_count = stmt.column_count();
    let names: Vec<String> = (0..col_count)
        .map(|i| stmt.column_name(i).unwrap_or("?").to_string())
        .collect();
    let mut tries: Vec<FieldTrie> = (0..col_count).map(|_| FieldTrie::default()).collect();
    let mut sampled: u64 = 0;
    let mut rows = stmt
        .query([i64::from(sample_size.max(1))])
        .map_err(map_sqlite_err)?;
    while let Some(row) = rows.next().map_err(map_sqlite_err)? {
        sampled += 1;
        for (i, trie) in tries.iter_mut().enumerate().take(col_count) {
            let vref = row.get_ref(i).map_err(map_sqlite_err)?;
            let value = sqlite_value_to_datagrep(vref, None);
            if let Some(ty) = value.logical_type() {
                trie.record(ty);
            }
        }
    }
    Ok(InferredSchema {
        sampled,
        root: names
            .into_iter()
            .zip(tries)
            .map(|(n, t)| (Arc::from(n.as_str()), t))
            .collect(),
    })
}

fn complete_impl(conn: &rusqlite::Connection, prefix: &str) -> Result<Vec<Completion>, DbError> {
    if prefix.is_empty() {
        return Ok(Vec::new());
    }
    let like = format!("{prefix}%");
    let mut stmt = conn
        .prepare(
            "SELECT name, type FROM sqlite_master WHERE type IN ('table','view') \
             AND name LIKE ?1 ORDER BY name LIMIT 50",
        )
        .map_err(map_sqlite_err)?;
    let mut rows = stmt.query([like]).map_err(map_sqlite_err)?;
    let mut out = Vec::new();
    while let Some(row) = rows.next().map_err(map_sqlite_err)? {
        let name: String = row.get(0).map_err(map_sqlite_err)?;
        let ty: String = row.get(1).map_err(map_sqlite_err)?;
        out.push(Completion {
            label: Arc::from(name.as_str()),
            kind: if ty == "view" {
                ObjectKind::View
            } else {
                ObjectKind::Table
            },
            detail: Some(Arc::from(ty.as_str())),
        });
    }
    Ok(out)
}

/// The identifier-ish word ending at `offset` in `text` — a minimal
/// tokenizer, not a SQL parser. The editor is language-agnostic and
/// datagrep never translates user text; this only finds the prefix to look up.
fn current_word(text: &str, offset: usize) -> &str {
    let bytes = text.as_bytes();
    let end = offset.min(bytes.len());
    let mut start = end;
    while start > 0 {
        let ch = bytes[start - 1];
        if ch.is_ascii_alphanumeric() || ch == b'_' {
            start -= 1;
        } else {
            break;
        }
    }
    std::str::from_utf8(&bytes[start..end]).unwrap_or("")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn current_word_finds_trailing_identifier() {
        assert_eq!(current_word("SELECT * FROM use", 18), "use");
        assert_eq!(current_word("SELECT * FROM ", 14), "");
        assert_eq!(current_word("x", 1), "x");
    }

    #[test]
    fn json_str_escapes_quotes_backslashes_and_control_chars() {
        assert_eq!(json_str("plain"), r#""plain""#);
        assert_eq!(json_str("a\"b\\c"), r#""a\"b\\c""#);
        assert_eq!(json_str("l1\nl2\t\r"), r#""l1\nl2\t\r""#);
        assert_eq!(json_str("\u{1}"), "\"\\u0001\"");
        assert_eq!(json_opt_str(None), "null");
    }

    fn memory_conn_with_indexes() -> rusqlite::Connection {
        let conn = rusqlite::Connection::open_in_memory().expect("open :memory:");
        conn.execute_batch(
            "CREATE TABLE users (\
                 id INTEGER PRIMARY KEY,\
                 email TEXT NOT NULL,\
                 age INTEGER DEFAULT 18,\
                 deleted_at TEXT\
             );\
             CREATE UNIQUE INDEX idx_users_email ON users(email);\
             CREATE INDEX idx_users_age_desc ON users(age DESC, email)\
                 WHERE deleted_at IS NULL;",
        )
        .expect("schema setup");
        conn
    }

    /// The full shape contract for one engine, verified by *parsing* the
    /// emitted JSON (with serde_json, dev-only) rather than substring poking:
    /// every entry carries the cross-engine keys, key order and direction
    /// are right, and the unique/partial facts land where the UI looks.
    #[test]
    fn describe_emits_index_json_with_the_cross_engine_shape() {
        let conn = memory_conn_with_indexes();
        let path = ObjectPath::new(vec![Arc::from("main"), Arc::from("users")]);
        let detail = describe_path(&conn, &path).expect("describe users");

        let raw = detail
            .extra
            .iter()
            .find(|(k, _)| k.as_ref() == "indexes")
            .map(|(_, v)| v.to_string())
            .expect("describe() must attach an `indexes` extra");
        let parsed: serde_json::Value = serde_json::from_str(&raw).expect("indexes is valid JSON");
        let list = parsed.as_array().expect("indexes is a JSON array");
        assert_eq!(list.len(), 2, "{raw}");

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
                assert!(
                    entry.get(key).is_some(),
                    "index entry missing {key}: {entry}"
                );
            }
            assert_eq!(entry["type"], "btree");
            assert_eq!(entry["size_bytes"], serde_json::Value::Null);
        }

        let email = list
            .iter()
            .find(|e| e["name"] == "idx_users_email")
            .expect("unique email index listed");
        assert_eq!(email["unique"], true);
        assert_eq!(email["primary"], false);
        assert_eq!(email["partial"], false);
        assert_eq!(email["columns"][0]["name"], "email");
        assert_eq!(email["columns"][0]["order"], "asc");
        assert!(email["definition"]
            .as_str()
            .expect("CREATE INDEX sql present")
            .contains("UNIQUE INDEX"));

        let age = list
            .iter()
            .find(|e| e["name"] == "idx_users_age_desc")
            .expect("compound partial index listed");
        assert_eq!(age["unique"], false);
        assert_eq!(age["partial"], true, "WHERE-claused index is partial");
        let cols = age["columns"].as_array().expect("columns array");
        assert_eq!(cols.len(), 2, "compound index keeps key order");
        assert_eq!(cols[0]["name"], "age");
        assert_eq!(cols[0]["order"], "desc");
        assert_eq!(cols[1]["name"], "email");
        assert_eq!(cols[1]["order"], "asc");
    }

    #[test]
    fn describe_reports_defaults_and_index_flags_on_columns() {
        let conn = memory_conn_with_indexes();
        let path = ObjectPath::new(vec![Arc::from("main"), Arc::from("users")]);
        let detail = describe_path(&conn, &path).expect("describe users");

        let defaults = detail
            .extra
            .iter()
            .find(|(k, _)| k.as_ref() == "column_defaults")
            .map(|(_, v)| v.to_string())
            .expect("age has a default, so the pair must exist");
        let parsed: serde_json::Value =
            serde_json::from_str(&defaults).expect("column_defaults is valid JSON");
        assert_eq!(parsed["age"], "18");
        assert!(parsed.get("email").is_none(), "no default, no key");

        let schema = detail.schema.expect("declared schema");
        let email = schema
            .fields
            .iter()
            .find(|f| f.name.as_ref() == "email")
            .expect("email field");
        assert!(email.flags.contains(FieldFlags::INDEXED));
        assert!(
            email.flags.contains(FieldFlags::UNIQUE),
            "single-column unique index marks the column unique"
        );
        let age = schema
            .fields
            .iter()
            .find(|f| f.name.as_ref() == "age")
            .expect("age field");
        assert!(
            age.flags.contains(FieldFlags::INDEXED),
            "leading column of the compound index"
        );
        assert!(!age.flags.contains(FieldFlags::UNIQUE));
    }

    /// A table with no indexes still reports `indexes` — as an honest `[]`,
    /// which the UI renders as "none" (distinct from "not reported").
    #[test]
    fn a_table_without_indexes_reports_an_empty_array() {
        let conn = rusqlite::Connection::open_in_memory().expect("open :memory:");
        conn.execute_batch("CREATE TABLE bare (v);").expect("setup");
        let path = ObjectPath::new(vec![Arc::from("main"), Arc::from("bare")]);
        let detail = describe_path(&conn, &path).expect("describe bare");
        let raw = detail
            .extra
            .iter()
            .find(|(k, _)| k.as_ref() == "indexes")
            .map(|(_, v)| v.to_string())
            .expect("indexes extra present even when empty");
        assert_eq!(raw, "[]");
    }
}
