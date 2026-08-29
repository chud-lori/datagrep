use std::ffi::c_char;

use datagrep_api::safety::Attestation;
use datagrep_core::SafetyDecision;
use datagrep_lang::StatementClass;
use serde_json::json;

use crate::core::{core_ref, CoreInner, DatagrepCore};
use crate::ffi_util::{cstr, guard, to_c_string};
use crate::runtime::runtime;

pub(crate) fn class_name(class: StatementClass) -> &'static str {
    match class {
        StatementClass::Read => "read",
        StatementClass::Write => "write",
        StatementClass::Ddl => "ddl",
        StatementClass::Tcl => "tcl",
        StatementClass::Admin => "admin",
        StatementClass::Unknown => "unknown",
    }
}

pub(crate) fn decision_json(decision: &SafetyDecision) -> serde_json::Value {
    let statements: Vec<_> = decision
        .statements
        .iter()
        .map(|s| {
            json!({
                "text": s.text,
                "class": class_name(s.class),
                "requires": s.requirement.as_str(),
            })
        })
        .collect();
    json!({
        "profile": decision.profile.as_ref(),
        "level": decision.level.as_str(),
        "requires": decision.requirement.as_str(),
        "challenge": decision.challenge.as_ref().map(|c| c.as_ref()),
        "statements": statements,
    })
}

async fn gate(
    core: &CoreInner,
    profile: &str,
) -> Result<std::sync::Arc<datagrep_core::SafetyGate>, String> {
    let (id, _) = core.open_profile(profile).await?;
    core.api.safety_gate(id).map_err(|e| e.to_string())
}

/// # Safety
/// `core` is a live handle; `profile`/`sql` are NUL-terminated; `err_out` is NULL or a writable slot.
#[no_mangle]
pub unsafe extern "C" fn datagrep_safety_evaluate_json(
    core: *mut DatagrepCore,
    profile: *const c_char,
    sql: *const c_char,
    err_out: *mut *mut c_char,
) -> *mut c_char {
    guard(
        err_out,
        std::ptr::null_mut(),
        "datagrep_safety_evaluate_json",
        || {
            // SAFETY: live DatagrepCore* and NUL-terminated strings per the module contract.
            let core = unsafe { core_ref(core) }?;
            let profile = unsafe { cstr(profile, "profile") }?;
            let sql = unsafe { cstr(sql, "sql") }?;
            let rt = runtime()?;
            let gate = rt.block_on(gate(core, profile))?;
            let text = serde_json::to_string(&decision_json(&gate.plan(sql)))
                .map_err(|e| format!("could not encode the safety decision: {e}"))?;
            Ok(to_c_string(text))
        },
    )
}

/// # Safety
/// `core` is a live handle; `profile` is NUL-terminated; `err_out` is NULL or a writable slot.
#[no_mangle]
pub unsafe extern "C" fn datagrep_safety_pending_json(
    core: *mut DatagrepCore,
    profile: *const c_char,
    err_out: *mut *mut c_char,
) -> *mut c_char {
    guard(
        err_out,
        std::ptr::null_mut(),
        "datagrep_safety_pending_json",
        || {
            // SAFETY: live DatagrepCore* and NUL-terminated strings per the module contract.
            let core = unsafe { core_ref(core) }?;
            let profile = unsafe { cstr(profile, "profile") }?;
            let rt = runtime()?;
            let gate = rt.block_on(gate(core, profile))?;
            let items: Vec<_> = gate.pending().iter().map(decision_json).collect();
            let text = serde_json::to_string(&items)
                .map_err(|e| format!("could not encode the open challenges: {e}"))?;
            Ok(to_c_string(text))
        },
    )
}

