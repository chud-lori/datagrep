//! SQL generation: identifier quoting and the `Op` → SQL compiler.
//!
//! The injection rules for this driver: values are ALWAYS bound as `$n`
//! parameters — never spliced as text — and identifiers always
//! go through [`quote_ident`]. Nothing in this module ever interpolates a
//! `Value` into the SQL string itself.

use std::fmt::Write as _;
#[cfg(test)]
use std::sync::Arc;

use datagrep_api::{
    DbError, DdlOp, FieldPath, ObjectKind, ObjectPath, PathSeg, Predicate, SortKey, Value,
};

/// Quote a Postgres identifier. Embedded `"` are doubled;
/// embedded NUL is rejected outright — Postgres identifiers cannot contain
/// NUL and silently truncating at the NUL would let a name lie about itself.
pub fn quote_ident(ident: &str) -> Result<String, DbError> {
    if ident.contains('\0') {
        return Err(DbError::Unsupported {
            feature: format!("identifier contains a NUL byte: {ident:?}"),
        });
    }
    let mut out = String::with_capacity(ident.len() + 2);
    out.push('"');
    for c in ident.chars() {
        if c == '"' {
            out.push('"');
        }
        out.push(c);
    }
    out.push('"');
    Ok(out)
}

/// Render an [`ObjectPath`] (`db.schema.table`) as a dot-joined, individually
/// quoted Postgres relation reference. The leading component is dropped when
/// it names the connection's own database — Postgres has no cross-database
/// qualified names, only `schema.table` — but callers that already trimmed
/// the path (catalog code does) can pass a 1–2 element path directly.
pub fn quote_object_path(path: &ObjectPath) -> Result<String, DbError> {
    quote_parts(path.parts())
}

fn quote_parts(parts: &[std::sync::Arc<str>]) -> Result<String, DbError> {
    if parts.is_empty() {
        return Err(DbError::Unsupported {
            feature: "empty object path".into(),
        });
    }
    let mut out = String::new();
    for (i, part) in parts.iter().enumerate() {
        if i > 0 {
            out.push('.');
        }
        out.push_str(&quote_ident(part)?);
    }
    Ok(out)
}

/// Render a [`FieldPath`] as a value-producing SQL expression against a
/// quoted base column.
///
/// A bare single-segment path (`status`) is just the quoted column. A deeper
/// path (`address.city`, `tags[0]`) is rendered as a `jsonb` path-extraction
/// expression (`"address"#>>'{city}'`) — the honest approximation for nested
/// data stored in a `jsonb`/`json` column; it will simply not match rows if
/// the column isn't JSON, which Postgres reports as a normal type error
/// rather than something silently wrong.
pub fn field_path_expr(path: &FieldPath) -> Result<String, DbError> {
    let segs = path.segments();
    let (head, rest) = segs.split_first().ok_or_else(|| DbError::Unsupported {
        feature: "empty field path".into(),
    })?;
    let head_name = match head {
        PathSeg::Field(name) => name,
        PathSeg::Index(_) => {
            return Err(DbError::Unsupported {
                feature: "field path cannot start with an array index".into(),
            })
        }
    };
    let base = quote_ident(head_name)?;
    if rest.is_empty() {
        return Ok(base);
    }
    let mut steps = String::from("{");
    for (i, seg) in rest.iter().enumerate() {
        if i > 0 {
            steps.push(',');
        }
        match seg {
            PathSeg::Field(name) => {
                if name.contains(['{', '}', ',', '"']) {
                    return Err(DbError::Unsupported {
                        feature: format!("field name unsafe for jsonb path: {name:?}"),
                    });
                }
                steps.push_str(name);
            }
            PathSeg::Index(n) => {
                let _ = write!(steps, "{n}");
            }
        }
    }
    steps.push('}');
    // The path literal is a fixed set of already-validated identifier/index
    // tokens, never user-controlled free text spliced in — it is not a
    // parameter binding site the way a *value* would be.
    Ok(format!("{base}#>>'{steps}'"))
}

