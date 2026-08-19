//! `datagrep_mutate` — Stage 1 of the ES editing chain: commit one guarded
//! [`MutationBatch`] and hand the report back as a single owned JSON `char*`.
//!
//! Unlike [`crate::query::datagrep_query_run`] this entry point is
//! **synchronous**: a save is a discrete commit the UI waits on, not a stream
//! it scrolls. It resolves the profile, acquires a lease on the profile's pool
//! (so connection pooling and read-only enforcement apply exactly as they do
//! for a query — see [`CoreInner::run_request`]), runs the mutation, and drains
//! the resulting cursor to completion before returning.
//!
//! ## Read-only enforcement
//!
//! A read-only profile takes `set_read_only(true)` on the exact socket the
//! write will run on, the same as `run_request`. The Elasticsearch connection's
//! `read_only_active` guard then refuses the generated write before compiling
//! anything, and that refusal surfaces here as an error string (NULL return),
//! never a silent no-op.
//!
//! ## The report shape
//!
//! The driver returns a `Shape::Documents` cursor: one `Value::Document` per
//! mutation (`op`/`_index`/`_id`/`outcome`/…) plus [`Notice`]s that ride the
//! cursor's batches. This module drains both and reshapes them into one stable,
//! Swift-friendly blob — the rows reuse the very same `Value` → clean-JSON
//! conversion the grid's detail pane uses ([`crate::cells::value_to_json`]), so
//! a mutation-report cell renders identically to any other cell:
//!
//! ```json
//! {
//!   "rows":    [ { "op":"update", "_index":"events", "_id":"abc",
//!                  "outcome":"applied", "result":"updated",
//!                  "_seq_no":42, "_primary_term":3 } ],
//!   "notices": [ {"severity":"warning","code":"es.bulk.partial","message":"…"} ],
//!   "summary": {"applied":1,"failed":0,"not_attempted":0,"conflicts":0}
//! }
//! ```

use std::ffi::c_char;
use std::sync::Arc;

use datagrep_api::driver::{FetchHint, Notice, NoticeSeverity, Payload};
use datagrep_api::request::{MutationBatch, Op, Request};
use datagrep_api::{Enforcement, Value};

use crate::cells::value_to_json;
use crate::core::{core_ref, CoreInner, DatagrepCore};
use crate::ffi_util::{cstr, guard, to_c_string};
use crate::runtime::runtime;

/// Commit a guarded document edit and return the batch report as JSON.
///
/// `mutation_json` is a serde-encoded [`MutationBatch`] — the exact wire form
/// exercised by `datagrep-api`'s
/// `mutation_key_carries_field_names_and_round_trips_through_serde` test:
/// externally-tagged `Mutation` variants, `FieldPath` as `[{"Field":"_id"}]`,
/// `Value` as `{"Str":"x"}`/`{"I64":42}`. For example:
///
/// ```json
/// {"mutations":[
///   {"Update":{
///     "path":["events"],
///     "key":[[[{"Field":"_index"}],{"Str":"events"}],
///            [[{"Field":"_id"}],{"Str":"abc"}]],
///     "sets":[[[{"Field":"status"}],{"Str":"done"}]],
///     "expect":[[[{"Field":"_seq_no"}],{"I64":41}],
///               [[{"Field":"_primary_term"}],{"I64":3}]]}}]}
/// ```
///
/// Blocks until the commit completes. Returns an **owned** JSON `char*` (free it
/// with `datagrep_string_free`), or NULL with `*err_out` set on a parse failure,
/// a read-only refusal, or any other error — the same `err_out` contract every
/// entry point in this crate follows.
///
/// # Safety
/// `core` must come from `datagrep_core_new` and be unfreed; `profile` and
/// `mutation_json` must be NULL or valid NUL-terminated strings that outlive the
/// call; `err_out` must be NULL or point at a writable `char*`.
#[no_mangle]
pub unsafe extern "C" fn datagrep_mutate(
    core: *mut DatagrepCore,
    profile: *const c_char,
    mutation_json: *const c_char,
    err_out: *mut *mut c_char,
) -> *mut c_char {
    guard(err_out, std::ptr::null_mut(), "datagrep_mutate", || {
        // SAFETY: `core` is a live handle from `datagrep_core_new` and the two
        // strings are NULL or NUL-terminated, per this function's contract;
        // `core_ref`/`cstr` turn NULL (and non-UTF-8) into errors rather than
        // dereferencing them.
        let core = unsafe { core_ref(core) }?;
        let profile = unsafe { cstr(profile, "profile") }?;
        let mutation_json = unsafe { cstr(mutation_json, "mutation_json") }?;

        let batch: MutationBatch = serde_json::from_str(mutation_json)
            .map_err(|e| format!("mutation_json is not a valid MutationBatch: {e}"))?;

        let rt = runtime()?;
        // Synchronous by design: a commit blocks the caller until it lands. The
        // runtime is process-global and this is never called from a worker
        // thread, so `block_on` here can never be "block_on from inside the
        // runtime" (see `runtime.rs`).
        let (rows, notices) = rt.block_on(run_mutation(core, profile, batch))?;

        let report = build_report(&rows, &notices);
        serde_json::to_string(&report)
            .map(to_c_string)
            .map_err(|e| format!("could not serialize the mutation report: {e}"))
    })
}

