use std::ffi::{c_char, CStr, CString};
use std::path::Path;
use std::ptr;

use datagrep_ffi::profiles::datagrep_profiles_add_json;
use datagrep_ffi::{
    datagrep_core_free, datagrep_core_new, datagrep_export_cancel, datagrep_export_free,
    datagrep_export_start, datagrep_export_status_json, datagrep_query_free, datagrep_query_run,
    datagrep_query_status_json, datagrep_safety_satisfy, datagrep_string_free, DatagrepCore,
    DatagrepExport,
};
use serde_json::Value;

unsafe fn take(ptr: *mut c_char) -> String {
    assert!(!ptr.is_null(), "the call returned NULL");
    let text = CStr::from_ptr(ptr).to_string_lossy().into_owned();
    datagrep_string_free(ptr);
    text
}

fn c(s: &str) -> CString {
    CString::new(s).unwrap()
}

unsafe fn export_status(e: *mut DatagrepExport) -> Value {
    let mut err: *mut c_char = ptr::null_mut();
    serde_json::from_str(&take(datagrep_export_status_json(e, &mut err))).expect("status is JSON")
}

unsafe fn settled(e: *mut DatagrepExport) -> Value {
    for _ in 0..1_000 {
        let status = export_status(e);
        if status["state"] != "running" {
            return status;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    panic!("the export never finished");
}

unsafe fn export(
    core: *mut DatagrepCore,
    profile: &str,
    sql: &str,
    format: &str,
    table: Option<&str>,
    path: &Path,
) -> Value {
    let (profile, sql, format) = (c(profile), c(sql), c(format));
    let table = table.map(c);
    let path = c(&path.display().to_string());
    let mut err: *mut c_char = ptr::null_mut();
    let e = datagrep_export_start(
        core,
        profile.as_ptr(),
        sql.as_ptr(),
        format.as_ptr(),
        table.as_ref().map_or(ptr::null(), |t| t.as_ptr()),
        path.as_ptr(),
        &mut err,
    );
    assert!(!e.is_null(), "export refused to start: {}", take(err));
    let status = settled(e);
    datagrep_export_free(e);
    status
}

unsafe fn run(core: *mut DatagrepCore, profile: &str, sql: &str) {
    let (profile, sql) = (c(profile), c(sql));
    let mut err: *mut c_char = ptr::null_mut();
    let q = datagrep_query_run(core, profile.as_ptr(), sql.as_ptr(), &mut err);
    for _ in 0..600 {
        let status: Value =
            serde_json::from_str(&take(datagrep_query_status_json(q, &mut err))).unwrap();
        if status["total_known"] == Value::Bool(true) {
            assert_eq!(status["state"], "done", "setup failed: {status}");
            datagrep_query_free(q);
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    panic!("setup never finished");
}

struct Fixture {
    _dir: tempfile::TempDir,
    root: std::path::PathBuf,
    core: *mut DatagrepCore,
}

impl Fixture {
    // "local" is silent, "guarded" asks before every statement; both point at one SQLite file.
    unsafe fn new() -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path().to_path_buf();
        let url = c(&format!("sqlite://{}", root.join("data.db").display()));
        let mut err: *mut c_char = ptr::null_mut();
        let core = datagrep_core_new(
            c(&root.join("p.db").display().to_string()).as_ptr(),
            &mut err,
        );
        assert!(!core.is_null());
        for (name, level) in [("local", "silent"), ("guarded", "auth_all")] {
            let options = c(&format!(r#"{{"safety":"{level}"}}"#));
            assert!(datagrep_profiles_add_json(
                core,
                c(name).as_ptr(),
                url.as_ptr(),
                options.as_ptr(),
                &mut err
            ));
        }
        run(core, "local", "create table people (id integer, name text)");
        run(
            core,
            "local",
            "insert into people values (1, 'ada'), (2, 'it''s | two'), (3, null)",
        );
        Fixture {
            _dir: dir,
            root,
            core,
        }
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        // SAFETY: the core is live and every export/query on it has been freed.
        unsafe { datagrep_core_free(self.core) };
    }
}

#[test]
fn every_format_writes_the_full_result_to_the_file() {
    unsafe {
        let fx = Fixture::new();
        let sql = "select id, name from people order by id";

        let csv = fx.root.join("out.csv");
        let status = export(fx.core, "local", sql, "csv", None, &csv);
        assert_eq!(status["state"], "done", "{status}");
        assert_eq!(status["rows_written"], 3);
        assert_eq!(
            std::fs::read_to_string(&csv).unwrap(),
            "id,name\r\n1,ada\r\n2,it's | two\r\n3,\r\n"
        );

        let json = fx.root.join("out.json");
        export(fx.core, "local", sql, "json", None, &json);
        let parsed: Value = serde_json::from_str(&std::fs::read_to_string(&json).unwrap()).unwrap();
        assert_eq!(parsed[1]["name"], "it's | two");
        assert_eq!(parsed[2]["name"], Value::Null);

        let md = fx.root.join("out.md");
        export(fx.core, "local", sql, "markdown", None, &md);
        let text = std::fs::read_to_string(&md).unwrap();
        assert!(text.starts_with("| id | name |\n| --- | --- |\n"), "{text}");
        assert!(text.contains(r"| 2 | it's \| two |"), "{text}");

        let inserts = fx.root.join("out.sql");
        export(fx.core, "local", sql, "sql", Some("people_copy"), &inserts);
        let text = std::fs::read_to_string(&inserts).unwrap();
        assert!(
            text.contains(r#"INSERT INTO "people_copy" ("id", "name") VALUES (2, 'it''s | two');"#),
            "{text}"
        );
        assert!(!fx.root.join("out.sql.part").exists());

        // The INSERTs are real SQL for this engine: replaying them recreates the rows.
        run(
            fx.core,
            "local",
            "create table people_copy (id integer, name text)",
        );
        run(fx.core, "local", &text);
        let roundtrip = fx.root.join("copy.csv");
        export(
            fx.core,
            "local",
            "select id, name from people_copy order by id",
            "csv",
            None,
            &roundtrip,
        );
        assert_eq!(
            std::fs::read_to_string(&roundtrip).unwrap(),
            std::fs::read_to_string(&csv).unwrap()
        );
    }
}

#[test]
fn the_safety_ladder_gates_an_export_like_any_run() {
    unsafe {
        let fx = Fixture::new();
        let out = fx.root.join("gated.csv");
        let sql = "select id from people";

        let status = export(fx.core, "guarded", sql, "csv", None, &out);
        assert_eq!(status["state"], "failed", "an unasked export ran: {status}");
        assert_eq!(status["safety"]["requires"], "authenticate");
        assert!(!out.exists(), "a refused export still wrote a file");

        let challenge = c(status["safety"]["challenge"].as_str().expect("a challenge"));
        let typed = c(r#"{"kind":"typed_phrase","typed":"guarded"}"#);
        let mut err: *mut c_char = ptr::null_mut();
        assert!(datagrep_safety_satisfy(
            fx.core,
            c("guarded").as_ptr(),
            challenge.as_ptr(),
            typed.as_ptr(),
            &mut err
        ));

        let status = export(fx.core, "guarded", sql, "csv", None, &out);
        assert_eq!(
            status["state"], "done",
            "the cleared export did not run: {status}"
        );
        assert_eq!(status["rows_written"], 3);
    }
}

#[test]
fn an_export_never_re_runs_a_statement_that_changes_data() {
    unsafe {
        let fx = Fixture::new();
        let out = fx.root.join("write.csv");
        let status = export(fx.core, "local", "delete from people", "csv", None, &out);
        assert_eq!(status["state"], "failed");
        assert!(!out.exists());

        let check = fx.root.join("check.csv");
        export(
            fx.core,
            "local",
            "select count(*) as n from people",
            "csv",
            None,
            &check,
        );
        assert_eq!(std::fs::read_to_string(&check).unwrap(), "n\r\n3\r\n");
    }
}

#[test]
fn sql_insert_needs_a_table_and_an_sql_engine() {
    unsafe {
        let fx = Fixture::new();
        let (profile, sql, format) = (c("local"), c("select 1"), c("sql"));
        let path = c(&fx.root.join("x.sql").display().to_string());
        let mut err: *mut c_char = ptr::null_mut();
        let e = datagrep_export_start(
            fx.core,
            profile.as_ptr(),
            sql.as_ptr(),
            format.as_ptr(),
            ptr::null(),
            path.as_ptr(),
            &mut err,
        );
        assert!(e.is_null());
        assert!(take(err).contains("table name"));
    }
}

#[test]
fn a_cancelled_export_leaves_no_file_behind() {
    unsafe {
        let fx = Fixture::new();
        let out = fx.root.join("huge.csv");
        let sql =
            c("with recursive n(x) as (select 1 union all select x + 1 from n) select x from n");
        let (profile, format) = (c("local"), c("csv"));
        let path = c(&out.display().to_string());
        let mut err: *mut c_char = ptr::null_mut();
        let e = datagrep_export_start(
            fx.core,
            profile.as_ptr(),
            sql.as_ptr(),
            format.as_ptr(),
            ptr::null(),
            path.as_ptr(),
            &mut err,
        );
        assert!(!e.is_null());
        for _ in 0..1_000 {
            if export_status(e)["rows_written"].as_u64().unwrap_or(0) > 0 {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        datagrep_export_cancel(e);
        let status = settled(e);
        assert_eq!(status["state"], "cancelled", "{status}");
        datagrep_export_free(e);
        assert!(!out.exists());
        assert!(!fx.root.join("huge.csv.part").exists());
    }
}
