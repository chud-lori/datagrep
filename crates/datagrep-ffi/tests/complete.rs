use std::ffi::{c_char, CStr, CString};
use std::ptr;

use datagrep_ffi::{
    datagrep_complete_forget, datagrep_complete_json, datagrep_core_free, datagrep_core_new,
    datagrep_profiles_add, datagrep_query_free, datagrep_query_run, datagrep_query_status_json,
    datagrep_sql_format, datagrep_string_free, DatagrepCore,
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

unsafe fn run(core: *mut DatagrepCore, sql: &str) {
    let sql = c(sql);
    let mut err: *mut c_char = ptr::null_mut();
    let q = datagrep_query_run(core, c("local").as_ptr(), sql.as_ptr(), &mut err);
    assert!(!q.is_null(), "run refused: {}", take(err));
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

// `|` marks the caret.
unsafe fn complete(core: *mut DatagrepCore, marked: &str) -> (String, Vec<(String, String)>) {
    let caret = marked.find('|').unwrap();
    let text = c(&marked.replace('|', ""));
    let mut err: *mut c_char = ptr::null_mut();
    let out = datagrep_complete_json(core, c("local").as_ptr(), text.as_ptr(), caret, &mut err);
    assert!(!out.is_null(), "complete failed: {}", take(err));
    let v: Value = serde_json::from_str(&take(out)).unwrap();
    assert_eq!(v["error"], Value::Null, "{v}");
    let items = v["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| {
            (
                i["insert"].as_str().unwrap().to_string(),
                i["kind"].as_str().unwrap().to_string(),
            )
        })
        .collect();
    (v["prefix"].as_str().unwrap().to_string(), items)
}

fn has(items: &[(String, String)], insert: &str, kind: &str) -> bool {
    items.iter().any(|(i, k)| i == insert && k == kind)
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
        assert!(datagrep_profiles_add(
            core,
            c("local").as_ptr(),
            url.as_ptr(),
            &mut err
        ));
        run(core, "create table people (id integer, full_name text)");
        run(core, "create view adults as select * from people");
        Fixture { _dir: dir, core }
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        // SAFETY: the core is live and every query on it has been freed.
        unsafe { datagrep_core_free(self.core) };
    }
}

#[test]
fn tables_and_views_complete_after_from() {
    unsafe {
        let fx = Fixture::new();
        let (prefix, items) = complete(fx.core, "select * from pe|");
        assert_eq!(prefix, "pe");
        assert!(has(&items, "people", "table"), "{items:?}");
        let (_, items) = complete(fx.core, "select * from ad|");
        assert!(has(&items, "adults", "view"), "{items:?}");
    }
}

#[test]
fn columns_come_from_the_tables_the_statement_names() {
    unsafe {
        let fx = Fixture::new();
        let (prefix, items) = complete(fx.core, "select p.na| from people p");
        assert_eq!(prefix, "na");
        assert_eq!(items, [("full_name".to_string(), "column".to_string())]);
        let (_, items) = complete(fx.core, "select | from people");
        assert!(has(&items, "id", "column"), "{items:?}");
    }
}

#[test]
fn a_ddl_statement_refreshes_the_cached_names() {
    unsafe {
        let fx = Fixture::new();
        assert!(!has(
            &complete(fx.core, "select * from or|").1,
            "orders",
            "table"
        ));
        run(fx.core, "create table orders (id integer)");
        assert!(has(
            &complete(fx.core, "select * from or|").1,
            "orders",
            "table"
        ));
    }
}

#[test]
fn forgetting_reloads_and_unknown_tables_are_never_described() {
    unsafe {
        let fx = Fixture::new();
        datagrep_complete_forget(fx.core, c("local").as_ptr());
        datagrep_complete_forget(fx.core, ptr::null());
        let (_, items) = complete(fx.core, "select x.| from \"people; drop table people\" x");
        assert!(items.is_empty(), "{items:?}");
        assert!(has(
            &complete(fx.core, "select * from pe|").1,
            "people",
            "table"
        ));
    }
}

#[test]
fn nothing_is_offered_inside_a_string() {
    unsafe {
        let fx = Fixture::new();
        let (prefix, items) = complete(fx.core, "select 'pe|");
        assert_eq!(prefix, "");
        assert!(items.is_empty());
    }
}

#[test]
fn format_rewrites_sql_and_refuses_other_languages() {
    unsafe {
        let mut err: *mut c_char = ptr::null_mut();
        let out = datagrep_sql_format(
            c("sqlite").as_ptr(),
            c("select a, b from t where x = 'a  b'").as_ptr(),
            &mut err,
        );
        assert_eq!(take(out), "SELECT a,\n       b\nFROM t\nWHERE x = 'a  b'");
        let out = datagrep_sql_format(c("redis").as_ptr(), c("GET k").as_ptr(), &mut err);
        assert!(out.is_null());
        assert!(take(err).contains("not SQL"));
    }
}
