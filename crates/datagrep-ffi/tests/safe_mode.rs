use std::ffi::{c_char, CStr, CString};
use std::ptr;

use datagrep_ffi::profiles::{datagrep_profiles_add_json, datagrep_profiles_update};
use datagrep_ffi::{
    datagrep_core_free, datagrep_core_new, datagrep_query_free, datagrep_query_rows,
    datagrep_query_run, datagrep_query_status_json, datagrep_rows_cell, datagrep_rows_free,
    datagrep_safety_evaluate_json, datagrep_safety_pending_json, datagrep_safety_satisfy,
    datagrep_string_free, DatagrepCore,
};

unsafe fn take(ptr: *mut c_char) -> String {
    assert!(!ptr.is_null(), "the call returned NULL");
    let text = CStr::from_ptr(ptr).to_string_lossy().into_owned();
    datagrep_string_free(ptr);
    text
}

// A second, silent profile on the same file: the server's own answer, not the engine's bookkeeping.
unsafe fn table_count(core: *mut DatagrepCore, checker: &CStr, table: &str) -> i64 {
    let sql = CString::new(format!(
        "select count(*) from sqlite_master where name = '{table}'"
    ))
    .unwrap();
    let mut err: *mut c_char = ptr::null_mut();
    let q = datagrep_query_run(core, checker.as_ptr(), sql.as_ptr(), &mut err);
    let status = status_when_settled(q);
    assert_eq!(status["state"], "done", "the check query failed: {status}");
    let rows = datagrep_query_rows(q, 0, 1, &mut err);
    let mut len = 0usize;
    let cell = datagrep_rows_cell(rows, 0, 0, &mut len);
    let text = std::str::from_utf8(std::slice::from_raw_parts(cell as *const u8, len))
        .expect("utf-8")
        .to_owned();
    datagrep_rows_free(rows);
    datagrep_query_free(q);
    text.parse().expect("a count")
}