/// Resolve the profile, run the mutation on a leased connection, and drain the
/// report cursor. Mirrors [`CoreInner::run_request`]'s lease/read-only handling,
/// but keeps the cursor rather than streaming it into the result store: a
/// mutation report is inherently small (one document per mutation), so it is
/// drained here in full instead of paged through the feeder.
async fn run_mutation(
    core: &Arc<CoreInner>,
    profile: &str,
    batch: MutationBatch,
) -> Result<(Vec<Value>, Vec<Notice>), String> {
    let (id, saved) = core.open_profile(profile).await?;
    let session = core.api.session(id).map_err(|e| e.to_string())?;
    let lease = session.acquire().await.map_err(|e| e.to_string())?;

    // Same guard `run_request` applies: for a read-only profile, take a
    // read-only session on this exact socket so the connection refuses the
    // write. The driver's honest `Enforcement` is recorded per profile (ES is
    // client-side only), and if the server half cannot be confirmed the badge
    // comes down to `Client` rather than over-promising.
    if saved.read_only {
        match lease.set_read_only(true).await {
            Ok(enforcement) => core.record_enforcement(profile, enforcement),
            Err(_) => core.record_enforcement(profile, Enforcement::Client),
        }
    }

    let mut cursor = lease
        .execute(Request::Op(Op::Mutate(batch)))
        .await
        .map_err(|e| e.to_string())?;

    // Drain the whole report: one `Value::Document` per mutation plus every
    // notice that rode the batches. `next_batch` is pull-only, so this loop is
    // what actually reads the report off the driver.
    let mut rows: Vec<Value> = Vec::new();
    let mut notices: Vec<Notice> = Vec::new();
    while let Some(batch) = cursor
        .next_batch(FetchHint::default())
        .await
        .map_err(|e| e.to_string())?
    {
        if let Payload::Docs(docs) = batch.payload {
            rows.extend(docs);
        }
        notices.extend(batch.notices);
    }
    // Best-effort: the report is fully drained, so releasing any server-side
    // resource early is courtesy, not correctness.
    let _ = cursor.close().await;

    Ok((rows, notices))
}

