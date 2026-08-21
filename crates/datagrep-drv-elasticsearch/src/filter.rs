use std::sync::Arc;

use serde_json::{json, Value as Json};

use datagrep_api::driver::{Notice, NoticeSeverity};
use datagrep_api::error::DbError;
use datagrep_api::request::Predicate;
use datagrep_api::value::{FieldPath, PathSeg, Value};

use crate::value::value_to_json;

#[derive(Debug, Clone, PartialEq)]
pub struct CompiledFilter {
    pub query: Json,
    pub notices: Vec<Notice>,
}

pub fn field_path_to_es(path: &FieldPath) -> (String, bool) {
    let mut out = String::new();
    let mut dropped = false;
    for seg in path.segments() {
        match seg {
            PathSeg::Field(name) => {
                if !out.is_empty() {
                    out.push('.');
                }
                out.push_str(name);
            }
            // Elasticsearch has no positional array access in a field name.
            PathSeg::Index(_) => dropped = true,
        }
    }
    (out, dropped)
}

struct Ctx {
    dropped_index_paths: Vec<String>,
    null_conflated_paths: Vec<String>,
}

impl Ctx {
    fn field(&mut self, path: &FieldPath) -> Result<String, DbError> {
        let (name, dropped) = field_path_to_es(path);
        if name.is_empty() {
            return Err(DbError::Unsupported {
                feature: format!(
                    "filter on `{path}`: Elasticsearch cannot address an array element positionally"
                ),
            });
        }
        if dropped && !self.dropped_index_paths.contains(&name) {
            self.dropped_index_paths.push(name.clone());
        }
        Ok(name)
    }

    fn into_notices(self) -> Vec<Notice> {
        let mut notices = Vec::new();
        if !self.dropped_index_paths.is_empty() {
            notices.push(Notice {
                severity: NoticeSeverity::Warning,
                code: Some(Arc::from("es.filter.array_index_dropped")),
                message: Arc::from(
                    format!(
                        "array index segments were dropped from filter path(s) {}: Elasticsearch \
                     flattens arrays at index time, so this matches ANY element, not a position",
                        self.dropped_index_paths.join(", ")
                    )
                    .as_str(),
                ),
            });
        }
        if !self.null_conflated_paths.is_empty() {
            notices.push(Notice {
                severity: NoticeSeverity::Warning,
                code: Some(Arc::from("es.filter.null_is_absent")),
                message: Arc::from(
                    format!(
                    "`is null` on {} compiled to `must_not exists`: Elasticsearch does not index \
                     JSON nulls, so a stored null and an absent field are indistinguishable here",
                    self.null_conflated_paths.join(", ")
                )
                    .as_str(),
                ),
            });
        }
        notices
    }
}

pub fn compile_predicate(pred: &Predicate) -> Result<CompiledFilter, DbError> {
    let mut ctx = Ctx {
        dropped_index_paths: Vec::new(),
        null_conflated_paths: Vec::new(),
    };
    let query = compile(pred, &mut ctx)?;
    Ok(CompiledFilter {
        query,
        notices: ctx.into_notices(),
    })
}

fn compile(pred: &Predicate, ctx: &mut Ctx) -> Result<Json, DbError> {
    Ok(match pred {
        Predicate::Eq { field, value } => term_clause(ctx.field(field)?, value, ctx)?,
        Predicate::Ne { field, value } => {
            let inner = term_clause(ctx.field(field)?, value, ctx)?;
            json!({ "bool": { "must_not": [inner] } })
        }
        Predicate::Lt { field, value } => range_clause(ctx.field(field)?, "lt", value),
        Predicate::Le { field, value } => range_clause(ctx.field(field)?, "lte", value),
        Predicate::Gt { field, value } => range_clause(ctx.field(field)?, "gt", value),
        Predicate::Ge { field, value } => range_clause(ctx.field(field)?, "gte", value),
        Predicate::In { field, values } => {
            let f = ctx.field(field)?;
            let arr: Vec<Json> = values.iter().map(value_to_json).collect();
            json!({ "terms": { f: Json::Array(arr) } })
        }
        Predicate::Like { field, pattern } => {
            let f = ctx.field(field)?;
            json!({
                "wildcard": {
                    f: { "value": like_to_wildcard(pattern) }
                }
            })
        }
        Predicate::Exists { field } => {
            let f = ctx.field(field)?;
            json!({ "exists": { "field": f } })
        }
        Predicate::IsNull { field } => {
            let f = ctx.field(field)?;
            if !ctx.null_conflated_paths.contains(&f) {
                ctx.null_conflated_paths.push(f.clone());
            }
            json!({ "bool": { "must_not": [ { "exists": { "field": f } } ] } })
        }
        Predicate::And(parts) => {
            json!({ "bool": { "filter": compile_many(parts, ctx)? } })
        }
        Predicate::Or(parts) => json!({
            "bool": { "should": compile_many(parts, ctx)?, "minimum_should_match": 1 }
        }),
        Predicate::Not(inner) => json!({ "bool": { "must_not": [compile(inner, ctx)?] } }),
    })
}

