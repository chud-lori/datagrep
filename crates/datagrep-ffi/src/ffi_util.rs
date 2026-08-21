use std::any::Any;
use std::ffi::{c_char, CStr, CString};
use std::panic::{catch_unwind, AssertUnwindSafe};

pub fn to_c_string(s: impl Into<Vec<u8>>) -> *mut c_char {
    let bytes: Vec<u8> = s.into();
    let cut = bytes.iter().position(|b| *b == 0).unwrap_or(bytes.len());
    match CString::new(&bytes[..cut]) {
        Ok(c) => c.into_raw(),
        // Unreachable: the slice is NUL-free by construction.
        Err(_) => CString::default().into_raw(),
    }
}

/// # Safety
/// `p` is NULL or points at a NUL-terminated buffer that outlives this call.
pub unsafe fn cstr<'a>(p: *const c_char, what: &str) -> Result<&'a str, String> {
    if p.is_null() {
        return Err(format!("{what} must not be NULL"));
    }
    // SAFETY: non-NULL (checked) and NUL-terminated for this call per the contract; the unbound 'a is consumed before returning to C.
    let raw = unsafe { CStr::from_ptr(p) };
    raw.to_str()
        .map_err(|_| format!("{what} is not valid UTF-8"))
}

/// # Safety
/// `err_out` is NULL or a writable `char*` slot.
pub unsafe fn set_err(err_out: *mut *mut c_char, msg: Option<String>) {
    if err_out.is_null() {
        return;
    }
    let value = match msg {
        Some(m) => to_c_string(m),
        None => std::ptr::null_mut(),
    };
    // SAFETY: non-NULL (checked) and writable per the contract; this overwrites and never reads, so an uninitialised slot is fine.
    unsafe { *err_out = value };
}

pub fn guard<T>(
    err_out: *mut *mut c_char,
    on_error: T,
    what: &str,
    f: impl FnOnce() -> Result<T, String>,
) -> T {
    // SAFETY (all three set_err calls): err_out is NULL or a writable char* per every entry-point contract; set_err null-checks before writing.
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
            let msg = format!("{what} panicked: {}", panic_text(payload.as_ref()));
            unsafe { set_err(err_out, Some(msg)) };
            on_error
        }
    }
}

pub fn guard_quiet<T>(on_error: T, f: impl FnOnce() -> T) -> T {
    catch_unwind(AssertUnwindSafe(f)).unwrap_or(on_error)
}

pub fn panic_text(payload: &(dyn Any + Send)) -> String {
    if let Some(s) = payload.downcast_ref::<&str>() {
        (*s).to_string()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        "non-string panic payload".to_string()
    }
}

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