/// Accumulates `$n` parameters while compiling a predicate/scan tree, so every
/// `Value` in the request ends up bound, never spliced.
#[derive(Default)]
pub struct ParamBuilder {
    pub params: Vec<Value>,
}

impl ParamBuilder {
    fn push(&mut self, v: Value) -> String {
        self.params.push(v);
        format!("${}", self.params.len())
    }
}

/// Compile a [`Predicate`] to a SQL boolean expression, appending bound
/// parameters to `pb`. Returns just the expression text — callers wrap in
/// `WHERE`.
pub fn compile_predicate(pred: &Predicate, pb: &mut ParamBuilder) -> Result<String, DbError> {
    Ok(match pred {
        Predicate::Eq { field, value } => cmp(field, "=", value, pb)?,
        Predicate::Ne { field, value } => cmp(field, "<>", value, pb)?,
        Predicate::Lt { field, value } => cmp(field, "<", value, pb)?,
        Predicate::Le { field, value } => cmp(field, "<=", value, pb)?,
        Predicate::Gt { field, value } => cmp(field, ">", value, pb)?,
        Predicate::Ge { field, value } => cmp(field, ">=", value, pb)?,
        Predicate::In { field, values } => {
            if values.is_empty() {
                // An empty IN-list matches nothing; write it as a tautological
                // false rather than emitting invalid `IN ()` SQL.
                return Ok("false".to_string());
            }
            let expr = field_path_expr(field)?;
            let mut placeholders = Vec::with_capacity(values.len());
            for v in values {
                placeholders.push(pb.push(v.clone()));
            }
            format!("{expr} IN ({})", placeholders.join(", "))
        }
        Predicate::Like { field, pattern } => {
            let expr = field_path_expr(field)?;
            let ph = pb.push(Value::Str(pattern.clone()));
            format!("{expr} LIKE {ph}")
        }
        Predicate::Exists { field } => {
            // Only meaningful for jsonb paths (top-level SQL columns always
            // "exist" once the row exists); render the deepest jsonb `?` /
            // path-presence test when nested, else a trivial true.
            let segs = field.segments();
            if segs.len() <= 1 {
                "true".to_string()
            } else {
                format!("({}) IS NOT NULL", field_path_expr(field)?)
            }
        }
        Predicate::IsNull { field } => format!("({}) IS NULL", field_path_expr(field)?),
        Predicate::And(parts) => join_bool(parts, "AND", pb)?,
        Predicate::Or(parts) => join_bool(parts, "OR", pb)?,
        Predicate::Not(inner) => format!("NOT ({})", compile_predicate(inner, pb)?),
    })
}

fn cmp(
    field: &FieldPath,
    op: &str,
    value: &Value,
    pb: &mut ParamBuilder,
) -> Result<String, DbError> {
    let expr = field_path_expr(field)?;
    let ph = pb.push(value.clone());
    Ok(format!("{expr} {op} {ph}"))
}

fn join_bool(parts: &[Predicate], op: &str, pb: &mut ParamBuilder) -> Result<String, DbError> {
    if parts.is_empty() {
        return Ok(if op == "AND" {
            "true".into()
        } else {
            "false".into()
        });
    }
    let mut rendered = Vec::with_capacity(parts.len());
    for p in parts {
        rendered.push(format!("({})", compile_predicate(p, pb)?));
    }
    Ok(rendered.join(&format!(" {op} ")))
}

/// Compile `ORDER BY` for a [`SortKey`] list.
pub fn compile_order(order: &[SortKey]) -> Result<String, DbError> {
    let mut parts = Vec::with_capacity(order.len());
    for k in order {
        let expr = field_path_expr(&k.path)?;
        let dir = if k.desc { "DESC" } else { "ASC" };
        let nulls = if k.nulls_first {
            "NULLS FIRST"
        } else {
            "NULLS LAST"
        };
        parts.push(format!("{expr} {dir} {nulls}"));
    }
    Ok(parts.join(", "))
}