fn compile_many(parts: &[Predicate], ctx: &mut Ctx) -> Result<Vec<Json>, DbError> {
    parts.iter().map(|p| compile(p, ctx)).collect()
}

fn term_clause(field: String, value: &Value, ctx: &mut Ctx) -> Result<Json, DbError> {
    if matches!(value, Value::Null | Value::Absent) {
        if !ctx.null_conflated_paths.contains(&field) {
            ctx.null_conflated_paths.push(field.clone());
        }
        return Ok(json!({ "bool": { "must_not": [ { "exists": { "field": field } } ] } }));
    }
    Ok(json!({ "term": { field: { "value": value_to_json(value) } } }))
}

fn range_clause(field: String, op: &str, value: &Value) -> Json {
    json!({ "range": { field: { op: value_to_json(value) } } })
}

fn like_to_wildcard(pattern: &str) -> String {
    let mut out = String::with_capacity(pattern.len() + 2);
    for c in pattern.chars() {
        match c {
            '%' => out.push('*'),
            '_' => out.push('?'),
            '*' | '?' | '\\' => {
                out.push('\\');
                out.push(c);
            }
            other => out.push(other),
        }
    }
    out
}

pub fn compile_sort(keys: &[datagrep_api::request::SortKey]) -> Result<Vec<Json>, DbError> {
    let mut out = Vec::with_capacity(keys.len());
    for key in keys {
        let (field, _) = field_path_to_es(&key.path);
        if field.is_empty() {
            return Err(DbError::Unsupported {
                feature: format!("sort on `{}`: not addressable in Elasticsearch", key.path),
            });
        }
        out.push(json!({
            field: {
                "order": if key.desc { "desc" } else { "asc" },
                "missing": if key.nulls_first { "_first" } else { "_last" },
            }
        }));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use datagrep_api::value::Document;

    fn q(pred: &Predicate) -> Json {
        compile_predicate(pred).unwrap().query
    }

    #[test]
    fn eq_compiles_to_the_explicit_options_form_not_the_bare_value_form() {
        let pred = Predicate::Eq {
            field: FieldPath::field("status"),
            value: Value::Str(Arc::from("active")),
        };
        assert_eq!(
            q(&pred),
            json!({ "term": { "status": { "value": "active" } } })
        );
    }

    #[test]
    fn object_shaped_parameter_value_cannot_inject_a_query_clause() {
        let malicious = Value::Document(Arc::new(Document::from_fields(vec![
            (Arc::from("value"), Value::Str(Arc::from("admin"))),
            (Arc::from("boost"), Value::I64(1000)),
            (Arc::from("case_insensitive"), Value::Bool(true)),
        ])));
        let compiled = q(&Predicate::Eq {
            field: FieldPath::field("role"),
            value: malicious,
        });

        let obj = compiled.as_object().unwrap();
        assert_eq!(obj.keys().collect::<Vec<_>>(), vec!["term"]);
        let opts = compiled["term"]["role"].as_object().unwrap();
        assert_eq!(
            opts.keys().collect::<Vec<_>>(),
            vec!["value"],
            "only `value` may appear in options position"
        );
        assert_eq!(
            compiled,
            json!({ "term": { "role": { "value": {
                "value": "admin", "boost": 1000, "case_insensitive": true
            } } } })
        );
    }

    #[test]
    fn in_always_emits_an_array_never_a_terms_lookup_object() {
        let lookup_attempt = Value::Document(Arc::new(Document::from_fields(vec![
            (Arc::from("index"), Value::Str(Arc::from("secrets"))),
            (Arc::from("id"), Value::Str(Arc::from("1"))),
            (Arc::from("path"), Value::Str(Arc::from("tokens"))),
        ])));
        let compiled = q(&Predicate::In {
            field: FieldPath::field("tag"),
            values: vec![Value::Str(Arc::from("a")), lookup_attempt],
        });
        let terms = &compiled["terms"]["tag"];
        assert!(
            terms.is_array(),
            "terms operand must be an array, got {terms}"
        );
        assert_eq!(terms.as_array().unwrap().len(), 2);
        // The lookup-shaped object is just element 1 of the array.
        assert_eq!(terms[1]["index"], json!("secrets"));
    }

    #[test]
    fn in_compiles_typed_values_never_strings() {
        let compiled = q(&Predicate::In {
            field: FieldPath::field("n"),
            values: vec![Value::I64(1), Value::I64(2)],
        });
        assert_eq!(compiled, json!({ "terms": { "n": [1, 2] } }));
        // Typed, not stringified: JSON numbers, not "1"/"2".
        assert!(compiled["terms"]["n"][0].is_number());
    }

    #[test]
    fn ranges_map_to_the_four_operators_with_typed_operands() {
        assert_eq!(
            q(&Predicate::Ge {
                field: FieldPath::field("age"),
                value: Value::I64(21)
            }),
            json!({ "range": { "age": { "gte": 21 } } })
        );
        assert_eq!(
            q(&Predicate::Lt {
                field: FieldPath::field("age"),
                value: Value::F64(1.5)
            }),
            json!({ "range": { "age": { "lt": 1.5 } } })
        );
        assert_eq!(
            q(&Predicate::Gt {
                field: FieldPath::field("id"),
                value: Value::Decimal(Arc::from("9007199254740993"))
            }),
            json!({ "range": { "id": { "gt": "9007199254740993" } } })
        );
    }

    #[test]
    fn boolean_composition_uses_filter_context_for_and() {
        let compiled = q(&Predicate::And(vec![
            Predicate::Eq {
                field: FieldPath::field("a"),
                value: Value::I64(1),
            },
            Predicate::Not(Box::new(Predicate::Exists {
                field: FieldPath::field("b"),
            })),
        ]));
        assert_eq!(
            compiled,
            json!({ "bool": { "filter": [
                { "term": { "a": { "value": 1 } } },
                { "bool": { "must_not": [ { "exists": { "field": "b" } } ] } }
            ] } })
        );
    }

    #[test]
    fn or_sets_minimum_should_match_so_it_is_a_filter_not_a_ranking_hint() {
        let compiled = q(&Predicate::Or(vec![
            Predicate::Eq {
                field: FieldPath::field("a"),
                value: Value::I64(1),
            },
            Predicate::Eq {
                field: FieldPath::field("b"),
                value: Value::I64(2),
            },
        ]));
        assert_eq!(compiled["bool"]["minimum_should_match"], json!(1));
        assert_eq!(compiled["bool"]["should"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn ne_wraps_the_same_term_clause_in_must_not() {
        assert_eq!(
            q(&Predicate::Ne {
                field: FieldPath::field("s"),
                value: Value::Str(Arc::from("x"))
            }),
            json!({ "bool": { "must_not": [ { "term": { "s": { "value": "x" } } } ] } })
        );
    }

    #[test]
    fn is_null_admits_that_elasticsearch_conflates_null_and_absent() {
        let compiled = compile_predicate(&Predicate::IsNull {
            field: FieldPath::field("deleted_at"),
        })
        .unwrap();
        assert_eq!(
            compiled.query,
            json!({ "bool": { "must_not": [ { "exists": { "field": "deleted_at" } } ] } })
        );
        assert_eq!(compiled.notices.len(), 1, "the limitation must be surfaced");
        assert_eq!(
            compiled.notices[0].code.as_deref(),
            Some("es.filter.null_is_absent")
        );
        assert!(compiled.notices[0].message.contains("indistinguishable"));
    }

    #[test]
    fn eq_null_degrades_to_the_same_clause_and_notice() {
        let compiled = compile_predicate(&Predicate::Eq {
            field: FieldPath::field("x"),
            value: Value::Null,
        })
        .unwrap();
        assert_eq!(
            compiled.query,
            json!({ "bool": { "must_not": [ { "exists": { "field": "x" } } ] } })
        );
        assert_eq!(compiled.notices.len(), 1);
    }

    #[test]
    fn like_translates_sql_wildcards_and_escapes_native_ones() {
        assert_eq!(like_to_wildcard("a%b_c"), "a*b?c");
        assert_eq!(
            like_to_wildcard("100%*"),
            "100*\\*",
            "a literal * in the pattern must be escaped"
        );
        assert_eq!(
            q(&Predicate::Like {
                field: FieldPath::field("name"),
                pattern: Arc::from("ali%")
            }),
            json!({ "wildcard": { "name": { "value": "ali*" } } })
        );
    }

    #[test]
    fn nested_paths_dot_join_and_array_indexes_are_dropped_loudly() {
        let (name, dropped) = field_path_to_es(&"address.city".parse().unwrap());
        assert_eq!(name, "address.city");
        assert!(!dropped);

        let compiled = compile_predicate(&Predicate::Eq {
            field: "tags[0]".parse().unwrap(),
            value: Value::Str(Arc::from("home")),
        })
        .unwrap();
        assert_eq!(
            compiled.query,
            json!({ "term": { "tags": { "value": "home" } } })
        );
        assert_eq!(
            compiled.notices[0].code.as_deref(),
            Some("es.filter.array_index_dropped"),
            "dropping the index must never be silent"
        );
    }

    #[test]
    fn a_path_that_is_only_an_index_is_refused_not_guessed() {
        let err = compile_predicate(&Predicate::Eq {
            field: "[0]".parse().unwrap(),
            value: Value::I64(1),
        });
        assert!(matches!(err, Err(DbError::Unsupported { .. })));
    }

    #[test]
    fn sort_keys_carry_explicit_missing_placement() {
        use datagrep_api::request::SortKey;
        let sorts = compile_sort(&[
            SortKey {
                path: FieldPath::field("ts"),
                desc: true,
                nulls_first: false,
            },
            SortKey {
                path: "a.b".parse().unwrap(),
                desc: false,
                nulls_first: true,
            },
        ])
        .unwrap();
        assert_eq!(
            sorts,
            vec![
                json!({ "ts": { "order": "desc", "missing": "_last" } }),
                json!({ "a.b": { "order": "asc", "missing": "_first" } }),
            ]
        );
    }
}