/// # Safety
/// `core` is a live handle; string arguments are NUL-terminated; `err_out` is NULL or a writable slot.
#[no_mangle]
pub unsafe extern "C" fn datagrep_safety_satisfy(
    core: *mut DatagrepCore,
    profile: *const c_char,
    challenge: *const c_char,
    attestation_json: *const c_char,
    err_out: *mut *mut c_char,
) -> bool {
    guard(err_out, false, "datagrep_safety_satisfy", || {
        // SAFETY: live DatagrepCore* and NUL-terminated strings per the module contract.
        let core = unsafe { core_ref(core) }?;
        let profile = unsafe { cstr(profile, "profile") }?;
        let challenge = unsafe { cstr(challenge, "challenge") }?;
        let attestation = unsafe { cstr(attestation_json, "attestation_json") }?;
        let attestation: Attestation = serde_json::from_str(attestation.trim()).map_err(|e| {
            format!(
                "attestation JSON is invalid ({e}); expected {{\"kind\":\"acknowledged\"}}, \
                 {{\"kind\":\"typed_phrase\",\"typed\":..}} or {{\"kind\":\"system_auth\",\"method\":..}}"
            )
        })?;
        let rt = runtime()?;
        let gate = rt.block_on(gate(core, profile))?;
        gate.satisfy(challenge, &attestation)
            .map_err(|e| e.to_string())?;
        Ok(true)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::datagrep_core_free;
    use std::ffi::{CStr, CString};

    fn test_core() -> *mut DatagrepCore {
        let store = datagrep_profiles::Store::open_in_memory();
        let core = DatagrepCore::with_store_in_memory_secrets(store).expect("core");
        Box::into_raw(Box::new(core))
    }

    unsafe fn add(core: *mut DatagrepCore, name: &str, options: &str) {
        let name = CString::new(name).unwrap();
        let url = CString::new(":memory:").unwrap();
        let options = CString::new(options).unwrap();
        let mut err: *mut c_char = std::ptr::null_mut();
        // SAFETY: a live core and NUL-terminated CStrings that outlive the call.
        let ok = unsafe {
            crate::profiles::datagrep_profiles_add_json(
                core,
                name.as_ptr(),
                url.as_ptr(),
                options.as_ptr(),
                &mut err,
            )
        };
        assert!(ok, "add failed");
    }

    unsafe fn take(ptr: *mut c_char) -> String {
        assert!(!ptr.is_null(), "call returned NULL");
        // SAFETY: a non-NULL, NUL-terminated string this library allocated and still owns.
        let text = unsafe { CStr::from_ptr(ptr) }
            .to_string_lossy()
            .into_owned();
        unsafe { crate::core::datagrep_string_free(ptr) };
        text
    }

    #[test]
    fn evaluate_names_the_rung_and_mints_a_challenge_the_frontend_must_clear() {
        unsafe {
            let core = test_core();
            add(core, "prod", r#"{"safety":"auth_writes"}"#);
            let profile = CString::new("prod").unwrap();
            let mut err: *mut c_char = std::ptr::null_mut();

            let sql = CString::new("select 1").unwrap();
            let read = take(datagrep_safety_evaluate_json(
                core,
                profile.as_ptr(),
                sql.as_ptr(),
                &mut err,
            ));
            let read: serde_json::Value = serde_json::from_str(&read).unwrap();
            assert_eq!(read["requires"], "none");
            assert_eq!(read["challenge"], serde_json::Value::Null);

            let sql = CString::new("delete from users").unwrap();
            let write = take(datagrep_safety_evaluate_json(
                core,
                profile.as_ptr(),
                sql.as_ptr(),
                &mut err,
            ));
            let write: serde_json::Value = serde_json::from_str(&write).unwrap();
            assert_eq!(write["requires"], "authenticate");
            assert_eq!(write["level"], "auth_writes");
            assert_eq!(write["statements"][0]["class"], "write");
            let challenge = write["challenge"].as_str().expect("a challenge").to_owned();

            let pending = take(datagrep_safety_pending_json(
                core,
                profile.as_ptr(),
                &mut err,
            ));
            assert!(pending.contains(&challenge), "pending was {pending}");

            let id = CString::new(challenge.clone()).unwrap();
            let ack = CString::new(r#"{"kind":"acknowledged"}"#).unwrap();
            let refused = datagrep_safety_satisfy(
                core,
                profile.as_ptr(),
                id.as_ptr(),
                ack.as_ptr(),
                &mut err,
            );
            assert!(!refused, "an acknowledgement cleared an authenticate rung");
            assert!(!err.is_null());
            crate::core::datagrep_string_free(err);
            err = std::ptr::null_mut();

            let typed = CString::new(r#"{"kind":"typed_phrase","typed":"prod"}"#).unwrap();
            let ok = datagrep_safety_satisfy(
                core,
                profile.as_ptr(),
                id.as_ptr(),
                typed.as_ptr(),
                &mut err,
            );
            assert!(ok, "the connection name must clear it");

            datagrep_core_free(core);
        }
    }
}