/// Compile a projection list, or `*` when unset.
pub fn compile_project(project: &Option<Vec<FieldPath>>) -> Result<String, DbError> {
    match project {
        None => Ok("*".to_string()),
        Some(fields) if fields.is_empty() => Ok("*".to_string()),
        Some(fields) => {
            let mut parts = Vec::with_capacity(fields.len());
            for f in fields {
                parts.push(field_path_expr(f)?);
            }
            Ok(parts.join(", "))
        }
    }
}

/// Compile `Op::Scan` to `(sql, params)`. Keyset pagination via `resume` is
/// not implemented in v1 (see `cursor.rs`'s `resume_token` doc) — `resume` is
/// accepted but ignored, which is honest here because the only producer of a
/// `ResumeToken` in this driver (`PgCursor::resume_token`) always returns
/// `None`, so no caller can actually construct one to pass back in.
pub fn compile_scan(
    path: &ObjectPath,
    filter: &Option<Predicate>,
    order: &[SortKey],
    project: &Option<Vec<FieldPath>>,
    limit: Option<u64>,
) -> Result<(String, Vec<Value>), DbError> {
    let mut pb = ParamBuilder::default();
    let table = quote_object_path(path)?;
    let cols = compile_project(project)?;
    let mut sql = format!("SELECT {cols} FROM {table}");
    if let Some(pred) = filter {
        let expr = compile_predicate(pred, &mut pb)?;
        let _ = write!(sql, " WHERE {expr}");
    }
    if !order.is_empty() {
        let _ = write!(sql, " ORDER BY {}", compile_order(order)?);
    }
    if let Some(n) = limit {
        let _ = write!(sql, " LIMIT {n}");
    }
    Ok((sql, pb.params))
}

/// Compile `Op::Count`.
pub fn compile_count(
    path: &ObjectPath,
    filter: &Option<Predicate>,
    exact: bool,
) -> Result<(String, Vec<Value>), DbError> {
    let mut pb = ParamBuilder::default();
    let table = quote_object_path(path)?;
    // `exact: false` still runs a real COUNT(*) in v1 — Postgres has no O(1)
    // exact-count shortcut, but reltuples (the cheap estimate the catalog
    // uses for `describe`) is not wired through `Op::Count`; see the gap
    // noted in the crate's top-level docs.
    let _ = exact;
    let mut sql = format!("SELECT COUNT(*) AS count FROM {table}");
    if let Some(pred) = filter {
        let expr = compile_predicate(pred, &mut pb)?;
        let _ = write!(sql, " WHERE {expr}");
    }
    Ok((sql, pb.params))
}

/// Trap: `View` covers both the `v` and `m` relkinds, and `DROP VIEW` on a
/// materialized view fails even with `IF EXISTS`. The server's message names
/// the statement to use, so it is surfaced rather than guessed at.
fn relation_keyword(kind: ObjectKind) -> Result<&'static str, DbError> {
    match kind {
        ObjectKind::Table => Ok("TABLE"),
        ObjectKind::View => Ok("VIEW"),
        ObjectKind::Index => Ok("INDEX"),
        ObjectKind::Schema => Ok("SCHEMA"),
        other => Err(DbError::Unsupported {
            feature: format!("{other:?} is not a Postgres object this driver can administer"),
        }),
    }
}

/// Compile a structured [`DdlOp`]. Names are identifiers, never values, so
/// nothing here binds a parameter — every one goes through [`quote_ident`].
pub fn compile_ddl(op: &DdlOp) -> Result<String, DbError> {
    match op {
        DdlOp::Native { text } => Ok(text.to_string()),
        DdlOp::Drop {
            path,
            kind,
            if_exists,
        } => {
            let keyword = relation_keyword(*kind)?;
            let target = object_ref(path, *kind)?;
            let guard = if *if_exists { "IF EXISTS " } else { "" };
            Ok(format!("DROP {keyword} {guard}{target}"))
        }
        DdlOp::Rename { from, to, kind } => {
            let keyword = relation_keyword(*kind)?;
            let target = object_ref(from, *kind)?;
            let new_name = quote_ident(DdlOp::rename_target(from, to)?)?;
            Ok(format!("ALTER {keyword} {target} RENAME TO {new_name}"))
        }
        DdlOp::CreateIndex {
            path,
            name,
            fields,
            unique,
            if_not_exists,
        } => {
            if fields.is_empty() {
                return Err(DbError::Unsupported {
                    feature: "CREATE INDEX with no fields".into(),
                });
            }
            let table = quote_object_path(path)?;
            let index = quote_ident(name)?;
            let unique = if *unique { "UNIQUE " } else { "" };
            let guard = if *if_not_exists { "IF NOT EXISTS " } else { "" };
            let mut cols = Vec::with_capacity(fields.len());
            for f in fields {
                cols.push(index_column(f)?);
            }
            Ok(format!(
                "CREATE {unique}INDEX {guard}{index} ON {table} ({})",
                cols.join(", ")
            ))
        }
    }
}

