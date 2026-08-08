//! Pointer/string/panic plumbing shared by every entry point.
//!
//! Two rules the whole ABI depends on:
//!
//! - **Every `char*` this library returns is allocated by [`to_c_string`]**
//!   and freed by `datagrep_string_free`, which is `CString::from_raw`. One
//!   allocator, one free — the caller never has to know which function
//!   produced a string.
//! - **No panic ever crosses the boundary.** Unwinding through an
//!   `extern "C"` frame is undefined behaviour; [`guard`] converts it to an
//!   error string instead.

use std::any::Any;
use std::ffi::{c_char, CStr, CString};
use std::panic::{catch_unwind, AssertUnwindSafe};

/// Allocate a NUL-terminated copy of `s` for the caller to `datagrep_string_free`.
///
/// Nothing this crate builds can contain an interior NUL (JSON escapes them;
/// error text is Rust `String`), but truncating at one beats returning NULL
/// and silently losing an error message.
pub fn to_c_string(s: impl Into<Vec<u8>>) -> *mut c_char {
    let bytes: Vec<u8> = s.into();
    let cut = bytes.iter().position(|b| *b == 0).unwrap_or(bytes.len());
    match CString::new(&bytes[..cut]) {
        Ok(c) => c.into_raw(),
        // Unreachable: the slice is NUL-free by construction.
        Err(_) => CString::default().into_raw(),
    }
}

/// Borrow a `const char*` argument as `&str`.
///
/// # Safety
/// `p` must be NULL or a valid NUL-terminated string that outlives the call.
pub unsafe fn cstr<'a>(p: *const c_char, what: &str) -> Result<&'a str, String> {
    if p.is_null() {
        return Err(format!("{what} must not be NULL"));
    }
    CStr::from_ptr(p)
        .to_str()
        .map_err(|_| format!("{what} is not valid UTF-8"))
}

/// Write `msg` (or NULL on success) into a caller-supplied `char** err_out`.
///
/// # Safety
/// `err_out` must be NULL or point at a writable `char*`.
pub unsafe fn set_err(err_out: *mut *mut c_char, msg: Option<String>) {
    if err_out.is_null() {
        return;
    }
    *err_out = match msg {
        Some(m) => to_c_string(m),
        None => std::ptr::null_mut(),
    };
}

/// Run `f` with an `err_out` contract: NULL on success, a freshly allocated
/// UTF-8 message on failure or panic. Returns `on_error` in both bad cases,
/// so the ABI never hands back a dangling or uninitialised pointer.
pub fn guard<T>(
    err_out: *mut *mut c_char,
    on_error: T,
    what: &str,
    f: impl FnOnce() -> Result<T, String>,
) -> T {
    match catch_unwind(AssertUnwindSafe(f)) {
        Ok(Ok(value)) => {
            unsafe { set_err(err_out, None) };
            value
        }
        Ok(Err(msg)) => {
            unsafe { set_err(err_out, Some(msg)) };
            on_error
        }
        Err(payload) => {
            // `payload.as_ref()`, not `&payload`: `&Box<dyn Any>` unsizes to
            // `&dyn Any` whose concrete type is the *Box*, and every downcast
            // then misses.
            let msg = format!("{what} panicked: {}", panic_text(payload.as_ref()));
            unsafe { set_err(err_out, Some(msg)) };
            on_error
        }
    }
}

/// [`guard`] for the entry points the frozen header gives no `err_out`
/// (`datagrep_rows_count`, `datagrep_rows_cell_kind`, the `free`s, …). A panic is
/// swallowed into `on_error`; there is nowhere to report it, and crashing the
/// host app is strictly worse than a `0`.
pub fn guard_quiet<T>(on_error: T, f: impl FnOnce() -> T) -> T {
    catch_unwind(AssertUnwindSafe(f)).unwrap_or(on_error)
}

/// Best-effort text of a panic payload.
pub fn panic_text(payload: &(dyn Any + Send)) -> String {
    if let Some(s) = payload.downcast_ref::<&str>() {
        (*s).to_string()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        "non-string panic payload".to_string()
    }
}

/// Parse the `path_json` argument the header specifies for the catalog calls:
/// a JSON array of path segments, `[]` for the roots.
pub fn parse_path_json(text: &str) -> Result<Vec<String>, String> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return Ok(Vec::new());
    }
    serde_json::from_str::<Vec<String>>(trimmed)
        .map_err(|e| format!("path_json must be a JSON array of strings ({e})"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_panic_becomes_an_error_string_not_an_unwind() {
        let mut err: *mut c_char = std::ptr::null_mut();
        let out = guard(&mut err, -1i32, "test_fn", || panic!("boom"));
        assert_eq!(out, -1);
        assert!(!err.is_null());
        let msg = unsafe { CStr::from_ptr(err) }.to_str().unwrap();
        assert!(msg.contains("test_fn panicked"), "message was {msg:?}");
        assert!(msg.contains("boom"), "message was {msg:?}");
        unsafe { drop(CString::from_raw(err)) };
    }

    #[test]
    fn success_nulls_the_error_slot_even_if_it_held_something() {
        let mut err: *mut c_char = to_c_string("stale");
        let out = guard(&mut err, 0i32, "test_fn", || Ok(7));
        assert_eq!(out, 7);
        assert!(err.is_null(), "err_out must be NULL on success");
    }

    #[test]
    fn null_err_out_is_tolerated() {
        let out = guard(std::ptr::null_mut(), 0i32, "test_fn", || Err("nope".into()));
        assert_eq!(out, 0);
    }

    #[test]
    fn path_json_accepts_empty_and_arrays() {
        assert_eq!(parse_path_json("[]").unwrap(), Vec::<String>::new());
        assert_eq!(parse_path_json("").unwrap(), Vec::<String>::new());
        assert_eq!(parse_path_json(r#"["main","t"]"#).unwrap(), ["main", "t"]);
        assert!(parse_path_json("{}").is_err());
    }
}
