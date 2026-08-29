use std::ffi::{c_char, CStr, CString};
use std::ptr;

use datagrep_ffi::profiles::{
    datagrep_connection_info_json, datagrep_profiles_add_json, datagrep_profiles_get_json,
    datagrep_profiles_update,
};
use datagrep_ffi::{
    datagrep_browse_statement, datagrep_catalog_children_json, datagrep_catalog_describe_json,
    datagrep_core_free, datagrep_core_new, datagrep_profiles_add, datagrep_profiles_list_json,
    datagrep_profiles_remove, datagrep_query_cancel, datagrep_query_free,
    datagrep_query_on_progress, datagrep_query_rows, datagrep_query_run,
    datagrep_query_status_json, datagrep_rows_cell, datagrep_rows_cell_detail_json,
    datagrep_rows_cell_kind, datagrep_rows_column_names_json, datagrep_rows_columns,
    datagrep_rows_count, datagrep_rows_envelope_json, datagrep_rows_free, datagrep_rows_pending,
    datagrep_string_free, DatagrepCore,
};

// ---- helpers -----------------------------------------------------------

fn core() -> *mut DatagrepCore {
    let path = CString::new(":memory:").expect("no interior NUL");
    let mut err: *mut c_char = ptr::null_mut();
    // SAFETY: a valid NUL-terminated path and a writable `err_out` slot.
    let core = unsafe { datagrep_core_new(path.as_ptr(), &mut err) };
    assert!(!core.is_null(), "the in-memory core must open");
    assert!(err.is_null());
    core
}

fn take(p: *mut c_char) -> Option<String> {
    if p.is_null() {
        return None;
    }
    // SAFETY: non-NULL and produced by this library; datagrep_string_free is the matching free.
    let s = unsafe { CStr::from_ptr(p) }.to_string_lossy().into_owned();
    unsafe { datagrep_string_free(p) };
    Some(s)
}

fn invalid_utf8() -> Vec<u8> {
    // 0x80 is a continuation byte with no lead byte: never valid UTF-8.
    vec![b'a', 0x80, 0xFF, b'b', 0]
}

fn embedded_nul() -> Vec<u8> {
    b"good\0evil\0".to_vec()
}

// ---- string arguments --------------------------------------------------

#[test]
fn null_string_arguments_are_errors_with_messages() {
    let c = core();
    let ok = CString::new("ok").unwrap();
    let mut err: *mut c_char = ptr::null_mut();

    // SAFETY: `c` is live throughout; the NULLs are exactly what is under test.
    unsafe {
        assert!(!datagrep_profiles_add(
            c,
            ptr::null(),
            ok.as_ptr(),
            &mut err
        ));
        assert!(take(err).is_some_and(|m| m.contains("name")), "name NULL");

        err = ptr::null_mut();
        assert!(!datagrep_profiles_add(
            c,
            ok.as_ptr(),
            ptr::null(),
            &mut err
        ));
        assert!(take(err).is_some_and(|m| m.contains("url")), "url NULL");

        err = ptr::null_mut();
        assert!(datagrep_profiles_get_json(c, ptr::null(), &mut err).is_null());
        assert!(take(err).is_some());

        err = ptr::null_mut();
        assert!(datagrep_connection_info_json(c, ptr::null(), &mut err).is_null());
        assert!(take(err).is_some());

        err = ptr::null_mut();
        assert!(!datagrep_profiles_remove(c, ptr::null(), &mut err));
        assert!(take(err).is_some());

        err = ptr::null_mut();
        let q = datagrep_query_run(c, ptr::null(), ok.as_ptr(), &mut err);
        assert!(q.is_null());
        assert!(take(err).is_some());

        err = ptr::null_mut();
        let q = datagrep_query_run(c, ok.as_ptr(), ptr::null(), &mut err);
        assert!(q.is_null());
        assert!(take(err).is_some());

        err = ptr::null_mut();
        assert!(datagrep_catalog_children_json(c, ptr::null(), ok.as_ptr(), &mut err).is_null());
        assert!(take(err).is_some());

        err = ptr::null_mut();
        assert!(datagrep_catalog_describe_json(c, ok.as_ptr(), ptr::null(), &mut err).is_null());
        assert!(take(err).is_some());

        err = ptr::null_mut();
        assert!(
            datagrep_browse_statement(ptr::null(), ok.as_ptr(), ptr::null(), &mut err).is_null()
        );
        assert!(
            take(err).is_some_and(|m| m.contains("driver_id")),
            "driver_id NULL"
        );

        err = ptr::null_mut();
        assert!(
            datagrep_browse_statement(ok.as_ptr(), ptr::null(), ptr::null(), &mut err).is_null()
        );
        assert!(take(err).is_some(), "path_json NULL");

        datagrep_core_free(c);
    }
}