/// An index is a sibling of its table in the schema, not a child: passing the
/// table through would make `schema.table.index`, which Postgres rejects as
/// "improper relation name (too many dotted names)".
fn object_ref(path: &ObjectPath, kind: ObjectKind) -> Result<String, DbError> {
    let parts = path.parts();
    if kind != ObjectKind::Index {
        return quote_parts(parts);
    }
    let Some(split) = parts.len().checked_sub(2) else {
        return Err(DbError::Unsupported {
            feature: format!(
                "index path {path} must name the object it is on followed by the index name"
            ),
        });
    };
    let name = quote_ident(&parts[parts.len() - 1])?;
    if split == 0 {
        return Ok(name);
    }
    let mut out = quote_parts(&parts[..split])?;
    out.push('.');
    out.push_str(&name);
    Ok(out)
}

/// Whole columns only: a nested path would silently become a `jsonb`
/// extraction, which is an expression index with quite different rules.
fn index_column(path: &FieldPath) -> Result<String, DbError> {
    match path.segments() {
        [PathSeg::Field(name)] => quote_ident(name),
        _ => Err(DbError::Unsupported {
            feature: format!(
                "index key {path} is not a plain column — an expression index is native DDL"
            ),
        }),
    }
}

/// Wrap an inner request's compiled SQL in `EXPLAIN [ANALYZE]`.
pub fn wrap_explain(inner_sql: &str, analyze: bool) -> String {
    if analyze {
        format!("EXPLAIN (ANALYZE, VERBOSE, FORMAT TEXT) {inner_sql}")
    } else {
        format!("EXPLAIN (VERBOSE, FORMAT TEXT) {inner_sql}")
    }
}

/// A single generated mutation statement plus its bound params.
pub struct MutationSql {
    pub sql: String,
    pub params: Vec<Value>,
}

/// Compile one `Mutation` into `UPDATE ... SET ... WHERE <pk> = $n [...] `,
/// `INSERT INTO ... VALUES (...)`, or `DELETE FROM ... WHERE <pk> = $n [...]`.
///
/// The row identity arrives as named `(FieldPath, Value)` pairs on the
/// mutation itself (the same shape as `sets`), so the WHERE clause compiles
/// directly — no `pg_index` lookup, no positional convention.
pub fn compile_mutation(m: &datagrep_api::Mutation) -> Result<MutationSql, DbError> {
    use datagrep_api::Mutation;
    let mut pb = ParamBuilder::default();
    match m {
        Mutation::Insert { path, doc } => {
            let table = quote_object_path(path)?;
            let fields = match doc {
                Value::Document(d) => d.iter().collect::<Vec<_>>(),
                other => {
                    return Err(DbError::Unsupported {
                        feature: format!("insert document must be Value::Document, got {other:?}"),
                    })
                }
            };
            let mut cols = Vec::with_capacity(fields.len());
            let mut phs = Vec::with_capacity(fields.len());
            for (name, value) in fields {
                cols.push(quote_ident(name)?);
                phs.push(pb.push(value.clone()));
            }
            let sql = format!(
                "INSERT INTO {table} ({}) VALUES ({})",
                cols.join(", "),
                phs.join(", ")
            );
            Ok(MutationSql {
                sql,
                params: pb.params,
            })
        }
        Mutation::Update {
            path,
            key,
            sets,
            expect,
        } => {
            refuse_expect(expect)?;
            let table = quote_object_path(path)?;
            if sets.is_empty() {
                return Err(DbError::Unsupported {
                    feature: "update with no fields to set".into(),
                });
            }
            let mut set_parts = Vec::with_capacity(sets.len());
            for (field, value) in sets {
                let col = field_path_expr(field)?;
                let ph = pb.push(value.clone());
                set_parts.push(format!("{col} = {ph}"));
            }
            let where_clause = key_where(key, &mut pb)?;
            let sql = format!(
                "UPDATE {table} SET {} WHERE {where_clause}",
                set_parts.join(", ")
            );
            Ok(MutationSql {
                sql,
                params: pb.params,
            })
        }
        Mutation::Delete { path, key, expect } => {
            refuse_expect(expect)?;
            let table = quote_object_path(path)?;
            let where_clause = key_where(key, &mut pb)?;
            let sql = format!("DELETE FROM {table} WHERE {where_clause}");
            Ok(MutationSql {
                sql,
                params: pb.params,
            })
        }
    }
}

