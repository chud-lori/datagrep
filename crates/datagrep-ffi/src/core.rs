use std::collections::HashMap;
use std::ffi::c_char;
use std::sync::{Arc, Mutex};

use datagrep_api::caps::Caps;
use datagrep_api::{ConfigValue, Enforcement};
use datagrep_core::session::ConnLease;
use datagrep_core::{CoreApi, ProfileId, QueryId};
use datagrep_profiles::Store;
use datagrep_secrets::{SecretRef, SecretResolver};

use crate::ffi_util::{guard, guard_quiet};
use crate::runtime::runtime;

#[derive(Debug)]
pub struct DatagrepCore(pub(crate) Arc<CoreInner>);

pub(crate) struct CoreInner {
    pub(crate) api: Arc<CoreApi>,
    pub(crate) store: Arc<Store>,
    pub(crate) secrets: Arc<SecretResolver>,
    registered: Mutex<HashMap<String, ProfileId>>,
    enforcement: Mutex<HashMap<String, Enforcement>>,
    server: Mutex<HashMap<String, (String, String)>>,
    caps: Mutex<HashMap<String, Caps>>,
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
    pub fn with_store(store: Store) -> Result<Self, String> {
        Self::build(store, SecretResolver::new())
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn with_store_in_memory_secrets(store: Store) -> Result<Self, String> {
        Self::build(store, SecretResolver::in_memory())
    }

    fn build(store: Store, secrets: SecretResolver) -> Result<Self, String> {
        let rt = runtime()?;
        let _guard = rt.enter();
        let api = CoreApi::new();
        crate::drivers::register_drivers(&api);
        Ok(Self(Arc::new(CoreInner {
            api: Arc::new(api),
            store: Arc::new(store),
            secrets: Arc::new(secrets),
            registered: Mutex::new(HashMap::new()),
            enforcement: Mutex::new(HashMap::new()),
            server: Mutex::new(HashMap::new()),
            caps: Mutex::new(HashMap::new()),
        })))
    }
}

impl CoreInner {
    pub(crate) async fn open_profile(
        &self,
        name: &str,
    ) -> Result<(ProfileId, datagrep_profiles::Profile), String> {
        let profile = self.saved_profile(name).await?;
        if let Some(id) = self.lock_registered().get(name).copied() {
            return Ok((id, profile));
        }

        let config = self.plaintext_config(&profile).await?;

        let id = self
            .api
            .add_profile_full(datagrep_core::Profile {
                id: ProfileId(0), // overwritten by add_profile_full
                name: Arc::from(profile.name.as_str()),
                driver: Arc::from(profile.driver_id.as_str()),
                config,
                read_only: profile.read_only,
                safety: profile.safety,
            })
            .await;

        let id = *self.lock_registered().entry(name.to_string()).or_insert(id);

        let _ = self.store.touch_profile_last_used(profile.id.clone()).await;
        Ok((id, profile))
    }

    pub(crate) async fn plaintext_config(
        &self,
        profile: &datagrep_profiles::Profile,
    ) -> Result<datagrep_api::ConnectionConfig, String> {
        let mut config = profile.config.clone();
        let Some(secret_ref) = &profile.secret_ref else {
            return Ok(config);
        };
        let reference: SecretRef = secret_ref
            .parse()
            .map_err(|e: datagrep_secrets::SecretError| e.to_string())?;
        let secret = self
            .secrets
            .resolve(&reference)
            .await
            .map_err(|e| format!("could not resolve the secret for `{}`: {e}", profile.name))?;
        if let Some(driver) = crate::drivers::driver_for(&profile.driver_id) {
            if let Some(field) = driver.config_schema().fields.iter().find(|f| f.secret) {
                config.values.insert(
                    field.key.to_string(),
                    ConfigValue::Str(secret.expose().to_string()),
                );
            }
        }
        Ok(config)
    }

    // Returns the DbError, not its text: a safety refusal carries the challenge the caller must clear.
    pub(crate) async fn run_request(
        &self,
        id: ProfileId,
        name: &str,
        read_only: bool,
        req: datagrep_api::Request,
    ) -> Result<QueryId, datagrep_api::DbError> {
        let session = self.api.session(id)?;
        let lease = session.acquire().await?;
        self.record_server_info(name, lease.server_info());
        self.record_caps(name, lease.capabilities().flags);
        if !read_only {
            return self.api.queries().run(lease, req).await;
        }
        // set_read_only runs on this exact socket — read-only is per-connection, not per-request; on failure never keep claiming Enforcement::Server from an earlier connection.
        match lease.set_read_only(true).await {
            Ok(enforcement) => {
                self.lock_enforcement()
                    .insert(name.to_string(), enforcement);
            }
            Err(_) => {
                self.lock_enforcement()
                    .insert(name.to_string(), Enforcement::Client);
            }
        }
        self.api.queries().run(lease, req).await
    }

