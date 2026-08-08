//! `Predicate` -> BSON filter compilation (ticket item 2), under datagrep's
//! NoSQL-injection rule: a value can never become part of the query's
//! structure.
//!
//! **The injection rule, concretely.** Every comparison compiles to an
//! explicit operator form (`{field: {"$eq": v}}`, never the bare
//! `{field: v}` shorthand). MongoDB only treats a field's value as an
//! operator document when *that exact position* holds a document whose keys
//! all start with `$`; once a value is nested one level inside `$eq`/`$in`/
//! etc. it is compared as an opaque literal and never reinterpreted for its
//! own embedded `$`-prefixed keys. So a parameter value shaped like
//! `{"$ne": null}` — the canonical NoSQL-injection payload — can
//! never promote itself to an operator: `value_to_bson` maps it to a
//! `Bson::Document` exactly like any other value, and the wrapping `$eq`
//! guarantees it is compared, not executed. Values are always taken from the
//! typed `Value` the caller supplied — nothing here ever touches query text.

use bson::{doc, Bson, Document as BsonDocument};

use datagrep_api::request::Predicate;
use datagrep_api::value::{FieldPath, PathSeg};
use datagrep_api::DbError;

use crate::value::value_to_bson_for_field;

/// `a.b[3].c` -> `"a.b.3.c"`, the dotted-path convention Mongo uses to
/// address nested fields and array elements alike.
pub fn field_path_to_mongo(path: &FieldPath) -> String {
    let mut out = String::new();
    for seg in path.segments() {
        if !out.is_empty() {
            out.push('.');
        }
        match seg {
            PathSeg::Field(name) => out.push_str(name),
            PathSeg::Index(n) => out.push_str(&n.to_string()),
        }
    }
    out
}

/// Compile a single comparison to `{field: {op: value}}`, routing the value
/// through [`value_to_bson_for_field`] (the `_id`-hex-string recovery
/// heuristic, and never string-spliced — see the module doc).
fn cmp_op(
    field: &FieldPath,
    op: &str,
    value: &datagrep_api::Value,
) -> Result<BsonDocument, DbError> {
    let f = field_path_to_mongo(field);
    let bson = value_to_bson_for_field(&f, value)?;
    Ok(doc! { f: { op: bson } })
}

/// Compile a [`Predicate`] tree to a Mongo filter document: `Op::Scan`'s
/// filter is built natively as BSON, never translated into query text.
pub fn compile_predicate(pred: &Predicate) -> Result<BsonDocument, DbError> {
    match pred {
        Predicate::Eq { field, value } => cmp_op(field, "$eq", value),
        Predicate::Ne { field, value } => cmp_op(field, "$ne", value),
        Predicate::Lt { field, value } => cmp_op(field, "$lt", value),
        Predicate::Le { field, value } => cmp_op(field, "$lte", value),
        Predicate::Gt { field, value } => cmp_op(field, "$gt", value),
        Predicate::Ge { field, value } => cmp_op(field, "$gte", value),
        Predicate::In { field, values } => {
            let f = field_path_to_mongo(field);
            let mut arr = Vec::with_capacity(values.len());
            for v in values {
                arr.push(value_to_bson_for_field(&f, v)?);
            }
            Ok(doc! { f: { "$in": Bson::Array(arr) } })
        }
        // SQL-LIKE-flavored pattern; the pattern text is data, carried under
        // `$regex` (an operator position), never spliced into the filter's
        // structure.
        Predicate::Like { field, pattern } => {
            let f = field_path_to_mongo(field);
            Ok(doc! { f: { "$regex": like_to_regex(pattern), "$options": "" } })
        }
        Predicate::Exists { field } => {
            let f = field_path_to_mongo(field);
            Ok(doc! { f: { "$exists": true } })
        }
        // `Predicate::IsNull` means "present, but null" — whereas
        // Mongo's bare `{field: null}` also matches a field that is entirely
        // *absent* (its well-known null/missing conflation), so this is
        // compiled as an explicit conjunction to keep faith with the
        // Absent-vs-Null distinction that is this crate's whole point.
        Predicate::IsNull { field } => {
            let f = field_path_to_mongo(field);
            Ok(doc! {
                "$and": [
                    { f.clone(): { "$eq": Bson::Null } },
                    { f: { "$exists": true } },
                ]
            })
        }
        Predicate::And(parts) => Ok(doc! { "$and": compile_many(parts)? }),
        Predicate::Or(parts) => Ok(doc! { "$or": compile_many(parts)? }),
        // `$not` only wraps a single-field operator expression in Mongo;
        // `$nor` is the general-purpose negation of an arbitrary filter
        // document and is what `Predicate::Not` needs here.
        Predicate::Not(inner) => Ok(doc! { "$nor": [compile_predicate(inner)?] }),
    }
}

fn compile_many(parts: &[Predicate]) -> Result<Vec<BsonDocument>, DbError> {
    parts.iter().map(compile_predicate).collect()
}

/// Translate `Predicate::Like`'s SQL-style `%`/`_` wildcards into an
/// anchored regex, escaping every other regex metacharacter in the pattern so
/// only the two wildcard characters carry special meaning.
fn like_to_regex(pattern: &str) -> String {
    let mut out = String::with_capacity(pattern.len() + 2);
    out.push('^');
    for c in pattern.chars() {
        match c {
            '%' => out.push_str(".*"),
            '_' => out.push('.'),
            '.' | '+' | '*' | '?' | '(' | ')' | '[' | ']' | '{' | '}' | '|' | '^' | '$' | '\\' => {
                out.push('\\');
                out.push(c);
            }
            other => out.push(other),
        }
    }
    out.push('$');
    out
}

