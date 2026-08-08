//! Lifecycle: [`DatagrepCore`], `datagrep_core_new`, `datagrep_core_free`, `datagrep_string_free`.
//!
//! A `DatagrepCore` is three collaborating objects the Swift side never sees
//! separately:
//!
//! | field | what it is | why it is here |
//! |---|---|---|
//! | `api` | [`datagrep_core::CoreApi`] | the only entry into the engine |
//! | `store` | [`datagrep_profiles::Store`] | the on-disk profile list |
//! | `secrets` | [`datagrep_secrets::SecretResolver`] | keychain lookups |
//!
//! `datagrep_core_new` **never blocks**, and that is load-bearing for the
//! ≤250 ms cold-start budget: `CoreApi::new()` is plain data structures,
//! `register_drivers` is three hashmap inserts that construct nothing, and
//! `Store::open` is documented lazy — its worker thread and SQLite file only
//! come alive on the first real call. Nothing here opens a socket: opening the
//! app connects to nothing, so startup is never gated on the network.
//!
//! ## Why the guts live behind an `Arc`
//!
//! [`DatagrepCore`] is a thin handle over an [`CoreInner`]; a running
//! [`crate::query::DatagrepQuery`] holds its own `Arc<CoreInner>`. So a Swift app
//! that frees the core while a query is still streaming gets an orderly
//! shutdown instead of a use-after-free — the engine survives until the last
//! query handle is freed. The C header cannot express that ownership, so the
//! Rust side enforces it.

use std::collections::HashMap;
use std::ffi::c_char;
use std::sync::{Arc, Mutex};

use datagrep_api::{ConfigValue, Enforcement};
use datagrep_core::{CoreApi, ProfileId, QueryId};
use datagrep_profiles::Store;
use datagrep_secrets::{SecretRef, SecretResolver};

use crate::ffi_util::{guard, guard_quiet};
use crate::runtime::runtime;

/// The engine, as one opaque handle.
#[derive(Debug)]
pub struct DatagrepCore(pub(crate) Arc<CoreInner>);

/// Everything a `DatagrepCore` owns, shared with every query it started.
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
    /// registered at most once per process. `datagrep_profiles_update` calls
    /// [`CoreInner::forget_profile`], which evicts the entry *and* closes the
    /// stale connection pool, so an edited profile takes effect on its next
    /// query instead of after a restart.
    registered: Mutex<HashMap<String, ProfileId>>,
    /// Saved-profile name → the [`Enforcement`] the driver reported the last
    /// time `set_read_only(true)` ran on one of its connections. The UI must
    /// say *which kind* of protection is in force and never imply server
    /// enforcement it does not have. Absent until the first connect.
    enforcement: Mutex<HashMap<String, Enforcement>>,
}

impl std::fmt::Debug for CoreInner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CoreInner")
            .field("api", &self.api)
            .field("registered", &self.lock_registered().len())
            .finish()
    }
}

impl DatagrepCore {
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
            enforcement: Mutex::new(HashMap::new()),
        })))
    }
}

