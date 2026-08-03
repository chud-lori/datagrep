//! Lifecycle: [`DbxCore`], `dbx_core_new`, `dbx_core_free`, `dbx_string_free`.
//!
//! A `DbxCore` is three collaborating objects the Swift side never sees
//! separately:
//!
//! | field | what it is | why it is here |
//! |---|---|---|
//! | `api` | [`dbx_core::CoreApi`] | the only entry into the engine (design §3) |
//! | `store` | [`dbx_profiles::Store`] | the on-disk profile list (design §3.7) |
//! | `secrets` | [`dbx_secrets::SecretResolver`] | keychain lookups (design §3.8) |
//!
//! `dbx_core_new` **never blocks**, and that is load-bearing (design P1,
//! ≤250 ms cold start): `CoreApi::new()` is plain data structures,
//! `register_drivers` is three hashmap inserts that construct nothing, and
//! `Store::open` is documented lazy — its worker thread and SQLite file only
//! come alive on the first real call. Nothing here opens a socket
//! (design §3.5: "Opening the app connects to nothing").
//!
//! ## Why the guts live behind an `Arc`
//!
//! [`DbxCore`] is a thin handle over an [`CoreInner`]; a running
//! [`crate::query::DbxQuery`] holds its own `Arc<CoreInner>`. So a Swift app
//! that frees the core while a query is still streaming gets an orderly
//! shutdown instead of a use-after-free — the engine survives until the last
//! query handle is freed. The C header cannot express that ownership, so the
//! Rust side enforces it.

use std::collections::HashMap;
use std::ffi::c_char;
use std::sync::{Arc, Mutex};

use dbx_api::ConfigValue;
use dbx_core::{CoreApi, ProfileId};
use dbx_profiles::Store;
use dbx_secrets::{SecretRef, SecretResolver};

use crate::ffi_util::{guard, guard_quiet};
use crate::runtime::runtime;

/// The engine, as one opaque handle.
#[derive(Debug)]
pub struct DbxCore(pub(crate) Arc<CoreInner>);

/// Everything a `DbxCore` owns, shared with every query it started.
pub(crate) struct CoreInner {
    pub(crate) api: Arc<CoreApi>,
    pub(crate) store: Arc<Store>,
    pub(crate) secrets: Arc<SecretResolver>,
    /// Saved-profile name → the id it is registered under inside `CoreApi`.
    ///
    /// **CoreApi gap.** `CoreApi` has `add_profile`/`add_profile_full` but no
    /// lookup-by-name, no update, and no *remove*. Registering afresh on every
    /// query would therefore leak one `Profile` entry **and one connection
    /// pool** per query — so the mapping is cached here and a profile is
    /// registered at most once per process. The cost is that editing a profile
    /// on disk does not affect an already-opened one until restart; noted in
    /// the README rather than papered over.
    registered: Mutex<HashMap<String, ProfileId>>,
}

impl std::fmt::Debug for CoreInner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CoreInner")
            .field("api", &self.api)
            .field("registered", &self.lock_registered().len())
            .finish()
    }
}

impl DbxCore {
    /// Build a core over an explicit profile store — the seam the integration
    /// tests use with [`Store::open_in_memory`].
    pub fn with_store(store: Store) -> Result<Self, String> {
        let rt = runtime()?;
        // `CoreApi::new` must be constructed inside a runtime: its shared
        // `TimerWheel` spawns the (armed-on-demand) worker task.
        let _guard = rt.enter();
        let api = CoreApi::new();
        crate::drivers::register_drivers(&api);
        Ok(Self(Arc::new(CoreInner {
            api: Arc::new(api),
            store: Arc::new(store),
            secrets: Arc::new(SecretResolver::new()),
            registered: Mutex::new(HashMap::new()),
        })))
    }
}

impl CoreInner {
    /// Resolve a saved profile by name, register it with `CoreApi` (once), and
    /// return its id and driver — the FFI's equivalent of `dbx-cli`'s
    /// `open_profile`.
    ///
    /// **CoreApi gap, worked around here.**
    /// `dbx_core::session::Session::acquire` always builds
    /// `ResolvedConfig::without_secrets(...)`: there is no seam for a frontend
    /// to hand a resolved secret to a running session. So the secret is
    /// resolved here and folded into the plaintext `ConnectionConfig` — the
    /// only way a driver in this build ever sees it. The on-disk profile still
    /// never holds it, but the resolved value sits in an un-zeroized `String`
    /// for the life of this `CoreApi` profile, weaker than the `SecretString`
    /// guarantee it started as. Same trade `dbx-cli` documents.
    pub(crate) async fn open_profile(&self, name: &str) -> Result<(ProfileId, String), String> {
        let profile = self.saved_profile(name).await?;
        if let Some(id) = self.lock_registered().get(name).copied() {
            return Ok((id, profile.driver_id));
        }

        let mut config = profile.config.clone();
        if let Some(secret_ref) = &profile.secret_ref {
            let reference: SecretRef = secret_ref
                .parse()
                .map_err(|e: dbx_secrets::SecretError| e.to_string())?;
            let secret = self
                .secrets
                .resolve(&reference)
                .await
                .map_err(|e| format!("could not resolve the secret for `{name}`: {e}"))?;
            if let Some(driver) = crate::drivers::driver_for(&profile.driver_id) {
                if let Some(field) = driver.config_schema().fields.iter().find(|f| f.secret) {
                    config.values.insert(
                        field.key.to_string(),
                        ConfigValue::Str(secret.expose().to_string()),
                    );
                }
            }
        }

        let id = self
            .api
            .add_profile_full(dbx_core::Profile {
                id: ProfileId(0), // overwritten by add_profile_full
                name: Arc::from(profile.name.as_str()),
                driver: Arc::from(profile.driver_id.as_str()),
                config,
                env: map_env(profile.env),
                read_only: profile.read_only,
            })
            .await;

        // Another task may have raced us here; keep whichever landed first so
        // one name never maps to two connection pools.
        let id = *self
            .lock_registered()
            .entry(name.to_string())
            .or_insert(id);

        let _ = self.store.touch_profile_last_used(profile.id.clone()).await;
        Ok((id, profile.driver_id))
    }

