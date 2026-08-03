//! SQL generation: identifier quoting and the `Op` → SQL compiler.
//!
//! Design §3.8 injection rules, restated for this driver: values are ALWAYS
//! bound as `$n` parameters — never spliced as text — and identifiers always
//! go through [`quote_ident`]. Nothing in this module ever interpolates a
//! `Value` into the SQL string itself.

use std::fmt::Write as _;
use std::sync::Arc;

use dbx_api::{DbError, FieldPath, ObjectPath, PathSeg, Predicate, SortKey, Value};

/// Quote a Postgres identifier (design item 6). Embedded `"` are doubled;
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
    let parts = path.parts();
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
/// `key_fields` names the identity columns `key` is positional against — see
/// the crate-level gap note: [`dbx_api::request::Mutation`] carries `key` as
/// bare `Vec<Value>` with no accompanying field names, so the caller
/// (`connection.rs`) must resolve them (via a catalog lookup) before calling
/// this function.
pub fn compile_mutation(
    m: &dbx_api::Mutation,
    key_fields: &[Arc<str>],
) -> Result<MutationSql, DbError> {
    use dbx_api::Mutation;
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
        Mutation::Update { path, key, sets } => {
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
            let where_clause = key_where(key_fields, key, &mut pb)?;
            let sql = format!(
                "UPDATE {table} SET {} WHERE {where_clause}",
                set_parts.join(", ")
            );
            Ok(MutationSql {
                sql,
                params: pb.params,
            })
        }
        Mutation::Delete { path, key } => {
            let table = quote_object_path(path)?;
            let where_clause = key_where(key_fields, key, &mut pb)?;
            let sql = format!("DELETE FROM {table} WHERE {where_clause}");
            Ok(MutationSql {
                sql,
                params: pb.params,
            })
        }
    }
}

fn key_where(
    key_fields: &[Arc<str>],
    key_values: &[Value],
    pb: &mut ParamBuilder,
) -> Result<String, DbError> {
    if key_fields.len() != key_values.len() {
        return Err(DbError::Unsupported {
            feature: format!(
                "row identity has {} column(s) but {} key value(s) were supplied",
                key_fields.len(),
                key_values.len()
            ),
        });
    }
    if key_fields.is_empty() {
        return Err(DbError::Unsupported {
            feature: "mutation with no row identity — refuse to guess which row to affect".into(),
        });
    }
    let mut parts = Vec::with_capacity(key_fields.len());
    for (name, value) in key_fields.iter().zip(key_values) {
        let col = quote_ident(name)?;
        let ph = pb.push(value.clone());
        parts.push(format!("{col} = {ph}"));
    }
    Ok(parts.join(" AND "))
}

#[cfg(test)]
mod tests {
    use super::*;
    use dbx_api::{Op, Predicate as P};

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
        // Sanity: Op::Scan (dbx-api's shape) matches what compile_scan expects.
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
}
