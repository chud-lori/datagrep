use std::sync::Arc;

use serde_json::{json, Map, Value as Json};

use datagrep_api::driver::{Notice, NoticeSeverity};
use datagrep_api::error::DbError;
use datagrep_api::request::Mutation;
use datagrep_api::shape::ObjectPath;
use datagrep_api::value::{Document, FieldPath, PathSeg, Value};

use crate::http::{version_pair, Method, Product};
use crate::value::value_to_json;

const ENVELOPE_FIELDS: &[&str] = &[
    "_index",
    "_id",
    "_routing",
    "_seq_no",
    "_primary_term",
    "_score",
    "_source",
    "_version",
    "_ignored",
];

#[derive(Debug, Clone, PartialEq)]
pub struct CompiledWrite {
    pub op: &'static str,
    pub index: String,
    pub id: String,
    pub routing: Option<String>,
    pub method: Method,
    pub path: String,
    pub query: Vec<(&'static str, String)>,
    pub body: Option<Json>,
}

pub fn supports_include_source_on_error(product: Product, version: &str) -> bool {
    matches!(product, Product::Elasticsearch) && version_pair(version) >= (8, 18)
}

pub fn compile_mutation(
    mutation: &Mutation,
    include_source_on_error: bool,
) -> Result<CompiledWrite, DbError> {
    match mutation {
        Mutation::Update {
            key, sets, expect, ..
        } => {
            let identity = identity_from_key(key)?;
            let guard = guard_from_expect(expect)?;
            let body = sets_to_update_body(sets)?;
            let mut query = guard_query(&guard, identity.routing.as_deref());
            if include_source_on_error {
                // Never let a malformed-document error echo the document.
                query.push(("include_source_on_error", "false".to_string()));
            }
            Ok(CompiledWrite {
                op: "update",
                path: format!(
                    "/{}/_update/{}",
                    encode_segment(&identity.index),
                    encode_segment(&identity.id)
                ),
                method: Method::Post,
                query,
                body: Some(body),
                index: identity.index,
                id: identity.id,
                routing: identity.routing,
            })
        }
        Mutation::Delete { key, expect, .. } => {
            let identity = identity_from_key(key)?;
            let guard = guard_from_expect(expect)?;
            let query = guard_query(&guard, identity.routing.as_deref());
            Ok(CompiledWrite {
                op: "delete",
                path: format!(
                    "/{}/_doc/{}",
                    encode_segment(&identity.index),
                    encode_segment(&identity.id)
                ),
                method: Method::Delete,
                query,
                body: None,
                index: identity.index,
                id: identity.id,
                routing: identity.routing,
            })
        }
        Mutation::Insert { path, doc } => compile_insert(path, doc, include_source_on_error),
    }
}

// Insert guards with op_type=create (409 on an existing id), not if_seq_no; the body is _source only — echoing envelope metadata breaks on ES 8.
fn compile_insert(
    path: &ObjectPath,
    doc: &Value,
    include_source_on_error: bool,
) -> Result<CompiledWrite, DbError> {
    let index = path
        .parts()
        .first()
        .map(|p| p.to_string())
        .filter(|i| !i.is_empty())
        .ok_or_else(|| DbError::Unsupported {
            feature: "an insert needs a concrete target index in its path".into(),
        })?;
    refuse_wildcard_index(&index)?;

    let fields = match doc {
        Value::Document(d) => d,
        other => {
            return Err(DbError::Unsupported {
                feature: format!("an insert document must be an object/document, got {other:?}"),
            })
        }
    };

    // Split the envelope (identity) from the `_source` body.
    let mut id: Option<String> = None;
    let mut routing: Option<String> = None;
    let mut source = Map::new();
    for (name, value) in fields.iter() {
        match name.as_ref() {
            "_id" => {
                if matches!(value, Value::Null | Value::Absent) {
                    continue;
                }
                let s = scalar_to_string(value).ok_or_else(|| DbError::Unsupported {
                    feature: format!("insert `_id` must be a string (got {value:?})"),
                })?;
                if s.is_empty() {
                    continue;
                }
                id = Some(s);
            }
            "_routing" => {
                if matches!(value, Value::Null | Value::Absent) {
                    continue;
                }
                let s = scalar_to_string(value).ok_or_else(|| DbError::Unsupported {
                    feature: format!("insert `_routing` must be a string (got {value:?})"),
                })?;
                if s.is_empty() {
                    continue;
                }
                routing = Some(s);
            }
            // Every other envelope field is metadata, never part of the document.
            other if ENVELOPE_FIELDS.contains(&other) => continue,
            _ => {
                if contains_absent(value) {
                    return Err(DbError::Unsupported {
                        feature: format!(
                            "inserting field `{name}` with an absent value: a new document either \
                             carries a field or omits it — a JSON null would insert an explicit \
                             null instead"
                        ),
                    });
                }
                source.insert(name.to_string(), value_to_json(value));
            }
        }
    }

    if source.is_empty() {
        return Err(DbError::Unsupported {
            feature: "an insert with no `_source` fields (only envelope metadata)".into(),
        });
    }

    let mut query: Vec<(&'static str, String)> = Vec::new();
    let (method, request_path, id) = match id {
        Some(id) => {
            refuse_unaddressable_id(&id)?;
            query.push(("op_type", "create".to_string()));
            let p = format!("/{}/_doc/{}", encode_segment(&index), encode_segment(&id));
            (Method::Put, p, id)
        }
        // No id: let the server generate one. `POST /<index>/_doc`.
        None => (
            Method::Post,
            format!("/{}/_doc", encode_segment(&index)),
            String::new(),
        ),
    };
    query.push(("refresh", "wait_for".to_string()));
    if let Some(routing) = &routing {
        query.push(("routing", routing.clone()));
    }
    if include_source_on_error {
        query.push(("include_source_on_error", "false".to_string()));
    }

    Ok(CompiledWrite {
        op: "insert",
        index,
        id,
        routing,
        method,
        path: request_path,
        query,
        body: Some(Json::Object(source)),
    })
}

struct WriteIdentity {
    index: String,
    id: String,
    routing: Option<String>,
}

fn identity_from_key(key: &[(FieldPath, Value)]) -> Result<WriteIdentity, DbError> {
    let mut index: Option<String> = None;
    let mut id: Option<String> = None;
    let mut routing: Option<String> = None;
    let mut routing_seen = false;
    for (path, value) in key {
        let name = single_field(path).ok_or_else(|| DbError::Unsupported {
            feature: format!(
                "mutation key path `{path}`: Elasticsearch document identity is the flat \
                 `_index` + `_id` (+ `_routing`) envelope, not a nested path"
            ),
        })?;
        let slot = match name {
            "_index" => &mut index,
            "_id" => &mut id,
            "_routing" => {
                if routing_seen {
                    return Err(DbError::Unsupported {
                        feature: "mutation key names `_routing` twice — refusing an ambiguous key"
                            .into(),
                    });
                }
                routing_seen = true;
                if matches!(value, Value::Null | Value::Absent) {
                    continue;
                }
                &mut routing
            }
            other => {
                return Err(DbError::Unsupported {
                    feature: format!(
                        "mutation key field `{other}`: Elasticsearch identifies a document by \
                         `_index` + `_id` (+ `_routing`) only"
                    ),
                })
            }
        };
        if slot.is_some() {
            return Err(DbError::Unsupported {
                feature: format!("mutation key names `{name}` twice — refusing an ambiguous key"),
            });
        }
        *slot = Some(scalar_to_string(value).ok_or_else(|| DbError::Unsupported {
            feature: format!("mutation key `{name}` must be a string (got {value:?})"),
        })?);
    }
    let index = index
        .filter(|i| !i.is_empty())
        .ok_or_else(|| missing_identity("_index"))?;
    let id = id
        .filter(|i| !i.is_empty())
        .ok_or_else(|| missing_identity("_id"))?;
    refuse_wildcard_index(&index)?;
    refuse_unaddressable_id(&id)?;
    Ok(WriteIdentity { index, id, routing })
}

fn refuse_wildcard_index(index: &str) -> Result<(), DbError> {
    if index.contains('*') || index.contains(',') {
        return Err(DbError::Unsupported {
            feature: format!(
                "writing to the index expression `{index}`: a write targets exactly one concrete \
                 index, never a wildcard or a list"
            ),
        });
    }
    Ok(())
}

fn refuse_unaddressable_id(id: &str) -> Result<(), DbError> {
    if id == "." || id == ".." {
        return Err(DbError::Unsupported {
            feature: format!(
                "a `_id` of `{id}`: a single/double-dot segment is normalised away by URL path \
                 resolution (a write to it would collapse onto the index endpoint), so a write \
                 cannot address it safely"
            ),
        });
    }
    Ok(())
}

fn missing_identity(field: &str) -> DbError {
    DbError::Unsupported {
        feature: format!(
            "a guarded write needs `{field}` in the mutation key; refusing to guess which \
             document to write"
        ),
    }
}

struct Guard {
    seq_no: u64,
    primary_term: u64,
}

fn guard_from_expect(expect: &[(FieldPath, Value)]) -> Result<Guard, DbError> {
    let mut seq_no: Option<u64> = None;
    let mut primary_term: Option<u64> = None;
    for (path, value) in expect {
        let name = single_field(path);
        let slot = match name {
            Some("_seq_no") => &mut seq_no,
            Some("_primary_term") => &mut primary_term,
            _ => {
                return Err(DbError::Unsupported {
                    feature: format!(
                        "precondition on `{path}`: Elasticsearch has no generic per-field \
                         compare-and-swap — the only preconditions it can enforce are `_seq_no` \
                         and `_primary_term` (`if_seq_no`/`if_primary_term`)"
                    ),
                })
            }
        };
        if slot.is_some() {
            return Err(DbError::Unsupported {
                feature: format!(
                    "precondition names `{}` twice — refusing an ambiguous guard",
                    name.unwrap_or_default()
                ),
            });
        }
        let n = match value {
            Value::I64(n) => *n,
            Value::U64(n) => i64::try_from(*n).unwrap_or(-1),
            other => {
                return Err(DbError::Unsupported {
                    feature: format!(
                        "precondition `{}` must be an integer (got {other:?})",
                        name.unwrap_or_default()
                    ),
                })
            }
        };
        let floor = if name == Some("_seq_no") { 0 } else { 1 };
        if n < floor {
            return Err(DbError::Unsupported {
                feature: format!(
                    "precondition `{}` is {n}, a sentinel — this index does not track sequence \
                     numbers (time-series indices on Elasticsearch >= 9.4 disable them), so an \
                     optimistic-concurrency guard cannot protect this write",
                    name.unwrap_or_default()
                ),
            });
        }
        *slot = Some(n as u64);
    }
    match (seq_no, primary_term) {
        (Some(seq_no), Some(primary_term)) => Ok(Guard {
            seq_no,
            primary_term,
        }),
        _ => Err(DbError::Unsupported {
            feature: "an unguarded write: this row carries no `_seq_no`/`_primary_term` \
                      precondition (an aggregation result, a fields-only projection, or a scan \
                      from before the guard was requested) — re-run the scan and retry, rather \
                      than overwriting whatever is there now"
                .into(),
        }),
    }
}

fn guard_query(guard: &Guard, routing: Option<&str>) -> Vec<(&'static str, String)> {
    let mut query = vec![
        ("if_seq_no", guard.seq_no.to_string()),
        ("if_primary_term", guard.primary_term.to_string()),
        // `wait_for`, never `true`: read-your-writes without forcing an immediate shard refresh.
        ("refresh", "wait_for".to_string()),
    ];
    if let Some(routing) = routing {
        query.push(("routing", routing.to_string()));
    }
    query
}

fn sets_to_update_body(sets: &[(FieldPath, Value)]) -> Result<Json, DbError> {
    if sets.is_empty() {
        return Err(DbError::Unsupported {
            feature: "an update that sets nothing".into(),
        });
    }
    let mut assignments: Vec<(&FieldPath, &Value)> = Vec::new();
    let mut removals: Vec<&FieldPath> = Vec::new();
    for (path, value) in sets {
        if matches!(value, Value::Absent) {
            // A top-level Absent is an explicit "remove this field".
            removals.push(path);
        } else if contains_absent(value) {
            return Err(DbError::Unsupported {
                feature: format!(
                    "set `{path}`: a nested absent value is ambiguous (it would silently become a \
                     JSON null); remove the field with a top-level absent set instead"
                ),
            });
        } else {
            assignments.push((path, value));
        }
    }

    if removals.is_empty() {
        // Pure set: the cheaper partial merge.
        Ok(json!({ "doc": build_partial_doc(&assignments)? }))
    } else {
        // At least one removal: one script for everything.
        build_script_body(&assignments, &removals)
    }
}

fn build_partial_doc(assignments: &[(&FieldPath, &Value)]) -> Result<Json, DbError> {
    let mut root = Map::new();
    for (path, value) in assignments {
        let names = set_field_names(path)?;
        insert_nested(&mut root, &names, value_to_json(value), path)?;
    }
    Ok(Json::Object(root))
}

fn build_script_body(
    assignments: &[(&FieldPath, &Value)],
    removals: &[&FieldPath],
) -> Result<Json, DbError> {
    let mut lines: Vec<String> = Vec::new();
    let mut params = Map::new();
    let mut seen: Vec<Vec<&str>> = Vec::new();

    for (i, (path, value)) in assignments.iter().enumerate() {
        let names = script_field_names(path)?;
        refuse_path_overlap(&seen, &names, path)?;
        seen.push(names.clone());
        for depth in 1..names.len() {
            let prefix = access_expr(&names[..depth]);
            lines.push(format!(
                "if (!({prefix} instanceof Map)) {{ {prefix} = [:]; }}"
            ));
        }
        let param = format!("p{i}");
        lines.push(format!("{} = params.{param};", access_expr(&names)));
        params.insert(param, value_to_json(value));
    }

    for path in removals {
        let names = script_field_names(path)?;
        refuse_path_overlap(&seen, &names, path)?;
        seen.push(names.clone());
        if names.len() == 1 {
            lines.push(format!("ctx._source.remove('{}');", names[0]));
        } else {
            let guards: Vec<String> = (1..names.len())
                .map(|depth| format!("{} instanceof Map", access_expr(&names[..depth])))
                .collect();
            let parent = access_expr(&names[..names.len() - 1]);
            let leaf = names[names.len() - 1];
            lines.push(format!(
                "if ({}) {{ {parent}.remove('{leaf}'); }}",
                guards.join(" && ")
            ));
        }
    }

    Ok(json!({
        "script": {
            "lang": "painless",
            "source": lines.join("\n"),
            "params": Json::Object(params),
        }
    }))
}

fn refuse_path_overlap(
    seen: &[Vec<&str>],
    names: &[&str],
    path: &FieldPath,
) -> Result<(), DbError> {
    for prior in seen {
        let common = prior.len().min(names.len());
        if prior[..common] == names[..common] {
            return Err(overlap(path));
        }
    }
    Ok(())
}

fn access_expr(names: &[&str]) -> String {
    let mut expr = String::from("ctx._source");
    for name in names {
        expr.push('.');
        expr.push_str(name);
    }
    expr
}

fn set_field_names(path: &FieldPath) -> Result<Vec<&str>, DbError> {
    let mut names: Vec<&str> = Vec::with_capacity(path.segments().len());
    for seg in path.segments() {
        match seg {
            PathSeg::Field(name) => names.push(name),
            PathSeg::Index(_) => {
                return Err(DbError::Unsupported {
                    feature: format!(
                        "set `{path}`: Elasticsearch cannot address an array element positionally, \
                         and a partial document replaces arrays wholesale"
                    ),
                })
            }
        }
    }
    match names.first() {
        Some(first) if ENVELOPE_FIELDS.contains(first) => {
            return Err(DbError::Unsupported {
                feature: format!(
                    "set `{path}`: the hit envelope is not writable — sets address fields inside \
                     `_source` (write back the document, never its metadata)"
                ),
            })
        }
        Some(_) => {}
        None => {
            return Err(DbError::Unsupported {
                feature: "set with an empty field path".into(),
            })
        }
    }
    Ok(names)
}

fn script_field_names(path: &FieldPath) -> Result<Vec<&str>, DbError> {
    let names = set_field_names(path)?;
    for name in &names {
        if !is_safe_painless_field(name) {
            return Err(DbError::Unsupported {
                feature: format!(
                    "removing/setting `{path}` needs a scripted update, but the field name `{name}` \
                     is not a plain identifier — this driver refuses to build Painless it cannot \
                     address safely rather than approximate it"
                ),
            });
        }
    }
    Ok(names)
}

fn is_safe_painless_field(name: &str) -> bool {
    let mut chars = name.chars();
    match chars.next() {
        Some(c) if c.is_ascii_alphabetic() || c == '_' => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

fn insert_nested(
    root: &mut Map<String, Json>,
    names: &[&str],
    value: Json,
    full_path: &FieldPath,
) -> Result<(), DbError> {
    let mut cursor = root;
    for (i, name) in names.iter().enumerate() {
        let last = i + 1 == names.len();
        if last {
            if cursor.contains_key(*name) {
                return Err(overlap(full_path));
            }
            cursor.insert((*name).to_string(), value);
            return Ok(());
        }
        let entry = cursor
            .entry((*name).to_string())
            .or_insert_with(|| Json::Object(Map::new()));
        cursor = entry.as_object_mut().ok_or_else(|| overlap(full_path))?;
    }
    unreachable!("names is never empty: set_field_names refuses an empty path")
}

fn overlap(path: &FieldPath) -> DbError {
    DbError::Unsupported {
        feature: format!(
            "field paths overlap at `{path}` — one set/remove is the same field as, or nested \
             inside, another; refusing to pick which wins"
        ),
    }
}

fn contains_absent(value: &Value) -> bool {
    match value {
        Value::Absent => true,
        Value::Array(items) => items.iter().any(contains_absent),
        Value::Document(doc) => doc.iter().any(|(_, v)| contains_absent(v)),
        _ => false,
    }
}

pub fn guard_unsupported_reason(settings: &Json, version: &str) -> Option<String> {
    let sentinel_era = version_pair(version) >= (9, 4);
    let indices = settings.as_object()?;
    for (name, entry) in indices {
        let index_settings = entry.get("settings").and_then(Json::as_object);
        let get = |key: &str| -> Option<String> {
            let v = index_settings?.get(key)?;
            match v {
                Json::String(s) => Some(s.clone()),
                Json::Bool(b) => Some(b.to_string()),
                _ => None,
            }
        };
        let disabled = get("index.disable_sequence_numbers");
        if disabled.as_deref() == Some("true") {
            return Some(format!(
                "index `{name}` has sequence numbers disabled \
                 (index.disable_sequence_numbers=true): searches return sentinel `_seq_no` \
                 values there, so the optimistic-concurrency guard cannot protect a write"
            ));
        }
        if sentinel_era
            && get("index.mode").as_deref() == Some("time_series")
            && disabled.as_deref() != Some("false")
        {
            return Some(format!(
                "index `{name}` is a time-series (TSDB) index: Elasticsearch >= 9.4 disables \
                 sequence numbers for these by default and searches return sentinel `_seq_no` \
                 values, so the optimistic-concurrency guard cannot protect a write (set \
                 `index.disable_sequence_numbers: false` in the index template to re-enable it)"
            ));
        }
    }
    None
}

#[derive(Debug)]
pub enum WriteOutcome {
    Applied(Json),
    Failed(DbError),
}

fn target_label(write: &CompiledWrite) -> String {
    if write.id.is_empty() {
        format!("{} (server-assigned id)", write.index)
    } else {
        format!("{}/{}", write.index, write.id)
    }
}

fn push_identity(doc: &mut Document, write: &CompiledWrite) {
    doc.push("op", Value::Str(Arc::from(write.op)));
    doc.push("_index", Value::Str(Arc::from(write.index.as_str())));
    if !write.id.is_empty() {
        doc.push("_id", Value::Str(Arc::from(write.id.as_str())));
    }
    if let Some(routing) = &write.routing {
        doc.push("_routing", Value::Str(Arc::from(routing.as_str())));
    }
}

fn push_applied_fields(
    doc: &mut Document,
    write: &CompiledWrite,
    response: &Json,
    notices: &mut Vec<Notice>,
) {
    doc.push("outcome", Value::Str(Arc::from("applied")));
    if write.id.is_empty() {
        if let Some(new_id) = response.get("_id").and_then(Json::as_str) {
            doc.push("_id", Value::Str(Arc::from(new_id)));
        }
    }
    if let Some(result) = response.get("result").and_then(Json::as_str) {
        doc.push("result", Value::Str(Arc::from(result)));
    }
    for key in ["_seq_no", "_primary_term"] {
        if let Some(n) = response.get(key).and_then(Json::as_i64) {
            doc.push(key, Value::I64(n));
        }
    }
    if response.get("forced_refresh") == Some(&Json::Bool(true)) {
        doc.push("forced_refresh", Value::Bool(true));
        notices.push(Notice {
            severity: NoticeSeverity::Warning,
            code: Some(Arc::from("es.mutate.forced_refresh")),
            message: Arc::from(
                format!(
                    "the write to `{}` forced an immediate refresh: the shard's \
                     refresh-listener queue was full, so refresh=wait_for degraded \
                     to refresh=true",
                    target_label(write)
                )
                .as_str(),
            ),
        });
    }
}

fn push_failed_fields(doc: &mut Document, error: &DbError) -> bool {
    doc.push("outcome", Value::Str(Arc::from("failed")));
    let (code, message, conflict) = match error {
        DbError::Conflict { code, message } => (code.clone(), message.clone(), true),
        DbError::Query { code, message, .. } => (code.clone(), message.clone(), false),
        other => (None, other.to_string(), false),
    };
    if conflict {
        doc.push("conflict", Value::Bool(true));
    }
    if let Some(code) = code {
        doc.push("error_code", Value::Str(Arc::from(code.as_str())));
    }
    doc.push("error", Value::Str(Arc::from(message.as_str())));
    conflict
}

pub fn batch_report(
    writes: &[CompiledWrite],
    outcomes: Vec<WriteOutcome>,
) -> (Vec<Value>, Vec<Notice>) {
    let total = writes.len();
    let mut docs = Vec::with_capacity(total);
    let mut notices = Vec::new();
    let mut applied = 0usize;
    let mut failed: Option<usize> = None;

    let mut outcomes = outcomes.into_iter();
    for (i, write) in writes.iter().enumerate() {
        let mut doc = Document::new();
        push_identity(&mut doc, write);
        match outcomes.next() {
            Some(WriteOutcome::Applied(response)) => {
                applied += 1;
                push_applied_fields(&mut doc, write, &response, &mut notices);
            }
            Some(WriteOutcome::Failed(error)) => {
                failed = Some(i);
                push_failed_fields(&mut doc, &error);
            }
            None => {
                doc.push("outcome", Value::Str(Arc::from("not attempted")));
            }
        }
        docs.push(Value::Document(Arc::new(doc)));
    }

    match failed {
        None => notices.push(Notice {
            severity: NoticeSeverity::Info,
            code: Some(Arc::from("es.mutate.applied")),
            message: Arc::from(
                format!(
                    "{applied} guarded write(s) applied one at a time with refresh=wait_for \
                     (Elasticsearch has no multi-document transaction)"
                )
                .as_str(),
            ),
        }),
        Some(i) => {
            let not_attempted = total - i - 1;
            notices.push(Notice {
                severity: NoticeSeverity::Warning,
                code: Some(Arc::from("es.mutate.halted")),
                message: Arc::from(
                    format!(
                        "mutation {} of {} (`{}` on `{}`) failed: {} applied, 1 failed, {} \
                         not attempted — Elasticsearch has no transaction, so the applied \
                         writes stay written and the rest were never sent",
                        i + 1,
                        total,
                        writes[i].op,
                        target_label(&writes[i]),
                        applied,
                        not_attempted
                    )
                    .as_str(),
                ),
            });
        }
    }
    (docs, notices)
}

pub const MAX_BULK_BODY_BYTES: usize = 100 * 1024 * 1024;

// The guard rides each action line; retry_on_conflict is deliberately never emitted — a silent retry is the clobber the guard prevents.
pub fn compile_bulk_body(writes: &[CompiledWrite], max_bytes: usize) -> Result<String, DbError> {
    let mut body = String::new();
    for write in writes {
        let (action, source) = bulk_lines(write)?;
        body.push_str(&action);
        body.push('\n');
        if let Some(source) = source {
            body.push_str(&source);
            body.push('\n');
        }
    }
    if body.len() > max_bytes {
        return Err(DbError::Unsupported {
            feature: format!(
                "this batch frames to {} bytes of _bulk NDJSON, over the {} MB \
                 http.max_content_length ceiling — split it into smaller batches rather than send \
                 a body Elasticsearch rejects whole",
                body.len(),
                max_bytes / (1024 * 1024)
            ),
        });
    }
    Ok(body)
}

fn bulk_lines(write: &CompiledWrite) -> Result<(String, Option<String>), DbError> {
    let mut meta = Map::new();
    meta.insert("_index".to_string(), Json::String(write.index.clone()));
    let (action_name, source) = match write.op {
        "update" => {
            meta.insert("_id".to_string(), Json::String(write.id.clone()));
            add_bulk_guard(&mut meta, write)?;
            add_bulk_routing(&mut meta, write);
            ("update", write.body.clone())
        }
        "delete" => {
            meta.insert("_id".to_string(), Json::String(write.id.clone()));
            add_bulk_guard(&mut meta, write)?;
            add_bulk_routing(&mut meta, write);
            ("delete", None)
        }
        "insert" => {
            add_bulk_routing(&mut meta, write);
            if write.id.is_empty() {
                // No id: the server generates one. `index` with no `_id`.
                ("index", write.body.clone())
            } else {
                meta.insert("_id".to_string(), Json::String(write.id.clone()));
                ("create", write.body.clone())
            }
        }
        other => {
            return Err(DbError::Protocol(format!(
                "cannot frame a _bulk action for the unknown write op `{other}`"
            )))
        }
    };
    let mut action = Map::new();
    action.insert(action_name.to_string(), Json::Object(meta));
    let action_line = serde_json::to_string(&Json::Object(action))
        .map_err(|e| DbError::Protocol(format!("serializing a _bulk action line: {e}")))?;
    let source_line = match source {
        Some(body) => Some(
            serde_json::to_string(&body)
                .map_err(|e| DbError::Protocol(format!("serializing a _bulk source line: {e}")))?,
        ),
        None => None,
    };
    Ok((action_line, source_line))
}

// Bulk wants if_seq_no/if_primary_term on the action line as JSON numbers, not query-param strings.
fn add_bulk_guard(meta: &mut Map<String, Json>, write: &CompiledWrite) -> Result<(), DbError> {
    match (
        query_u64(write, "if_seq_no"),
        query_u64(write, "if_primary_term"),
    ) {
        (Some(seq_no), Some(primary_term)) => {
            meta.insert("if_seq_no".to_string(), Json::from(seq_no));
            meta.insert("if_primary_term".to_string(), Json::from(primary_term));
            Ok(())
        }
        _ => Err(DbError::Unsupported {
            feature: format!(
                "framing an unguarded `{}` into a _bulk batch: its \
                 `if_seq_no`/`if_primary_term` precondition is missing",
                write.op
            ),
        }),
    }
}

fn add_bulk_routing(meta: &mut Map<String, Json>, write: &CompiledWrite) {
    if let Some(routing) = &write.routing {
        meta.insert("routing".to_string(), Json::String(routing.clone()));
    }
}

fn query_u64(write: &CompiledWrite, key: &str) -> Option<u64> {
    write
        .query
        .iter()
        .find(|(k, _)| *k == key)
        .and_then(|(_, v)| v.parse().ok())
}

// Bulk is not atomic and returns HTTP 200 with per-item failures; every item executes — no early stop, so no not_attempted, unlike the serial batch_report.
pub fn bulk_report(
    writes: &[CompiledWrite],
    response: &Json,
) -> Result<(Vec<Value>, Vec<Notice>), DbError> {
    let items = response
        .get("items")
        .and_then(Json::as_array)
        .ok_or_else(|| DbError::Protocol("a _bulk response with no `items` array".into()))?;
    if items.len() != writes.len() {
        return Err(DbError::Protocol(format!(
            "a _bulk response carried {} item(s) for {} submitted action(s)",
            items.len(),
            writes.len()
        )));
    }

    let total = writes.len();
    let mut docs = Vec::with_capacity(total);
    let mut notices = Vec::new();
    let mut applied = 0usize;
    let mut failed = 0usize;
    let mut conflicts = 0usize;

    for (write, item) in writes.iter().zip(items) {
        let mut doc = Document::new();
        push_identity(&mut doc, write);
        let inner = bulk_item_inner(item)?;
        let status = inner.get("status").and_then(Json::as_i64).unwrap_or(0);
        if (200i64..300).contains(&status) {
            applied += 1;
            push_applied_fields(&mut doc, write, inner, &mut notices);
        } else {
            failed += 1;
            if push_failed_fields(&mut doc, &bulk_item_error(status, inner)) {
                conflicts += 1;
            }
        }
        docs.push(Value::Document(Arc::new(doc)));
    }

    if failed == 0 {
        notices.push(Notice {
            severity: NoticeSeverity::Info,
            code: Some(Arc::from("es.bulk.applied")),
            message: Arc::from(
                format!(
                    "{applied} document(s) written in one _bulk request with refresh=wait_for — \
                     Elasticsearch bulk is not atomic (reported per item), and every item here \
                     applied"
                )
                .as_str(),
            ),
        });
    } else {
        let conflict_note = if conflicts > 0 {
            format!(" ({conflicts} of them a version conflict)")
        } else {
            String::new()
        };
        notices.push(Notice {
            severity: NoticeSeverity::Warning,
            code: Some(Arc::from("es.bulk.partial")),
            message: Arc::from(
                format!(
                    "_bulk applied {applied} of {total} document(s); {failed} failed{conflict_note}. \
                     Elasticsearch bulk is not atomic and has no transaction — it executed every \
                     item and applied the ones it could, so the failed items were left unwritten \
                     while the successful writes stay written (it did not stop at the first failure)"
                )
                .as_str(),
            ),
        });
    }

    Ok((docs, notices))
}

fn bulk_item_inner(item: &Json) -> Result<&Json, DbError> {
    item.as_object()
        .and_then(|o| o.values().next())
        .ok_or_else(|| DbError::Protocol("a _bulk item with no action object".into()))
}

fn bulk_item_error(status: i64, inner: &Json) -> DbError {
    let err = inner.get("error");
    let code = err
        .and_then(|e| e.get("type"))
        .and_then(Json::as_str)
        .map(str::to_string);
    let message = err
        .and_then(|e| e.get("reason"))
        .and_then(Json::as_str)
        .map(str::to_string)
        .unwrap_or_else(|| format!("_bulk item failed with status {status}"));
    if status == 409 {
        DbError::Conflict { code, message }
    } else {
        DbError::Query {
            code,
            message,
            position: None,
        }
    }
}

fn encode_segment(segment: &str) -> String {
    let mut out = String::with_capacity(segment.len());
    for ch in segment.chars() {
        match ch {
            'a'..='z' | 'A'..='Z' | '0'..='9' | '-' | '_' | '.' | '~' => out.push(ch),
            other => {
                let mut buf = [0u8; 4];
                for b in other.encode_utf8(&mut buf).as_bytes() {
                    out.push('%');
                    out.push_str(&format!("{b:02X}"));
                }
            }
        }
    }
    out
}

fn single_field(path: &FieldPath) -> Option<&str> {
    match path.segments() {
        [PathSeg::Field(name)] => Some(name),
        _ => None,
    }
}

fn scalar_to_string(value: &Value) -> Option<String> {
    match value {
        Value::Str(s) => Some(s.to_string()),
        Value::I64(n) => Some(n.to_string()),
        Value::U64(n) => Some(n.to_string()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use datagrep_api::shape::ObjectPath;

    fn fp(s: &str) -> FieldPath {
        s.parse().unwrap()
    }

    fn path() -> ObjectPath {
        ObjectPath::new(vec![Arc::from("events")])
    }

    fn key(index: &str, id: &str) -> Vec<(FieldPath, Value)> {
        vec![
            (fp("_index"), Value::Str(Arc::from(index))),
            (fp("_id"), Value::Str(Arc::from(id))),
        ]
    }

    fn guard(seq: i64, term: i64) -> Vec<(FieldPath, Value)> {
        vec![
            (fp("_seq_no"), Value::I64(seq)),
            (fp("_primary_term"), Value::I64(term)),
        ]
    }

    fn doc_val(fields: Vec<(&str, Value)>) -> Value {
        Value::Document(Arc::new(Document::from_fields(
            fields.into_iter().map(|(k, v)| (Arc::from(k), v)).collect(),
        )))
    }

    fn q<'a>(w: &'a CompiledWrite, k: &str) -> Option<&'a str> {
        w.query
            .iter()
            .find(|(kk, _)| *kk == k)
            .map(|(_, v)| v.as_str())
    }

    #[test]
    fn update_compiles_to_a_guarded_update_url_query_and_partial_doc() {
        let m = Mutation::Update {
            path: path(),
            key: key("events", "abc"),
            sets: vec![(fp("status"), Value::Str(Arc::from("done")))],
            expect: guard(41, 3),
        };
        let w = compile_mutation(&m, true).unwrap();
        assert_eq!(w.op, "update");
        assert_eq!(w.method, Method::Post);
        assert_eq!(w.path, "/events/_update/abc");
        assert_eq!(q(&w, "if_seq_no"), Some("41"));
        assert_eq!(q(&w, "if_primary_term"), Some("3"));
        assert_eq!(q(&w, "refresh"), Some("wait_for"));
        assert_eq!(q(&w, "include_source_on_error"), Some("false"));
        assert!(w.routing.is_none());
        assert!(q(&w, "routing").is_none());
        assert_eq!(w.body, Some(json!({ "doc": { "status": "done" } })));
    }

    #[test]
    fn include_source_on_error_is_omitted_where_the_server_does_not_support_it() {
        let m = Mutation::Update {
            path: path(),
            key: key("events", "abc"),
            sets: vec![(fp("status"), Value::Str(Arc::from("done")))],
            expect: guard(1, 1),
        };
        let w = compile_mutation(&m, false).unwrap();
        assert!(
            q(&w, "include_source_on_error").is_none(),
            "an unsupported parameter must not be sent — older ES/OpenSearch 400 on it"
        );
    }

    #[test]
    fn a_nested_set_builds_nested_objects_so_the_merge_touches_one_leaf() {
        let m = Mutation::Update {
            path: path(),
            key: key("events", "abc"),
            sets: vec![(fp("a.b.c"), Value::I64(5))],
            expect: guard(1, 1),
        };
        let w = compile_mutation(&m, true).unwrap();
        assert_eq!(w.body, Some(json!({ "doc": { "a": { "b": { "c": 5 } } } })));
    }

    #[test]
    fn delete_compiles_to_a_guarded_delete_with_no_body() {
        let m = Mutation::Delete {
            path: path(),
            key: key("events", "abc"),
            expect: guard(7, 2),
        };
        let w = compile_mutation(&m, true).unwrap();
        assert_eq!(w.op, "delete");
        assert_eq!(w.method, Method::Delete);
        assert_eq!(w.path, "/events/_doc/abc");
        assert_eq!(q(&w, "if_seq_no"), Some("7"));
        assert_eq!(q(&w, "if_primary_term"), Some("2"));
        assert_eq!(q(&w, "refresh"), Some("wait_for"));
        // A delete never carries `include_source_on_error` — there is no source.
        assert!(q(&w, "include_source_on_error").is_none());
        assert!(w.body.is_none());
    }

    #[test]
    fn routing_rides_in_the_query_when_the_hit_carried_it() {
        let mut k = key("events", "abc");
        k.push((fp("_routing"), Value::Str(Arc::from("tenant-7"))));
        let m = Mutation::Delete {
            path: path(),
            key: k,
            expect: guard(7, 2),
        };
        let w = compile_mutation(&m, true).unwrap();
        assert_eq!(w.routing.as_deref(), Some("tenant-7"));
        assert_eq!(q(&w, "routing"), Some("tenant-7"));
    }

    #[test]
    fn a_null_or_absent_routing_is_not_treated_as_a_routing_value() {
        for routing in [Value::Null, Value::Absent] {
            let mut k = key("events", "abc");
            k.push((fp("_routing"), routing));
            let w = compile_mutation(
                &Mutation::Delete {
                    path: path(),
                    key: k,
                    expect: guard(7, 2),
                },
                true,
            )
            .unwrap();
            assert!(w.routing.is_none());
            assert!(q(&w, "routing").is_none());
        }
    }

    #[test]
    fn a_write_without_the_guard_is_refused_never_sent_unguarded() {
        let update = Mutation::Update {
            path: path(),
            key: key("events", "abc"),
            sets: vec![(fp("status"), Value::Str(Arc::from("done")))],
            expect: Vec::new(),
        };
        let err = compile_mutation(&update, true).unwrap_err();
        assert!(matches!(err, DbError::Unsupported { .. }));
        assert!(err.to_string().contains("unguarded"));

        let delete = Mutation::Delete {
            path: path(),
            key: key("events", "abc"),
            expect: Vec::new(),
        };
        assert!(matches!(
            compile_mutation(&delete, true),
            Err(DbError::Unsupported { .. })
        ));
    }

    #[test]
    fn a_half_guard_is_refused() {
        // `_seq_no` without `_primary_term` cannot form a compare-and-swap.
        let m = Mutation::Delete {
            path: path(),
            key: key("events", "abc"),
            expect: vec![(fp("_seq_no"), Value::I64(41))],
        };
        assert!(matches!(
            compile_mutation(&m, true),
            Err(DbError::Unsupported { .. })
        ));
    }

    #[test]
    fn a_sentinel_guard_is_refused_as_a_tsdb_index() {
        let m = Mutation::Delete {
            path: path(),
            key: key("events", "abc"),
            expect: vec![
                (fp("_seq_no"), Value::I64(-1)),
                (fp("_primary_term"), Value::I64(0)),
            ],
        };
        let err = compile_mutation(&m, true).unwrap_err();
        assert!(matches!(err, DbError::Unsupported { .. }));
        assert!(err.to_string().contains("sentinel"));
    }

    #[test]
    fn an_absent_set_compiles_to_a_single_guarded_removal_script() {
        let m = Mutation::Update {
            path: path(),
            key: key("events", "abc"),
            sets: vec![(fp("status"), Value::Absent)],
            expect: guard(41, 3),
        };
        let w = compile_mutation(&m, true).unwrap();
        assert_eq!(w.op, "update");
        assert_eq!(w.method, Method::Post);
        assert_eq!(w.path, "/events/_update/abc");
        // The optimistic-concurrency guard still rides on the URL.
        assert_eq!(q(&w, "if_seq_no"), Some("41"));
        assert_eq!(q(&w, "if_primary_term"), Some("3"));
        assert_eq!(q(&w, "refresh"), Some("wait_for"));
        let body = w.body.as_ref().unwrap();
        assert!(
            body.get("doc").is_none(),
            "a removal must not send `doc` — ES would ignore the script otherwise"
        );
        let script = body.get("script").expect("removal compiles to a script");
        let source = script.get("source").and_then(Json::as_str).unwrap();
        assert_eq!(source, "ctx._source.remove('status');", "{source}");
    }

    #[test]
    fn a_mixed_set_and_remove_is_one_script_never_doc_plus_script() {
        let m = Mutation::Update {
            path: path(),
            key: key("events", "abc"),
            sets: vec![
                (fp("status"), Value::Str(Arc::from("done"))),
                (fp("obsolete"), Value::Absent),
            ],
            expect: guard(1, 1),
        };
        let w = compile_mutation(&m, true).unwrap();
        let body = w.body.as_ref().unwrap();
        assert!(
            body.get("doc").is_none(),
            "set + remove must be ONE script, never doc + script"
        );
        let script = body
            .get("script")
            .expect("a mixed update compiles to a script");
        let source = script.get("source").and_then(Json::as_str).unwrap();
        assert!(
            source.contains("ctx._source.status = params.p0;"),
            "the set becomes a params-driven assignment: {source}"
        );
        assert!(
            source.contains("ctx._source.remove('obsolete');"),
            "the removal becomes a remove(): {source}"
        );
        // The value rides in `params`, never interpolated into the Painless.
        assert_eq!(script.get("params").unwrap(), &json!({ "p0": "done" }));
        assert!(
            !source.contains("done"),
            "the user value must not be interpolated into the script source: {source}"
        );
    }

    #[test]
    fn a_pure_set_update_stays_a_doc_partial_merge() {
        let m = Mutation::Update {
            path: path(),
            key: key("events", "abc"),
            sets: vec![
                (fp("status"), Value::Str(Arc::from("done"))),
                (fp("count"), Value::I64(3)),
            ],
            expect: guard(1, 1),
        };
        let w = compile_mutation(&m, true).unwrap();
        let body = w.body.as_ref().unwrap();
        assert!(
            body.get("script").is_none(),
            "a pure-set update must NOT escalate to a script"
        );
        assert_eq!(body, &json!({ "doc": { "status": "done", "count": 3 } }));
    }

    #[test]
    fn a_scripted_update_addresses_nested_paths_and_creates_missing_parents() {
        let m = Mutation::Update {
            path: path(),
            key: key("events", "abc"),
            sets: vec![(fp("a.b.c"), Value::I64(5)), (fp("d.e"), Value::Absent)],
            expect: guard(1, 1),
        };
        let w = compile_mutation(&m, true).unwrap();
        let body = w.body.as_ref().unwrap();
        assert!(body.get("doc").is_none());
        let script = body.get("script").unwrap();
        let source = script.get("source").and_then(Json::as_str).unwrap();
        assert!(
            source.contains("if (!(ctx._source.a instanceof Map)) { ctx._source.a = [:]; }"),
            "{source}"
        );
        assert!(
            source.contains("if (!(ctx._source.a.b instanceof Map)) { ctx._source.a.b = [:]; }"),
            "{source}"
        );
        assert!(
            source.contains("ctx._source.a.b.c = params.p0;"),
            "{source}"
        );
        assert!(
            source.contains("if (ctx._source.d instanceof Map) { ctx._source.d.remove('e'); }"),
            "{source}"
        );
        assert_eq!(script.get("params").unwrap(), &json!({ "p0": 5 }));
    }

    #[test]
    fn a_scripted_update_refuses_a_field_name_it_cannot_express_in_painless() {
        let m = Mutation::Update {
            path: path(),
            key: key("events", "abc"),
            sets: vec![(FieldPath::field("weird name"), Value::Absent)],
            expect: guard(1, 1),
        };
        let err = compile_mutation(&m, true).unwrap_err();
        assert!(matches!(err, DbError::Unsupported { .. }));
        assert!(err.to_string().contains("plain identifier"), "{err}");
    }

    #[test]
    fn an_array_element_edit_is_refused() {
        let m = Mutation::Update {
            path: path(),
            key: key("events", "abc"),
            sets: vec![(fp("tags[0]"), Value::Str(Arc::from("home")))],
            expect: guard(1, 1),
        };
        let err = compile_mutation(&m, true).unwrap_err();
        assert!(matches!(err, DbError::Unsupported { .. }));
        assert!(err.to_string().contains("array element"));
    }

    #[test]
    fn setting_an_envelope_field_is_refused() {
        let m = Mutation::Update {
            path: path(),
            key: key("events", "abc"),
            sets: vec![(fp("_id"), Value::Str(Arc::from("x")))],
            expect: guard(1, 1),
        };
        assert!(matches!(
            compile_mutation(&m, true),
            Err(DbError::Unsupported { .. })
        ));
    }

    #[test]
    fn a_wildcard_or_missing_identity_is_refused() {
        // Wildcard target.
        let m = Mutation::Delete {
            path: path(),
            key: vec![
                (fp("_index"), Value::Str(Arc::from("events-*"))),
                (fp("_id"), Value::Str(Arc::from("abc"))),
            ],
            expect: guard(1, 1),
        };
        assert!(matches!(
            compile_mutation(&m, true),
            Err(DbError::Unsupported { .. })
        ));
        // Missing `_id`.
        let m = Mutation::Delete {
            path: path(),
            key: vec![(fp("_index"), Value::Str(Arc::from("events")))],
            expect: guard(1, 1),
        };
        assert!(matches!(
            compile_mutation(&m, true),
            Err(DbError::Unsupported { .. })
        ));
    }

    #[test]
    fn a_dot_or_dotdot_id_is_refused_as_unaddressable() {
        for bad in [".", ".."] {
            let m = Mutation::Delete {
                path: path(),
                key: key("events", bad),
                expect: guard(1, 1),
            };
            let err = compile_mutation(&m, true).unwrap_err();
            assert!(
                matches!(err, DbError::Unsupported { .. }),
                "`_id` of {bad:?} must be refused"
            );
            assert!(err.to_string().contains("normalised away"));
        }
        // A dot *inside* an id is fine — only a whole `.`/`..` segment collapses.
        let ok = compile_mutation(
            &Mutation::Delete {
                path: path(),
                key: key("events", "a.b"),
                expect: guard(1, 1),
            },
            true,
        )
        .unwrap();
        assert_eq!(ok.path, "/events/_doc/a.b");
    }

    #[test]
    fn a_nested_absent_in_a_set_value_is_refused_not_turned_into_a_null() {
        let nested_doc = Value::Document(Arc::new(Document::from_fields(vec![
            (Arc::from("keep"), Value::I64(1)),
            (Arc::from("f"), Value::Absent),
        ])));
        let m = Mutation::Update {
            path: path(),
            key: key("events", "abc"),
            sets: vec![(fp("obj"), nested_doc)],
            expect: guard(1, 1),
        };
        assert!(matches!(
            compile_mutation(&m, true),
            Err(DbError::Unsupported { .. })
        ));

        // Same hazard one level down inside an array.
        let arr = Value::Array(vec![Value::I64(1), Value::Absent].into());
        let m = Mutation::Update {
            path: path(),
            key: key("events", "abc"),
            sets: vec![(fp("tags"), arr)],
            expect: guard(1, 1),
        };
        assert!(matches!(
            compile_mutation(&m, true),
            Err(DbError::Unsupported { .. })
        ));
    }

    #[test]
    fn a_duplicate_routing_key_is_refused_even_when_the_first_is_null() {
        let m = Mutation::Delete {
            path: path(),
            key: vec![
                (fp("_index"), Value::Str(Arc::from("events"))),
                (fp("_id"), Value::Str(Arc::from("abc"))),
                (fp("_routing"), Value::Null),
                (fp("_routing"), Value::Str(Arc::from("tenant-7"))),
            ],
            expect: guard(1, 1),
        };
        let err = compile_mutation(&m, true).unwrap_err();
        assert!(matches!(err, DbError::Unsupported { .. }));
        assert!(err.to_string().contains("_routing` twice"));
    }

    #[test]
    fn an_empty_update_is_refused() {
        let m = Mutation::Update {
            path: path(),
            key: key("events", "abc"),
            sets: Vec::new(),
            expect: guard(1, 1),
        };
        assert!(matches!(
            compile_mutation(&m, true),
            Err(DbError::Unsupported { .. })
        ));
    }

    #[test]
    fn an_insert_with_a_user_id_compiles_to_put_op_type_create_with_routing() {
        let m = Mutation::Insert {
            path: path(),
            doc: doc_val(vec![
                ("_id", Value::Str(Arc::from("abc"))),
                ("_routing", Value::Str(Arc::from("tenant-7"))),
                ("status", Value::Str(Arc::from("new"))),
                ("count", Value::I64(3)),
            ]),
        };
        let w = compile_mutation(&m, true).unwrap();
        assert_eq!(w.op, "insert");
        assert_eq!(w.method, Method::Put);
        assert_eq!(w.path, "/events/_doc/abc");
        // `op_type=create` is the whole guard — 409s instead of overwriting.
        assert_eq!(q(&w, "op_type"), Some("create"));
        // An insert carries no `if_seq_no` guard: a new doc has no seq_no.
        assert!(q(&w, "if_seq_no").is_none());
        assert_eq!(q(&w, "refresh"), Some("wait_for"));
        assert_eq!(q(&w, "routing"), Some("tenant-7"));
        assert_eq!(w.routing.as_deref(), Some("tenant-7"));
        assert_eq!(q(&w, "include_source_on_error"), Some("false"));
        assert_eq!(w.id, "abc");
        assert_eq!(w.body, Some(json!({ "status": "new", "count": 3 })));
    }

    #[test]
    fn an_insert_without_an_id_compiles_to_post_doc_for_a_server_generated_id() {
        let m = Mutation::Insert {
            path: path(),
            doc: doc_val(vec![("status", Value::Str(Arc::from("new")))]),
        };
        let w = compile_mutation(&m, true).unwrap();
        assert_eq!(w.op, "insert");
        assert_eq!(w.method, Method::Post);
        assert_eq!(w.path, "/events/_doc");
        assert!(
            q(&w, "op_type").is_none(),
            "no id, no create collision to guard"
        );
        assert_eq!(q(&w, "refresh"), Some("wait_for"));
        assert!(w.id.is_empty(), "the server generates the id");
        assert_eq!(w.body, Some(json!({ "status": "new" })));
    }

    #[test]
    fn an_insert_refuses_a_nested_absent_in_the_document() {
        let nested = Value::Document(Arc::new(Document::from_fields(vec![
            (Arc::from("keep"), Value::I64(1)),
            (Arc::from("f"), Value::Absent),
        ])));
        let m = Mutation::Insert {
            path: path(),
            doc: doc_val(vec![("obj", nested)]),
        };
        assert!(matches!(
            compile_mutation(&m, true),
            Err(DbError::Unsupported { .. })
        ));
        let m = Mutation::Insert {
            path: path(),
            doc: doc_val(vec![("status", Value::Absent), ("keep", Value::I64(1))]),
        };
        let err = compile_mutation(&m, true).unwrap_err();
        assert!(matches!(err, DbError::Unsupported { .. }));
        assert!(err.to_string().contains("absent value"), "{err}");
    }

    #[test]
    fn an_insert_refuses_a_dot_or_dotdot_id() {
        for bad in [".", ".."] {
            let m = Mutation::Insert {
                path: path(),
                doc: doc_val(vec![
                    ("_id", Value::Str(Arc::from(bad))),
                    ("x", Value::I64(1)),
                ]),
            };
            let err = compile_mutation(&m, true).unwrap_err();
            assert!(
                matches!(err, DbError::Unsupported { .. }),
                "`_id` of {bad:?} must be refused"
            );
            assert!(err.to_string().contains("normalised away"), "{err}");
        }
    }

    #[test]
    fn an_insert_refuses_an_empty_source_or_a_non_document() {
        // Only envelope metadata, no `_source`.
        let m = Mutation::Insert {
            path: path(),
            doc: doc_val(vec![("_id", Value::Str(Arc::from("x")))]),
        };
        let err = compile_mutation(&m, true).unwrap_err();
        assert!(matches!(err, DbError::Unsupported { .. }));
        assert!(err.to_string().contains("no `_source`"), "{err}");

        // A non-document insert value.
        let m = Mutation::Insert {
            path: path(),
            doc: Value::Null,
        };
        assert!(matches!(
            compile_mutation(&m, true),
            Err(DbError::Unsupported { .. })
        ));
    }

    #[test]
    fn an_insert_without_include_source_on_error_omits_the_param() {
        let m = Mutation::Insert {
            path: path(),
            doc: doc_val(vec![("status", Value::Str(Arc::from("new")))]),
        };
        let w = compile_mutation(&m, false).unwrap();
        assert!(q(&w, "include_source_on_error").is_none());
    }

    #[test]
    fn a_server_generated_insert_id_is_echoed_from_the_response() {
        let w = compile_mutation(
            &Mutation::Insert {
                path: path(),
                doc: doc_val(vec![("status", Value::Str(Arc::from("new")))]),
            },
            true,
        )
        .unwrap();
        let outcomes = vec![WriteOutcome::Applied(json!({
            "result": "created",
            "_id": "generated-xyz",
            "_seq_no": 0,
            "_primary_term": 1
        }))];
        let (docs, _) = batch_report(&[w], outcomes);
        let Value::Document(doc) = &docs[0] else {
            panic!("expected a document row");
        };
        assert_eq!(doc.get("outcome"), Some(&Value::Str(Arc::from("applied"))));
        assert_eq!(
            doc.get("_id"),
            Some(&Value::Str(Arc::from("generated-xyz")))
        );
    }

    #[test]
    fn overlapping_sets_are_refused_rather_than_last_write_wins() {
        let m = Mutation::Update {
            path: path(),
            key: key("events", "abc"),
            sets: vec![(fp("a.b"), Value::I64(1)), (fp("a.b"), Value::I64(2))],
            expect: guard(1, 1),
        };
        assert!(matches!(
            compile_mutation(&m, true),
            Err(DbError::Unsupported { .. })
        ));
    }

    #[test]
    fn overlapping_paths_are_refused_on_the_script_path_too() {
        let m = Mutation::Update {
            path: path(),
            key: key("events", "abc"),
            sets: vec![
                (fp("a"), Value::I64(5)),
                (fp("a.b"), Value::I64(1)),
                (fp("junk"), Value::Absent),
            ],
            expect: guard(1, 1),
        };
        let err = compile_mutation(&m, true).unwrap_err();
        assert!(matches!(err, DbError::Unsupported { .. }));
        assert!(err.to_string().contains("overlap"), "{err}");

        for sets in [
            vec![(fp("a"), Value::Absent), (fp("a.b"), Value::I64(1))],
            vec![(fp("a.b"), Value::I64(1)), (fp("a"), Value::Absent)],
        ] {
            let m = Mutation::Update {
                path: path(),
                key: key("events", "abc"),
                sets,
                expect: guard(1, 1),
            };
            assert!(
                matches!(compile_mutation(&m, true), Err(DbError::Unsupported { .. })),
                "a set/remove prefix overlap must be refused in either order"
            );
        }

        let m = Mutation::Update {
            path: path(),
            key: key("events", "abc"),
            sets: vec![
                (fp("x"), Value::Str(Arc::from("v"))),
                (fp("x"), Value::Absent),
            ],
            expect: guard(1, 1),
        };
        assert!(matches!(
            compile_mutation(&m, true),
            Err(DbError::Unsupported { .. })
        ));

        let m = Mutation::Update {
            path: path(),
            key: key("events", "abc"),
            sets: vec![(fp("a.b"), Value::I64(1)), (fp("a.c"), Value::Absent)],
            expect: guard(1, 1),
        };
        assert!(compile_mutation(&m, true).is_ok());
    }

    #[test]
    fn an_empty_insert_id_becomes_a_server_generated_post_not_a_broken_put() {
        let m = Mutation::Insert {
            path: path(),
            doc: doc_val(vec![
                ("_id", Value::Str(Arc::from(""))),
                ("status", Value::Str(Arc::from("new"))),
            ]),
        };
        let w = compile_mutation(&m, true).unwrap();
        assert_eq!(w.method, Method::Post);
        assert_eq!(w.path, "/events/_doc");
        assert!(q(&w, "op_type").is_none());
        assert!(w.id.is_empty());
        // An empty `_routing` is likewise dropped, not sent as `routing=`.
        let m = Mutation::Insert {
            path: path(),
            doc: doc_val(vec![
                ("_routing", Value::Str(Arc::from(""))),
                ("status", Value::Str(Arc::from("new"))),
            ]),
        };
        let w = compile_mutation(&m, true).unwrap();
        assert!(w.routing.is_none());
        assert!(q(&w, "routing").is_none());
    }

    #[test]
    fn an_id_with_a_slash_is_percent_encoded_into_the_path() {
        let m = Mutation::Delete {
            path: path(),
            key: key("events", "a/b c"),
            expect: guard(1, 1),
        };
        let w = compile_mutation(&m, true).unwrap();
        assert_eq!(w.path, "/events/_doc/a%2Fb%20c");
        // The report still echoes the raw id, not the encoded one.
        assert_eq!(w.id, "a/b c");
    }

    #[test]
    fn include_source_on_error_support_tracks_product_and_version() {
        assert!(supports_include_source_on_error(
            Product::Elasticsearch,
            "8.18.0"
        ));
        assert!(supports_include_source_on_error(
            Product::Elasticsearch,
            "9.0.0"
        ));
        assert!(!supports_include_source_on_error(
            Product::Elasticsearch,
            "8.17.4"
        ));
        assert!(!supports_include_source_on_error(
            Product::OpenSearch,
            "2.18.0"
        ));
    }

    #[test]
    fn guard_unsupported_reason_flags_time_series_and_disabled_sequence_numbers() {
        let disabled = json!({
            "metrics-000001": {
                "settings": { "index.disable_sequence_numbers": "true" }
            }
        });
        assert!(guard_unsupported_reason(&disabled, "8.10.0")
            .unwrap()
            .contains("sequence numbers disabled"));

        // Time-series mode on ES >= 9.4: refused (sentinel `_seq_no`).
        let tsdb = json!({
            "metrics-000001": { "settings": { "index.mode": "time_series" } }
        });
        assert!(guard_unsupported_reason(&tsdb, "9.4.0")
            .unwrap()
            .contains("time-series"));

        // A TSDB index that has explicitly re-enabled sequence numbers is fine.
        let reenabled = json!({
            "metrics-000001": {
                "settings": {
                    "index.mode": "time_series",
                    "index.disable_sequence_numbers": "false"
                }
            }
        });
        assert!(guard_unsupported_reason(&reenabled, "9.4.0").is_none());

        // A plain index is fine.
        let plain = json!({ "events": { "settings": { "index.mode": "standard" } } });
        assert!(guard_unsupported_reason(&plain, "9.4.0").is_none());
    }

    #[test]
    fn time_series_refusal_is_gated_on_es_9_4_and_up() {
        let tsdb = json!({
            "metrics-000001": { "settings": { "index.mode": "time_series" } }
        });
        assert!(guard_unsupported_reason(&tsdb, "9.3.0").is_none());
        assert!(guard_unsupported_reason(&tsdb, "8.13.0").is_none());
        // 9.4 and up: refused.
        assert!(guard_unsupported_reason(&tsdb, "9.4.0").is_some());
        assert!(guard_unsupported_reason(&tsdb, "10.0.0").is_some());
        // Explicit disable is version-independent even on an old server.
        let disabled = json!({
            "metrics-000001": { "settings": { "index.disable_sequence_numbers": "true" } }
        });
        assert!(guard_unsupported_reason(&disabled, "9.3.0").is_some());
    }

    #[test]
    fn batch_report_names_applied_and_carries_the_new_guard_values() {
        let writes = vec![compile_mutation(
            &Mutation::Update {
                path: path(),
                key: key("events", "abc"),
                sets: vec![(fp("status"), Value::Str(Arc::from("done")))],
                expect: guard(41, 3),
            },
            true,
        )
        .unwrap()];
        let outcomes = vec![WriteOutcome::Applied(json!({
            "result": "updated",
            "_seq_no": 42,
            "_primary_term": 3
        }))];
        let (docs, notices) = batch_report(&writes, outcomes);
        assert_eq!(docs.len(), 1);
        let Value::Document(doc) = &docs[0] else {
            panic!("expected a document row");
        };
        assert_eq!(doc.get("outcome"), Some(&Value::Str(Arc::from("applied"))));
        assert_eq!(doc.get("_seq_no"), Some(&Value::I64(42)));
        assert!(notices
            .iter()
            .any(|n| n.code.as_deref() == Some("es.mutate.applied")));
    }

    #[test]
    fn batch_report_halts_and_marks_the_rest_not_attempted() {
        let mk = |id: &str| {
            compile_mutation(
                &Mutation::Delete {
                    path: path(),
                    key: key("events", id),
                    expect: guard(1, 1),
                },
                true,
            )
            .unwrap()
        };
        let writes = vec![mk("a"), mk("b"), mk("c")];
        // First applied, second conflicts, third never attempted.
        let outcomes = vec![
            WriteOutcome::Applied(json!({ "result": "deleted" })),
            WriteOutcome::Failed(DbError::Conflict {
                code: Some("version_conflict_engine_exception".into()),
                message: "version conflict".into(),
            }),
        ];
        let (docs, notices) = batch_report(&writes, outcomes);
        let outcome = |i: usize| {
            let Value::Document(d) = &docs[i] else {
                panic!("row {i} is not a document");
            };
            match d.get("outcome") {
                Some(Value::Str(s)) => s.to_string(),
                other => panic!("row {i} outcome: {other:?}"),
            }
        };
        assert_eq!(outcome(0), "applied");
        assert_eq!(outcome(1), "failed");
        assert_eq!(outcome(2), "not attempted");
        // The failed row is flagged as a conflict (a UI state, not a toast).
        let Value::Document(failed) = &docs[1] else {
            unreachable!()
        };
        assert_eq!(failed.get("conflict"), Some(&Value::Bool(true)));
        assert!(notices
            .iter()
            .any(|n| n.code.as_deref() == Some("es.mutate.halted")));
    }

    #[test]
    fn batch_report_surfaces_a_forced_refresh_degradation_as_a_notice() {
        let writes = vec![compile_mutation(
            &Mutation::Update {
                path: path(),
                key: key("events", "abc"),
                sets: vec![(fp("status"), Value::Str(Arc::from("done")))],
                expect: guard(1, 1),
            },
            true,
        )
        .unwrap()];
        let outcomes = vec![WriteOutcome::Applied(json!({
            "result": "updated",
            "forced_refresh": true
        }))];
        let (_docs, notices) = batch_report(&writes, outcomes);
        assert!(
            notices
                .iter()
                .any(|n| n.code.as_deref() == Some("es.mutate.forced_refresh")),
            "a wait_for that degraded to an immediate refresh must be surfaced"
        );
    }

    // ---- P1-2: multi-document `_bulk` batching ---------------------------

    fn update(id: &str, field: &str, value: Value, seq: i64, term: i64) -> CompiledWrite {
        compile_mutation(
            &Mutation::Update {
                path: path(),
                key: key("events", id),
                sets: vec![(fp(field), value)],
                expect: guard(seq, term),
            },
            true,
        )
        .unwrap()
    }

    fn delete(id: &str, seq: i64, term: i64) -> CompiledWrite {
        compile_mutation(
            &Mutation::Delete {
                path: path(),
                key: key("events", id),
                expect: guard(seq, term),
            },
            true,
        )
        .unwrap()
    }

    fn line(body: &str, n: usize) -> Json {
        serde_json::from_str(body.lines().nth(n).unwrap())
            .unwrap_or_else(|e| panic!("line {n} of {body:?} is not JSON: {e}"))
    }

    #[test]
    fn bulk_frames_an_update_as_a_guarded_action_line_plus_a_doc_source_line() {
        let w = update("abc", "status", Value::Str(Arc::from("done")), 41, 3);
        let body = compile_bulk_body(&[w], MAX_BULK_BODY_BYTES).unwrap();
        // Two lines (action + source), body newline-terminated.
        assert!(body.ends_with('\n'));
        assert_eq!(body.lines().count(), 2, "{body:?}");
        assert_eq!(
            line(&body, 0),
            json!({"update":{"_index":"events","_id":"abc","if_seq_no":41,"if_primary_term":3}})
        );
        assert!(!body.contains("retry_on_conflict"));
        // The source line is the compiled partial doc, reused verbatim.
        assert_eq!(line(&body, 1), json!({"doc":{"status":"done"}}));
        // Each line is a single compact line of JSON.
        for l in body.lines() {
            assert!(!l.contains('\n'));
        }
    }

    #[test]
    fn bulk_frames_a_delete_as_a_guarded_action_line_with_no_source_line() {
        let body = compile_bulk_body(&[delete("abc", 7, 2)], MAX_BULK_BODY_BYTES).unwrap();
        assert_eq!(
            body.lines().count(),
            1,
            "a delete has no source line: {body:?}"
        );
        assert_eq!(
            line(&body, 0),
            json!({"delete":{"_index":"events","_id":"abc","if_seq_no":7,"if_primary_term":2}})
        );
        assert!(body.ends_with('\n'));
    }

    #[test]
    fn bulk_frames_a_removal_update_with_a_script_source_line_never_a_doc() {
        let w = update("abc", "status", Value::Absent, 41, 3);
        let body = compile_bulk_body(&[w], MAX_BULK_BODY_BYTES).unwrap();
        assert_eq!(body.lines().count(), 2);
        // The guard still rides the action line.
        assert_eq!(
            line(&body, 0)
                .get("update")
                .and_then(|u| u.get("if_seq_no")),
            Some(&json!(41))
        );
        let source = line(&body, 1);
        assert!(
            source.get("doc").is_none(),
            "a removal must not send `doc` — ES would ignore the script otherwise"
        );
        assert_eq!(
            source
                .get("script")
                .and_then(|s| s.get("source"))
                .and_then(Json::as_str),
            Some("ctx._source.remove('status');")
        );
    }

    #[test]
    fn bulk_frames_an_insert_with_an_id_as_create_never_index() {
        let w = compile_mutation(
            &Mutation::Insert {
                path: path(),
                doc: doc_val(vec![
                    ("_id", Value::Str(Arc::from("abc"))),
                    ("_routing", Value::Str(Arc::from("t7"))),
                    ("status", Value::Str(Arc::from("new"))),
                ]),
            },
            true,
        )
        .unwrap();
        let body = compile_bulk_body(&[w], MAX_BULK_BODY_BYTES).unwrap();
        assert_eq!(body.lines().count(), 2);
        let action = line(&body, 0);
        // `create`, not `index` — op_type=create is the guard for an id'd insert.
        assert!(
            action.get("index").is_none(),
            "NEVER a bare `index` with an id — that is the blind overwrite: {action}"
        );
        let create = action.get("create").expect("an id'd insert is a create");
        assert_eq!(create.get("_id"), Some(&json!("abc")));
        assert_eq!(create.get("routing"), Some(&json!("t7")));
        // An insert carries no `if_seq_no` guard: a new doc has no seq_no.
        assert!(create.get("if_seq_no").is_none());
        assert_eq!(line(&body, 1), json!({"status":"new"}));
    }

    #[test]
    fn bulk_frames_an_insert_without_an_id_as_index_with_no_id() {
        let w = compile_mutation(
            &Mutation::Insert {
                path: path(),
                doc: doc_val(vec![("status", Value::Str(Arc::from("new")))]),
            },
            true,
        )
        .unwrap();
        let body = compile_bulk_body(&[w], MAX_BULK_BODY_BYTES).unwrap();
        assert_eq!(body.lines().count(), 2);
        let action = line(&body, 0);
        assert!(action.get("create").is_none());
        let index = action
            .get("index")
            .expect("a no-id insert is an `index` action");
        assert!(
            index.get("_id").is_none(),
            "no id must be sent for a server-generated insert"
        );
        assert_eq!(line(&body, 1), json!({"status":"new"}));
    }

    #[test]
    fn bulk_puts_routing_on_the_action_line() {
        let mut k = key("events", "abc");
        k.push((fp("_routing"), Value::Str(Arc::from("t7"))));
        let w = compile_mutation(
            &Mutation::Delete {
                path: path(),
                key: k,
                expect: guard(7, 2),
            },
            true,
        )
        .unwrap();
        let body = compile_bulk_body(&[w], MAX_BULK_BODY_BYTES).unwrap();
        assert_eq!(
            line(&body, 0).get("delete").and_then(|d| d.get("routing")),
            Some(&json!("t7"))
        );
    }

    #[test]
    fn bulk_body_is_compact_newline_terminated_and_pairs_actions_with_sources() {
        let update = update("a", "x", Value::I64(1), 1, 1);
        let delete = delete("b", 2, 2);
        let insert = compile_mutation(
            &Mutation::Insert {
                path: path(),
                doc: doc_val(vec![
                    ("_id", Value::Str(Arc::from("c"))),
                    ("y", Value::I64(2)),
                ]),
            },
            true,
        )
        .unwrap();
        let body = compile_bulk_body(&[update, delete, insert], MAX_BULK_BODY_BYTES).unwrap();
        // update(2) + delete(1) + insert(2) = 5 lines.
        assert!(body.ends_with('\n'));
        assert_eq!(body.lines().count(), 5, "{body:?}");
        // Every line is valid standalone JSON (NDJSON), never pretty-printed.
        for l in body.lines() {
            let v: Json = serde_json::from_str(l).unwrap_or_else(|e| panic!("{l:?}: {e}"));
            assert!(v.is_object());
            assert!(!l.contains('\n'));
        }
        // No pretty-print spacing, no blank lines.
        assert!(!body.contains(": "));
        assert!(!body.contains("\n\n"));
    }

    #[test]
    fn bulk_never_emits_retry_on_conflict() {
        let body = compile_bulk_body(
            &[update("a", "x", Value::I64(1), 1, 1), delete("b", 2, 2)],
            MAX_BULK_BODY_BYTES,
        )
        .unwrap();
        assert!(
            !body.contains("retry_on_conflict"),
            "retry_on_conflict is the silent clobber the guard prevents: {body}"
        );
    }

    #[test]
    fn bulk_refuses_an_oversize_body_rather_than_send_a_truncated_one() {
        let writes: Vec<CompiledWrite> = (0..50)
            .map(|i| update(&format!("id-{i}"), "x", Value::I64(i), 1, 1))
            .collect();
        // A tiny ceiling forces the refusal deterministically.
        let err = compile_bulk_body(&writes, 64).unwrap_err();
        assert!(matches!(err, DbError::Unsupported { .. }));
        assert!(err.to_string().contains("http.max_content_length"), "{err}");
        // The same batch frames fine under the real ceiling.
        assert!(compile_bulk_body(&writes, MAX_BULK_BODY_BYTES).is_ok());
    }

    fn row(docs: &[Value], i: usize) -> &Document {
        match &docs[i] {
            Value::Document(d) => d,
            other => panic!("row {i} is not a document: {other:?}"),
        }
    }

    #[test]
    fn bulk_report_classifies_mixed_success_conflict_and_error() {
        let writes = vec![
            update("a", "x", Value::I64(1), 1, 1),
            update("b", "x", Value::I64(2), 1, 1),
            delete("c", 1, 1),
        ];
        // HTTP 200 overall, with `errors:true` and per-item statuses.
        let response = json!({
            "errors": true,
            "items": [
                {"update": {"_index":"events","_id":"a","status":200,"result":"updated","_seq_no":42,"_primary_term":3}},
                {"update": {"_index":"events","_id":"b","status":409,"error":{"type":"version_conflict_engine_exception","reason":"[b]: version conflict"}}},
                {"delete": {"_index":"events","_id":"c","status":400,"error":{"type":"illegal_argument_exception","reason":"bad request"}}}
            ]
        });
        let (docs, notices) = bulk_report(&writes, &response).unwrap();
        assert_eq!(docs.len(), 3);

        // Item 0: applied, new guard echoed.
        assert_eq!(
            row(&docs, 0).get("outcome"),
            Some(&Value::Str(Arc::from("applied")))
        );
        assert_eq!(row(&docs, 0).get("_seq_no"), Some(&Value::I64(42)));

        // Item 1: a 409 is a failure AND a conflict, engine type as error_code.
        assert_eq!(
            row(&docs, 1).get("outcome"),
            Some(&Value::Str(Arc::from("failed")))
        );
        assert_eq!(row(&docs, 1).get("conflict"), Some(&Value::Bool(true)));
        assert_eq!(
            row(&docs, 1).get("error_code"),
            Some(&Value::Str(Arc::from("version_conflict_engine_exception")))
        );

        // Item 2: a 400 is a failure but NOT a conflict.
        assert_eq!(
            row(&docs, 2).get("outcome"),
            Some(&Value::Str(Arc::from("failed")))
        );
        assert_eq!(row(&docs, 2).get("conflict"), None);
        assert_eq!(
            row(&docs, 2).get("error_code"),
            Some(&Value::Str(Arc::from("illegal_argument_exception")))
        );

        // No row is "not attempted": bulk executed every item.
        for i in 0..3 {
            assert_ne!(
                row(&docs, i).get("outcome"),
                Some(&Value::Str(Arc::from("not attempted")))
            );
        }
        // The honest partial-bulk summary.
        let partial = notices
            .iter()
            .find(|n| n.code.as_deref() == Some("es.bulk.partial"))
            .expect("a partial bulk must summarise as partial");
        assert!(
            partial.message.contains("not atomic"),
            "{}",
            partial.message
        );
        assert!(
            partial.message.contains("version conflict"),
            "{}",
            partial.message
        );
    }

    #[test]
    fn bulk_report_notes_bulk_is_not_atomic_even_when_every_item_applied() {
        let response = json!({
            "errors": false,
            "items": [{"update":{"_index":"events","_id":"a","status":200,"result":"updated","_seq_no":9,"_primary_term":1}}]
        });
        let (docs, notices) =
            bulk_report(&[update("a", "x", Value::I64(1), 1, 1)], &response).unwrap();
        assert_eq!(
            row(&docs, 0).get("outcome"),
            Some(&Value::Str(Arc::from("applied")))
        );
        let n = notices
            .iter()
            .find(|n| n.code.as_deref() == Some("es.bulk.applied"))
            .expect("an all-success bulk still notes it is not atomic");
        assert!(n.message.contains("not atomic"), "{}", n.message);
    }

    #[test]
    fn bulk_report_echoes_a_server_generated_insert_id() {
        let w = compile_mutation(
            &Mutation::Insert {
                path: path(),
                doc: doc_val(vec![("status", Value::Str(Arc::from("new")))]),
            },
            true,
        )
        .unwrap();
        let response = json!({
            "errors": false,
            "items": [{"index":{"_index":"events","_id":"gen-xyz","status":201,"result":"created","_seq_no":0,"_primary_term":1}}]
        });
        let (docs, _) = bulk_report(&[w], &response).unwrap();
        assert_eq!(
            row(&docs, 0).get("_id"),
            Some(&Value::Str(Arc::from("gen-xyz")))
        );
        assert_eq!(
            row(&docs, 0).get("result"),
            Some(&Value::Str(Arc::from("created")))
        );
    }

    #[test]
    fn bulk_report_surfaces_a_per_item_forced_refresh() {
        let response = json!({
            "errors": false,
            "items": [{"update":{"_index":"events","_id":"a","status":200,"result":"updated","forced_refresh":true}}]
        });
        let (docs, notices) =
            bulk_report(&[update("a", "x", Value::I64(1), 1, 1)], &response).unwrap();
        assert_eq!(
            row(&docs, 0).get("forced_refresh"),
            Some(&Value::Bool(true))
        );
        assert!(notices
            .iter()
            .any(|n| n.code.as_deref() == Some("es.mutate.forced_refresh")));
    }

    #[test]
    fn bulk_report_refuses_an_item_count_mismatch_or_missing_items() {
        // Two items for one submitted action: outcomes cannot be lined up.
        let two = json!({"items":[{"delete":{"status":200}},{"delete":{"status":200}}]});
        assert!(matches!(
            bulk_report(&[delete("a", 1, 1)], &two),
            Err(DbError::Protocol(_))
        ));
        // No `items` array at all.
        assert!(matches!(
            bulk_report(&[delete("a", 1, 1)], &json!({"took": 1})),
            Err(DbError::Protocol(_))
        ));
    }
}
