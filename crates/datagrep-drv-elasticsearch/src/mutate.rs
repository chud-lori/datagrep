//! `Op::Mutate` -> guarded single-document Elasticsearch writes.
//!
//! Everything in this module is pure — compiling a [`Mutation`] into the HTTP
//! request it becomes, and folding per-write outcomes into the batch report —
//! so the whole write path is unit-testable without a cluster. The network
//! side (issuing the requests serially, the TSDB gate) lives in
//! [`crate::connection`].
//!
//! # The model: guarded, serial, halt-and-report
//!
//! Elasticsearch has no multi-document transactions, so a batch cannot be
//! atomic and this driver does not pretend otherwise (`Caps::ATOMIC_BATCH` is
//! off). What the engine *does* have — and SQL does not — is a real
//! per-document compare-and-swap: `if_seq_no`/`if_primary_term`. Every
//! generated write therefore:
//!
//! - **carries the guard, or is refused.** A mutation whose `expect` does not
//!   name `_seq_no` and `_primary_term` is never sent unguarded — the same
//!   rule as "an empty identity is refused, never guessed at".
//! - **is applied one at a time, and the first failure halts the batch.** Not
//!   roll back (impossible), not continue (an unbounded unreviewed partial
//!   write). The report names *applied*, *failed* and *not attempted* per
//!   mutation, as one document each (`Shape::Documents`), plus a summary
//!   `Notice` — so a partial batch is a readable prefix, never a mystery.
//! - **uses `refresh=wait_for`, never `refresh=true`.** A forced refresh per
//!   save is paid three times over by the cluster; `wait_for` is bounded by
//!   the request's own deadline (`EsHttp` always applies a timeout, so an
//!   index with `refresh_interval: -1` cannot hang a write forever), and a
//!   silent degradation to an immediate refresh (`forced_refresh: true` in
//!   the response) is surfaced as a `Notice`.
//!
//! # The TSDB caveat, because a returned `_seq_no` can be a lie
//!
//! Elasticsearch >= 9.4 disables sequence numbers by default for
//! time-series-mode indices: searches return **sentinel** `_seq_no` values
//! and `if_seq_no` writes error out. A guard built from a sentinel protects
//! nothing, so [`guard_unsupported_reason`] classifies an index's settings
//! and the connection refuses the batch up front with the reason, rather
//! than discovering it as a per-document 400. Two further layers keep this
//! from ever silently clobbering: negative/zero guard values are refused at
//! compile time as sentinels, and even if detection is impossible (settings
//! unreadable) the write still carries `if_seq_no`, which such an index
//! rejects rather than applies.

use std::sync::Arc;

use serde_json::{json, Map, Value as Json};

use datagrep_api::driver::{Notice, NoticeSeverity};
use datagrep_api::error::DbError;
use datagrep_api::request::Mutation;
use datagrep_api::shape::ObjectPath;
use datagrep_api::value::{Document, FieldPath, PathSeg, Value};

use crate::http::{version_pair, Method, Product};
use crate::value::value_to_json;

/// Hit-envelope fields that are never writable. Generated writes send back
/// `_source` fields only — echoing envelope metadata into a document is the
/// exact class of bug that has kept Dejavu's update button broken on ES 8
/// since 2022 (`Field [_ignored] is a metadata field…`).
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