/// A non-empty `expect` precondition must be honoured or refused, never
/// dropped — dropping it would turn a guarded write into a clobber. This
/// driver does not compile preconditions yet, so it refuses.
fn refuse_expect(expect: &[(FieldPath, Value)]) -> Result<(), DbError> {
    if expect.is_empty() {
        return Ok(());
    }
    Err(DbError::Unsupported {
        feature: "conditional mutation (`expect`) — this driver cannot check-and-set".into(),
    })
}

/// The named row identity as `"col" = $n AND …`. An empty key is refused —
/// we never guess which row to affect.
fn key_where(key: &[(FieldPath, Value)], pb: &mut ParamBuilder) -> Result<String, DbError> {
    if key.is_empty() {
        return Err(DbError::Unsupported {
            feature: "mutation with no row identity — refuse to guess which row to affect".into(),
        });
    }
    let mut parts = Vec::with_capacity(key.len());
    for (field, value) in key {
        let col = field_path_expr(field)?;
        let ph = pb.push(value.clone());
        parts.push(format!("{col} = {ph}"));
    }
    Ok(parts.join(" AND "))
}

#[cfg(test)]
mod tests {
    use super::*;
    use datagrep_api::{Op, Predicate as P};

    fn path(parts: &[&str]) -> ObjectPath {
        ObjectPath::new(parts.iter().map(|p| Arc::from(*p)).collect())
    }

    #[test]
    fn ddl_names_the_statement_for_the_catalog_kind() {
        let users = path(&["app", "users"]);
        for (kind, expected) in [
            (ObjectKind::Table, "DROP TABLE IF EXISTS \"app\".\"users\""),
            (ObjectKind::View, "DROP VIEW IF EXISTS \"app\".\"users\""),
            (
                ObjectKind::Schema,
                "DROP SCHEMA IF EXISTS \"app\".\"users\"",
            ),
        ] {
            let sql = compile_ddl(&DdlOp::Drop {
                path: users.clone(),
                kind,
                if_exists: true,
            })
            .unwrap();
            assert_eq!(sql, expected);
        }
        assert!(compile_ddl(&DdlOp::Drop {
            path: users.clone(),
            kind: ObjectKind::Collection,
            if_exists: false,
        })
        .is_err());
    }

    #[test]
    fn ddl_quotes_every_name_it_splices() {
        // Object names reach the statement as identifiers, so a name that
        // tries to close its own quoting must not be able to.
        let sql = compile_ddl(&DdlOp::Drop {
            path: path(&["app", "users\"; DROP TABLE secrets; --"]),
            kind: ObjectKind::Table,
            if_exists: false,
        })
        .unwrap();
        assert_eq!(
            sql,
            "DROP TABLE \"app\".\"users\"\"; DROP TABLE secrets; --\""
        );
        assert!(compile_ddl(&DdlOp::Drop {
            path: path(&["app", "nul\0name"]),
            kind: ObjectKind::Table,
            if_exists: false,
        })
        .is_err());
    }