/// Reshape the drained report into the stable Swift-facing schema. Rows are
/// each a report `Value::Document` run through [`value_to_json`] — the same
/// clean-JSON form `datagrep_rows_cell_detail_json` emits, so a report field
/// renders exactly like any other cell. The summary is recomputed from the
/// rows' own `outcome`/`conflict` fields rather than trusted from the driver.
pub(crate) fn build_report(rows: &[Value], notices: &[Notice]) -> serde_json::Value {
    let row_json: Vec<serde_json::Value> = rows.iter().map(value_to_json).collect();

    let mut applied = 0u64;
    let mut failed = 0u64;
    let mut not_attempted = 0u64;
    let mut conflicts = 0u64;
    for row in &row_json {
        match row.get("outcome").and_then(serde_json::Value::as_str) {
            Some("applied") => applied += 1,
            Some("failed") => failed += 1,
            Some("not attempted") => not_attempted += 1,
            _ => {}
        }
        if row.get("conflict").and_then(serde_json::Value::as_bool) == Some(true) {
            conflicts += 1;
        }
    }

    serde_json::json!({
        "rows": row_json,
        "notices": notices.iter().map(notice_to_json).collect::<Vec<_>>(),
        "summary": {
            "applied": applied,
            "failed": failed,
            "not_attempted": not_attempted,
            "conflicts": conflicts,
        }
    })
}