/// One compiled write: the exact HTTP request a mutation becomes, plus the
/// identity fields the report echoes back.
#[derive(Debug, Clone, PartialEq)]
pub struct CompiledWrite {
    /// `"update"`, `"delete"` or `"insert"` — for the report.
    pub op: &'static str,
    /// Raw (unencoded) index name, for the per-index TSDB gate.
    pub index: String,
    pub id: String,
    pub routing: Option<String>,
    pub method: Method,
    /// URL path with the index and id percent-encoded.
    pub path: String,
    /// Query parameters, guard included.
    pub query: Vec<(&'static str, String)>,
    /// The request body: a `{"doc": …}` partial merge or a `{"script": …}`
    /// (an update, depending on whether it removes a field), the bare
    /// `_source` object (an insert), or `None` (a delete).
    pub body: Option<Json>,
}

/// Whether generated writes may send `include_source_on_error=false`.
///
/// ES >= 8.18 defaults `include_source_on_error` to **true**, so a
/// parse-failure error body can echo the whole document into an error
/// message; this driver goes to real trouble never to leak user data that
/// way, so the flag is turned off. It cannot be sent unconditionally:
/// older Elasticsearch and OpenSearch reject unrecognized parameters with a
/// 400, which would break every write there.
pub fn supports_include_source_on_error(product: Product, version: &str) -> bool {
    matches!(product, Product::Elasticsearch) && version_pair(version) >= (8, 18)
}

/// Compile one [`Mutation`] into the guarded request it becomes, or refuse
/// with the reason. Refusals here are *pre-flight*: the connection compiles
/// the whole batch before sending anything, so a refused mutation means no
/// document was touched.
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
            // A pure-set update stays a `{"doc": …}` partial merge (cheaper, and
            // it keeps `_update`'s recursive-merge semantics). It escalates to a
            // single `{"script": …}` only when a field removal (`Value::Absent`)
            // is present — because ES ignores `doc` outright when a script is
            // also given, so set + remove must live in one script.
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

/// Compile a [`Mutation::Insert`] into the create request it becomes.
///
/// The insert guard is `op_type=create`, **not** `if_seq_no`: a document that
/// does not exist yet has no sequence number to compare, and `op_type=create`
/// already 409s on an existing id instead of silently overwriting it (the exact
/// blind `PUT` this driver refuses to emit). So an insert carries no `expect`.
///
/// - **user-supplied id** (`_id` in the document envelope) →
///   `PUT /<index>/_doc/<id>?op_type=create`.
/// - **no id** → `POST /<index>/_doc`, and the server generates one.
///
/// The body is the new document's `_source` only: the envelope metadata
/// (`_index`/`_id`/`_routing`/`_seq_no`/…) is stripped, never written back into
/// the document — echoing it is the class of bug that has kept Dejavu's insert
/// path broken on ES 8 since 2022 (`Field [_ignored] is a metadata field…`).
fn compile_insert(
    path: &ObjectPath,
    doc: &Value,
    include_source_on_error: bool,
) -> Result<CompiledWrite, DbError> {
    // The target index is the object path the grid is bound to — an insert has
    // no `key` to carry `_index`, the same way Mongo/SQL inserts take the
    // collection/table from the path.
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
                // An empty id is no id: fall through to a server-generated
                // `POST /<index>/_doc` rather than `PUT /<index>/_doc/`, which
                // 400s server-side mid-batch. Mirrors `identity_from_key`'s
                // empty-id filter.
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
                // An empty routing string is no routing, not a `routing=` with
                // an empty value.
                if s.is_empty() {
                    continue;
                }
                routing = Some(s);
            }
            // Every other envelope field is metadata, never part of the document.
            other if ENVELOPE_FIELDS.contains(&other) => continue,
            _ => {
                if contains_absent(value) {
                    // In a brand-new document an `Absent` field is meaningless —
                    // `value_to_json` would degrade it to a JSON null, silently
                    // inserting a null-valued field rather than omitting it.
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
            // `op_type=create` is the whole guard: it 409s on an existing id
            // instead of overwriting it. NEVER a bare `PUT` (a blind clobber).
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

/// `_index` + `_id` (+ `_routing`) out of a mutation's `key`. Anything else —
/// a missing field, an unknown field, an ambiguous duplicate — is refused:
/// this driver never guesses which document a write is for.
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
                // `_routing` is refused twice regardless of value: a null entry
                // used to `continue` before the ambiguity check, so a key that
                // named `_routing` a second time slipped through.
                if routing_seen {
                    return Err(DbError::Unsupported {
                        feature: "mutation key names `_routing` twice — refusing an ambiguous key"
                            .into(),
                    });
                }
                routing_seen = true;
                // A hit without custom routing legitimately carries no
                // `_routing`; a null/absent value means exactly that.
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

/// A guarded/generated write targets exactly one concrete index, never a
/// wildcard or a comma-list — those would fan a single-document write out
/// across more than one thing.
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

/// A `_id` of `.` or `..` is a legal document id (creatable via `_bulk`) but an
/// unaddressable write target: URL path resolution (WHATWG, which reqwest's
/// `url` applies) normalises the dot-segment away — `/<index>/_doc/..` collapses
/// to `/<index>`, the delete-index endpoint — and percent-encoding does not save
/// it, because that normalisation also folds `%2e`. Refuse it rather than write
/// to the wrong resource.
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

/// The optimistic-concurrency precondition: exactly `_seq_no` +
/// `_primary_term`, both real (non-sentinel) values. Anything else is
/// refused — Elasticsearch has no generic per-field compare-and-swap, and a
/// write without the guard would be a blind clobber.
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
        // Negative sequence numbers (and a primary term of 0) are engine
        // sentinels, not real positions: an index with sequence numbers
        // disabled (a time-series index on ES >= 9.4) returns them from
        // search, and a guard built from one protects nothing.
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
        // `wait_for`, never `true`: see the module doc.
        ("refresh", "wait_for".to_string()),
    ];
    if let Some(routing) = routing {
        // Routing is part of identity: without it the write lands on the
        // wrong shard of a custom-routed index (or 400s).
        query.push(("routing", routing.to_string()));
    }
    query
}

/// `sets` -> an `_update` body: either a `{"doc": …}` partial merge or a
/// single `{"script": …}`.
///
/// A `Value::Absent` in `sets` means "remove this field" (`value.rs`'s
/// Absent -> null degradation would otherwise silently set it to null).
/// Removal can only be expressed with a scripted update, and Elasticsearch is
/// explicit that *"if both `doc` and `script` are specified, then `doc` is
/// ignored"* — so a mutation that both sets and removes fields must compile to
/// **one script**, never `doc` + `script`. A pure-set update therefore stays a
/// cheaper `{"doc": …}` partial merge (which keeps recursive-merge semantics),
/// and only the presence of at least one removal escalates the whole thing to a
/// script.
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
            // A *nested* Absent (inside an object/array set-value) is a
            // different animal: `value_to_json` would degrade it to null one
            // level down, and there is no unambiguous "remove `obj.f` while
            // setting the rest of `obj`" — refuse it rather than approximate.
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

/// The `{"doc": …}` partial document, built as *nested* objects so `_update`'s
/// recursive merge touches only the named leaves.
fn build_partial_doc(assignments: &[(&FieldPath, &Value)]) -> Result<Json, DbError> {
    let mut root = Map::new();
    for (path, value) in assignments {
        let names = set_field_names(path)?;
        insert_nested(&mut root, &names, value_to_json(value), path)?;
    }
    Ok(Json::Object(root))
}

/// Compile the assignments and removals into a **single** scripted update:
/// `{"script": {"lang":"painless", "source": …, "params": …}}`. Injected values
/// ride in `params` — never string-interpolated into the Painless source, which
/// would be both an injection hole and a type-fidelity loss.
fn build_script_body(
    assignments: &[(&FieldPath, &Value)],
    removals: &[&FieldPath],
) -> Result<Json, DbError> {
    let mut lines: Vec<String> = Vec::new();
    let mut params = Map::new();
    // The same overlap discipline as the doc path's `insert_nested`: a leaf that
    // is written twice, AND a path that is a *prefix* of another in either
    // direction (`a` alongside `a.b`), are a caller bug — not a
    // last-write-wins. This spans assignments and removals together, so a set
    // and a remove of overlapping paths is refused rather than silently
    // resolved by emission order (all sets, then all removes).
    let mut seen: Vec<Vec<&str>> = Vec::new();

    for (i, (path, value)) in assignments.iter().enumerate() {
        let names = script_field_names(path)?;
        refuse_path_overlap(&seen, &names, path)?;
        seen.push(names.clone());
        // Create any missing intermediate maps, shallow-to-deep, so the leaf
        // assignment cannot NPE on an absent parent — the script analogue of
        // the partial doc's nested-object building.
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
            // Remove the leaf from its parent, but only once every prefix is
            // confirmed a Map — `&&` short-circuits, so an absent parent is a
            // no-op (the field is already gone) rather than an NPE.
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

/// Refuse `names` if it collides with any path already in `seen`: an exact
/// duplicate, or one being a prefix of the other in either direction (`a`
/// versus `a.b`). Two paths collide exactly when their shared leading segments
/// are equal — siblings (`a.b` versus `a.c`) do not. This is the script-path
/// mirror of `insert_nested`'s scalar-vs-object overlap refusal.
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

/// `ctx._source.a.b.c` for the field-name path `["a","b","c"]`. Only valid on
/// names [`script_field_names`] has already proven safe Painless identifiers.
fn access_expr(names: &[&str]) -> String {
    let mut expr = String::from("ctx._source");
    for name in names {
        expr.push('.');
        expr.push_str(name);
    }
    expr
}

/// The field-name segments of a set path, refusing what neither an `_update`
/// partial doc nor a script can address: an array-element index, the hit
/// envelope, or an empty path.
fn set_field_names(path: &FieldPath) -> Result<Vec<&str>, DbError> {
    let mut names: Vec<&str> = Vec::with_capacity(path.segments().len());
    for seg in path.segments() {
        match seg {
            PathSeg::Field(name) => names.push(name),
            // A partial doc replaces arrays wholesale and Painless has no safe
            // positional address either; pretending to edit one element would
            // silently drop the others.
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

/// [`set_field_names`], plus the extra discipline a scripted update needs: every
/// segment must be a plain identifier, so it is safe both as Painless map
/// member access (`ctx._source.name`) and as a single-quoted string literal
/// (`.remove('name')`). A name that is not — a space, a dot inside the name, a
/// quote — is refused rather than approximated into Painless the driver cannot
/// vouch for (the same discipline as the array-element refusal).
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

/// A plain ASCII identifier: `[A-Za-z_][A-Za-z0-9_]*`. Safe as both Painless
/// member access and a single-quoted literal, and it cannot carry an injection.
fn is_safe_painless_field(name: &str) -> bool {
    let mut chars = name.chars();
    match chars.next() {
        Some(c) if c.is_ascii_alphabetic() || c == '_' => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// Insert `value` at `names` inside `root`, creating intermediate objects,
/// refusing overlap: two sets that address the same leaf, or a leaf through
/// which another set already wrote a scalar, are a caller bug that would
/// otherwise resolve by silent last-write-wins.
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

/// Whether `value` is, or nests, a [`Value::Absent`]. `value_to_json` degrades
/// Absent to a JSON null *recursively*, so a document/array set-value carrying a
/// nested Absent would silently set that leaf to null in the partial document —
/// the same field-removal ambiguity refused at the top level, hidden one level
/// down.
fn contains_absent(value: &Value) -> bool {
    match value {
        Value::Absent => true,
        Value::Array(items) => items.iter().any(contains_absent),
        Value::Document(doc) => doc.iter().any(|(_, v)| contains_absent(v)),
        _ => false,
    }
}

/// From a `GET /<index>/_settings?flat_settings=true` response, the reason a
/// guarded write against that index must be refused — or `None` when the
/// guard is real. An alias/data-stream write target may expand to several
/// concrete indices; if *any* of them cannot honour the guard, the whole
/// target is refused.
///
/// `version` is the server's reported version. The time-series-mode hazard is
/// **ES >= 9.4 only** (plan §3.2.3): that is where TSDB indices default to
/// disabled sequence numbers and return sentinel `_seq_no` values. On 8.7–9.3 a
/// time-series index tracks sequence numbers normally and the
/// `disable_sequence_numbers` setting does not exist, so refusing there would
/// reject legitimate edits and point at an unfollowable escape hatch. An index
/// with sequence numbers *explicitly* disabled is refused on any version.
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

/// What one issued write came back with.
#[derive(Debug)]
pub enum WriteOutcome {
    /// 2xx — the parsed response body.
    Applied(Json),
    /// The error the write failed with. A 409 arrives as
    /// [`DbError::Conflict`] — recoverable, the connection survives.
    Failed(DbError),
}

/// `index/id` for a notice, or `index (server-assigned id)` for an insert
/// whose id the server generates — so a notice never prints a dangling
/// `index/` with an empty id.
fn target_label(write: &CompiledWrite) -> String {
    if write.id.is_empty() {
        format!("{} (server-assigned id)", write.index)
    } else {
        format!("{}/{}", write.index, write.id)
    }
}

/// Fold the compiled writes plus the outcomes gathered before the halt into
/// the batch report: one `Value::Document` per mutation (applied / failed /
/// not attempted) plus the summary and refresh notices.
///
/// `outcomes` is as long as `writes` when everything applied; on a halt it is
/// exactly one longer than the applied prefix, ending in the failure —
/// everything after it was deliberately never sent.
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
        doc.push("op", Value::Str(Arc::from(write.op)));
        doc.push("_index", Value::Str(Arc::from(write.index.as_str())));
        // An insert with no user-supplied id has an empty compile-time id; the
        // server generates one, echoed back from the response below.
        if !write.id.is_empty() {
            doc.push("_id", Value::Str(Arc::from(write.id.as_str())));
        }
        if let Some(routing) = &write.routing {
            doc.push("_routing", Value::Str(Arc::from(routing.as_str())));
        }
        match outcomes.next() {
            Some(WriteOutcome::Applied(response)) => {
                applied += 1;
                doc.push("outcome", Value::Str(Arc::from("applied")));
                // A server-generated insert id is only known now: echo it so the
                // grid can address the new document on a follow-up edit.
                if write.id.is_empty() {
                    if let Some(new_id) = response.get("_id").and_then(Json::as_str) {
                        doc.push("_id", Value::Str(Arc::from(new_id)));
                    }
                }
                if let Some(result) = response.get("result").and_then(Json::as_str) {
                    doc.push("result", Value::Str(Arc::from(result)));
                }
                // The NEW guard values, so a follow-up edit of the same row
                // can be guarded without re-reading.
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
            Some(WriteOutcome::Failed(error)) => {
                failed = Some(i);
                doc.push("outcome", Value::Str(Arc::from("failed")));
                let (code, message, conflict) = match &error {
                    DbError::Conflict { code, message } => (code.clone(), message.clone(), true),
                    DbError::Query { code, message, .. } => (code.clone(), message.clone(), false),
                    other => (None, other.to_string(), false),
                };
                if conflict {
                    // The precondition no longer held: someone else wrote the
                    // document since it was read. A UI state, not a toast.
                    doc.push("conflict", Value::Bool(true));
                }
                if let Some(code) = code {
                    doc.push("error_code", Value::Str(Arc::from(code.as_str())));
                }
                doc.push("error", Value::Str(Arc::from(message.as_str())));
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

/// A single path/id URL segment, percent-encoded. Unlike an index
/// *expression*, an identity segment never keeps `*` or `,` — those would
/// address more than one thing, and a write targets exactly one.
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
        // A hand-built mutation may carry a numeric id; base-10 rendering is
        // canonical and unambiguous, so this is not a guess.
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
        // Empty `expect` — an aggregation row, a fields-only projection, a scan
        // from before the guard was requested.
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
        // A negative `_seq_no` is the sentinel a sequence-numbers-disabled
        // (time-series) index returns; guarding with it protects nothing.
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
        // Was "field removal is refused". A top-level `Value::Absent` now means
        // "remove this field", compiled to a scripted update — and, crucially,
        // to a `{"script": …}` with NO `doc` key (ES ignores `doc` when a
        // script is present, so removal cannot be a `doc` + `script` pair).
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
        // Missing intermediate maps are created shallow-to-deep before the leaf
        // assignment, so it cannot NPE on an absent parent.
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
        // A nested removal guards every prefix is a Map (short-circuiting on an
        // absent parent), then removes the leaf from its parent.
        assert!(
            source.contains("if (ctx._source.d instanceof Map) { ctx._source.d.remove('e'); }"),
            "{source}"
        );
        assert_eq!(script.get("params").unwrap(), &json!({ "p0": 5 }));
    }

    #[test]
    fn a_scripted_update_refuses_a_field_name_it_cannot_express_in_painless() {
        // A removal (or set-alongside-removal) whose field name is not a plain
        // identifier is refused rather than approximated into unsafe Painless.
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
        // An object set-value carrying a nested Absent: `value_to_json` would
        // degrade it to `{"obj":{"f":null}}`, silently setting `f` to null.
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
        // The null entry used to `continue` before the ambiguity check, so a
        // second `_routing` slipped through.
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
        // The body is the `_source` only — the envelope is stripped, never
        // written back into the document.
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
        // A nested Absent would degrade to a JSON null via `value_to_json`,
        // silently inserting a null-valued field.
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
        // A top-level Absent field is refused the same way (an insert is not an
        // update — there is no field to remove).
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
        // A set of a scalar `a` alongside a nested set `a.b` is a prefix
        // overlap the doc path refuses via `insert_nested`; the presence of a
        // removal (`junk`) forces the script path, which must refuse it just
        // the same rather than silently turn `a=5` into `{b:1}`.
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

        // A set and a remove that overlap must be refused regardless of the
        // caller's ordering — the emit-sets-then-removes shape must not silently
        // pick a winner.
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

        // An exact set-then-remove of the same leaf is also refused (it is not a
        // way to "set then clear").
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

        // Siblings under a shared parent do NOT overlap — a genuine multi-field
        // scripted edit still compiles.
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
        // `PUT /events/_doc/` (empty id segment) would 400 server-side mid-batch;
        // an empty `_id` is treated as no id, like `identity_from_key`'s filter.
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
        // Explicitly disabled: refused on any version — even one before the
        // setting nominally existed.
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
        // Before 9.4 a TSDB index tracks sequence numbers normally: editing it
        // is legitimate, and refusing would point at an escape hatch (the
        // `disable_sequence_numbers` setting) that does not exist there.
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
        // The new `_seq_no` is echoed so a follow-up edit can be guarded
        // without re-reading.
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
}