    #[test]
    fn ddl_rename_and_create_index() {
        let sql = compile_ddl(&DdlOp::Rename {
            from: path(&["app", "users"]),
            to: path(&["app", "people"]),
            kind: ObjectKind::Table,
        })
        .unwrap();
        assert_eq!(sql, "ALTER TABLE \"app\".\"users\" RENAME TO \"people\"");

        // A Postgres index is a sibling of its table in the schema, so the
        // table component comes back out of the index path.
        let sql = compile_ddl(&DdlOp::Drop {
            path: path(&["db", "app", "users", "users_email"]),
            kind: ObjectKind::Index,
            if_exists: true,
        })
        .unwrap();
        assert_eq!(sql, "DROP INDEX IF EXISTS \"db\".\"app\".\"users_email\"");
        // A path already trimmed to `table.index` leaves the index bare.
        assert_eq!(
            compile_ddl(&DdlOp::Drop {
                path: path(&["users", "users_email"]),
                kind: ObjectKind::Index,
                if_exists: false,
            })
            .unwrap(),
            "DROP INDEX \"users_email\""
        );
        assert!(compile_ddl(&DdlOp::Drop {
            path: path(&["users_email"]),
            kind: ObjectKind::Index,
            if_exists: false,
        })
        .is_err());

        let sql = compile_ddl(&DdlOp::CreateIndex {
            path: path(&["app", "users"]),
            name: Arc::from("users_email"),
            fields: vec![FieldPath::field("email"), FieldPath::field("tenant")],
            unique: true,
            if_not_exists: true,
        })
        .unwrap();
        assert_eq!(
            sql,
            "CREATE UNIQUE INDEX IF NOT EXISTS \"users_email\" ON \"app\".\"users\" \
             (\"email\", \"tenant\")"
        );

        // A nested key would silently become a jsonb extraction expression.
        assert!(compile_ddl(&DdlOp::CreateIndex {
            path: path(&["app", "users"]),
            name: Arc::from("bad"),
            fields: vec!["address.city".parse().unwrap()],
            unique: false,
            if_not_exists: false,
        })
        .is_err());
        assert!(compile_ddl(&DdlOp::CreateIndex {
            path: path(&["app", "users"]),
            name: Arc::from("bad"),
            fields: Vec::new(),
            unique: false,
            if_not_exists: false,
        })
        .is_err());
    }

    #[test]
    fn quote_ident_doubles_embedded_quotes() {
        assert_eq!(quote_ident("users").unwrap(), "\"users\"");
        assert_eq!(quote_ident("a\"b").unwrap(), "\"a\"\"b\"");
        assert_eq!(quote_ident("a\"\"b").unwrap(), "\"a\"\"\"\"b\"");
    }

    #[test]
    fn quote_ident_preserves_unicode() {
        assert_eq!(quote_ident("héllo_wörld").unwrap(), "\"héllo_wörld\"");
        assert_eq!(quote_ident("名前").unwrap(), "\"名前\"");
    }

    #[test]
    fn quote_ident_rejects_nul() {
        let err = quote_ident("ab\0cd").unwrap_err();
        assert!(matches!(err, DbError::Unsupported { .. }));
    }

    #[test]
    fn quote_ident_empty_and_whitespace() {
        assert_eq!(quote_ident("").unwrap(), "\"\"");
        assert_eq!(quote_ident(" has space ").unwrap(), "\" has space \"");
    }

    #[test]
    fn quote_object_path_joins_and_quotes_each_part() {
        let p = ObjectPath::new(vec![Arc::from("public"), Arc::from("Users")]);
        assert_eq!(quote_object_path(&p).unwrap(), "\"public\".\"Users\"");
    }