unsafe fn status_when_settled(q: *mut datagrep_ffi::DatagrepQuery) -> serde_json::Value {
    let mut err: *mut c_char = ptr::null_mut();
    for _ in 0..600 {
        let text = take(datagrep_query_status_json(q, &mut err));
        let value: serde_json::Value = serde_json::from_str(&text).expect("status is JSON");
        if value["total_known"] == serde_json::Value::Bool(true) {
            return value;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    panic!("the query never reached a terminal state");
}

// A real SQLite server on the other end: only the file can prove nothing was written.
#[test]
fn a_write_reaches_the_server_only_after_the_ladder_is_cleared() {
    let dir = tempfile::tempdir().expect("tempdir");
    let db = dir.path().join("safe.db");
    let profiles_db = dir.path().join("profiles.db");

    let profiles_path = CString::new(profiles_db.display().to_string()).unwrap();
    let name = CString::new("prod").unwrap();
    let url = CString::new(format!("sqlite://{}", db.display())).unwrap();
    let options = CString::new(r#"{"safety":"auth_writes"}"#).unwrap();
    let mut err: *mut c_char = ptr::null_mut();

    // SAFETY: every pointer below is a live CString or the live core; each returned string is freed.
    unsafe {
        let core: *mut DatagrepCore = datagrep_core_new(profiles_path.as_ptr(), &mut err);
        assert!(!core.is_null());
        assert!(datagrep_profiles_add_json(
            core,
            name.as_ptr(),
            url.as_ptr(),
            options.as_ptr(),
            &mut err
        ));
        let checker = CString::new("checker").unwrap();
        let silent = CString::new(r#"{"safety":"silent"}"#).unwrap();
        assert!(datagrep_profiles_add_json(
            core,
            checker.as_ptr(),
            url.as_ptr(),
            silent.as_ptr(),
            &mut err
        ));

        let read = CString::new("select 1 as one").unwrap();
        let q = datagrep_query_run(core, name.as_ptr(), read.as_ptr(), &mut err);
        let status = status_when_settled(q);
        assert_eq!(status["state"], "done", "a read is exempt at auth_writes");
        assert_eq!(status["safety"], serde_json::Value::Null);
        datagrep_query_free(q);

        let write = CString::new("create table t (a integer)").unwrap();
        let q = datagrep_query_run(core, name.as_ptr(), write.as_ptr(), &mut err);
        let status = status_when_settled(q);
        assert_eq!(status["state"], "failed", "an unasked write ran");
        assert_eq!(status["safety"]["requires"], "authenticate");
        let challenge = status["safety"]["challenge"]
            .as_str()
            .expect("the refusal carries a challenge")
            .to_owned();
        datagrep_query_free(q);

        assert_eq!(
            table_count(core, &checker, "t"),
            0,
            "the DDL reached the server despite the refusal"
        );

        let id = CString::new(challenge).unwrap();
        let ack = CString::new(r#"{"kind":"acknowledged"}"#).unwrap();
        assert!(
            !datagrep_safety_satisfy(core, name.as_ptr(), id.as_ptr(), ack.as_ptr(), &mut err),
            "an acknowledgement cleared an authenticate rung"
        );
        assert!(!err.is_null());
        datagrep_string_free(err);
        err = ptr::null_mut();

        let typed = CString::new(r#"{"kind":"typed_phrase","typed":"prod"}"#).unwrap();
        assert!(
            datagrep_safety_satisfy(core, name.as_ptr(), id.as_ptr(), typed.as_ptr(), &mut err),
            "the connection name must clear the rung"
        );

        let q = datagrep_query_run(core, name.as_ptr(), write.as_ptr(), &mut err);
        let status = status_when_settled(q);
        assert_eq!(status["state"], "done", "the cleared write did not run");
        datagrep_query_free(q);

        assert_eq!(
            table_count(core, &checker, "t"),
            1,
            "the write never landed on the server"
        );

        // The grant was spent on that statement; the same DDL has to be cleared again.
        let again = CString::new("create table t2 (a integer)").unwrap();
        let q = datagrep_query_run(core, name.as_ptr(), again.as_ptr(), &mut err);
        assert_eq!(status_when_settled(q)["state"], "failed");
        datagrep_query_free(q);

        datagrep_core_free(core);
    }
}

#[test]
fn evaluating_a_script_clears_every_statement_it_listed() {
    let dir = tempfile::tempdir().expect("tempdir");
    let db = dir.path().join("script.db");
    let profiles_db = dir.path().join("profiles.db");

    let profiles_path = CString::new(profiles_db.display().to_string()).unwrap();
    let name = CString::new("staging").unwrap();
    let url = CString::new(format!("sqlite://{}", db.display())).unwrap();
    let options = CString::new(r#"{"safety":"warn_writes"}"#).unwrap();
    let mut err: *mut c_char = ptr::null_mut();

    // SAFETY: every pointer below is a live CString or the live core; each returned string is freed.
    unsafe {
        let core: *mut DatagrepCore = datagrep_core_new(profiles_path.as_ptr(), &mut err);
        assert!(datagrep_profiles_add_json(
            core,
            name.as_ptr(),
            url.as_ptr(),
            options.as_ptr(),
            &mut err
        ));

        let sql = CString::new("create table a (x integer); insert into a values (1)").unwrap();
        let decision = take(datagrep_safety_evaluate_json(
            core,
            name.as_ptr(),
            sql.as_ptr(),
            &mut err,
        ));
        let decision: serde_json::Value = serde_json::from_str(&decision).unwrap();
        assert_eq!(decision["requires"], "warn");
        assert_eq!(decision["statements"].as_array().unwrap().len(), 2);
        let challenge = decision["challenge"].as_str().unwrap().to_owned();

        let pending = take(datagrep_safety_pending_json(core, name.as_ptr(), &mut err));
        assert!(pending.contains(&challenge), "pending was {pending}");

        let id = CString::new(challenge).unwrap();
        let ack = CString::new(r#"{"kind":"acknowledged"}"#).unwrap();
        assert!(datagrep_safety_satisfy(
            core,
            name.as_ptr(),
            id.as_ptr(),
            ack.as_ptr(),
            &mut err
        ));

        let q = datagrep_query_run(core, name.as_ptr(), sql.as_ptr(), &mut err);
        let status = status_when_settled(q);
        assert_eq!(status["state"], "done", "one warning covered the script");
        datagrep_query_free(q);

        // Lowering the rung is the only way off the ladder, and it is a persisted per-connection edit.
        let patch = CString::new(r#"{"safety":"silent"}"#).unwrap();
        assert!(datagrep_profiles_update(
            core,
            name.as_ptr(),
            patch.as_ptr(),
            &mut err
        ));
        let drop_it = CString::new("drop table a").unwrap();
        let q = datagrep_query_run(core, name.as_ptr(), drop_it.as_ptr(), &mut err);
        assert_eq!(status_when_settled(q)["state"], "done");
        datagrep_query_free(q);

        datagrep_core_free(core);
    }
}
