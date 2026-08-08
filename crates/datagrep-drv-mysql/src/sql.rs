//! SQL generation for MySQL/MariaDB: identifier quoting and the `Op` → SQL
//! compiler.
//!
//! The injection rules for this driver: values are ALWAYS bound as `?`
//! parameters through the binary (prepared) protocol — never
//! spliced as text — and identifiers always go through [`quote_ident`].
//! (`mysql_async` has no client-side parameter interpolation for positional
//! `?` params the way mysql-js's `interpolateParams` does: `exec_*` sends a
//! real `COM_STMT_PREPARE`/`COM_STMT_EXECUTE` pair, so a bound value can
//! never be re-parsed as SQL. The text-protocol `query_*` path takes no
//! params at all, which is exactly why this module never emits a literal.)

use std::fmt::Write as _;

use datagrep_api::{DbError, FieldPath, ObjectPath, PathSeg, Predicate, SortKey, Value};

/// Which server we are actually talking to — decided from `@@version` at
/// connect. Only used where the two dialects genuinely diverge (EXPLAIN
/// ANALYZE syntax); never for anything a capability flag should cover.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Flavor {
    MySql,
    MariaDb,
}

/// Quote a MySQL identifier: backtick style, embedded backticks doubled.
/// Embedded NUL is rejected outright — MySQL identifiers cannot contain NUL
/// and truncating at it would let a name lie about itself.
pub fn quote_ident(ident: &str) -> Result<String, DbError> {
    if ident.contains('\0') {
        return Err(DbError::Unsupported {
            feature: format!("identifier contains a NUL byte: {ident:?}"),
        });
    }
    let mut out = String::with_capacity(ident.len() + 2);
    out.push('`');
    for c in ident.chars() {
        if c == '`' {
            out.push('`');
        }
        out.push(c);
    }
    out.push('`');
    Ok(out)
}

/// Render an [`ObjectPath`] as a dot-joined, individually quoted MySQL
/// reference. MySQL's namespace is two-level (`database`.`table`) — there is
/// no schema tier — so paths deeper than 2 parts are rejected honestly
/// rather than silently truncated.
pub fn quote_object_path(path: &ObjectPath) -> Result<String, DbError> {
    let parts = path.parts();
    if parts.is_empty() {
        return Err(DbError::Unsupported {
            feature: "empty object path".into(),
        });
    }
    if parts.len() > 2 {
        return Err(DbError::Unsupported {
            feature: format!(
                "MySQL object paths are `database`.`table` (2 levels); got {} levels: {path}",
                parts.len()
            ),
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
/// path (`address.city`, `tags[0]`) becomes
/// `JSON_UNQUOTE(JSON_EXTRACT(`col`, '$.city'))` — the function form is used
/// (not the `->>` operator) because MariaDB never implemented `->>`. The
/// JSON path is built only from validated identifier/index tokens; anything
/// that could escape the single-quoted literal is rejected, never escaped.
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
    let mut json_path = String::from("$");
    for seg in rest {
        match seg {
            PathSeg::Field(name) => {
                // MySQL JSON path unquoted members must be identifier-like;
                // anything else would need double-quoting inside the path
                // literal, which opens escaping questions we refuse to have.
                if name.is_empty()
                    || !name
                        .chars()
                        .all(|c| c.is_ascii_alphanumeric() || c == '_' || !c.is_ascii())
                {
                    return Err(DbError::Unsupported {
                        feature: format!("field name unsafe for a JSON path literal: {name:?}"),
                    });
                }
                let _ = write!(json_path, ".{name}");
            }
            PathSeg::Index(n) => {
                let _ = write!(json_path, "[{n}]");
            }
        }
    }
    Ok(format!("JSON_UNQUOTE(JSON_EXTRACT({base}, '{json_path}'))"))
}

/// Accumulates `?` parameters while compiling a predicate/scan tree, so every
/// `Value` in the request ends up bound, never spliced.
#[derive(Default)]
pub struct ParamBuilder {
    pub params: Vec<Value>,
}

impl ParamBuilder {
    fn push(&mut self, v: Value) -> &'static str {
        self.params.push(v);
        "?"
    }
}

/// Compile a [`Predicate`] to a SQL boolean expression, appending bound
/// parameters to `pb`. Returns just the expression text — callers wrap it in
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
                // An empty IN-list matches nothing; a tautological false is
                // valid SQL where `IN ()` is not.
                return Ok("FALSE".to_string());
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
            // Top-level SQL columns always "exist" once the row does; a
            // nested path is a JSON-presence test.
            if field.segments().len() <= 1 {
                "TRUE".to_string()
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
            "TRUE".into()
        } else {
            "FALSE".into()
        });
    }
    let mut rendered = Vec::with_capacity(parts.len());
    for p in parts {
        rendered.push(format!("({})", compile_predicate(p, pb)?));
    }
    Ok(rendered.join(&format!(" {op} ")))
}

