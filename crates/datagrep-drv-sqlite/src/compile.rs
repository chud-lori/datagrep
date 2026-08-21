use datagrep_api::{
    DbError, DdlOp, FieldPath, ObjectKind, ObjectPath, PathSeg, Predicate, ResumeToken, SortKey,
    Value,
};

use crate::quote_ident;

#[derive(Debug)]
pub(crate) struct Compiled {
    pub sql: String,
    pub params: Vec<Value>,
}

pub(crate) fn compile_object_path(path: &ObjectPath) -> Result<String, DbError> {
    quote_parts(path.parts())
}

fn quote_parts(parts: &[std::sync::Arc<str>]) -> Result<String, DbError> {
    if parts.is_empty() {
        return Err(DbError::Query {
            code: None,
            message: "cannot compile an empty object path".to_string(),
            position: None,
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

pub(crate) fn field_ident(path: &FieldPath) -> Result<String, DbError> {
    match path.segments() {
        [PathSeg::Field(name)] => quote_ident(name),
        _ => Err(DbError::Unsupported {
            feature: format!(
                "nested/indexed field path `{path}` in a SQLite predicate/order/project — \
                 SQLite tables are flat"
            ),
        }),
    }
}

pub(crate) fn field_name(path: &FieldPath) -> Option<&str> {
    match path.segments() {
        [PathSeg::Field(name)] => Some(name),
        _ => None,
    }
}

pub(crate) fn compile_ddl(op: &DdlOp) -> Result<String, DbError> {
    match op {
        DdlOp::Native { text } => Ok(text.to_string()),
        DdlOp::Drop {
            path,
            kind,
            if_exists,
        } => {
            let guard = if *if_exists { "IF EXISTS " } else { "" };
            let keyword = match kind {
                ObjectKind::Table => "TABLE",
                ObjectKind::View => "VIEW",
                ObjectKind::Index => "INDEX",
                other => {
                    return Err(DbError::Unsupported {
                        feature: format!(
                            "{other:?} is not a SQLite object this driver can administer"
                        ),
                    })
                }
            };
            let target = if *kind == ObjectKind::Index {
                index_ref(path)?
            } else {
                compile_object_path(path)?
            };
            Ok(format!("DROP {keyword} {guard}{target}"))
        }
        DdlOp::Rename { from, to, kind } => {
            let new_name = quote_ident(DdlOp::rename_target(from, to)?)?;
            if *kind != ObjectKind::Table {
                return Err(DbError::Unsupported {
                    feature: format!(
                        "SQLite can only rename a table — a {kind:?} has to be dropped and \
                         recreated (`ALTER VIEW`/`ALTER INDEX` are syntax errors)"
                    ),
                });
            }
            Ok(format!(
                "ALTER TABLE {} RENAME TO {new_name}",
                compile_object_path(from)?
            ))
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
            let (table, schema) =
                path.parts()
                    .split_last()
                    .ok_or_else(|| DbError::Unsupported {
                        feature: "CREATE INDEX on an empty object path".into(),
                    })?;
            if schema.len() > 1 {
                return Err(DbError::Unsupported {
                    feature: format!("object path `{path}` is deeper than SQLite's schema.table"),
                });
            }
            let mut index = String::new();
            if !schema.is_empty() {
                index.push_str(&quote_parts(schema)?);
                index.push('.');
            }
            index.push_str(&quote_ident(name)?);
            let mut cols = Vec::with_capacity(fields.len());
            for f in fields {
                cols.push(field_ident(f)?);
            }
            Ok(format!(
                "CREATE {}INDEX {}{index} ON {} ({})",
                if *unique { "UNIQUE " } else { "" },
                if *if_not_exists { "IF NOT EXISTS " } else { "" },
                quote_ident(table)?,
                cols.join(", ")
            ))
        }
    }
}

fn index_ref(path: &ObjectPath) -> Result<String, DbError> {
    let parts = path.parts();
    let Some(split) = parts.len().checked_sub(2) else {
        return Err(DbError::Unsupported {
            feature: format!(
                "index path `{path}` must name the table it is on followed by the index name"
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

fn bind(params: &mut Vec<Value>, v: Value) -> String {
    params.push(v);
    "?".to_string()
}

fn compile_predicate(pred: &Predicate, params: &mut Vec<Value>) -> Result<String, DbError> {
    Ok(match pred {
        Predicate::Eq { field, value } => {
            format!("{} = {}", field_ident(field)?, bind(params, value.clone()))
        }
        Predicate::Ne { field, value } => {
            format!("{} <> {}", field_ident(field)?, bind(params, value.clone()))
        }
        Predicate::Lt { field, value } => {
            format!("{} < {}", field_ident(field)?, bind(params, value.clone()))
        }
        Predicate::Le { field, value } => {
            format!("{} <= {}", field_ident(field)?, bind(params, value.clone()))
        }
        Predicate::Gt { field, value } => {
            format!("{} > {}", field_ident(field)?, bind(params, value.clone()))
        }
        Predicate::Ge { field, value } => {
            format!("{} >= {}", field_ident(field)?, bind(params, value.clone()))
        }
        Predicate::In { field, values } => {
            let f = field_ident(field)?;
            if values.is_empty() {
                "0".to_string()
            } else {
                let placeholders: Vec<String> =
                    values.iter().map(|v| bind(params, v.clone())).collect();
                format!("{f} IN ({})", placeholders.join(", "))
            }
        }
        Predicate::Like { field, pattern } => format!(
            "{} LIKE {}",
            field_ident(field)?,
            bind(params, Value::Str(pattern.clone()))
        ),
        Predicate::Exists { field } => {
            let f = field_ident(field)?;
            format!("({f} IS NULL OR {f} IS NOT NULL)")
        }
        Predicate::IsNull { field } => format!("{} IS NULL", field_ident(field)?),
        Predicate::And(parts) => {
            if parts.is_empty() {
                "1".to_string()
            } else {
                let clauses = parts
                    .iter()
                    .map(|p| compile_predicate(p, params))
                    .collect::<Result<Vec<_>, _>>()?;
                format!("({})", clauses.join(" AND "))
            }
        }
        Predicate::Or(parts) => {
            if parts.is_empty() {
                "0".to_string()
            } else {
                let clauses = parts
                    .iter()
                    .map(|p| compile_predicate(p, params))
                    .collect::<Result<Vec<_>, _>>()?;
                format!("({})", clauses.join(" OR "))
            }
        }
        Predicate::Not(inner) => format!("NOT ({})", compile_predicate(inner, params)?),
    })
}

fn compile_sort_key(sk: &SortKey) -> Result<String, DbError> {
    let field = field_ident(&sk.path)?;
    let dir = if sk.desc { "DESC" } else { "ASC" };
    let nulls = if sk.nulls_first {
        "NULLS FIRST"
    } else {
        "NULLS LAST"
    };
    Ok(format!("{field} {dir} {nulls}"))
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn compile_scan(
    path: &ObjectPath,
    filter: &Option<Predicate>,
    order: &[SortKey],
    project: &Option<Vec<FieldPath>>,
    limit: &Option<u64>,
    resume: &Option<ResumeToken>,
) -> Result<Compiled, DbError> {
    let mut params = Vec::new();
    let table = compile_object_path(path)?;

    let select_list = match project {
        Some(fields) if !fields.is_empty() => fields
            .iter()
            .map(field_ident)
            .collect::<Result<Vec<_>, _>>()?
            .join(", "),
        _ => "*".to_string(),
    };

    let mut sql = format!("SELECT {select_list} FROM {table}");
    let mut where_clauses = Vec::new();
    if let Some(pred) = filter {
        where_clauses.push(compile_predicate(pred, &mut params)?);
    }
    if let Some(token) = resume {
        where_clauses.push(crate::scan::compile_resume_clause(
            order,
            token,
            &mut params,
        )?);
    }
    if !where_clauses.is_empty() {
        sql.push_str(" WHERE ");
        sql.push_str(&where_clauses.join(" AND "));
    }
    if !order.is_empty() {
        let parts = order
            .iter()
            .map(compile_sort_key)
            .collect::<Result<Vec<_>, _>>()?;
        sql.push_str(" ORDER BY ");
        sql.push_str(&parts.join(", "));
    }
    if let Some(n) = limit {
        sql.push_str(" LIMIT ?");
        params.push(Value::I64(i64::try_from(*n).unwrap_or(i64::MAX)));
    }
    Ok(Compiled { sql, params })
}

pub(crate) fn compile_count(
    path: &ObjectPath,
    filter: &Option<Predicate>,
) -> Result<Compiled, DbError> {
    let mut params = Vec::new();
    let table = compile_object_path(path)?;
    let mut sql = format!("SELECT COUNT(*) FROM {table}");
    if let Some(pred) = filter {
        sql.push_str(" WHERE ");
        sql.push_str(&compile_predicate(pred, &mut params)?);
    }
    Ok(Compiled { sql, params })
}

#[cfg(test)]
mod tests {
    use super::*;
    use datagrep_api::{FieldPath, SortKey};
    use std::sync::Arc;

    fn path(parts: &[&str]) -> ObjectPath {
        ObjectPath::new(parts.iter().map(|p| Arc::from(*p)).collect())
    }

    #[test]
    fn scan_projects_and_quotes_table() {
        let c = compile_scan(&path(&["users"]), &None, &[], &None, &None, &None).unwrap();
        assert_eq!(c.sql, "SELECT * FROM \"users\"");
        assert!(c.params.is_empty());
    }

    #[test]
    fn scan_qualified_table_quotes_each_part() {
        let c = compile_scan(&path(&["main", "users"]), &None, &[], &None, &None, &None).unwrap();
        assert_eq!(c.sql, "SELECT * FROM \"main\".\"users\"");
    }

    #[test]
    fn scan_predicate_binds_value_never_interpolates() {
        let filter = Some(Predicate::Eq {
            field: FieldPath::field("email"),
            value: Value::Str(Arc::from("payload'; DROP TABLE users; --")),
        });
        let c = compile_scan(&path(&["users"]), &filter, &[], &None, &None, &None).unwrap();
        assert!(
            !c.sql.contains("DROP TABLE"),
            "predicate value must never be spliced into SQL text: {}",
            c.sql
        );
        assert!(c.sql.contains("\"email\" = ?"));
        assert_eq!(c.params.len(), 1);
        assert_eq!(
            c.params[0],
            Value::Str(Arc::from("payload'; DROP TABLE users; --"))
        );
    }

    #[test]
    fn scan_and_or_not_compose() {
        let filter = Some(Predicate::And(vec![
            Predicate::Ge {
                field: FieldPath::field("age"),
                value: Value::I64(21),
            },
            Predicate::Not(Box::new(Predicate::IsNull {
                field: FieldPath::field("email"),
            })),
        ]));
        let c = compile_scan(&path(&["users"]), &filter, &[], &None, &None, &None).unwrap();
        assert_eq!(
            c.sql,
            "SELECT * FROM \"users\" WHERE (\"age\" >= ? AND NOT (\"email\" IS NULL))"
        );
        assert_eq!(c.params, vec![Value::I64(21)]);
    }

    #[test]
    fn scan_in_with_empty_values_is_always_false() {
        let filter = Some(Predicate::In {
            field: FieldPath::field("id"),
            values: vec![],
        });
        let c = compile_scan(&path(&["t"]), &filter, &[], &None, &None, &None).unwrap();
        assert_eq!(c.sql, "SELECT * FROM \"t\" WHERE 0");
    }

    #[test]
    fn scan_order_and_limit() {
        let order = vec![SortKey {
            path: FieldPath::field("created_at"),
            desc: true,
            nulls_first: false,
        }];
        let c = compile_scan(&path(&["t"]), &None, &order, &None, &Some(10), &None).unwrap();
        assert_eq!(
            c.sql,
            "SELECT * FROM \"t\" ORDER BY \"created_at\" DESC NULLS LAST LIMIT ?"
        );
        assert_eq!(c.params, vec![Value::I64(10)]);
    }

    #[test]
    fn scan_project_quotes_each_field() {
        let project = Some(vec![FieldPath::field("id"), FieldPath::field("name")]);
        let c = compile_scan(&path(&["t"]), &None, &[], &project, &None, &None).unwrap();
        assert_eq!(c.sql, "SELECT \"id\", \"name\" FROM \"t\"");
    }

    #[test]
    fn scan_rejects_nested_field_path() {
        let filter = Some(Predicate::Eq {
            field: "address.city".parse().unwrap(),
            value: Value::Str(Arc::from("sg")),
        });
        let err = compile_scan(&path(&["t"]), &filter, &[], &None, &None, &None).unwrap_err();
        assert!(matches!(err, DbError::Unsupported { .. }));
    }

    #[test]
    fn count_compiles_with_filter() {
        let filter = Some(Predicate::Eq {
            field: FieldPath::field("status"),
            value: Value::Str(Arc::from("active")),
        });
        let c = compile_count(&path(&["users"]), &filter).unwrap();
        assert_eq!(c.sql, "SELECT COUNT(*) FROM \"users\" WHERE \"status\" = ?");
        assert_eq!(c.params, vec![Value::Str(Arc::from("active"))]);
    }
}