    pub(crate) async fn leased(
        &self,
        profile: &str,
    ) -> Result<(ConnLease, datagrep_profiles::Profile), String> {
        let (id, saved) = self.open_profile(profile).await?;
        let session = self.api.session(id).map_err(|e| e.to_string())?;
        let lease = session.acquire().await.map_err(|e| e.to_string())?;
        self.record_server_info(profile, lease.server_info());
        self.record_caps(profile, lease.capabilities().flags);
        if saved.read_only {
            match lease.set_read_only(true).await {
                Ok(enforcement) => self.record_enforcement(profile, enforcement),
                Err(_) => self.record_enforcement(profile, Enforcement::Client),
            }
        }
        Ok((lease, saved))
    }

    pub(crate) fn enforcement_for(&self, name: &str) -> Option<Enforcement> {
        self.lock_enforcement().get(name).copied()
    }

    pub(crate) fn record_enforcement(&self, name: &str, enforcement: Enforcement) {
        self.lock_enforcement()
            .insert(name.to_string(), enforcement);
    }

    pub(crate) fn server_info_for(&self, name: &str) -> Option<(String, String)> {
        self.lock_server().get(name).cloned()
    }

    pub(crate) fn caps_for(&self, name: &str) -> Option<Caps> {
        self.lock_caps().get(name).copied()
    }

    pub(crate) fn record_caps(&self, name: &str, caps: Caps) {
        self.lock_caps().insert(name.to_string(), caps);
    }

    pub(crate) fn record_server_info(&self, name: &str, info: &datagrep_api::ServerInfo) {
        self.lock_server().insert(
            name.to_string(),
            (info.product.to_string(), info.version.to_string()),
        );
    }

    pub(crate) async fn ensure_server_info(&self, name: &str) -> Option<(String, String)> {
        if let Some(hit) = self.server_info_for(name) {
            return Some(hit);
        }
        let (id, _) = self.open_profile(name).await.ok()?;
        let session = self.api.session(id).ok()?;
        let lease = session.acquire().await.ok()?;
        self.record_server_info(name, lease.server_info());
        self.server_info_for(name)
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

    pub(crate) fn forget_profile(&self, name: &str) {
        let stale = self.lock_registered().remove(name);
        self.lock_enforcement().remove(name);
        self.lock_server().remove(name);
        self.lock_caps().remove(name);
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

    fn lock_server(&self) -> std::sync::MutexGuard<'_, HashMap<String, (String, String)>> {
        self.server
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn lock_enforcement(&self) -> std::sync::MutexGuard<'_, HashMap<String, Enforcement>> {
        self.enforcement
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn lock_caps(&self) -> std::sync::MutexGuard<'_, HashMap<String, Caps>> {
        self.caps
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

pub fn read_only_json(
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

pub(crate) unsafe fn core_ref<'a>(core: *mut DatagrepCore) -> Result<&'a Arc<CoreInner>, String> {
    if core.is_null() {
        return Err("DatagrepCore* must not be NULL".to_string());
    }
    // SAFETY: non-NULL (checked) and a live handle per the contract; the unbound 'a is dropped before returning to C.
    Ok(unsafe { &(*core).0 })
}

// ---- lifecycle ---------------------------------------------------------

/// # Safety
/// `profiles_db_path` is NULL or NUL-terminated; `err_out` is NULL or a writable slot.
#[no_mangle]
pub unsafe extern "C" fn datagrep_core_new(
    profiles_db_path: *const c_char,
    err_out: *mut *mut c_char,
) -> *mut DatagrepCore {
    guard(err_out, std::ptr::null_mut(), "datagrep_core_new", || {
        // SAFETY: NULL or NUL-terminated per the contract; cstr errors on NULL and non-UTF-8.
        let path = unsafe { crate::ffi_util::cstr(profiles_db_path, "profiles_db_path") }?;
        let store = if path.is_empty() || path == ":memory:" {
            Store::open_in_memory()
        } else {
            Store::open(path)
        };
        let core = DatagrepCore::with_store(store)?;
        Ok(Box::into_raw(Box::new(core)))
    })
}

/// # Safety
/// `core` is NULL or an unfreed handle from `datagrep_core_new`; a second call is a double free.
#[no_mangle]
pub unsafe extern "C" fn datagrep_core_free(core: *mut DatagrepCore) {
    guard_quiet((), || {
        if core.is_null() {
            return;
        }
        // SAFETY: non-NULL (checked) and unfreed per the contract; a second call would be a double free.
        let core = unsafe { Box::from_raw(core) };
        if let Ok(rt) = runtime() {
            rt.block_on(core.0.api.shutdown());
        }
        drop(core);
    })
}

/// # Safety
/// `s` is NULL or an unfreed string allocated by this library.
#[no_mangle]
pub unsafe extern "C" fn datagrep_string_free(s: *mut c_char) {
    guard_quiet((), || {
        if !s.is_null() {
            // SAFETY: every char* leaving this crate comes from CString::into_raw, so from_raw is the matching free.
            drop(unsafe { std::ffi::CString::from_raw(s) });
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