#[test]
fn non_utf8_arguments_are_rejected_not_lossily_converted() {
    let c = core();
    let bad = invalid_utf8();
    let ok = CString::new("ok").unwrap();
    let mut err: *mut c_char = ptr::null_mut();

    // SAFETY: bad outlives every call and ends in NUL; being invalid UTF-8 is the half under test.
    unsafe {
        let bad_ptr = bad.as_ptr() as *const c_char;

        assert!(!datagrep_profiles_add(c, bad_ptr, ok.as_ptr(), &mut err));
        let msg = take(err).expect("a message");
        assert!(msg.contains("not valid UTF-8"), "message was {msg:?}");

        err = ptr::null_mut();
        assert!(!datagrep_profiles_add(c, ok.as_ptr(), bad_ptr, &mut err));
        assert!(take(err).is_some_and(|m| m.contains("not valid UTF-8")));

        err = ptr::null_mut();
        let q = datagrep_query_run(c, bad_ptr, ok.as_ptr(), &mut err);
        assert!(q.is_null());
        assert!(take(err).is_some_and(|m| m.contains("not valid UTF-8")));

        err = ptr::null_mut();
        assert!(datagrep_catalog_children_json(c, bad_ptr, ok.as_ptr(), &mut err).is_null());
        assert!(take(err).is_some_and(|m| m.contains("not valid UTF-8")));

        datagrep_core_free(c);
    }
}

#[test]
fn an_embedded_nul_truncates_and_the_tail_never_reaches_the_store() {
    let c = core();
    let bytes = embedded_nul();
    let url = CString::new(":memory:").unwrap();
    let mut err: *mut c_char = ptr::null_mut();

    // SAFETY: `bytes` outlives the calls and ends in NUL.
    unsafe {
        let name = bytes.as_ptr() as *const c_char;
        assert!(
            datagrep_profiles_add(c, name, url.as_ptr(), &mut err),
            "{:?}",
            take(err)
        );
        assert!(err.is_null());

        let list = take(datagrep_profiles_list_json(c, &mut err)).expect("a list");
        assert!(list.contains("\"good\""), "list was {list}");
        assert!(
            !list.contains("evil"),
            "the tail past the NUL leaked: {list}"
        );

        datagrep_core_free(c);
    }
}

#[test]
fn oversized_strings_do_not_panic() {
    let c = core();
    let huge_sql = CString::new("SELECT ".to_string() + &"a".repeat(1 << 20)).unwrap();
    let huge_name = CString::new("n".repeat(1 << 20)).unwrap();
    let mut err: *mut c_char = ptr::null_mut();

    // SAFETY: both `CString`s outlive the calls and are NUL-terminated.
    unsafe {
        let q = datagrep_query_run(c, huge_name.as_ptr(), huge_sql.as_ptr(), &mut err);
        if q.is_null() {
            assert!(take(err).is_some());
        } else {
            assert!(err.is_null());
            let status = take(datagrep_query_status_json(q, &mut err));
            assert!(status.is_some());
            datagrep_query_free(q);
        }
        datagrep_core_free(c);
    }
}

