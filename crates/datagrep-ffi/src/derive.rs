use std::ffi::c_char;

use datagrep_api::SqlDialect;
use datagrep_core::format::sql::QuoteIdent;
use serde::Deserialize;
use serde_json::json;

use crate::export::sql_target;
use crate::ffi_util::{cstr, guard, guard_quiet, to_c_string};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Op {
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
    Contains,
    NotContains,
    StartsWith,
    EndsWith,
    IsNull,
    IsNotNull,
    IsEmpty,
}

impl Op {
    const ALL: [Op; 13] = [
        Op::Eq,
        Op::Ne,
        Op::Lt,
        Op::Le,
        Op::Gt,
        Op::Ge,
        Op::Contains,
        Op::NotContains,
        Op::StartsWith,
        Op::EndsWith,
        Op::IsNull,
        Op::IsNotNull,
        Op::IsEmpty,
    ];

    fn id(self) -> &'static str {
        match self {
            Op::Eq => "eq",
            Op::Ne => "ne",
            Op::Lt => "lt",
            Op::Le => "le",
            Op::Gt => "gt",
            Op::Ge => "ge",
            Op::Contains => "contains",
            Op::NotContains => "not_contains",
            Op::StartsWith => "starts_with",
            Op::EndsWith => "ends_with",
            Op::IsNull => "is_null",
            Op::IsNotNull => "is_not_null",
            Op::IsEmpty => "is_empty",
        }
    }

    fn label(self) -> &'static str {
        match self {
            Op::Eq => "=",
            Op::Ne => "≠",
            Op::Lt => "<",
            Op::Le => "≤",
            Op::Gt => ">",
            Op::Ge => "≥",
            Op::Contains => "contains",
            Op::NotContains => "does not contain",
            Op::StartsWith => "starts with",
            Op::EndsWith => "ends with",
            Op::IsNull => "is NULL",
            Op::IsNotNull => "is not NULL",
            Op::IsEmpty => "is empty",
        }
    }

    fn needs_value(self) -> bool {
        !matches!(self, Op::IsNull | Op::IsNotNull | Op::IsEmpty)
    }
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Spec {
    #[serde(default)]
    pub filters: Vec<Filter>,
    #[serde(default)]
    pub sort: Option<Sort>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Filter {
    pub column: String,
    pub op: Op,
    #[serde(default)]
    pub value: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Sort {
    pub column: String,
    pub ascending: bool,
}

/// # Safety
/// `driver_id` is NULL or NUL-terminated.
#[no_mangle]
pub unsafe extern "C" fn datagrep_filter_operators_json(driver_id: *const c_char) -> *mut c_char {
    guard_quiet(std::ptr::null_mut(), || {
        // SAFETY: NULL or NUL-terminated per the contract; cstr rejects NULL.
        let driver = unsafe { cstr(driver_id, "driver_id") }.unwrap_or_default();
        to_c_string(operators_json(driver))
    })
}

/// # Safety
/// String arguments are NUL-terminated; `err_out` is NULL or a writable slot.
#[no_mangle]
pub unsafe extern "C" fn datagrep_derive_statement(
    driver_id: *const c_char,
    statement: *const c_char,
    spec_json: *const c_char,
    err_out: *mut *mut c_char,
) -> *mut c_char {
    guard(
        err_out,
        std::ptr::null_mut(),
        "datagrep_derive_statement",
        || {
            // SAFETY: NUL-terminated strings per the contract; cstr rejects NULL and non-UTF-8 before any deref.
            let driver_id = unsafe { cstr(driver_id, "driver_id") }?;
            let statement = unsafe { cstr(statement, "statement") }?;
            let spec: Spec = serde_json::from_str(unsafe { cstr(spec_json, "spec_json") }?)
                .map_err(|e| format!("spec_json is not a filter spec: {e}"))?;
            Ok(to_c_string(derive_statement(driver_id, statement, &spec)?))
        },
    )
}

pub fn operators_json(driver_id: &str) -> String {
    if sql_target(driver_id).is_none() {
        return "[]".to_string();
    }
    let ops: Vec<_> = Op::ALL
        .iter()
        .map(|op| json!({"op": op.id(), "label": op.label(), "needs_value": op.needs_value()}))
        .collect();
    json!(ops).to_string()
}

/// `statement` wrapped once in a subquery carrying the spec's WHERE and ORDER BY.
pub fn derive_statement(driver_id: &str, statement: &str, spec: &Spec) -> Result<String, String> {
    if spec.filters.is_empty() && spec.sort.is_none() {
        return Ok(statement.to_string());
    }
    let Some((dialect, quote)) = sql_target(driver_id) else {
        return Err(format!(
            "sorting and filtering re-run the statement inside an SQL subquery, which `{driver_id}` \
             cannot take. Put the condition in the statement itself"
        ));
    };
    let inner = statement.trim().trim_end_matches(';').trim_end();
    if inner.is_empty() {
        return Err("there is no statement to sort or filter".to_string());
    }
    let sql = Sql { dialect, quote };
    let mut out = format!("SELECT * FROM (\n{inner}\n) AS datagrep_result");
    if !spec.filters.is_empty() {
        let clauses = spec
            .filters
            .iter()
            .map(|f| sql.predicate(f))
            .collect::<Result<Vec<_>, _>>()?;
        out.push_str("\nWHERE (");
        out.push_str(&clauses.join(") AND ("));
        out.push(')');
    }
    if let Some(sort) = &spec.sort {
        out.push_str("\nORDER BY ");
        out.push_str(&sql.ident(&sort.column)?);
        out.push_str(if sort.ascending { " ASC" } else { " DESC" });
    }
    Ok(out)
}

struct Sql {
    dialect: SqlDialect,
    quote: QuoteIdent,
}

impl Sql {
    fn ident(&self, name: &str) -> Result<String, String> {
        (self.quote)(name).map_err(|e| e.to_string())
    }

    fn predicate(&self, filter: &Filter) -> Result<String, String> {
        let column = self.ident(&filter.column)?;
        // Postgres has no LIKE or = '' on non-text types; everyone else coerces.
        let text = if self.dialect == SqlDialect::Postgres {
            format!("CAST({column} AS TEXT)")
        } else {
            column.clone()
        };
        let value = filter.value.as_str();
        let compare = |op: &str| Ok::<_, String>(format!("{column} {op} {}", self.literal(value)?));
        let like = |negate: bool, pattern: String| {
            let op = match (self.dialect == SqlDialect::Postgres, negate) {
                (true, false) => "ILIKE",
                (true, true) => "NOT ILIKE",
                (false, false) => "LIKE",
                (false, true) => "NOT LIKE",
            };
            Ok::<_, String>(format!(
                "{text} {op} {} ESCAPE '!'",
                self.literal(&pattern)?
            ))
        };
        match filter.op {
            Op::Eq => compare("="),
            Op::Ne => compare("<>"),
            Op::Lt => compare("<"),
            Op::Le => compare("<="),
            Op::Gt => compare(">"),
            Op::Ge => compare(">="),
            Op::Contains => like(false, format!("%{}%", like_escaped(value))),
            Op::NotContains => like(true, format!("%{}%", like_escaped(value))),
            Op::StartsWith => like(false, format!("{}%", like_escaped(value))),
            Op::EndsWith => like(false, format!("%{}", like_escaped(value))),
            Op::IsNull => Ok(format!("{column} IS NULL")),
            Op::IsNotNull => Ok(format!("{column} IS NOT NULL")),
            Op::IsEmpty => Ok(format!("{column} IS NULL OR {text} = ''")),
        }
    }

    fn literal(&self, value: &str) -> Result<String, String> {
        if value.contains('\0') {
            return Err("a filter value cannot contain a NUL character".to_string());
        }
        let quoted = value.replace('\'', "''");
        Ok(match self.dialect {
            // Doubling stays inert under NO_BACKSLASH_ESCAPES too; only the match would differ.
            SqlDialect::Mysql => format!("'{}'", quoted.replace('\\', "\\\\")),
            // E'' reads backslashes the same whatever standard_conforming_strings is set to.
            SqlDialect::Postgres if value.contains('\\') => {
                format!("E'{}'", quoted.replace('\\', "\\\\"))
            }
            _ => format!("'{quoted}'"),
        })
    }
}

fn like_escaped(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for c in value.chars() {
        if matches!(c, '!' | '%' | '_') {
            out.push('!');
        }
        out.push(c);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn one(column: &str, op: Op, value: &str) -> Spec {
        Spec {
            filters: vec![Filter {
                column: column.to_string(),
                op,
                value: value.to_string(),
            }],
            sort: None,
        }
    }

    fn where_of(driver: &str, column: &str, op: Op, value: &str) -> String {
        let sql = derive_statement(driver, "SELECT * FROM t", &one(column, op, value)).unwrap();
        sql.split_once("\nWHERE ").unwrap().1.to_string()
    }

    #[test]
    fn an_empty_spec_sends_the_statement_exactly_as_typed() {
        let sql = derive_statement("postgres", "SELECT * FROM users;", &Spec::default()).unwrap();
        assert_eq!(sql, "SELECT * FROM users;");
    }

    #[test]
    fn a_sort_wraps_once_and_strips_the_trailing_semicolon() {
        let spec = Spec {
            filters: vec![],
            sort: Some(Sort {
                column: "created at".to_string(),
                ascending: false,
            }),
        };
        assert_eq!(
            derive_statement("postgres", "SELECT * FROM users;  ", &spec).unwrap(),
            "SELECT * FROM (\nSELECT * FROM users\n) AS datagrep_result\nORDER BY \"created at\" DESC"
        );
    }

    #[test]
    fn filters_are_anded_and_come_before_the_order_by() {
        let spec: Spec = serde_json::from_str(
            r#"{"filters":[{"column":"a","op":"eq","value":"1"},{"column":"b","op":"is_null"}],
                "sort":{"column":"a","ascending":true}}"#,
        )
        .unwrap();
        assert_eq!(
            derive_statement("sqlite", "SELECT * FROM t", &spec).unwrap(),
            "SELECT * FROM (\nSELECT * FROM t\n) AS datagrep_result\nWHERE (\"a\" = '1') AND (\"b\" IS NULL)\nORDER BY \"a\" ASC"
        );
    }

    #[test]
    fn every_operator_id_round_trips_through_the_spec() {
        for op in Op::ALL {
            let parsed: Op = serde_json::from_str(&format!("\"{}\"", op.id())).unwrap();
            assert_eq!(parsed, op);
        }
        let listed: serde_json::Value = serde_json::from_str(&operators_json("mysql")).unwrap();
        assert_eq!(listed.as_array().unwrap().len(), Op::ALL.len());
        assert_eq!(
            listed[0],
            json!({"op": "eq", "label": "=", "needs_value": true})
        );
    }

    #[test]
    fn engines_that_cannot_take_a_subquery_offer_no_operators_and_refuse_to_derive() {
        for driver in ["mongodb", "elasticsearch", "redis", "duckdb"] {
            assert_eq!(operators_json(driver), "[]");
            let err =
                derive_statement(driver, "db.t.find({})", &one("a", Op::Eq, "1")).unwrap_err();
            assert!(err.contains(driver), "{err}");
        }
    }

    #[test]
    fn a_quote_in_a_value_cannot_close_the_literal() {
        let hostile = "'; DROP TABLE x; --";
        assert_eq!(
            where_of("sqlite", "name", Op::Eq, hostile),
            "(\"name\" = '''; DROP TABLE x; --')"
        );
        assert_eq!(
            where_of("postgres", "name", Op::Eq, hostile),
            "(\"name\" = '''; DROP TABLE x; --')"
        );
        assert_eq!(
            where_of("mysql", "name", Op::Eq, hostile),
            "(`name` = '''; DROP TABLE x; --')"
        );
    }

    #[test]
    fn mysql_doubles_backslashes_so_one_cannot_escape_the_closing_quote() {
        assert_eq!(
            where_of("mysql", "name", Op::Eq, "\\' OR 1=1 -- "),
            "(`name` = '\\\\'' OR 1=1 -- ')"
        );
    }

    #[test]
    fn postgres_uses_an_escape_string_only_when_a_backslash_is_present() {
        assert_eq!(where_of("postgres", "p", Op::Eq, "a'b"), "(\"p\" = 'a''b')");
        assert_eq!(
            where_of("postgres", "p", Op::Eq, "\\' OR 1=1 --"),
            "(\"p\" = E'\\\\'' OR 1=1 --')"
        );
    }

    #[test]
    fn sqlite_takes_backslashes_literally() {
        assert_eq!(
            where_of("sqlite", "p", Op::Eq, "C:\\x"),
            "(\"p\" = 'C:\\x')"
        );
    }

    #[test]
    fn a_hostile_column_name_stays_one_identifier() {
        assert_eq!(
            where_of("postgres", "a\" = 1; DROP TABLE x; --", Op::IsNull, ""),
            "(\"a\"\" = 1; DROP TABLE x; --\" IS NULL)"
        );
        assert_eq!(
            where_of("mysql", "a` = 1; --", Op::IsNotNull, ""),
            "(`a`` = 1; --` IS NOT NULL)"
        );
    }

    #[test]
    fn like_wildcards_in_a_value_match_themselves() {
        assert_eq!(
            where_of("sqlite", "n", Op::Contains, "50%_off!"),
            "(\"n\" LIKE '%50!%!_off!!%' ESCAPE '!')"
        );
        assert_eq!(
            where_of("mysql", "n", Op::StartsWith, "a"),
            "(`n` LIKE 'a%' ESCAPE '!')"
        );
        assert_eq!(
            where_of("sqlite", "n", Op::EndsWith, "z"),
            "(\"n\" LIKE '%z' ESCAPE '!')"
        );
    }

    #[test]
    fn postgres_matches_text_case_insensitively_on_any_column_type() {
        assert_eq!(
            where_of("postgres", "id", Op::Contains, "4"),
            "(CAST(\"id\" AS TEXT) ILIKE '%4%' ESCAPE '!')"
        );
        assert_eq!(
            where_of("postgres", "id", Op::NotContains, "4"),
            "(CAST(\"id\" AS TEXT) NOT ILIKE '%4%' ESCAPE '!')"
        );
    }

    #[test]
    fn is_empty_matches_null_as_well_as_the_empty_string() {
        assert_eq!(
            where_of("sqlite", "note", Op::IsEmpty, "ignored"),
            "(\"note\" IS NULL OR \"note\" = '')"
        );
        assert_eq!(
            where_of("postgres", "n", Op::IsEmpty, ""),
            "(\"n\" IS NULL OR CAST(\"n\" AS TEXT) = '')"
        );
    }

    #[test]
    fn comparison_operators_map_to_their_sql_spelling() {
        for (op, sql) in [
            (Op::Ne, "<>"),
            (Op::Lt, "<"),
            (Op::Le, "<="),
            (Op::Gt, ">"),
            (Op::Ge, ">="),
        ] {
            assert_eq!(
                where_of("sqlite", "n", op, "5"),
                format!("(\"n\" {sql} '5')")
            );
        }
    }

    #[test]
    fn a_nul_in_a_value_or_column_is_refused_not_truncated() {
        let err = derive_statement("sqlite", "SELECT 1", &one("n", Op::Eq, "a\0b")).unwrap_err();
        assert!(err.contains("NUL"), "{err}");
        assert!(derive_statement("postgres", "SELECT 1", &one("a\0", Op::IsNull, "")).is_err());
    }

    #[test]
    fn a_blank_statement_or_an_unknown_operator_is_an_error() {
        assert!(derive_statement("sqlite", " ;\n", &one("n", Op::IsNull, "")).is_err());
        assert!(
            serde_json::from_str::<Spec>(r#"{"filters":[{"column":"a","op":"like"}]}"#).is_err()
        );
        assert!(serde_json::from_str::<Spec>(r#"{"where":"1=1"}"#).is_err());
    }
}