    #[test]
    fn predicate_compiles_to_placeholders_never_literal_values() {
        let pred = P::And(vec![
            P::Eq {
                field: FieldPath::field("status"),
                value: Value::Str(Arc::from("super-secret-literal")),
            },
            P::Ge {
                field: FieldPath::field("age"),
                value: Value::I64(21),
            },
        ]);
        let mut pb = ParamBuilder::default();
        let sql = compile_predicate(&pred, &mut pb).unwrap();
        assert!(sql.contains('$'), "expected a $n placeholder: {sql}");
        assert!(
            !sql.contains("super-secret-literal"),
            "value leaked into SQL text: {sql}"
        );
        assert!(
            !sql.contains('2') || sql.contains("$2"),
            "age value should not appear bare"
        );
        assert_eq!(pb.params.len(), 2);
        assert_eq!(pb.params[0], Value::Str(Arc::from("super-secret-literal")));
    }

    #[test]
    fn scan_op_uses_dollar_placeholders_and_quoted_identifiers() {
        let path = ObjectPath::new(vec![Arc::from("app"), Arc::from("users")]);
        let filter = Some(P::Eq {
            field: FieldPath::field("email"),
            value: Value::Str(Arc::from("a@b.com")),
        });
        let order = vec![SortKey {
            path: FieldPath::field("id"),
            desc: true,
            nulls_first: false,
        }];
        let (sql, params) = compile_scan(&path, &filter, &order, &None, Some(50)).unwrap();
        assert!(sql.starts_with("SELECT * FROM \"app\".\"users\""), "{sql}");
        assert!(sql.contains("WHERE \"email\" = $1"), "{sql}");
        assert!(sql.contains("ORDER BY \"id\" DESC NULLS LAST"), "{sql}");
        assert!(sql.contains("LIMIT 50"), "{sql}");
        assert!(!sql.contains("a@b.com"), "literal leaked: {sql}");
        assert_eq!(params, vec![Value::Str(Arc::from("a@b.com"))]);
    }

    #[test]
    fn nested_field_path_becomes_jsonb_extraction() {
        let expr = field_path_expr(&"address.city".parse().unwrap()).unwrap();
        assert_eq!(expr, "\"address\"#>>'{city}'");
    }

    #[test]
    fn in_predicate_empty_list_is_tautological_false() {
        let mut pb = ParamBuilder::default();
        let sql = compile_predicate(
            &P::In {
                field: FieldPath::field("id"),
                values: vec![],
            },
            &mut pb,
        )
        .unwrap();
        assert_eq!(sql, "false");
        assert!(pb.params.is_empty());
    }

    #[test]
    fn scan_op_round_trips_through_compiler_object_safety() {
        // Sanity: Op::Scan (datagrep-api's shape) matches what compile_scan expects.
        let op = Op::Scan {
            path: ObjectPath::new(vec![Arc::from("t")]),
            filter: None,
            order: vec![],
            project: None,
            limit: None,
            resume: None,
        };
        if let Op::Scan {
            path,
            filter,
            order,
            project,
            limit,
            ..
        } = op
        {
            let (sql, params) = compile_scan(&path, &filter, &order, &project, limit).unwrap();
            assert_eq!(sql, "SELECT * FROM \"t\"");
            assert!(params.is_empty());
        }
    }

    #[test]
    fn non_empty_expect_is_refused_not_dropped() {
        // This driver does not compile `expect` preconditions yet; a caller
        // asking for check-and-set must get Unsupported, never a plain write.
        let expect = vec![(FieldPath::field("version"), Value::I64(3))];
        let update = datagrep_api::Mutation::Update {
            path: ObjectPath::new(vec![Arc::from("t")]),
            key: vec![(FieldPath::field("id"), Value::I64(1))],
            sets: vec![(FieldPath::field("name"), Value::Str(Arc::from("x")))],
            expect: expect.clone(),
        };
        assert!(matches!(
            compile_mutation(&update),
            Err(DbError::Unsupported { .. })
        ));
        let delete = datagrep_api::Mutation::Delete {
            path: ObjectPath::new(vec![Arc::from("t")]),
            key: vec![(FieldPath::field("id"), Value::I64(1))],
            expect,
        };
        assert!(matches!(
            compile_mutation(&delete),
            Err(DbError::Unsupported { .. })
        ));
    }
}
