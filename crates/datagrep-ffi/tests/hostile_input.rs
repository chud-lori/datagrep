//! Hostile input at the C ABI: nothing the Swift side can send may panic,
//! dereference garbage, or return a pointer the caller cannot free.
//!
//! The unit tests in `src/` drive each entry point the way a correct caller
//! would. This file does the opposite — it is the adversary. Every argument
//! that can be NULL is NULL, every string is malformed in a different way,
//! every index is out of range, and every call is made in an order the header
//! does not describe.
//!
//! ## What this can and cannot prove
//!
//! It proves the *checked* half of each contract: NULL, non-UTF-8, embedded
//! NUL, oversized, and out-of-range arguments become error strings or benign
//! defaults rather than crashes. It cannot prove the *unchecked* half — a
//! fabricated `DatagrepCore*`, a double free, or a `char*` with no terminator
//! are undefined behaviour by construction and no test can make them safe.
//! Those live in each function's `# Safety` section, which is where a Swift
//! author has to read them.
//!
//! Everything runs against an in-memory profile store and never opens a
//! socket or touches the OS keychain: the arguments below are rejected long
//! before anything would resolve a secret.

use std::ffi::{c_char, CStr, CString};
use std::ptr;

use datagrep_ffi::profiles::{
    datagrep_connection_info_json, datagrep_profiles_add_json, datagrep_profiles_get_json,
    datagrep_profiles_update,
};
use datagrep_ffi::{
    datagrep_catalog_children_json, datagrep_catalog_describe_json, datagrep_core_free,
    datagrep_core_new, datagrep_profiles_add, datagrep_profiles_list_json,
    datagrep_profiles_remove, datagrep_query_cancel, datagrep_query_free,
    datagrep_query_on_progress, datagrep_query_rows, datagrep_query_run,
    datagrep_query_status_json, datagrep_rows_cell, datagrep_rows_cell_detail_json,
    datagrep_rows_cell_kind, datagrep_rows_columns, datagrep_rows_count, datagrep_rows_free,
    datagrep_rows_pending, datagrep_string_free, DatagrepCore,
};

// ---- helpers -----------------------------------------------------------

/// A core over an ephemeral store. Freed by the caller.
fn core() -> *mut DatagrepCore {
    let path = CString::new(":memory:").expect("no interior NUL");
    let mut err: *mut c_char = ptr::null_mut();
    // SAFETY: a valid NUL-terminated path and a writable `err_out` slot.
    let core = unsafe { datagrep_core_new(path.as_ptr(), &mut err) };
    assert!(!core.is_null(), "the in-memory core must open");
    assert!(err.is_null());
    core
}

/// Take ownership of a `char*` the ABI returned, freeing it the way the header
/// says to. Returns `None` for NULL.
fn take(p: *mut c_char) -> Option<String> {
    if p.is_null() {
        return None;
    }
    // SAFETY: non-NULL and produced by this library, so `from_ptr` sees a
    // NUL-terminated buffer and `datagrep_string_free` is the matching free.
    let s = unsafe { CStr::from_ptr(p) }.to_string_lossy().into_owned();
    unsafe { datagrep_string_free(p) };
    Some(s)
}

/// A `*const c_char` over bytes that are **not** valid UTF-8 but *are*
/// NUL-terminated — the shape a Swift `Data` blob mistakenly passed as a
/// string would have. Kept alive by the returned `Vec`.
fn invalid_utf8() -> Vec<u8> {
    // 0x80 is a continuation byte with no lead byte: never valid UTF-8.
    vec![b'a', 0x80, 0xFF, b'b', 0]
}

/// NUL-terminated bytes with a NUL in the middle. `CStr` stops at the first
/// one, so the tail is unreachable — the point is that this truncates rather
/// than confusing the length arithmetic.
fn embedded_nul() -> Vec<u8> {
    b"good\0evil\0".to_vec()
}

// ---- string arguments --------------------------------------------------

/// Every `*const c_char` argument, NULL, on every entry point that takes one.
/// The contract says NULL is an *error*, not a precondition, so each must come
/// back with a message and no crash.
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

        datagrep_core_free(c);
    }
}

