use std::ffi::c_char;
use std::sync::Arc;

use datagrep_api::driver::{FetchHint, Notice, NoticeSeverity, Payload};
use datagrep_api::request::{MutationBatch, Op, Request};
use datagrep_api::Value;

use crate::cells::value_to_json;
use crate::core::{core_ref, CoreInner, DatagrepCore};
use crate::ffi_util::{cstr, guard, to_c_string};
use crate::runtime::runtime;

/// # Safety
/// `core` is a live handle from `datagrep_core_new`; string arguments are NULL or NUL-terminated; `err_out` is NULL or a writable slot.
#[no_mangle]
pub unsafe extern "C" fn datagrep_mutate(
    core: *mut DatagrepCore,
    profile: *const c_char,
    mutation_json: *const c_char,
    err_out: *mut *mut c_char,
) -> *mut c_char {
    guard(err_out, std::ptr::null_mut(), "datagrep_mutate", || {
        // SAFETY: live core handle and strings NULL or NUL-terminated per the contract; core_ref/cstr error before any deref.
        let core = unsafe { core_ref(core) }?;
        let profile = unsafe { cstr(profile, "profile") }?;
        let mutation_json = unsafe { cstr(mutation_json, "mutation_json") }?;

        let batch: MutationBatch = serde_json::from_str(mutation_json)
            .map_err(|e| format!("mutation_json is not a valid MutationBatch: {e}"))?;

        let rt = runtime()?;
        let (rows, notices) = rt.block_on(run_mutation(core, profile, batch))?;

        let report = build_report(&rows, &notices);
        serde_json::to_string(&report)
            .map(to_c_string)
            .map_err(|e| format!("could not serialize the mutation report: {e}"))
    })
}

async fn run_mutation(
    core: &Arc<CoreInner>,
    profile: &str,
    batch: MutationBatch,
) -> Result<(Vec<Value>, Vec<Notice>), String> {
    let (lease, _) = core.leased(profile).await?;

    let mut cursor = lease
        .execute(Request::Op(Op::Mutate(batch)))
        .await
        .map_err(|e| e.to_string())?;

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
    let _ = cursor.close().await;

    Ok((rows, notices))
}

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

    #[test]
    fn the_json_the_macos_grid_sends_parses_and_compiles_to_a_guarded_write() {
        let json = r#"{"mutations":[{"Update":{"expect":[[[{"Field":"_seq_no"}],{"I64":41}],[[{"Field":"_primary_term"}],{"I64":3}]],"key":[[[{"Field":"_index"}],{"Str":"events"}],[[{"Field":"_id"}],{"Str":"abc"}],[[{"Field":"_routing"}],{"Str":"tenant-7"}]],"sets":[[[{"Field":"status"}],{"Str":"done"}],[[{"Field":"retries"}],{"I64":2}],[[{"Field":"score"}],{"F64":1.5}],[[{"Field":"archived"}],{"Bool":true}]],"path":[]}},{"Delete":{"expect":[[[{"Field":"_seq_no"}],{"I64":7}],[[{"Field":"_primary_term"}],{"I64":1}]],"path":[],"key":[[[{"Field":"_index"}],{"Str":"events"}],[[{"Field":"_id"}],{"Str":"gone"}]]}}]}"#;

        let batch: MutationBatch = serde_json::from_str(json).expect("the grid's batch must parse");
        assert_eq!(batch.mutations.len(), 2);

        let update =
            datagrep_drv_elasticsearch::mutate::compile_mutation(&batch.mutations[0], true)
                .expect("the grid's update must compile");
        assert_eq!(update.op, "update");
        assert_eq!(update.path, "/events/_update/abc");
        assert_eq!(update.routing.as_deref(), Some("tenant-7"));
        // The compare-and-swap the whole design turns on.
        assert!(update
            .query
            .iter()
            .any(|(k, v)| *k == "if_seq_no" && v == "41"));
        assert!(update
            .query
            .iter()
            .any(|(k, v)| *k == "if_primary_term" && v == "3"));
        let body = update.body.as_ref().expect("an update has a body");
        assert_eq!(body["doc"]["status"], serde_json::json!("done"));
        assert_eq!(body["doc"]["retries"], serde_json::json!(2));
        assert_eq!(body["doc"]["score"], serde_json::json!(1.5));
        assert_eq!(body["doc"]["archived"], serde_json::json!(true));

        let delete =
            datagrep_drv_elasticsearch::mutate::compile_mutation(&batch.mutations[1], true)
                .expect("the grid's delete must compile");
        assert_eq!(delete.op, "delete");
        assert_eq!(delete.path, "/events/_doc/gone");
        assert!(delete.body.is_none());
        assert!(delete
            .query
            .iter()
            .any(|(k, v)| *k == "if_seq_no" && v == "7"));
    }

    #[test]
    fn a_mutation_without_expect_still_parses() {
        let json = r#"{"mutations":[
          {"Delete":{"path":["events"],
                     "key":[[[{"Field":"_id"}],{"Str":"x"}]]}}]}"#;
        let batch: MutationBatch = serde_json::from_str(json).expect("valid batch");
        assert_eq!(batch.mutations.len(), 1);
    }

    #[test]
    fn malformed_json_is_a_clean_parse_error() {
        let err = serde_json::from_str::<MutationBatch>("{not json}")
            .map_err(|e| format!("mutation_json is not a valid MutationBatch: {e}"))
            .expect_err("must fail");
        assert!(err.contains("not a valid MutationBatch"), "{err}");
    }

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
