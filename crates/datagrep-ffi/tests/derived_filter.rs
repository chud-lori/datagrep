use std::ffi::{c_char, CStr, CString};
use std::ptr;

use datagrep_ffi::profiles::datagrep_profiles_add_json;
use datagrep_ffi::{
    datagrep_core_free, datagrep_core_new, datagrep_derive_statement, datagrep_query_free,
    datagrep_query_run, datagrep_query_status_json, datagrep_string_free, DatagrepCore,
};
use serde_json::{json, Value};

unsafe fn take(ptr: *mut c_char) -> String {
    assert!(!ptr.is_null(), "the call returned NULL");
    let text = CStr::from_ptr(ptr).to_string_lossy().into_owned();
    datagrep_string_free(ptr);
    text
}

fn c(s: &str) -> CString {
    CString::new(s).unwrap()
}

unsafe fn derive(statement: &str, spec: &Value) -> String {
    let (driver, statement, spec) = (c("sqlite"), c(statement), c(&spec.to_string()));
    let mut err: *mut c_char = ptr::null_mut();
    let out =
        datagrep_derive_statement(driver.as_ptr(), statement.as_ptr(), spec.as_ptr(), &mut err);
    assert!(err.is_null(), "derive failed: {}", take(err));
    take(out)
}

unsafe fn settle(core: *mut DatagrepCore, profile: &str, sql: &str) -> Value {
    let (profile, sql) = (c(profile), c(sql));
    let mut err: *mut c_char = ptr::null_mut();
    let q = datagrep_query_run(core, profile.as_ptr(), sql.as_ptr(), &mut err);
    assert!(!q.is_null(), "run refused: {}", take(err));
    for _ in 0..600 {
        let status: Value =
            serde_json::from_str(&take(datagrep_query_status_json(q, &mut err))).unwrap();
        if status["total_known"] == Value::Bool(true) || status["state"] == "failed" {
            datagrep_query_free(q);
            return status;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    panic!("the query never finished");
}

unsafe fn rows(core: *mut DatagrepCore, profile: &str, sql: &str) -> u64 {
    let status = settle(core, profile, sql);
    assert_eq!(status["state"], "done", "{sql}\n{status}");
    status["rows_loaded"].as_u64().unwrap()
}

struct Fixture {
    _dir: tempfile::TempDir,
    core: *mut DatagrepCore,
}

impl Fixture {
    unsafe fn new() -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        let url = c(&format!(
            "sqlite://{}",
            dir.path().join("data.db").display()
        ));
        let mut err: *mut c_char = ptr::null_mut();
        let core = datagrep_core_new(
            c(&dir.path().join("p.db").display().to_string()).as_ptr(),
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
        settle(core, "local", "create table people (id integer, name text)");
        settle(
            core,
            "local",
            r"insert into people values (1, 'ada'), (2, 'it''s 50% off'), (3, null), (4, 'C:\x'), (5, '')",
        );
        Fixture { _dir: dir, core }
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        // SAFETY: the core is live and every query on it has been freed.
        unsafe { datagrep_core_free(self.core) };
    }
}

fn filter(column: &str, op: &str, value: &str) -> Value {
    json!({"filters": [{"column": column, "op": op, "value": value}]})
}

#[test]
fn derived_filters_select_the_rows_they_name_on_a_real_engine() {
    unsafe {
        let fx = Fixture::new();
        let base = "-- @limit 500\nSELECT * FROM people;";
        let count = |op: &str, value: &str| {
            rows(fx.core, "local", &derive(base, &filter("name", op, value)))
        };

        assert_eq!(count("eq", "it's 50% off"), 1);
        assert_eq!(count("eq", r"C:\x"), 1);
        assert_eq!(count("contains", "%"), 1);
        assert_eq!(count("contains", "_"), 0);
        assert_eq!(count("starts_with", "AD"), 1);
        assert_eq!(count("is_null", ""), 1);
        assert_eq!(count("is_not_null", ""), 4);
        assert_eq!(count("is_empty", ""), 2);
        assert_eq!(
            rows(fx.core, "local", &derive(base, &filter("id", "ge", "4"))),
            2
        );
    }
}

#[test]
fn injection_attempts_match_nothing_and_leave_the_table_standing() {
    unsafe {
        let fx = Fixture::new();
        let base = "SELECT * FROM people";
        for hostile in [
            "'; DROP TABLE people; --",
            "' OR '1'='1",
            r"\'; DROP TABLE people; --",
            "x') OR ('1'='1",
        ] {
            for op in ["eq", "ne", "contains"] {
                let sql = derive(base, &filter("name", op, hostile));
                let status = settle(fx.core, "local", &sql);
                assert_eq!(status["state"], "done", "{sql}\n{status}");
                // Every non-NULL name differs from the hostile text; none contains it.
                let expected = if op == "ne" { 4 } else { 0 };
                assert_eq!(status["rows_loaded"], expected, "{sql}");
            }
        }
        let column = derive(
            base,
            &filter("name\" IS NULL; DROP TABLE people; --", "is_null", ""),
        );
        let status = settle(fx.core, "local", &column);
        // SQLite reads an unknown "quoted" name as a string, so this is a constant false, not an error.
        assert_eq!(status["rows_loaded"], 0, "{column}\n{status}");
        assert_eq!(rows(fx.core, "local", "SELECT * FROM people"), 5);
    }
}

#[test]
fn a_derived_statement_still_passes_through_the_safety_gate() {
    unsafe {
        let fx = Fixture::new();
        let sql = derive("SELECT * FROM people", &filter("id", "eq", "1"));
        let status = settle(fx.core, "guarded", &sql);
        assert_eq!(status["safety"]["requires"], "authenticate", "{status}");
        assert_eq!(status["rows_loaded"], 0, "{status}");
    }
}