    pub(crate) async fn saved_profile(&self, name: &str) -> Result<dbx_profiles::Profile, String> {
        self.store
            .list_profiles(None)
            .await
            .map_err(|e| format!("could not read the profile store: {e}"))?
            .into_iter()
            .find(|p| p.name == name)
            .ok_or_else(|| format!("no profile named `{name}`"))
    }

    pub(crate) fn forget_profile(&self, name: &str) {
        self.lock_registered().remove(name);
    }

    fn lock_registered(&self) -> std::sync::MutexGuard<'_, HashMap<String, ProfileId>> {
        self.registered
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

pub(crate) fn map_env(env: dbx_profiles::Env) -> dbx_core::api::Env {
    match env {
        dbx_profiles::Env::Dev => dbx_core::api::Env::Dev,
        dbx_profiles::Env::Staging => dbx_core::api::Env::Staging,
        dbx_profiles::Env::Prod => dbx_core::api::Env::Prod,
    }
}

/// Borrow a `DbxCore*` argument as the shared guts every entry point works
/// against.
///
/// # Safety
/// `core` must be a pointer returned by `dbx_core_new` and not yet freed.
pub(crate) unsafe fn core_ref<'a>(core: *mut DbxCore) -> Result<&'a Arc<CoreInner>, String> {
    if core.is_null() {
        return Err("DbxCore* must not be NULL".to_string());
    }
    Ok(&(*core).0)
}

// ---- lifecycle ---------------------------------------------------------

/// Creates the engine + its own tokio runtime thread. Never blocks.
///
/// Pass `""` or `":memory:"` for an ephemeral profile store (what the smoke
/// test uses); anything else is a path, created lazily on first use.
///
/// # Safety
/// `profiles_db_path` must be a valid NUL-terminated UTF-8 path; `err_out`
/// must be NULL or point at a writable `char*`.
#[no_mangle]
pub unsafe extern "C" fn dbx_core_new(
    profiles_db_path: *const c_char,
    err_out: *mut *mut c_char,
) -> *mut DbxCore {
    guard(err_out, std::ptr::null_mut(), "dbx_core_new", || {
        let path = crate::ffi_util::cstr(profiles_db_path, "profiles_db_path")?;
        let store = if path.is_empty() || path == ":memory:" {
            Store::open_in_memory()
        } else {
            Store::open(path)
        };
        let core = DbxCore::with_store(store)?;
        Ok(Box::into_raw(Box::new(core)))
    })
}

/// Stop every query, close every socket, and free the handle.
///
/// Any `DbxQuery` still alive keeps the engine alive until it too is freed —
/// but its queries have already been stopped by the `shutdown` below, which is
/// what "free the core" means.
///
/// # Safety
/// `core` must be a pointer from `dbx_core_new`, freed at most once.
#[no_mangle]
pub unsafe extern "C" fn dbx_core_free(core: *mut DbxCore) {
    guard_quiet((), || {
        if core.is_null() {
            return;
        }
        let core = Box::from_raw(core);
        // Sound from any thread: the runtime is process-global and is never
        // dropped here, so this can never be "drop a runtime from inside its
        // own worker" (see `runtime.rs`).
        if let Ok(rt) = runtime() {
            rt.block_on(core.0.api.shutdown());
        }
        drop(core);
    })
}

/// Frees any `char*` this API returned.
///
/// # Safety
/// `s` must be NULL or a pointer this library returned, freed at most once.
#[no_mangle]
pub unsafe extern "C" fn dbx_string_free(s: *mut c_char) {
    guard_quiet((), || {
        if !s.is_null() {
            drop(std::ffi::CString::from_raw(s));
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::CString;

    #[test]
    fn a_core_over_an_in_memory_store_starts_and_frees() {
        let path = CString::new(":memory:").unwrap();
        let mut err: *mut c_char = std::ptr::null_mut();
        let core = unsafe { dbx_core_new(path.as_ptr(), &mut err) };
        assert!(!core.is_null());
        assert!(err.is_null(), "err_out must be NULL on success");
        unsafe { dbx_core_free(core) };
    }

    #[test]
    fn a_null_path_is_an_error_not_a_crash() {
        let mut err: *mut c_char = std::ptr::null_mut();
        let core = unsafe { dbx_core_new(std::ptr::null(), &mut err) };
        assert!(core.is_null());
        assert!(!err.is_null());
        unsafe { dbx_string_free(err) };
    }

    #[test]
    fn freeing_null_handles_is_a_no_op() {
        unsafe {
            dbx_core_free(std::ptr::null_mut());
            dbx_string_free(std::ptr::null_mut());
        }
    }
}