/// One [`Notice`] as the flat `{severity, code, message}` object the schema
/// promises. `severity` is lowercased so Swift can switch on a stable string;
/// `code` is `null` when the driver attached none.
fn notice_to_json(notice: &Notice) -> serde_json::Value {
    let severity = match notice.severity {
        NoticeSeverity::Info => "info",
        NoticeSeverity::Warning => "warning",
    };
    serde_json::json!({
        "severity": severity,
        "code": notice.code.as_deref(),
        "message": notice.message.as_ref(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::{c_char, CStr, CString};

    use datagrep_api::value::Document;

    fn doc(fields: Vec<(&str, Value)>) -> Value {
        Value::Document(Arc::new(Document::from_fields(
            fields.into_iter().map(|(k, v)| (Arc::from(k), v)).collect(),
        )))
    }

    fn notice(severity: NoticeSeverity, code: &str, message: &str) -> Notice {
        Notice {
            severity,
            code: Some(Arc::from(code)),
            message: Arc::from(message),
        }
    }

    /// The canonical wire form (`datagrep-api`'s round-trip test) parses into a
    /// `MutationBatch` and wraps as the request the driver expects — the parse
    /// half of `datagrep_mutate`, provable without a cluster.
    #[test]
    fn a_canonical_mutation_batch_parses_and_wraps_as_an_op() {
        let json = r#"{
          "mutations":[
            {"Update":{
              "path":["events"],
              "key":[[[{"Field":"_index"}],{"Str":"events"}],
                     [[{"Field":"_id"}],{"Str":"abc"}]],
              "sets":[[[{"Field":"status"}],{"Str":"done"}]],
              "expect":[[[{"Field":"_seq_no"}],{"I64":41}],
                        [[{"Field":"_primary_term"}],{"I64":3}]]}},
            {"Delete":{
              "path":["events"],
              "key":[[[{"Field":"_index"}],{"Str":"events"}],
                     [[{"Field":"_id"}],{"Str":"gone"}]]}}
          ]
        }"#;
        let batch: MutationBatch = serde_json::from_str(json).expect("valid batch");
        assert_eq!(batch.mutations.len(), 2);
        // Wraps into exactly the request `execute_mutate` matches on.
        let req = Request::Op(Op::Mutate(batch));
        assert!(matches!(req, Request::Op(Op::Mutate(_))));
    }

    /// `expect` is `#[serde(default)]`, so an update/delete without a
    /// precondition still parses (the guard is then refused downstream, not
    /// here).
    #[test]
    fn a_mutation_without_expect_still_parses() {
        let json = r#"{"mutations":[
          {"Delete":{"path":["events"],
                     "key":[[[{"Field":"_id"}],{"Str":"x"}]]}}]}"#;
        let batch: MutationBatch = serde_json::from_str(json).expect("valid batch");
        assert_eq!(batch.mutations.len(), 1);
    }

    /// Malformed JSON is a parse error, not a panic.
    #[test]
    fn malformed_json_is_a_clean_parse_error() {
        let err = serde_json::from_str::<MutationBatch>("{not json}")
            .map_err(|e| format!("mutation_json is not a valid MutationBatch: {e}"))
            .expect_err("must fail");
        assert!(err.contains("not a valid MutationBatch"), "{err}");
    }

    /// The report serializer emits the exact schema: clean flat rows, a
    /// per-notice `{severity,code,message}`, and a summary counted from the
    /// rows' own `outcome`/`conflict` — one applied, one failed-conflict, one
    /// not-attempted.
    #[test]
    fn build_report_emits_the_stable_schema_with_counts() {
        let rows = vec![
            doc(vec![
                ("op", Value::Str(Arc::from("update"))),
                ("_index", Value::Str(Arc::from("events"))),
                ("_id", Value::Str(Arc::from("a"))),
                ("outcome", Value::Str(Arc::from("applied"))),
                ("result", Value::Str(Arc::from("updated"))),
                ("_seq_no", Value::I64(42)),
                ("_primary_term", Value::I64(3)),
            ]),
            doc(vec![
                ("op", Value::Str(Arc::from("update"))),
                ("_index", Value::Str(Arc::from("events"))),
                ("_id", Value::Str(Arc::from("b"))),
                ("outcome", Value::Str(Arc::from("failed"))),
                ("conflict", Value::Bool(true)),
                (
                    "error_code",
                    Value::Str(Arc::from("version_conflict_engine_exception")),
                ),
                ("error", Value::Str(Arc::from("current version is newer"))),
            ]),
            doc(vec![
                ("op", Value::Str(Arc::from("delete"))),
                ("_index", Value::Str(Arc::from("events"))),
                ("_id", Value::Str(Arc::from("c"))),
                ("outcome", Value::Str(Arc::from("not attempted"))),
            ]),
        ];
        let notices = vec![notice(
            NoticeSeverity::Warning,
            "es.bulk.partial",
            "applied 1 of 3",
        )];

        let report = build_report(&rows, &notices);

        // Rows are clean flat JSON — a plain string/number per field, not the
        // externally-tagged `Value` form.
        let out_rows = report["rows"].as_array().expect("rows array");
        assert_eq!(out_rows.len(), 3);
        assert_eq!(out_rows[0]["op"], serde_json::json!("update"));
        assert_eq!(out_rows[0]["outcome"], serde_json::json!("applied"));
        assert_eq!(out_rows[0]["_seq_no"], serde_json::json!(42));
        assert_eq!(out_rows[1]["conflict"], serde_json::json!(true));
        assert_eq!(
            out_rows[1]["error_code"],
            serde_json::json!("version_conflict_engine_exception")
        );

        // Notices carry lowercased severity + code + message.
        let out_notices = report["notices"].as_array().expect("notices array");
        assert_eq!(out_notices.len(), 1);
        assert_eq!(out_notices[0]["severity"], serde_json::json!("warning"));
        assert_eq!(out_notices[0]["code"], serde_json::json!("es.bulk.partial"));
        assert_eq!(
            out_notices[0]["message"],
            serde_json::json!("applied 1 of 3")
        );

        // Summary is recomputed from the rows.
        assert_eq!(report["summary"]["applied"], serde_json::json!(1));
        assert_eq!(report["summary"]["failed"], serde_json::json!(1));
        assert_eq!(report["summary"]["not_attempted"], serde_json::json!(1));
        assert_eq!(report["summary"]["conflicts"], serde_json::json!(1));
    }

    /// A notice with no code serializes `code: null` rather than omitting it,
    /// so the Swift shape is stable.
    #[test]
    fn a_notice_without_a_code_serializes_null() {
        let n = Notice {
            severity: NoticeSeverity::Info,
            code: None,
            message: Arc::from("all applied"),
        };
        let json = notice_to_json(&n);
        assert_eq!(json["severity"], serde_json::json!("info"));
        assert_eq!(json["code"], serde_json::Value::Null);
        assert_eq!(json["message"], serde_json::json!("all applied"));
    }

    /// An empty report is still the full schema with zeroed counts — never a
    /// bare `{}` the Swift decoder would choke on.
    #[test]
    fn an_empty_report_is_the_full_schema() {
        let report = build_report(&[], &[]);
        assert!(report["rows"].as_array().unwrap().is_empty());
        assert!(report["notices"].as_array().unwrap().is_empty());
        assert_eq!(report["summary"]["applied"], serde_json::json!(0));
        assert_eq!(report["summary"]["conflicts"], serde_json::json!(0));
    }

    // ---- FFI-boundary error paths (no socket, no live server) ----------

    fn core() -> *mut DatagrepCore {
        let path = CString::new(":memory:").unwrap();
        let mut err: *mut c_char = std::ptr::null_mut();
        let core = unsafe { crate::core::datagrep_core_new(path.as_ptr(), &mut err) };
        assert!(!core.is_null());
        core
    }

    /// A NULL core is an error string, not a crash.
    #[test]
    fn a_null_core_is_an_error() {
        let profile = CString::new("p").unwrap();
        let body = CString::new("{\"mutations\":[]}").unwrap();
        let mut err: *mut c_char = std::ptr::null_mut();
        let out = unsafe {
            datagrep_mutate(
                std::ptr::null_mut(),
                profile.as_ptr(),
                body.as_ptr(),
                &mut err,
            )
        };
        assert!(out.is_null());
        assert!(!err.is_null());
        unsafe { crate::core::datagrep_string_free(err) };
    }

    /// Malformed JSON is refused with `*err_out` set and a NULL return, before
    /// any profile lookup or socket.
    #[test]
    fn malformed_mutation_json_sets_err_and_returns_null() {
        let core = core();
        let profile = CString::new("anything").unwrap();
        let body = CString::new("{not valid json").unwrap();
        let mut err: *mut c_char = std::ptr::null_mut();
        let out = unsafe { datagrep_mutate(core, profile.as_ptr(), body.as_ptr(), &mut err) };
        assert!(out.is_null());
        assert!(!err.is_null());
        let msg = unsafe { CStr::from_ptr(err) }.to_str().unwrap();
        assert!(msg.contains("not a valid MutationBatch"), "{msg}");
        unsafe { crate::core::datagrep_string_free(err) };
        unsafe { crate::core::datagrep_core_free(core) };
    }

    /// Valid JSON against an unknown profile fails at profile resolution — a
    /// clear error, NULL return, no panic (still no socket).
    #[test]
    fn a_valid_batch_on_an_unknown_profile_is_an_error() {
        let core = core();
        let profile = CString::new("does-not-exist").unwrap();
        let body = CString::new("{\"mutations\":[]}").unwrap();
        let mut err: *mut c_char = std::ptr::null_mut();
        let out = unsafe { datagrep_mutate(core, profile.as_ptr(), body.as_ptr(), &mut err) };
        assert!(out.is_null());
        assert!(!err.is_null());
        let msg = unsafe { CStr::from_ptr(err) }.to_str().unwrap();
        assert!(msg.contains("no profile named"), "{msg}");
        unsafe { crate::core::datagrep_string_free(err) };
        unsafe { crate::core::datagrep_core_free(core) };
    }

    /// A NULL `mutation_json` is a checked error, not a deref.
    #[test]
    fn a_null_mutation_json_is_an_error() {
        let core = core();
        let profile = CString::new("p").unwrap();
        let mut err: *mut c_char = std::ptr::null_mut();
        let out = unsafe { datagrep_mutate(core, profile.as_ptr(), std::ptr::null(), &mut err) };
        assert!(out.is_null());
        assert!(!err.is_null());
        unsafe { crate::core::datagrep_string_free(err) };
        unsafe { crate::core::datagrep_core_free(core) };
    }
}