/// AND a keyset resume constraint (`_id > last`) into a compiled filter
/// (ticket item 3: `find` resumes via `{_id: {$gt: last}}`, so a resumed
/// scan can't re-yield or skip documents the way an offset would).
pub fn and_keyset(filter: Option<BsonDocument>, last_id: Bson) -> BsonDocument {
    let keyset = doc! { "_id": { "$gt": last_id } };
    match filter {
        None => keyset,
        Some(f) if f.is_empty() => keyset,
        Some(f) => doc! { "$and": [f, keyset] },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use datagrep_api::value::Document as DatagrepDocument;
    use datagrep_api::Value;
    use std::sync::Arc;

    #[test]
    fn eq_compiles_to_explicit_operator_not_bare_shorthand() {
        let pred = Predicate::Eq {
            field: FieldPath::field("status"),
            value: Value::Str(Arc::from("active")),
        };
        let compiled = compile_predicate(&pred).unwrap();
        assert_eq!(compiled, doc! { "status": { "$eq": "active" } });
    }

    /// The canonical NoSQL-injection scenario: a parameter value shaped
    /// like `{"$ne": null}` must never be able to rewrite the query
    /// structure. Compiling through `$eq` guarantees the attacker-controlled
    /// document only ever appears as $eq's literal operand, never as the
    /// direct value of the field key (the one position Mongo treats as an
    /// operator document).
    #[test]
    fn ne_null_shaped_parameter_value_cannot_alter_query_structure() {
        let malicious = Value::Document(Arc::new(DatagrepDocument::from_fields(vec![(
            Arc::from("$ne"),
            Value::Null,
        )])));
        let pred = Predicate::Eq {
            field: FieldPath::field("x"),
            value: malicious,
        };
        let compiled = compile_predicate(&pred).unwrap();
        // The ENTIRE compiled filter's only top-level key is "x", and "x"'s
        // value is a document whose only key is "$eq" — never "$ne" at the
        // position Mongo would interpret it as an operator.
        assert_eq!(compiled.keys().collect::<Vec<_>>(), vec!["x"]);
        let inner = compiled.get_document("x").unwrap();
        assert_eq!(inner.keys().collect::<Vec<_>>(), vec!["$eq"]);
        // The attacker's "$ne" survives only nested two levels deep, as an
        // inert literal being compared for equality — not executed.
        let literal = inner.get_document("$eq").unwrap();
        assert_eq!(literal, &doc! { "$ne": Bson::Null });
        assert_eq!(
            compiled,
            doc! { "x": { "$eq": { "$ne": Bson::Null } } },
            "the whole point: $ne never reaches operator position"
        );
    }

    #[test]
    fn in_compiles_typed_values_never_strings() {
        let pred = Predicate::In {
            field: FieldPath::field("n"),
            values: vec![Value::I64(1), Value::I64(2), Value::I64(3)],
        };
        let compiled = compile_predicate(&pred).unwrap();
        assert_eq!(
            compiled,
            doc! { "n": { "$in": [Bson::Int64(1), Bson::Int64(2), Bson::Int64(3)] } }
        );
    }

    #[test]
    fn is_null_requires_present_and_null_not_merely_absent() {
        let pred = Predicate::IsNull {
            field: FieldPath::field("deleted_at"),
        };
        let compiled = compile_predicate(&pred).unwrap();
        assert_eq!(
            compiled,
            doc! {
                "$and": [
                    { "deleted_at": { "$eq": Bson::Null } },
                    { "deleted_at": { "$exists": true } },
                ]
            }
        );
    }

    #[test]
    fn and_or_not_compose() {
        let pred = Predicate::And(vec![
            Predicate::Eq {
                field: FieldPath::field("a"),
                value: Value::I64(1),
            },
            Predicate::Not(Box::new(Predicate::Exists {
                field: FieldPath::field("b"),
            })),
        ]);
        let compiled = compile_predicate(&pred).unwrap();
        assert_eq!(
            compiled,
            doc! {
                "$and": [
                    { "a": { "$eq": 1_i64 } },
                    { "$nor": [ { "b": { "$exists": true } } ] },
                ]
            }
        );
    }

    #[test]
    fn nested_field_path_compiles_dotted() {
        let f: FieldPath = "address.tags[0]".parse().unwrap();
        assert_eq!(field_path_to_mongo(&f), "address.tags.0");
    }

    #[test]
    fn like_translates_wildcards_and_escapes_regex_metachars() {
        assert_eq!(like_to_regex("a%b_c"), "^a.*b.c$");
        assert_eq!(like_to_regex("a.b"), "^a\\.b$");
    }

    #[test]
    fn and_keyset_wraps_existing_filter() {
        let base = Some(doc! { "status": { "$eq": "active" } });
        let combined = and_keyset(base, Bson::Int64(42));
        assert_eq!(
            combined,
            doc! {
                "$and": [
                    { "status": { "$eq": "active" } },
                    { "_id": { "$gt": 42_i64 } },
                ]
            }
        );
        let empty = and_keyset(None, Bson::Int64(1));
        assert_eq!(empty, doc! { "_id": { "$gt": 1_i64 } });
    }
}