/// Bytes that are NUL-terminated but not UTF-8. `CStr::to_str` must reject
/// them before anything treats them as text — a lossy conversion here would
/// let a mangled profile name reach the store.
#[test]
fn non_utf8_arguments_are_rejected_not_lossily_converted() {
    let c = core();
    let bad = invalid_utf8();
    let ok = CString::new("ok").unwrap();
    let mut err: *mut c_char = ptr::null_mut();

    // SAFETY: `bad` outlives every call and its last byte is NUL, so the
    // pointer satisfies `cstr`'s "NUL-terminated" half. Being invalid UTF-8 is
    // the checked half under test.
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

/// A string with a NUL in the middle truncates at it. The risk is not the
/// truncation — C has no other option — but that the *store* might end up with
/// the tail bytes anyway. It must see exactly `"good"`.
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

/// A megabyte of SQL and a megabyte of profile name. Neither may panic; the
/// name is only bounded by what the store accepts, and the SQL never leaves
/// this process because the profile does not exist.
#[test]
fn oversized_strings_do_not_panic() {
    let c = core();
    let huge_sql = CString::new("SELECT ".to_string() + &"a".repeat(1 << 20)).unwrap();
    let huge_name = CString::new("n".repeat(1 << 20)).unwrap();
    let mut err: *mut c_char = ptr::null_mut();

    // SAFETY: both `CString`s outlive the calls and are NUL-terminated.
    unsafe {
        // No such profile — the point is that a megabyte of argument is an
        // ordinary error, not a crash or a stack overflow.
        let q = datagrep_query_run(c, huge_name.as_ptr(), huge_sql.as_ptr(), &mut err);
        if q.is_null() {
            assert!(take(err).is_some());
        } else {
            // `datagrep_query_run` is non-blocking, so an unknown profile is
            // reported through the status JSON instead of `err_out`.
            assert!(err.is_null());
            let status = take(datagrep_query_status_json(q, &mut err));
            assert!(status.is_some());
            datagrep_query_free(q);
        }
        datagrep_core_free(c);
    }
}

/// Whitespace-only SQL is refused synchronously — the one class of query error
/// that *is* knowable without touching the network.
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

/// The JSON-shaped arguments are parsed with `serde_json`, which is not the
/// problem; the problem would be a parse error reaching a `.unwrap()`. Feed
/// each one a spread of malformed and merely surprising input.
#[test]
fn malformed_json_arguments_are_errors_not_panics() {
    let c = core();
    let name = CString::new("jsontest").unwrap();
    let url = CString::new(":memory:").unwrap();
    let mut err: *mut c_char = ptr::null_mut();

    // Malformed for both `options_json` and `patch_json` — the two share
    // `parse_patch`, and `deny_unknown_fields` is what turns a misspelled
    // safety setting into an error instead of a silently ignored guardrail.
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
        // The profile has to exist first, or `update` would error for the
        // uninteresting reason (no such profile) and prove nothing about
        // parsing.
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

        // `path_json` wants a JSON array of strings; anything else is an error
        // with a message, never a panic. It is parsed before the profile is
        // even looked up, so these fail on the JSON and nothing else.
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

/// A NULL `options_json` is documented as "use the defaults", so it is the one
/// NULL string in this ABI that must *succeed*. Worth pinning: the obvious
/// hardening (reject every NULL) would silently break it.
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

/// Every entry point taking an opaque handle, with NULL. The header gives some
/// of them no `err_out` at all, so those must degrade to a documented default
/// rather than reporting anything.
#[test]
fn null_handles_are_errors_or_documented_defaults() {
    let mut err: *mut c_char = ptr::null_mut();
    let ok = CString::new("ok").unwrap();

    // SAFETY: NULL is explicitly in-contract for all of these ("`r` must be
    // NULL or …"), and the free functions document NULL as a no-op.
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

        // No `err_out` in the frozen header: these report by returning the
        // safest possible answer.
        assert_eq!(datagrep_rows_count(ptr::null_mut()), 0);
        assert_eq!(datagrep_rows_columns(ptr::null_mut()), 0);
        assert!(!datagrep_rows_pending(ptr::null_mut()));
        assert_eq!(datagrep_rows_cell_kind(ptr::null_mut(), 0, 0), 2, "ABSENT");
        assert!(datagrep_rows_cell_detail_json(ptr::null_mut(), 0, 0).is_null());

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

/// A NULL `err_out` must be tolerated on every entry point that takes one:
/// a caller who does not want the message should not have to allocate a slot
/// for it. The failing calls below would each write a message if they could.
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

/// `err_out` pointing at a slot that already holds a stale pointer. Success
/// must NULL it, or a caller who frees on non-NULL frees the same string twice.
#[test]
fn a_stale_err_out_slot_is_nulled_on_success() {
    let c = core();
    let name = CString::new("stale").unwrap();
    let url = CString::new(":memory:").unwrap();

    // SAFETY: live core and NUL-terminated arguments. `err` deliberately starts
    // life holding a dangling-looking value to prove the success path clears it.
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

/// Extreme row/column coordinates against a real (skeleton) window. The
/// arithmetic behind `datagrep_rows_cell` is `row * cols + col`, so `u64::MAX`
/// is the value that would overflow it if it were not bounds-checked first.
#[test]
fn extreme_window_coordinates_are_absent_not_a_crash() {
    let c = core();
    let name = CString::new("nowhere").unwrap();
    let sql = CString::new("SELECT 1").unwrap();
    let mut err: *mut c_char = ptr::null_mut();

    // SAFETY: live core and NUL-terminated arguments; the handle is freed below
    // in the order the header requires (rows first, then query, then core).
    unsafe {
        let q = datagrep_query_run(c, name.as_ptr(), sql.as_ptr(), &mut err);
        assert!(!q.is_null(), "run is non-blocking: {:?}", take(err));

        // `offset + len` saturates rather than wrapping, and an unaccepted
        // query yields a skeleton window regardless.
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

/// Calls in orders the header does not describe: cancel before the server has
/// accepted anything, cancel twice, status after cancel, progress attached and
/// detached repeatedly. None of these is UB — they are just wrong-looking, and
/// a UI under a user's fingers produces all of them.
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

        // Cancel before acceptance, then again — idempotent, and each call
        // hands back a JSON report the caller owns.
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

/// Two cores at once over the same process-global runtime, each freed while
/// the other is live. The runtime is deliberately never dropped, so this is
/// the case that would panic if `datagrep_core_free` owned it.
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
