//! Profiles: plain-text, git-committable connections (design §4 killer
//! feature #5), with secrets in the OS keychain and never on disk (§3.8).
//!
//! `dbx_profiles_add` splits any inline password out of the parsed URL into a
//! keychain [`SecretRef`] *before* the profile reaches
//! [`dbx_profiles::Store::create_profile`] — which independently refuses a
//! secret-shaped config key anyway, so this is defence in depth rather than
//! the only thing between a password and disk.

use std::ffi::c_char;

use dbx_api::ConfigValue;
use dbx_secrets::SecretRef;
use serde_json::json;

use crate::core::{core_ref, CoreInner, DbxCore};
use crate::ffi_util::{cstr, guard, to_c_string};
use crate::runtime::runtime;

/// `[{"name":..,"driver":..,"env":..,"has_secret":bool}, ...]`
///
/// # Safety
/// `core` must come from `dbx_core_new`; `err_out` must be NULL or writable.
#[no_mangle]
pub unsafe extern "C" fn dbx_profiles_list_json(
    core: *mut DbxCore,
    err_out: *mut *mut c_char,
) -> *mut c_char {
    guard(
        err_out,
        std::ptr::null_mut(),
        "dbx_profiles_list_json",
        || {
            let core = core_ref(core)?;
            let rt = runtime()?;
            let profiles = rt
                .block_on(core.store.list_profiles(None))
                .map_err(|e| format!("could not read the profile store: {e}"))?;
            let items: Vec<_> = profiles
                .iter()
                .map(|p| {
                    json!({
                        "name": p.name,
                        "driver": p.driver_id,
                        "env": p.env.to_string(),
                        "has_secret": p.secret_ref.is_some(),
                    })
                })
                .collect();
            let text = serde_json::to_string(&items)
                .map_err(|e| format!("could not encode the profile list: {e}"))?;
            Ok(to_c_string(text))
        },
    )
}

/// Save a profile parsed from a connection URL.
///
/// # Safety
/// `core` must come from `dbx_core_new`; `name`/`url` must be valid
/// NUL-terminated UTF-8; `err_out` must be NULL or writable.
#[no_mangle]
pub unsafe extern "C" fn dbx_profiles_add(
    core: *mut DbxCore,
    name: *const c_char,
    url: *const c_char,
    err_out: *mut *mut c_char,
) -> bool {
    guard(err_out, false, "dbx_profiles_add", || {
        let core = core_ref(core)?;
        let name = cstr(name, "name")?;
        let url = cstr(url, "url")?;
        if name.is_empty() {
            return Err("name must not be empty".to_string());
        }
        let rt = runtime()?;
        rt.block_on(add_profile(core, name, url))?;
        Ok(true)
    })
}

async fn add_profile(core: &CoreInner, name: &str, url: &str) -> Result<(), String> {
    let existing = core
        .store
        .list_profiles(None)
        .await
        .map_err(|e| format!("could not read the profile store: {e}"))?;
    if existing.iter().any(|p| p.name == name) {
        return Err(format!("a profile named `{name}` already exists"));
    }

    let (driver_id, driver) = crate::drivers::driver_for_url(url).ok_or_else(|| {
        format!(
            "could not tell which driver `{url}` is for (this build knows {})",
            crate::drivers::known_driver_ids().join(", ")
        )
    })?;
    let mut config = driver.parse_url(url).map_err(|e| e.to_string())?;

    let id = dbx_profiles::new_id();
    let mut secret_ref = None;
    for field in driver.config_schema().fields.iter().filter(|f| f.secret) {
        let Some(ConfigValue::Str(value)) = config.values.remove(field.key.as_ref()) else {
            continue;
        };
        if value.is_empty() {
            continue;
        }
        let reference = SecretRef::Keychain {
            service: "dbx".to_string(),
            account: format!("{id}:{}", field.key),
        };
        core.secrets
            .store(&reference, dbx_api::SecretString::new(value))
            .await
            .map_err(|e| format!("could not store the secret in the keychain: {e}"))?;
        secret_ref = Some(reference.to_string());
    }

    let now = dbx_profiles::now_ms();
    core.store
        .create_profile(dbx_profiles::Profile {
            id,
            folder_id: None,
            name: name.to_string(),
            driver_id: driver_id.to_string(),
            config,
            secret_ref,
            tunnel_id: None,
            env: dbx_profiles::Env::Dev,
            color: None,
            read_only: false,
            confirm_writes: false,
            auto_limit: None,
            idle_timeout_s: None,
            last_used_at: None,
            created_at: now,
            updated_at: now,
        })
        .await
        .map_err(|e| format!("could not save the profile: {e}"))?;
    Ok(())
}

/// Delete a saved profile and its keychain entry.
///
/// # Safety
/// `core` must come from `dbx_core_new`; `name` must be valid NUL-terminated
/// UTF-8; `err_out` must be NULL or writable.
#[no_mangle]
pub unsafe extern "C" fn dbx_profiles_remove(
    core: *mut DbxCore,
    name: *const c_char,
    err_out: *mut *mut c_char,
) -> bool {
    guard(err_out, false, "dbx_profiles_remove", || {
        let core = core_ref(core)?;
        let name = cstr(name, "name")?;
        let rt = runtime()?;
        rt.block_on(remove_profile(core, name))?;
        // The engine keeps its own copy of an opened profile and cannot be
        // told to drop one (CoreApi gap); at least stop handing the stale id
        // to new queries.
        core.forget_profile(name);
        Ok(true)
    })
}

async fn remove_profile(core: &CoreInner, name: &str) -> Result<(), String> {
    let profiles = core
        .store
        .list_profiles(None)
        .await
        .map_err(|e| format!("could not read the profile store: {e}"))?;
    let profile = profiles
        .into_iter()
        .find(|p| p.name == name)
        .ok_or_else(|| format!("no profile named `{name}`"))?;

    if let Some(secret_ref) = &profile.secret_ref {
        if let Ok(reference) = secret_ref.parse::<SecretRef>() {
            if reference.is_writable() {
                let _ = core.secrets.delete(&reference).await;
            }
        }
    }
    core.store
        .delete_profile(profile.id)
        .await
        .map_err(|e| format!("could not delete the profile: {e}"))
}