impl CoreInner {
    /// Resolve a saved profile by name, register it with `CoreApi` (once), and
    /// return its id and driver — the FFI's equivalent of `datagrep-cli`'s
    /// `open_profile`.
    ///
    /// **CoreApi gap, worked around here.**
    /// `datagrep_core::session::Session::acquire` always builds
    /// `ResolvedConfig::without_secrets(...)`: there is no seam for a frontend
    /// to hand a resolved secret to a running session. So the secret is
    /// resolved here and folded into the plaintext `ConnectionConfig` — the
    /// only way a driver in this build ever sees it. The on-disk profile still
    /// never holds it, but the resolved value sits in an un-zeroized `String`
    /// for the life of this `CoreApi` profile, weaker than the `SecretString`
    /// guarantee it started as. Same trade `datagrep-cli` documents.
    pub(crate) async fn open_profile(
        &self,
        name: &str,
    ) -> Result<(ProfileId, datagrep_profiles::Profile), String> {
        let profile = self.saved_profile(name).await?;
        if let Some(id) = self.lock_registered().get(name).copied() {
            return Ok((id, profile));
        }

        let mut config = profile.config.clone();
        if let Some(secret_ref) = &profile.secret_ref {
            let reference: SecretRef = secret_ref
                .parse()
                .map_err(|e: datagrep_secrets::SecretError| e.to_string())?;
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
            .add_profile_full(datagrep_core::Profile {
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
        let id = *self.lock_registered().entry(name.to_string()).or_insert(id);

        let _ = self.store.touch_profile_last_used(profile.id.clone()).await;
        Ok((id, profile))
    }

    /// Dispatch one request, honouring the profile's read-only flag.
    ///
    /// For a read-only profile the lease is acquired here (instead of inside
    /// `CoreApi::run_query`) so `set_read_only(true)` runs **on the exact
    /// connection that will execute the statement** — a pooled socket opened
    /// by an earlier writeable use, or a brand-new dial, gets the server-side
    /// guard either way, every time. The driver's honest answer
    /// ([`Enforcement`]) is recorded per profile for
    /// `datagrep_connection_info_json` / the query status JSON to report.
    pub(crate) async fn run_request(
        &self,
        id: ProfileId,
        name: &str,
        read_only: bool,
        req: datagrep_api::Request,
    ) -> Result<QueryId, String> {
        if !read_only {
            return self.api.run_query(id, req).await.map_err(|e| e.to_string());
        }
        let session = self.api.session(id).map_err(|e| e.to_string())?;
        let lease = session.acquire().await.map_err(|e| e.to_string())?;
        match lease.set_read_only(true).await {
            Ok(enforcement) => {
                self.lock_enforcement()
                    .insert(name.to_string(), enforcement);
            }
            Err(_) => {
                // The server half could not be (re)confirmed on this socket.
                // Never keep claiming `Server` from an earlier connection: a
                // read-only badge that over-promises is worse than no badge,
                // because the user relaxes on the strength of it. The
                // client-side classifier already vetted this statement, so
                // running it is still safe; only the claim has to come down.
                self.lock_enforcement()
                    .insert(name.to_string(), Enforcement::Client);
            }
        }
        self.api
            .queries()
            .run(lease, req)
            .await
            .map_err(|e| e.to_string())
    }

    /// The last [`Enforcement`] a live connection of this profile reported,
    /// if it has connected at all since this process started.
    pub(crate) fn enforcement_for(&self, name: &str) -> Option<Enforcement> {
        self.lock_enforcement().get(name).copied()
    }

    pub(crate) async fn saved_profile(
        &self,
        name: &str,
    ) -> Result<datagrep_profiles::Profile, String> {
        self.store
            .list_profiles(None)
            .await
            .map_err(|e| format!("could not read the profile store: {e}"))?
            .into_iter()
            .find(|p| p.name == name)
            .ok_or_else(|| format!("no profile named `{name}`"))
    }

    /// Drop the name → `ProfileId` mapping *and* close the stale pool it
    /// pointed at, so an edited or removed profile stops answering with its
    /// old configuration on the very next query.
    pub(crate) fn forget_profile(&self, name: &str) {
        let stale = self.lock_registered().remove(name);
        self.lock_enforcement().remove(name);
        if let Some(id) = stale {
            let api = self.api.clone();
            if let Ok(rt) = runtime() {
                rt.spawn(async move { api.disconnect(id).await });
            }
        }
    }

    fn lock_registered(&self) -> std::sync::MutexGuard<'_, HashMap<String, ProfileId>> {
        self.registered
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn lock_enforcement(&self) -> std::sync::MutexGuard<'_, HashMap<String, Enforcement>> {
        self.enforcement
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

/// The read-only story of one profile, as one JSON value the ABI can embed in
/// both `datagrep_query_status_json` and `datagrep_connection_info_json`:
///
/// ```json
/// null                                                  // profile is not read-only
/// {"enforcement":"server"|"client"|"none",
///  "server_confirmed":bool}                             // profile is read-only
/// ```
///
/// `enforcement` is the strongest thing the badge may honestly claim:
/// - `"server"` — a live connection of this profile accepted a server-side
///   read-only session (PG `SET SESSION … READ ONLY`, SQLite
///   `PRAGMA query_only`, …). `server_confirmed` is `true`.
/// - `"client"` — only this process stands in the way: statements classified
///   `Write`/`Ddl`/`Admin` by `datagrep-lang` are refused before dispatch, but
///   the server itself would accept a write (Redis, or not yet connected).
/// - `"none"` — nothing enforces it and the UI must say so (an engine this
///   build cannot classify statements for).
pub(crate) fn read_only_json(
    read_only: bool,
    driver_id: &str,
    reported: Option<Enforcement>,
) -> serde_json::Value {
    if !read_only {
        return serde_json::Value::Null;
    }
    let client_guard = crate::query::language_for_driver(driver_id).is_some();
    let label = match reported {
        Some(Enforcement::Server) => "server",
        Some(Enforcement::Client) | Some(Enforcement::None) | None if client_guard => "client",
        Some(Enforcement::Client) => "client",
        _ => "none",
    };
    serde_json::json!({
        "enforcement": label,
        "server_confirmed": matches!(reported, Some(Enforcement::Server)),
    })
}

pub(crate) fn map_env(env: datagrep_profiles::Env) -> datagrep_core::api::Env {
    match env {
        datagrep_profiles::Env::Dev => datagrep_core::api::Env::Dev,
        datagrep_profiles::Env::Staging => datagrep_core::api::Env::Staging,
        datagrep_profiles::Env::Prod => datagrep_core::api::Env::Prod,
    }
}

/// Borrow a `DatagrepCore*` argument as the shared guts every entry point works
/// against.
///
/// # Safety
/// `core` must be a pointer returned by `datagrep_core_new` and not yet freed.
pub(crate) unsafe fn core_ref<'a>(core: *mut DatagrepCore) -> Result<&'a Arc<CoreInner>, String> {
    if core.is_null() {
        return Err("DatagrepCore* must not be NULL".to_string());
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
pub unsafe extern "C" fn datagrep_core_new(
    profiles_db_path: *const c_char,
    err_out: *mut *mut c_char,
) -> *mut DatagrepCore {
    guard(err_out, std::ptr::null_mut(), "datagrep_core_new", || {
        let path = crate::ffi_util::cstr(profiles_db_path, "profiles_db_path")?;
        let store = if path.is_empty() || path == ":memory:" {
            Store::open_in_memory()
        } else {
            Store::open(path)
        };
        let core = DatagrepCore::with_store(store)?;
        Ok(Box::into_raw(Box::new(core)))
    })
}

/// Stop every query, close every socket, and free the handle.
///
/// Any `DatagrepQuery` still alive keeps the engine alive until it too is freed —
/// but its queries have already been stopped by the `shutdown` below, which is
/// what "free the core" means.
///
/// # Safety
/// `core` must be a pointer from `datagrep_core_new`, freed at most once.
#[no_mangle]
pub unsafe extern "C" fn datagrep_core_free(core: *mut DatagrepCore) {
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
pub unsafe extern "C" fn datagrep_string_free(s: *mut c_char) {
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
        let core = unsafe { datagrep_core_new(path.as_ptr(), &mut err) };
        assert!(!core.is_null());
        assert!(err.is_null(), "err_out must be NULL on success");
        unsafe { datagrep_core_free(core) };
    }

    #[test]
    fn a_null_path_is_an_error_not_a_crash() {
        let mut err: *mut c_char = std::ptr::null_mut();
        let core = unsafe { datagrep_core_new(std::ptr::null(), &mut err) };
        assert!(core.is_null());
        assert!(!err.is_null());
        unsafe { datagrep_string_free(err) };
    }

    #[test]
    fn freeing_null_handles_is_a_no_op() {
        unsafe {
            datagrep_core_free(std::ptr::null_mut());
            datagrep_string_free(std::ptr::null_mut());
        }
    }
}