/// Compile `ORDER BY` for a [`SortKey`] list. MySQL/MariaDB have no
/// `NULLS FIRST`/`NULLS LAST` clause, and the engines' implicit placement
/// differs by direction — so null placement is always emulated explicitly
/// with a leading `(expr IS NULL)` key, making the request's `nulls_first`
/// deterministic instead of silently engine-defined (`SortKey` doc's whole
/// point).
pub fn compile_order(order: &[SortKey]) -> Result<String, DbError> {
    let mut parts = Vec::with_capacity(order.len());
    for k in order {
        let expr = field_path_expr(&k.path)?;
        let dir = if k.desc { "DESC" } else { "ASC" };
        let nulls = if k.nulls_first { "DESC" } else { "ASC" };
        parts.push(format!("({expr} IS NULL) {nulls}, {expr} {dir}"));
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

/// Compile `Op::Scan` to `(sql, params)`. Keyset `resume` is accepted but
/// ignored in v1 — same honest situation as the sibling drivers: this
/// driver's cursor never emits a `ResumeToken`, so no caller can construct
/// one to pass back in.
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

/// Compile `Op::Count`. `EXACT_COUNT_CHEAP` is honest for InnoDB only in the
/// "cheap enough" sense (a `COUNT(*)` walks the smallest index, it does not
/// scan rows); `exact: false` still runs the real count in v1 — the
/// `information_schema.tables.table_rows` estimate is surfaced through
/// `describe()` instead.
pub fn compile_count(
    path: &ObjectPath,
    filter: &Option<Predicate>,
    _exact: bool,
) -> Result<(String, Vec<Value>), DbError> {
    let mut pb = ParamBuilder::default();
    let table = quote_object_path(path)?;
    let mut sql = format!("SELECT COUNT(*) AS count FROM {table}");
    if let Some(pred) = filter {
        let expr = compile_predicate(pred, &mut pb)?;
        let _ = write!(sql, " WHERE {expr}");
    }
    Ok((sql, pb.params))
}

/// Wrap an inner request's compiled SQL in the flavor's EXPLAIN form.
/// `analyze` genuinely executes the statement: MySQL 8.0.18+ spells it
/// `EXPLAIN ANALYZE`, MariaDB spells it `ANALYZE` — MariaDB has no
/// `EXPLAIN ANALYZE` at all, so emitting the MySQL spelling there would be a
/// guaranteed syntax error, not a degraded result.
pub fn wrap_explain(inner_sql: &str, analyze: bool, flavor: Flavor) -> String {
    match (analyze, flavor) {
        (false, _) => format!("EXPLAIN {inner_sql}"),
        (true, Flavor::MySql) => format!("EXPLAIN ANALYZE {inner_sql}"),
        (true, Flavor::MariaDb) => format!("ANALYZE {inner_sql}"),
    }
}

/// A single generated mutation statement plus its bound params.
pub struct MutationSql {
    pub sql: String,
    pub params: Vec<Value>,
}

/// Compile one `Mutation` into `UPDATE … SET … WHERE <key> = ?`,
/// `INSERT INTO … VALUES (…)`, or `DELETE FROM … WHERE <key> = ?`. The row
/// identity arrives as named `(FieldPath, Value)` pairs, so the WHERE clause
/// compiles directly — no `information_schema` lookup, no positional
/// convention.
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
            if fields.is_empty() {
                return Err(DbError::Unsupported {
                    feature: "insert with no fields".into(),
                });
            }
            let mut cols = Vec::with_capacity(fields.len());
            let mut phs = Vec::with_capacity(fields.len());
            for (name, value) in fields {
                cols.push(quote_ident(name)?);
                phs.push(pb.push(value.clone()));
            }
            Ok(MutationSql {
                sql: format!(
                    "INSERT INTO {table} ({}) VALUES ({})",
                    cols.join(", "),
                    phs.join(", ")
                ),
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
            let where_clause = key_where(key, &mut pb)?;
            Ok(MutationSql {
                sql: format!(
                    "UPDATE {table} SET {} WHERE {where_clause}",
                    set_parts.join(", ")
                ),
                params: pb.params,
            })
        }
        Mutation::Delete { path, key } => {
            let table = quote_object_path(path)?;
            let where_clause = key_where(key, &mut pb)?;
            Ok(MutationSql {
                sql: format!("DELETE FROM {table} WHERE {where_clause}"),
                params: pb.params,
            })
        }
    }
}

/// The named row identity as `` `col` = ? AND … ``. An empty key is refused —
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

/// Minimal JSON string escaper for the `describe()` `indexes` array — kept
/// local so this crate doesn't grow a serde_json dependency for one field.
pub fn json_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => {
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use datagrep_api::Predicate as P;
    use std::sync::Arc;

    #[test]
    fn quote_ident_backtick_style() {
        assert_eq!(quote_ident("users").unwrap(), "`users`");
        assert_eq!(quote_ident("a`b").unwrap(), "`a``b`");
        assert_eq!(quote_ident("a``b").unwrap(), "`a````b`");
        assert_eq!(quote_ident("").unwrap(), "``");
        assert_eq!(quote_ident(" has space ").unwrap(), "` has space `");
        // A double-quote is NOT special in backtick quoting.
        assert_eq!(quote_ident("a\"b").unwrap(), "`a\"b`");
    }

    #[test]
    fn quote_ident_preserves_unicode() {
        assert_eq!(quote_ident("héllo_wörld").unwrap(), "`héllo_wörld`");
        assert_eq!(quote_ident("名前").unwrap(), "`名前`");
    }

    #[test]
    fn quote_ident_rejects_nul() {
        assert!(matches!(
            quote_ident("ab\0cd").unwrap_err(),
            DbError::Unsupported { .. }
        ));
    }

    #[test]
    fn object_path_is_two_level_and_quoted() {
        let p = ObjectPath::new(vec![Arc::from("app"), Arc::from("Users")]);
        assert_eq!(quote_object_path(&p).unwrap(), "`app`.`Users`");
        let single = ObjectPath::new(vec![Arc::from("t")]);
        assert_eq!(quote_object_path(&single).unwrap(), "`t`");
        // MySQL has no db.schema.table — a 3-level path is refused, not
        // silently truncated.
        let deep = ObjectPath::new(vec![Arc::from("a"), Arc::from("b"), Arc::from("c")]);
        assert!(quote_object_path(&deep).is_err());
        assert!(quote_object_path(&ObjectPath::root()).is_err());
    }

    #[test]
    fn predicate_compiles_to_question_marks_never_literal_values() {
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
        assert_eq!(sql, "(`status` = ?) AND (`age` >= ?)");
        assert!(
            !sql.contains("super-secret-literal") && !sql.contains("21"),
            "value leaked into SQL text: {sql}"
        );
        assert_eq!(pb.params.len(), 2);
        assert_eq!(pb.params[0], Value::Str(Arc::from("super-secret-literal")));
        assert_eq!(pb.params[1], Value::I64(21));
    }

    #[test]
    fn injection_shaped_value_stays_a_parameter() {
        let mut pb = ParamBuilder::default();
        let sql = compile_predicate(
            &P::Eq {
                field: FieldPath::field("name"),
                value: Value::Str(Arc::from("x'; DROP TABLE users; --")),
            },
            &mut pb,
        )
        .unwrap();
        assert_eq!(sql, "`name` = ?");
        assert!(!sql.contains("DROP TABLE"), "spliced: {sql}");
        assert_eq!(pb.params.len(), 1);
    }

    #[test]
    fn scan_op_uses_question_placeholders_and_backticks() {
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
        assert!(sql.starts_with("SELECT * FROM `app`.`users`"), "{sql}");
        assert!(sql.contains("WHERE `email` = ?"), "{sql}");
        assert!(
            sql.contains("ORDER BY (`id` IS NULL) ASC, `id` DESC"),
            "nulls_last emulation missing: {sql}"
        );
        assert!(sql.contains("LIMIT 50"), "{sql}");
        assert!(!sql.contains("a@b.com"), "literal leaked: {sql}");
        assert_eq!(params, vec![Value::Str(Arc::from("a@b.com"))]);
    }

    #[test]
    fn order_nulls_first_is_emulated() {
        let order = vec![SortKey {
            path: FieldPath::field("score"),
            desc: false,
            nulls_first: true,
        }];
        assert_eq!(
            compile_order(&order).unwrap(),
            "(`score` IS NULL) DESC, `score` ASC"
        );
    }

    #[test]
    fn nested_field_path_uses_json_extract_function_form() {
        let expr = field_path_expr(&"address.city".parse().unwrap()).unwrap();
        assert_eq!(expr, "JSON_UNQUOTE(JSON_EXTRACT(`address`, '$.city'))");
        let expr = field_path_expr(&"tags[3]".parse().unwrap()).unwrap();
        assert_eq!(expr, "JSON_UNQUOTE(JSON_EXTRACT(`tags`, '$[3]'))");
    }

    #[test]
    fn json_path_rejects_quote_smuggling() {
        // A field name that could close the '$…' literal must be refused.
        let path = FieldPath::new(vec![
            PathSeg::Field(Arc::from("doc")),
            PathSeg::Field(Arc::from("a'||1--")),
        ]);
        assert!(field_path_expr(&path).is_err());
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
        assert_eq!(sql, "FALSE");
        assert!(pb.params.is_empty());
    }

    #[test]
    fn explain_spelling_per_flavor() {
        assert_eq!(
            wrap_explain("SELECT 1", false, Flavor::MySql),
            "EXPLAIN SELECT 1"
        );
        assert_eq!(
            wrap_explain("SELECT 1", false, Flavor::MariaDb),
            "EXPLAIN SELECT 1"
        );
        assert_eq!(
            wrap_explain("SELECT 1", true, Flavor::MySql),
            "EXPLAIN ANALYZE SELECT 1"
        );
        assert_eq!(
            wrap_explain("SELECT 1", true, Flavor::MariaDb),
            "ANALYZE SELECT 1"
        );
    }

    #[test]
    fn mutation_update_binds_key_and_sets() {
        let m = datagrep_api::Mutation::Update {
            path: ObjectPath::new(vec![Arc::from("app"), Arc::from("users")]),
            key: vec![(FieldPath::field("id"), Value::I64(42))],
            sets: vec![(FieldPath::field("name"), Value::Str(Arc::from("amy")))],
        };
        let out = compile_mutation(&m).unwrap();
        assert_eq!(
            out.sql,
            "UPDATE `app`.`users` SET `name` = ? WHERE `id` = ?"
        );
        assert_eq!(
            out.params,
            vec![Value::Str(Arc::from("amy")), Value::I64(42)]
        );
    }

    #[test]
    fn mutation_delete_refuses_empty_key() {
        let m = datagrep_api::Mutation::Delete {
            path: ObjectPath::new(vec![Arc::from("t")]),
            key: vec![],
        };
        assert!(compile_mutation(&m).is_err(), "must never guess the row");
    }

    #[test]
    fn json_escape_escapes_quotes_and_control() {
        assert_eq!(json_escape("plain"), "plain");
        assert_eq!(json_escape("a\"b\\c"), "a\\\"b\\\\c");
        assert_eq!(json_escape("x\ny"), "x\\ny");
        assert_eq!(json_escape("\u{1}"), "\\u0001");
    }
}