#[test]
fn blank_sql_is_refused_before_anything_is_spawned() {
    let c = core();
    let name = CString::new("nope").unwrap();
    let mut err: *mut c_char = ptr::null_mut();

    // SAFETY: NUL-terminated arguments over a live core.
    unsafe {
        for blank in ["", "   ", "\n\t \r\n"] {
            let sql = CString::new(blank).unwrap();
            let q = datagrep_query_run(c, name.as_ptr(), sql.as_ptr(), &mut err);
            assert!(q.is_null(), "blank SQL {blank:?} must not start a query");
            let msg = take(err).expect("a message");
            assert!(msg.contains("empty"), "message was {msg:?}");
            err = ptr::null_mut();
        }
        datagrep_core_free(c);
    }
}

// ---- JSON arguments ----------------------------------------------------

#[test]
fn malformed_json_arguments_are_errors_not_panics() {
    let c = core();
    let name = CString::new("jsontest").unwrap();
    let url = CString::new(":memory:").unwrap();
    let mut err: *mut c_char = ptr::null_mut();

    let malformed = [
        "{",
        "]",
        "null",
        "0",
        "\"a string where an object goes\"",
        "{\"read_only\": \"yes\"}", // right key, wrong type
        "{\"read_olny\": true}",    // typo — must be rejected, not ignored
        "[[[[[[[[[[[[[[[[[[[[]]]]]]]]]]]]]]]]]]]]", // deep nesting
        "\u{feff}{}",               // byte-order mark
    ];

    // SAFETY: every pointer below is a live `CString` or the live core.
    unsafe {
        assert!(
            datagrep_profiles_add(c, name.as_ptr(), url.as_ptr(), &mut err),
            "{:?}",
            take(err)
        );

        for text in malformed {
            let json = CString::new(text).unwrap();
            assert!(
                !datagrep_profiles_update(c, name.as_ptr(), json.as_ptr(), &mut err),
                "patch_json {text:?} must be refused"
            );
            assert!(take(err).is_some(), "patch_json {text:?} gave no message");
            err = ptr::null_mut();

            let fresh = CString::new(format!("fresh-{}", text.len())).unwrap();
            let ok = datagrep_profiles_add_json(
                c,
                fresh.as_ptr(),
                url.as_ptr(),
                json.as_ptr(),
                &mut err,
            );
            assert!(!ok, "options_json {text:?} must be refused");
            assert!(take(err).is_some(), "options_json {text:?} gave no message");
            err = ptr::null_mut();
        }

        // Well-formed JSON carrying a key `options_json` explicitly forbids.
        let smuggled = CString::new(r#"{"name":"smuggled"}"#).unwrap();
        let fresh = CString::new("fresh-smuggled").unwrap();
        assert!(!datagrep_profiles_add_json(
            c,
            fresh.as_ptr(),
            url.as_ptr(),
            smuggled.as_ptr(),
            &mut err
        ));
        assert!(take(err).is_some_and(|m| m.contains("name")));
        err = ptr::null_mut();

        for text in ["{}", "[1,2,3]", "[null]", "not json at all", "[", "\"x\""] {
            let json = CString::new(text).unwrap();
            let out = datagrep_catalog_children_json(c, name.as_ptr(), json.as_ptr(), &mut err);
            assert!(out.is_null(), "path_json {text:?} must be refused");
            let msg = take(err).expect("a message");
            assert!(msg.contains("path_json"), "path_json {text:?}: {msg}");
            err = ptr::null_mut();

            let out = datagrep_catalog_describe_json(c, name.as_ptr(), json.as_ptr(), &mut err);
            assert!(out.is_null(), "describe path_json {text:?} must be refused");
            assert!(take(err).is_some());
            err = ptr::null_mut();
        }

        datagrep_core_free(c);
    }
}

#[test]
fn null_options_json_means_defaults_not_an_error() {
    let c = core();
    let name = CString::new("defaults").unwrap();
    let url = CString::new(":memory:").unwrap();
    let mut err: *mut c_char = ptr::null_mut();

    // SAFETY: live core, NUL-terminated name/url, deliberate NULL options.
    unsafe {
        let ok = datagrep_profiles_add_json(c, name.as_ptr(), url.as_ptr(), ptr::null(), &mut err);
        assert!(ok, "{:?}", take(err));
        assert!(err.is_null());
        datagrep_core_free(c);
    }
}

// ---- handle arguments --------------------------------------------------

#[test]
fn null_handles_are_errors_or_documented_defaults() {
    let mut err: *mut c_char = ptr::null_mut();
    let ok = CString::new("ok").unwrap();

    // SAFETY: NULL is in-contract for all of these, and the free functions document NULL as a no-op.
    unsafe {
        assert!(datagrep_profiles_list_json(ptr::null_mut(), &mut err).is_null());
        assert!(take(err).is_some_and(|m| m.contains("NULL")));

        err = ptr::null_mut();
        let q = datagrep_query_run(ptr::null_mut(), ok.as_ptr(), ok.as_ptr(), &mut err);
        assert!(q.is_null());
        assert!(take(err).is_some());

        err = ptr::null_mut();
        assert!(datagrep_query_status_json(ptr::null_mut(), &mut err).is_null());
        assert!(take(err).is_some());

        err = ptr::null_mut();
        assert!(datagrep_query_rows(ptr::null_mut(), 0, 10, &mut err).is_null());
        assert!(take(err).is_some());

        assert_eq!(datagrep_rows_count(ptr::null_mut()), 0);
        assert_eq!(datagrep_rows_columns(ptr::null_mut()), 0);
        assert!(!datagrep_rows_pending(ptr::null_mut()));
        assert_eq!(datagrep_rows_cell_kind(ptr::null_mut(), 0, 0), 2, "ABSENT");
        assert!(datagrep_rows_cell_detail_json(ptr::null_mut(), 0, 0).is_null());
        assert!(datagrep_rows_column_names_json(ptr::null_mut()).is_null());
        assert!(datagrep_rows_envelope_json(ptr::null_mut(), 0).is_null());

        let mut len: usize = 99;
        assert!(datagrep_rows_cell(ptr::null_mut(), 0, 0, &mut len).is_null());
        assert_eq!(len, 0, "a NULL cell must zero the out-length");

        // Freeing NULL is a no-op, and stays one however often it happens.
        datagrep_core_free(ptr::null_mut());
        datagrep_query_free(ptr::null_mut());
        datagrep_rows_free(ptr::null_mut());
        datagrep_string_free(ptr::null_mut());
        datagrep_query_cancel(ptr::null_mut(), ptr::null_mut());
    }
}

#[test]
fn a_null_err_out_is_tolerated_everywhere() {
    let c = core();
    let ok = CString::new("no-such-profile").unwrap();
    let bad_json = CString::new("{").unwrap();

    // SAFETY: live core, NUL-terminated strings, deliberate NULL `err_out`.
    unsafe {
        assert!(datagrep_profiles_get_json(c, ok.as_ptr(), ptr::null_mut()).is_null());
        assert!(datagrep_connection_info_json(c, ok.as_ptr(), ptr::null_mut()).is_null());
        assert!(!datagrep_profiles_remove(c, ok.as_ptr(), ptr::null_mut()));
        assert!(!datagrep_profiles_update(
            c,
            ok.as_ptr(),
            bad_json.as_ptr(),
            ptr::null_mut()
        ));
        assert!(
            datagrep_catalog_children_json(c, ok.as_ptr(), bad_json.as_ptr(), ptr::null_mut())
                .is_null()
        );
        assert!(datagrep_query_rows(ptr::null_mut(), 0, 1, ptr::null_mut()).is_null());
        datagrep_core_free(c);
    }
}

#[test]
fn a_stale_err_out_slot_is_nulled_on_success() {
    let c = core();
    let name = CString::new("stale").unwrap();
    let url = CString::new(":memory:").unwrap();

    // SAFETY: live core and NUL-terminated arguments; err starts dangling to prove the success path clears it.
    unsafe {
        let mut err: *mut c_char = 0xdead_beef_usize as *mut c_char;
        assert!(datagrep_profiles_add(
            c,
            name.as_ptr(),
            url.as_ptr(),
            &mut err
        ));
        assert!(err.is_null(), "success must NULL a stale err_out slot");
        datagrep_core_free(c);
    }
}

// ---- window coordinates ------------------------------------------------

#[test]
fn extreme_window_coordinates_are_absent_not_a_crash() {
    let c = core();
    let name = CString::new("nowhere").unwrap();
    let sql = CString::new("SELECT 1").unwrap();
    let mut err: *mut c_char = ptr::null_mut();

    // SAFETY: live core and NUL-terminated arguments; freed below in header order (rows, then query, then core).
    unsafe {
        let q = datagrep_query_run(c, name.as_ptr(), sql.as_ptr(), &mut err);
        assert!(!q.is_null(), "run is non-blocking: {:?}", take(err));

        for (off, len) in [(0, 0), (0, u64::MAX), (u64::MAX, u64::MAX), (u64::MAX, 1)] {
            let rows = datagrep_query_rows(q, off, len, &mut err);
            assert!(!rows.is_null(), "window ({off},{len}): {:?}", take(err));

            for (r, col) in [
                (0u64, 0u32),
                (u64::MAX, u32::MAX),
                (u64::MAX, 0),
                (0, u32::MAX),
            ] {
                assert_eq!(datagrep_rows_cell_kind(rows, r, col), 2, "ABSENT");
                assert!(datagrep_rows_cell_detail_json(rows, r, col).is_null());
                let mut n: usize = 99;
                assert!(datagrep_rows_cell(rows, r, col, &mut n).is_null());
                assert_eq!(n, 0);
            }
            // A NULL `len_out` must be tolerated too.
            assert!(datagrep_rows_cell(rows, 0, 0, ptr::null_mut()).is_null());
            datagrep_rows_free(rows);
        }

        datagrep_query_free(q);
        datagrep_core_free(c);
    }
}

// ---- call ordering -----------------------------------------------------

#[test]
fn out_of_order_calls_stay_well_defined() {
    let c = core();
    let name = CString::new("nowhere").unwrap();
    let sql = CString::new("SELECT 1").unwrap();
    let mut err: *mut c_char = ptr::null_mut();

    // SAFETY: live core and query handle; `ctx` is NULL and never dereferenced.
    unsafe {
        let q = datagrep_query_run(c, name.as_ptr(), sql.as_ptr(), &mut err);
        assert!(!q.is_null(), "{:?}", take(err));

        // Detach on a handle that never had a callback, then attach NULL again.
        datagrep_query_on_progress(q, None, ptr::null_mut());
        datagrep_query_on_progress(q, None, ptr::null_mut());

        let mut out: *mut c_char = ptr::null_mut();
        datagrep_query_cancel(q, &mut out);
        let first = take(out).expect("a cancel report");
        assert!(first.contains("local_stopped"), "report was {first}");

        out = ptr::null_mut();
        datagrep_query_cancel(q, &mut out);
        assert!(take(out).is_some(), "the second cancel must still report");

        // A NULL out-parameter on cancel is fine.
        datagrep_query_cancel(q, ptr::null_mut());

        // Status still answers after cancellation.
        assert!(take(datagrep_query_status_json(q, &mut err)).is_some());

        datagrep_query_free(q);
        datagrep_core_free(c);
    }
}

#[test]
fn two_cores_can_be_opened_and_freed_independently() {
    let a = core();
    let b = core();
    let mut err: *mut c_char = ptr::null_mut();

    // SAFETY: both handles are live and each is freed exactly once.
    unsafe {
        assert!(take(datagrep_profiles_list_json(a, &mut err)).is_some());
        datagrep_core_free(a);
        // `b` must be unaffected by `a`'s shutdown.
        assert!(take(datagrep_profiles_list_json(b, &mut err)).is_some());
        datagrep_core_free(b);
    }
}
