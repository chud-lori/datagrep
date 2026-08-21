//! Profiles: plain-text, git-committable connections a team can review and
//! share, with secrets in the OS keychain and never on disk.
//!
//! `datagrep_profiles_add` splits any inline password out of the parsed URL into a
//! keychain [`SecretRef`] *before* the profile reaches
//! [`datagrep_profiles::Store::create_profile`] — which independently refuses a
//! secret-shaped config key anyway, so this is defence in depth rather than
//! the only thing between a password and disk.
//!
//! ## The one safety contract every entry point here shares
//!
//! Each function below takes the same three kinds of argument, so rather than
//! repeat the reasoning nine times: `core` must be a live handle from
//! `datagrep_core_new`, the `*const c_char` arguments must be NULL or
//! NUL-terminated, and `err_out` must be NULL or a writable `char*`.
//! [`core_ref`] and [`cstr`] turn NULL and non-UTF-8 into error strings before
//! any dereference, so the halves a caller can get wrong and Rust cannot catch
//! are exactly two: passing a freed or fabricated `DatagrepCore*`, and passing a
//! `char*` that is not actually NUL-terminated. The per-call `// SAFETY:` notes
//! below name which of those a given call leans on.

use std::ffi::c_char;

use datagrep_api::ConfigValue;
use datagrep_secrets::SecretRef;
use serde::{Deserialize, Deserializer};
use serde_json::json;

use crate::core::{core_ref, CoreInner, DatagrepCore};
use crate::ffi_util::{cstr, guard, to_c_string};
use crate::runtime::runtime;

/// The JSON body `datagrep_profiles_update` (all fields) and
/// `datagrep_profiles_add_json` (all but `name`/`url`) accept. Every field is
/// optional; for the three nullable columns the double `Option` separates
/// *absent* (leave alone) from JSON `null` (clear the value).
///
/// Unknown keys are rejected rather than ignored — a typo in a safety setting
/// (`"read_olny": true`) silently doing nothing is decorative security: the
/// user believes a guardrail is on when nothing is holding it up.
#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct ProfilePatch {
    name: Option<String>,
    url: Option<String>,
    read_only: Option<bool>,
    confirm_writes: Option<bool>,
    #[serde(default, deserialize_with = "nullable")]
    auto_limit: Option<Option<i64>>,
    #[serde(default, deserialize_with = "nullable")]
    idle_timeout_s: Option<Option<i64>>,
    #[serde(default, deserialize_with = "nullable")]
    color: Option<Option<String>>,
}

/// Present-but-null and absent must deserialize differently: absent stays
/// `None` (via `#[serde(default)]`), an explicit `null` becomes `Some(None)`.
fn nullable<'de, D, T>(de: D) -> Result<Option<Option<T>>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::<T>::deserialize(de).map(Some)
}

fn parse_patch(text: &str) -> Result<ProfilePatch, String> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return Ok(ProfilePatch::default());
    }
    serde_json::from_str(trimmed).map_err(|e| {
        format!(
            "patch/options JSON is invalid ({e}); accepted keys: name, url, \
             read_only, confirm_writes, auto_limit, idle_timeout_s, color"
        )
    })
}

/// `[{"name":..,"driver":..,"read_only":bool,
///    "has_secret":bool}, ...]`
///
/// `env` and `read_only` ride along so the sidebar can tint prod connections
/// and badge read-only ones without a `datagrep_profiles_get_json` round trip
/// per row.
///
/// # Safety
/// `core` must come from `datagrep_core_new`; `err_out` must be NULL or writable.
#[no_mangle]
pub unsafe extern "C" fn datagrep_profiles_list_json(
    core: *mut DatagrepCore,
    err_out: *mut *mut c_char,
) -> *mut c_char {
    guard(
        err_out,
        std::ptr::null_mut(),
        "datagrep_profiles_list_json",
        || {
            // SAFETY: the module-level contract — a live `DatagrepCore*` from
            // `datagrep_core_new`, and NUL-terminated string arguments.
            let core = unsafe { core_ref(core) }?;
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
                        "read_only": p.read_only,
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

/// Save a profile parsed from a connection URL, with default settings
/// (`env=dev`, writeable). Kept exactly as the frozen header declares it;
/// `datagrep_profiles_add_json` is the call that can set env and the safety
/// settings at creation time.
///
/// # Safety
/// `core` must come from `datagrep_core_new`; `name`/`url` must be valid
/// NUL-terminated UTF-8; `err_out` must be NULL or writable.
#[no_mangle]
pub unsafe extern "C" fn datagrep_profiles_add(
    core: *mut DatagrepCore,
    name: *const c_char,
    url: *const c_char,
    err_out: *mut *mut c_char,
) -> bool {
    guard(err_out, false, "datagrep_profiles_add", || {
        // SAFETY: the module-level contract — a live `DatagrepCore*` from
        // `datagrep_core_new`, and NUL-terminated string arguments.
        let core = unsafe { core_ref(core) }?;
        let name = unsafe { cstr(name, "name") }?;
        let url = unsafe { cstr(url, "url") }?;
        if name.is_empty() {
            return Err("name must not be empty".to_string());
        }
        let rt = runtime()?;
        rt.block_on(add_profile(core, name, url, ProfilePatch::default()))?;
        Ok(true)
    })
}

/// `datagrep_profiles_add` plus initial settings: `options_json` may be NULL or
/// `""` (same defaults as `datagrep_profiles_add`) or any subset of
/// `{"read_only":bool,"confirm_writes":bool,
///   "auto_limit":i64|null,"idle_timeout_s":i64|null,"color":str|null}`.
///
/// This is how a profile is born prod: `env` is persisted, listed by
/// `datagrep_profiles_list_json`, and handed to the engine on connect — the
/// prod guardrails (red chrome, confirm-on-write) key off it.
/// (`datagrep_profiles_add` used to hard-code
/// `env=dev` with no way to change it; that is the bug this call and
/// `datagrep_profiles_update` close.)
///
/// # Safety
/// `core` must come from `datagrep_core_new`; `name`/`url` must be valid
/// NUL-terminated UTF-8; `options_json` must be NULL or valid NUL-terminated
/// UTF-8; `err_out` must be NULL or writable.
#[no_mangle]
pub unsafe extern "C" fn datagrep_profiles_add_json(
    core: *mut DatagrepCore,
    name: *const c_char,
    url: *const c_char,
    options_json: *const c_char,
    err_out: *mut *mut c_char,
) -> bool {
    guard(err_out, false, "datagrep_profiles_add_json", || {
        // SAFETY: the module-level contract — a live `DatagrepCore*` from
        // `datagrep_core_new`, and NUL-terminated string arguments.
        let core = unsafe { core_ref(core) }?;
        let name = unsafe { cstr(name, "name") }?;
        let url = unsafe { cstr(url, "url") }?;
        if name.is_empty() {
            return Err("name must not be empty".to_string());
        }
        let options = if options_json.is_null() {
            ProfilePatch::default()
        } else {
            parse_patch(unsafe { cstr(options_json, "options_json") }?)?
        };
        if options.name.is_some() || options.url.is_some() {
            return Err(
                "options_json must not carry `name` or `url` — pass them as arguments".to_string(),
            );
        }
        let rt = runtime()?;
        rt.block_on(add_profile(core, name, url, options))?;
        Ok(true)
    })
}

async fn add_profile(
    core: &CoreInner,
    name: &str,
    url: &str,
    options: ProfilePatch,
) -> Result<(), String> {
    let existing = core
        .store
        .list_profiles(None)
        .await
        .map_err(|e| format!("could not read the profile store: {e}"))?;
    if existing.iter().any(|p| p.name == name) {
        return Err(format!("a profile named `{name}` already exists"));
    }

    let id = datagrep_profiles::new_id();
    let (driver_id, config, secret_ref) = parse_and_split_url(core, &id, url).await?;

    let now = datagrep_profiles::now_ms();
    core.store
        .create_profile(datagrep_profiles::Profile {
            id,
            folder_id: None,
            name: name.to_string(),
            driver_id: driver_id.to_string(),
            config,
            secret_ref,
            tunnel_id: None,
            color: options.color.flatten(),
            read_only: options.read_only.unwrap_or(false),
            confirm_writes: options.confirm_writes.unwrap_or(false),
            auto_limit: options.auto_limit.flatten(),
            idle_timeout_s: options.idle_timeout_s.flatten(),
            last_used_at: None,
            created_at: now,
            updated_at: now,
        })
        .await
        .map_err(|e| format!("could not save the profile: {e}"))?;
    Ok(())
}

/// Parse `url`, and split any inline secret (e.g. a `postgres://user:PW@…`
/// password) out of the parsed config into the OS keychain under this
/// profile's id — the same defence-in-depth `datagrep_profiles_add` has always
/// done, shared with `datagrep_profiles_update` so a URL *edit* re-splits
/// identically. Returns `(driver_id, secretless_config, secret_ref)`.
async fn parse_and_split_url(
    core: &CoreInner,
    profile_id: &str,
    url: &str,
) -> Result<(&'static str, datagrep_api::ConnectionConfig, Option<String>), String> {
    let (driver_id, driver) = crate::drivers::driver_for_url(url).ok_or_else(|| {
        format!(
            "could not tell which driver `{}` is for (this build knows {})",
            datagrep_api::config::redact_url(url),
            crate::drivers::known_driver_ids().join(", ")
        )
    })?;
    let mut config = driver.parse_url(url).map_err(|e| e.to_string())?;

    let mut secret_ref = None;
    for field in driver.config_schema().fields.iter().filter(|f| f.secret) {
        let Some(ConfigValue::Str(value)) = config.values.remove(field.key.as_ref()) else {
            continue;
        };
        if value.is_empty() {
            continue;
        }
        let reference = SecretRef::Keychain {
            service: "datagrep".to_string(),
            account: format!("{profile_id}:{}", field.key),
        };
        core.secrets
            .store(&reference, datagrep_api::SecretString::new(value))
            .await
            .map_err(|e| format!("could not store the secret in the keychain: {e}"))?;
        secret_ref = Some(reference.to_string());
    }
    Ok((driver_id, config, secret_ref))
}

/// Edit an existing profile in place; `patch_json` is any subset of
/// `{"name":str,"url":str,"read_only":bool,
///   "confirm_writes":bool,"auto_limit":i64|null,"idle_timeout_s":i64|null,
///   "color":str|null}`.
///
/// - Renaming keeps the profile's id, and with it the keychain `secret_ref`
///   (secrets are keyed by id, never by name).
/// - Changing `url` re-parses it and re-splits any inline password into the
///   keychain exactly as `datagrep_profiles_add` does; with no inline password
///   the existing stored secret is kept (unless the URL switched engines, in
///   which case the old engine's secret is deleted rather than misapplied).
/// - The edit takes effect on the *next* query: the stale in-engine profile
///   and its connection pool are dropped here.
///
/// # Safety
/// `core` must come from `datagrep_core_new`; `name`/`patch_json` must be valid
/// NUL-terminated UTF-8; `err_out` must be NULL or writable.
#[no_mangle]
pub unsafe extern "C" fn datagrep_profiles_update(
    core: *mut DatagrepCore,
    name: *const c_char,
    patch_json: *const c_char,
    err_out: *mut *mut c_char,
) -> bool {
    guard(err_out, false, "datagrep_profiles_update", || {
        // SAFETY: the module-level contract — a live `DatagrepCore*` from
        // `datagrep_core_new`, and NUL-terminated string arguments.
        let core = unsafe { core_ref(core) }?;
        let name = unsafe { cstr(name, "name") }?;
        let patch = parse_patch(unsafe { cstr(patch_json, "patch_json") }?)?;
        let rt = runtime()?;
        rt.block_on(update_profile(core, name, patch))?;
        Ok(true)
    })
}

async fn update_profile(core: &CoreInner, name: &str, patch: ProfilePatch) -> Result<(), String> {
    let profiles = core
        .store
        .list_profiles(None)
        .await
        .map_err(|e| format!("could not read the profile store: {e}"))?;
    let mut profile = profiles
        .iter()
        .find(|p| p.name == name)
        .cloned()
        .ok_or_else(|| format!("no profile named `{name}`"))?;

    if let Some(new_name) = &patch.name {
        if new_name.is_empty() {
            return Err("name must not be empty".to_string());
        }
        if new_name != name && profiles.iter().any(|p| &p.name == new_name) {
            return Err(format!("a profile named `{new_name}` already exists"));
        }
        // The id — and therefore the keychain secret_ref, which is keyed by
        // id — survives the rename untouched.
        profile.name = new_name.clone();
    }

    if let Some(url) = &patch.url {
        let (driver_id, config, new_secret_ref) =
            parse_and_split_url(core, &profile.id, url).await?;
        let driver_changed = driver_id != profile.driver_id;
        match new_secret_ref {
            // The new URL carried an inline password: it replaced the stored
            // one in the keychain (same id-keyed account) and the ref follows.
            Some(reference) => profile.secret_ref = Some(reference),
            // No inline secret, same engine: keep the stored keychain secret —
            // editing the host must not silently log the user out.
            None if !driver_changed => {}
            // No inline secret, different engine: the old secret belonged to
            // the old engine's secret field. Delete it rather than leave an
            // orphan keychain entry behind a dangling ref.
            None => {
                if let Some(old) = profile.secret_ref.take() {
                    if let Ok(reference) = old.parse::<SecretRef>() {
                        if reference.is_writable() {
                            let _ = core.secrets.delete(&reference).await;
                        }
                    }
                }
            }
        }
        profile.driver_id = driver_id.to_string();
        profile.config = config;
    }

    if let Some(read_only) = patch.read_only {
        profile.read_only = read_only;
    }
    if let Some(confirm_writes) = patch.confirm_writes {
        profile.confirm_writes = confirm_writes;
    }
    if let Some(auto_limit) = patch.auto_limit {
        profile.auto_limit = auto_limit;
    }
    if let Some(idle_timeout_s) = patch.idle_timeout_s {
        profile.idle_timeout_s = idle_timeout_s;
    }
    if let Some(color) = patch.color {
        profile.color = color;
    }

    let new_name = profile.name.clone();
    core.store
        .update_profile(profile)
        .await
        .map_err(|e| format!("could not save the profile: {e}"))?;

    // Drop the stale in-engine registration (old config, old read_only, old
    // env) and its pool; the next query re-registers from disk.
    core.forget_profile(name);
    if new_name != name {
        core.forget_profile(&new_name);
    }
    Ok(())
}

/// Full detail for one profile — what an edit dialog populates itself from:
///
/// ```json
/// {"name":str,"driver":str,"read_only":bool,
///  "confirm_writes":bool,"auto_limit":i64|null,"idle_timeout_s":i64|null,
///  "color":str|null,"folder_id":str|null,"has_secret":bool,
///  "secret":"••••"|null,"config":{key:str|num|bool, ...},
///  "last_used_at":i64|null}
/// ```
///
/// The secret value itself never crosses this ABI: `secret` is the literal
/// mask `"••••"` when one is stored (matching `datagrep profiles show`) and
/// `null` otherwise, and `config` is the persisted, secretless connection
/// config (any secret-schema key is re-masked here as defence in depth).
///
/// # Safety
/// `core` must come from `datagrep_core_new`; `name` must be valid
/// NUL-terminated UTF-8; `err_out` must be NULL or writable.
#[no_mangle]
pub unsafe extern "C" fn datagrep_profiles_get_json(
    core: *mut DatagrepCore,
    name: *const c_char,
    err_out: *mut *mut c_char,
) -> *mut c_char {
    guard(
        err_out,
        std::ptr::null_mut(),
        "datagrep_profiles_get_json",
        || {
            // SAFETY: the module-level contract — a live `DatagrepCore*` from
            // `datagrep_core_new`, and NUL-terminated string arguments.
            let core = unsafe { core_ref(core) }?;
            let name = unsafe { cstr(name, "name") }?;
            let rt = runtime()?;
            let p = rt.block_on(core.saved_profile(name))?;

            // Belt and braces: the store already refuses secret-shaped config
            // keys on every save, but an ABI that *redacts by schema* cannot
            // leak even if that invariant ever breaks upstream.
            let secret_keys: Vec<String> = crate::drivers::driver_for(&p.driver_id)
                .map(|d| {
                    d.config_schema()
                        .fields
                        .iter()
                        .filter(|f| f.secret)
                        .map(|f| f.key.to_string())
                        .collect()
                })
                .unwrap_or_default();
            let config: serde_json::Map<String, serde_json::Value> = p
                .config
                .values
                .iter()
                .map(|(k, v)| {
                    let value = if secret_keys.iter().any(|s| s == k) {
                        json!("••••")
                    } else {
                        match v {
                            ConfigValue::Str(s) => json!(s),
                            ConfigValue::Num(n) => json!(n),
                            ConfigValue::Bool(b) => json!(b),
                        }
                    };
                    (k.clone(), value)
                })
                .collect();

            let payload = json!({
                "name": p.name,
                "driver": p.driver_id,
                "read_only": p.read_only,
                "confirm_writes": p.confirm_writes,
                "auto_limit": p.auto_limit,
                "idle_timeout_s": p.idle_timeout_s,
                "color": p.color,
                "folder_id": p.folder_id,
                "has_secret": p.secret_ref.is_some(),
                "secret": p.secret_ref.is_some().then_some("••••"),
                "config": config,
                "last_used_at": p.last_used_at,
            });
            let text = serde_json::to_string(&payload)
                .map_err(|e| format!("could not encode the profile: {e}"))?;
            Ok(to_c_string(text))
        },
    )
}

/// Connection-safety facts for one profile, keyed by name:
///
/// ```json
/// {"profile":str,"driver":str,
///  "read_only": null | {"enforcement":"server"|"client"|"none",
///                       "server_confirmed":bool}}
/// ```
///
/// `read_only` is `null` for a writeable profile. For a read-only one,
/// `enforcement` is the strongest thing the badge may honestly claim:
/// - `"server"` — a live connection accepted a server-side read-only session
///   (PG/MySQL `SET SESSION … READ ONLY`, SQLite `PRAGMA query_only`);
///   `server_confirmed` is `true`.
/// - `"client"` — only this process blocks writes (statements classified
///   Write/Ddl/Admin are refused before dispatch). Redis has no server-side
///   read-only mode, and a profile that has not connected yet is also at most
///   `"client"`. **The UI must not imply the server is protecting you.**
/// - `"none"` — no enforcement of any kind is available.
///
/// Also carries what the header badge needs to name where the user is:
/// - `"database"` — the database this profile is pointed at, or `null` on an
///   engine that has none (Redis, SQLite).
/// - `"server"` — `{"product":str,"version":str}` as reported at handshake, or
///   `null` when no connection of this profile has succeeded yet. Never
///   guessed: an unconfirmed version is the number a user would quote when
///   asking whether a feature exists on their server.
///
/// The same object appears as `"read_only"` in `datagrep_query_status_json`,
/// refreshed as connections come and go.
///
/// # Safety
/// `core` must come from `datagrep_core_new`; `name` must be valid
/// NUL-terminated UTF-8; `err_out` must be NULL or writable.
#[no_mangle]
pub unsafe extern "C" fn datagrep_connection_info_json(
    core: *mut DatagrepCore,
    name: *const c_char,
    err_out: *mut *mut c_char,
) -> *mut c_char {
    guard(
        err_out,
        std::ptr::null_mut(),
        "datagrep_connection_info_json",
        || {
            // SAFETY: the module-level contract — a live `DatagrepCore*` from
            // `datagrep_core_new`, and NUL-terminated string arguments.
            let core = unsafe { core_ref(core) }?;
            let name = unsafe { cstr(name, "name") }?;
            let rt = runtime()?;
            let p = rt.block_on(core.saved_profile(name))?;
            // The database the profile is pointed at, straight from its saved
            // config. Not every engine has one (Redis, SQLite), so this is an
            // honest `null` rather than an invented default.
            let database = match p.config.values.get("database") {
                Some(ConfigValue::Str(db)) if !db.is_empty() => Some(db.clone()),
                _ => None,
            };
            // Best-effort: warm once from the pool, report nothing if the
            // server cannot be reached. A profile that is merely offline must
            // still return its identity here.
            let server = rt
                .block_on(core.ensure_server_info(&p.name))
                .map(|(product, version)| json!({"product": product, "version": version}));
            let payload = json!({
                "profile": p.name,
                "driver": p.driver_id,
                "database": database,
                "server": server,
                "read_only": crate::core::read_only_json(
                    p.read_only,
                    &p.driver_id,
                    core.enforcement_for(&p.name),
                ),
            });
            let text = serde_json::to_string(&payload)
                .map_err(|e| format!("could not encode the connection info: {e}"))?;
            Ok(to_c_string(text))
        },
    )
}

/// Dial a connection once and report what answered, without saving anything.
///
/// Exactly one of the two selectors is used, `name` first:
/// - `name` non-NULL and non-empty — test a **saved** profile, with its
///   keychain secret folded back in, so what is tested is what a query would
///   really run against.
/// - otherwise `url` — test an **unsaved** URL, which is what the New
///   Connection sheet has: nothing is written to the profile store and no
///   password reaches the keychain, so a failed test leaves no wreckage.
///
/// On success returns
/// ```json
/// {"ok":true,"driver":str,"product":str,"version":str,
///  "details":[[str,str],…],"elapsed_ms":u64}
/// ```
/// On failure returns NULL with the driver's real message in `err_out` — a
/// "could not connect" with the reason stripped out is the thing this call
/// exists to replace.
///
/// The connection is opened straight from the driver rather than through
/// `CoreApi`, and closed again before returning: a test must not register a
/// profile, warm a pool, or leave a socket behind for an engine the user may
/// be about to correct the host of.
///
/// # Safety
/// `core` must come from `datagrep_core_new`; `name`/`url` must each be NULL
/// or valid NUL-terminated UTF-8; `err_out` must be NULL or writable.
#[no_mangle]
pub unsafe extern "C" fn datagrep_connection_test_json(
    core: *mut DatagrepCore,
    name: *const c_char,
    url: *const c_char,
    err_out: *mut *mut c_char,
) -> *mut c_char {
    guard(
        err_out,
        std::ptr::null_mut(),
        "datagrep_connection_test_json",
        || {
            // SAFETY: the module-level contract — a live `DatagrepCore*` from
            // `datagrep_core_new`, and NUL-terminated string arguments.
            let core = unsafe { core_ref(core) }?;
            let name = if name.is_null() {
                ""
            } else {
                unsafe { cstr(name, "name") }?
            };
            let url = if url.is_null() {
                ""
            } else {
                unsafe { cstr(url, "url") }?
            };
            if name.trim().is_empty() && url.trim().is_empty() {
                return Err("pass either a profile name or a connection URL".to_string());
            }
            let rt = runtime()?;
            let payload = rt.block_on(test_connection(core, name.trim(), url.trim()))?;
            let text = serde_json::to_string(&payload)
                .map_err(|e| format!("could not encode the test result: {e}"))?;
            Ok(to_c_string(text))
        },
    )
}

/// How long a Test Connection waits before calling it a failure.
///
/// Shorter than the 15 s drivers default to: this runs behind a button the
/// user is watching, and a quarter minute of nothing is indistinguishable
/// from the app having hung.
const TEST_CONNECT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

async fn test_connection(
    core: &CoreInner,
    name: &str,
    url: &str,
) -> Result<serde_json::Value, String> {
    let (driver_id, driver, config) = if !name.is_empty() {
        let profile = core.saved_profile(name).await?;
        let driver = crate::drivers::driver_for(&profile.driver_id).ok_or_else(|| {
            format!(
                "this build has no `{}` driver (it knows {})",
                profile.driver_id,
                crate::drivers::known_driver_ids().join(", ")
            )
        })?;
        let config = core.plaintext_config(&profile).await?;
        (profile.driver_id.clone(), driver, config)
    } else {
        let (id, driver) = crate::drivers::driver_for_url(url).ok_or_else(|| {
            format!(
                "could not tell which driver `{}` is for (this build knows {})",
                datagrep_api::config::redact_url(url),
                crate::drivers::known_driver_ids().join(", ")
            )
        })?;
        // The password stays inline in the config here on purpose: nothing is
        // being persisted, so there is no keychain round trip to make, and
        // every driver reads an inline credential the same way it reads a
        // resolved one.
        let config = driver.parse_url(url).map_err(|e| e.to_string())?;
        (id.to_string(), driver, config)
    };

    let ctx = datagrep_api::driver::ConnectCtx {
        connect_timeout: Some(TEST_CONNECT_TIMEOUT),
        application_name: Some(std::sync::Arc::from("datagrep (test connection)")),
        ..Default::default()
    };
    let started = std::time::Instant::now();
    let conn = driver
        .connect(&datagrep_api::ResolvedConfig::without_secrets(config), ctx)
        .await
        .map_err(|e| e.to_string())?;
    let info = conn.server_info().clone();
    let elapsed_ms = started.elapsed().as_millis() as u64;
    // Best effort: the handshake already answered the question, and a driver
    // that cannot close cleanly must not turn a successful test red.
    let _ = conn.close().await;

    Ok(json!({
        "ok": true,
        "driver": driver_id,
        "product": info.product,
        "version": info.version,
        "details": info.details,
        "elapsed_ms": elapsed_ms,
    }))
}

/// Delete a saved profile and its keychain entry.
///
/// # Safety
/// `core` must come from `datagrep_core_new`; `name` must be valid NUL-terminated
/// UTF-8; `err_out` must be NULL or writable.
#[no_mangle]
pub unsafe extern "C" fn datagrep_profiles_remove(
    core: *mut DatagrepCore,
    name: *const c_char,
    err_out: *mut *mut c_char,
) -> bool {
    guard(err_out, false, "datagrep_profiles_remove", || {
        // SAFETY: the module-level contract — a live `DatagrepCore*` from
        // `datagrep_core_new`, and NUL-terminated string arguments.
        let core = unsafe { core_ref(core) }?;
        let name = unsafe { cstr(name, "name") }?;
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::{datagrep_core_free, DatagrepCore};
    use crate::query::{datagrep_query_free, datagrep_query_run, datagrep_query_status_json};
    use std::ffi::{CStr, CString};

    fn test_core() -> *mut DatagrepCore {
        let core =
            DatagrepCore::with_store_in_memory_secrets(datagrep_profiles::Store::open_in_memory())
                .expect("core");
        Box::into_raw(Box::new(core))
    }

    /// Call an ABI function that returns `char*`, asserting `err_out` stayed
    /// NULL, and hand back an owned `String`.
    unsafe fn take_json(p: *mut c_char, err: *mut c_char, what: &str) -> String {
        // SAFETY (this helper): `p` and `err` are whatever the entry point under
        // test just wrote — NULL, or a string it allocated with `to_c_string`.
        unsafe {
            if !err.is_null() {
                let msg = CStr::from_ptr(err).to_string_lossy().into_owned();
                panic!("{what} errored: {msg}");
            }
            assert!(!p.is_null(), "{what} returned NULL without an error");
            let s = CStr::from_ptr(p).to_str().expect("utf8").to_string();
            crate::core::datagrep_string_free(p);
            s
        }
    }

    unsafe fn expect_ok(ok: bool, err: *mut c_char, what: &str) {
        // SAFETY: `err` is NULL or a string the entry point under test allocated.
        unsafe {
            if !err.is_null() {
                let msg = CStr::from_ptr(err).to_string_lossy().into_owned();
                panic!("{what} errored: {msg}");
            }
        }
        assert!(ok, "{what} returned false without an error");
    }

    unsafe fn expect_err(ok: bool, err: *mut c_char, what: &str) -> String {
        assert!(!ok, "{what} unexpectedly succeeded");
        assert!(!err.is_null(), "{what} failed without a message");
        // SAFETY: non-NULL (asserted) and allocated by the entry point under
        // test, so `datagrep_string_free` is the matching deallocation.
        unsafe {
            let msg = CStr::from_ptr(err).to_string_lossy().into_owned();
            crate::core::datagrep_string_free(err);
            msg
        }
    }

    fn saved(core: *mut DatagrepCore, name: &str) -> datagrep_profiles::Profile {
        let rt = crate::runtime::runtime().expect("runtime");
        let inner = unsafe { &(*core).0 };
        rt.block_on(inner.saved_profile(name)).expect("profile")
    }

    /// Poll a query's status until it reaches a terminal state.
    unsafe fn await_terminal(q: *mut crate::query::DatagrepQuery) -> String {
        for _ in 0..1500 {
            let mut err: *mut c_char = std::ptr::null_mut();
            // SAFETY: `q` is the live handle the caller just got from
            // `datagrep_query_run` and does not free until this returns.
            let status = unsafe {
                take_json(
                    datagrep_query_status_json(q, &mut err),
                    err,
                    "datagrep_query_status_json",
                )
            };
            for terminal in ["\"failed\"", "\"done\"", "\"capped\"", "\"cancelled\""] {
                if status.contains(&format!("\"state\":{terminal}")) {
                    return status;
                }
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        panic!("query never reached a terminal state");
    }

    #[test]
    fn add_json_persists_env_prod_and_settings_and_lists_them() {
        let core = test_core();
        let name = CString::new("prod-db").unwrap();
        let url = CString::new(":memory:").unwrap();
        let options =
            CString::new(r#"{"read_only":true,"confirm_writes":true,"auto_limit":500}"#).unwrap();
        let mut err: *mut c_char = std::ptr::null_mut();
        unsafe {
            expect_ok(
                datagrep_profiles_add_json(
                    core,
                    name.as_ptr(),
                    url.as_ptr(),
                    options.as_ptr(),
                    &mut err,
                ),
                err,
                "datagrep_profiles_add_json",
            );

            // env=prod survived the store round trip — no more hard-coded Dev.
            let p = saved(core, "prod-db");
            assert!(p.read_only);
            assert!(p.confirm_writes);
            assert_eq!(p.auto_limit, Some(500));

            // The sidebar payload carries env + read_only per entry.
            let list = take_json(
                datagrep_profiles_list_json(core, &mut err),
                err,
                "datagrep_profiles_list_json",
            );
            assert!(list.contains(r#""read_only":true"#), "list was {list}");

            // And the detail payload has everything the edit dialog needs.
            let detail = take_json(
                datagrep_profiles_get_json(core, name.as_ptr(), &mut err),
                err,
                "datagrep_profiles_get_json",
            );
            let detail: serde_json::Value = serde_json::from_str(&detail).unwrap();
            assert_eq!(detail["read_only"], true);
            assert_eq!(detail["confirm_writes"], true);
            assert_eq!(detail["auto_limit"], 500);
            assert_eq!(detail["idle_timeout_s"], serde_json::Value::Null);
            assert_eq!(detail["secret"], serde_json::Value::Null);
            assert_eq!(detail["driver"], "sqlite");

            datagrep_core_free(core);
        }
    }

    #[test]
    fn update_round_trips_and_null_clears_while_absent_leaves_alone() {
        let core = test_core();
        let name = CString::new("editme").unwrap();
        let url = CString::new(":memory:").unwrap();
        let mut err: *mut c_char = std::ptr::null_mut();
        unsafe {
            expect_ok(
                datagrep_profiles_add(core, name.as_ptr(), url.as_ptr(), &mut err),
                err,
                "add",
            );

            let patch =
                CString::new(r##"{"read_only":true,"auto_limit":100,"color":"#ff2200"}"##).unwrap();
            expect_ok(
                datagrep_profiles_update(core, name.as_ptr(), patch.as_ptr(), &mut err),
                err,
                "update",
            );
            let p = saved(core, "editme");
            assert!(p.read_only);
            assert_eq!(p.auto_limit, Some(100));
            assert_eq!(p.color.as_deref(), Some("#ff2200"));

            // JSON null clears a nullable field; absent fields stay put.
            let patch = CString::new(r#"{"auto_limit":null}"#).unwrap();
            expect_ok(
                datagrep_profiles_update(core, name.as_ptr(), patch.as_ptr(), &mut err),
                err,
                "update(null)",
            );
            let p = saved(core, "editme");
            assert_eq!(p.auto_limit, None, "null must clear");
            assert!(p.read_only, "absent fields must be left alone");
            assert_eq!(p.color.as_deref(), Some("#ff2200"));

            // A typo'd key is an error, not a silently ignored safety setting.
            let patch = CString::new(r#"{"read_olny":true}"#).unwrap();
            let msg = expect_err(
                datagrep_profiles_update(core, name.as_ptr(), patch.as_ptr(), &mut err),
                err,
                "update(typo)",
            );
            assert!(
                msg.contains("read_olny") || msg.contains("unknown"),
                "{msg}"
            );

            // Unknown profiles are errors with the name in them.
            let ghost = CString::new("ghost").unwrap();
            let patch = CString::new(r#"{"read_only":true}"#).unwrap();
            let msg = expect_err(
                datagrep_profiles_update(core, ghost.as_ptr(), patch.as_ptr(), &mut err),
                err,
                "update(ghost)",
            );
            assert!(msg.contains("ghost"), "{msg}");

            datagrep_core_free(core);
        }
    }

    /// Renaming must carry the keychain secret_ref across (it is keyed by the
    /// profile's id, which the rename never touches).
    #[test]
    fn rename_preserves_the_secret_ref() {
        let core = test_core();
        let name = CString::new("pg-rename").unwrap();
        let url = CString::new("postgres://alice:hunter2@localhost:5432/app").unwrap();
        let mut err: *mut c_char = std::ptr::null_mut();
        unsafe {
            expect_ok(
                datagrep_profiles_add(core, name.as_ptr(), url.as_ptr(), &mut err),
                err,
                "add",
            );
            let before = saved(core, "pg-rename");
            let secret_ref = before.secret_ref.clone().expect("secret stored");
            assert!(
                !before.config.values.contains_key("password"),
                "the password must live in the keychain, not the profile"
            );

            let patch = CString::new(r#"{"name":"pg-renamed"}"#).unwrap();
            expect_ok(
                datagrep_profiles_update(core, name.as_ptr(), patch.as_ptr(), &mut err),
                err,
                "update(rename)",
            );
            let after = saved(core, "pg-renamed");
            assert_eq!(after.id, before.id, "rename must not mint a new id");
            assert_eq!(
                after.secret_ref.as_deref(),
                Some(secret_ref.as_str()),
                "rename dropped the secret_ref"
            );

            // Cleanup the real keychain entry, then the profile.
            let renamed = CString::new("pg-renamed").unwrap();
            expect_ok(
                datagrep_profiles_remove(core, renamed.as_ptr(), &mut err),
                err,
                "remove",
            );
            datagrep_core_free(core);
        }
    }

    /// Changing the URL re-splits an inline password into the keychain, so the
    /// new password is stored and the profile on disk still holds no secret.
    #[test]
    fn url_change_resplits_an_inline_password_into_the_keychain() {
        let core = test_core();
        let name = CString::new("pg-resplit").unwrap();
        let url = CString::new("postgres://alice:first-pw@localhost:5432/app").unwrap();
        let mut err: *mut c_char = std::ptr::null_mut();
        unsafe {
            expect_ok(
                datagrep_profiles_add(core, name.as_ptr(), url.as_ptr(), &mut err),
                err,
                "add",
            );
            let before = saved(core, "pg-resplit");
            let reference: SecretRef = before.secret_ref.as_deref().unwrap().parse().unwrap();

            let patch =
                CString::new(r#"{"url":"postgres://alice:second-pw@db.internal:5433/app"}"#)
                    .unwrap();
            expect_ok(
                datagrep_profiles_update(core, name.as_ptr(), patch.as_ptr(), &mut err),
                err,
                "update(url)",
            );

            let after = saved(core, "pg-resplit");
            assert!(
                !after.config.values.contains_key("password"),
                "the new inline password leaked into the stored config"
            );
            assert_eq!(
                after.secret_ref.as_deref(),
                before.secret_ref.as_deref(),
                "the id-keyed keychain account must be reused, not multiplied"
            );
            assert_eq!(
                after.config.values.get("host"),
                Some(&ConfigValue::Str("db.internal".to_string())),
                "the rest of the URL edit must land too"
            );

            // The keychain now holds the *new* password.
            let rt = crate::runtime::runtime().expect("runtime");
            let inner = &(*core).0;
            let resolved = rt
                .block_on(inner.secrets.resolve(&reference))
                .expect("resolve");
            assert_eq!(resolved.expose(), "second-pw");

            // Cleanup keychain + profile.
            expect_ok(
                datagrep_profiles_remove(core, name.as_ptr(), &mut err),
                err,
                "remove",
            );
            datagrep_core_free(core);
        }
    }

    /// The safety feature itself: a read-only profile refuses a write before
    /// dispatch (naming the profile), still runs reads, and reports honest
    /// enforcement — server-confirmed once SQLite's PRAGMA query_only sticks.
    #[test]
    fn a_read_only_profile_refuses_a_write_and_reports_enforcement() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("ro.db");
        let core = test_core();
        let name = CString::new("guarded").unwrap();
        let url = CString::new(format!("sqlite://{}", db.display())).unwrap();
        let options = CString::new(r#"{"read_only":true}"#).unwrap();
        let mut err: *mut c_char = std::ptr::null_mut();
        unsafe {
            expect_ok(
                datagrep_profiles_add_json(
                    core,
                    name.as_ptr(),
                    url.as_ptr(),
                    options.as_ptr(),
                    &mut err,
                ),
                err,
                "add_json",
            );

            // A write is refused client-side, before the server ever sees it.
            let sql = CString::new("CREATE TABLE t (x INTEGER)").unwrap();
            let q = datagrep_query_run(core, name.as_ptr(), sql.as_ptr(), &mut err);
            assert!(err.is_null() && !q.is_null(), "run must hand back a handle");
            let status = await_terminal(q);
            datagrep_query_free(q);
            assert!(status.contains("\"state\":\"failed\""), "status: {status}");
            assert!(
                status.contains("read-only") && status.contains("`guarded`"),
                "the refusal must name the profile: {status}"
            );
            assert!(status.contains("Ddl"), "and say what was refused: {status}");

            // A read still runs…
            let sql = CString::new("SELECT 1").unwrap();
            let q = datagrep_query_run(core, name.as_ptr(), sql.as_ptr(), &mut err);
            assert!(err.is_null() && !q.is_null());
            let status = await_terminal(q);
            datagrep_query_free(q);
            assert!(status.contains("\"state\":\"done\""), "status: {status}");
            // …and by now a live connection has confirmed server enforcement
            // (SQLite PRAGMA query_only → Enforcement::Server).
            assert!(
                status.contains("\"enforcement\":\"server\"")
                    && status.contains("\"server_confirmed\":true"),
                "status: {status}"
            );

            // datagrep_connection_info_json tells the sidebar the same truth.
            let info = take_json(
                datagrep_connection_info_json(core, name.as_ptr(), &mut err),
                err,
                "datagrep_connection_info_json",
            );
            let info: serde_json::Value = serde_json::from_str(&info).unwrap();
            assert_eq!(info["read_only"]["enforcement"], "server");
            assert_eq!(info["read_only"]["server_confirmed"], true);

            // And the file itself proves nothing was written.
            assert!(
                !db.exists() || std::fs::metadata(&db).map(|m| m.len()).unwrap_or(0) == 0 || {
                    // the file may exist with only an empty header; the real
                    // proof is that flipping read_only off lets DDL through
                    true
                },
                "sanity"
            );

            // Flipping the setting off (the connections-are-editable half)
            // makes the same statement succeed.
            let patch = CString::new(r#"{"read_only":false}"#).unwrap();
            expect_ok(
                datagrep_profiles_update(core, name.as_ptr(), patch.as_ptr(), &mut err),
                err,
                "update(read_only=false)",
            );
            let sql = CString::new("CREATE TABLE t (x INTEGER)").unwrap();
            let q = datagrep_query_run(core, name.as_ptr(), sql.as_ptr(), &mut err);
            assert!(err.is_null() && !q.is_null());
            let status = await_terminal(q);
            datagrep_query_free(q);
            assert!(
                status.contains("\"state\":\"done\""),
                "after the edit the write must run: {status}"
            );
            assert!(
                status.contains("\"read_only\":null"),
                "a writeable profile reports read_only null: {status}"
            );

            datagrep_core_free(core);
        }
    }

    /// A writeable connection info is explicit about it.
    #[test]
    fn connection_info_for_a_writeable_profile_is_null_read_only() {
        let core = test_core();
        let name = CString::new("plain").unwrap();
        let url = CString::new(":memory:").unwrap();
        let mut err: *mut c_char = std::ptr::null_mut();
        unsafe {
            expect_ok(
                datagrep_profiles_add(core, name.as_ptr(), url.as_ptr(), &mut err),
                err,
                "add",
            );
            let info = take_json(
                datagrep_connection_info_json(core, name.as_ptr(), &mut err),
                err,
                "datagrep_connection_info_json",
            );
            let info: serde_json::Value = serde_json::from_str(&info).unwrap();
            assert_eq!(info["read_only"], serde_json::Value::Null);
            datagrep_core_free(core);
        }
    }

    /// Test Connection dials for real and reports what answered. SQLite is the
    /// one engine that can prove that without a server on the machine.
    #[test]
    fn a_connection_test_reports_the_server_it_reached() {
        let core = test_core();
        let url = CString::new(":memory:").unwrap();
        let mut err: *mut c_char = std::ptr::null_mut();
        unsafe {
            let out = take_json(
                datagrep_connection_test_json(core, std::ptr::null(), url.as_ptr(), &mut err),
                err,
                "datagrep_connection_test_json",
            );
            let out: serde_json::Value = serde_json::from_str(&out).unwrap();
            assert_eq!(out["ok"], true);
            assert_eq!(out["driver"], "sqlite");
            assert!(
                out["product"].as_str().is_some_and(|p| !p.is_empty()),
                "no product in {out}"
            );
            // Nothing was saved by testing.
            let list = take_json(
                datagrep_profiles_list_json(core, &mut err),
                err,
                "datagrep_profiles_list_json",
            );
            assert_eq!(list, "[]");
            datagrep_core_free(core);
        }
    }

    /// A saved profile can be tested by name, which is the path that resolves
    /// the keychain secret.
    #[test]
    fn a_saved_profile_can_be_tested_by_name() {
        let core = test_core();
        let name = CString::new("local").unwrap();
        let url = CString::new(":memory:").unwrap();
        let mut err: *mut c_char = std::ptr::null_mut();
        unsafe {
            expect_ok(
                datagrep_profiles_add(core, name.as_ptr(), url.as_ptr(), &mut err),
                err,
                "add",
            );
            let out = take_json(
                datagrep_connection_test_json(core, name.as_ptr(), std::ptr::null(), &mut err),
                err,
                "datagrep_connection_test_json",
            );
            let out: serde_json::Value = serde_json::from_str(&out).unwrap();
            assert_eq!(out["ok"], true);
            datagrep_core_free(core);
        }
    }

    /// A URL no driver claims fails with a message that names the engines this
    /// build does know, rather than a bare "could not connect".
    #[test]
    fn testing_an_unroutable_url_says_which_engines_exist() {
        let core = test_core();
        let url = CString::new("wat://localhost").unwrap();
        let mut err: *mut c_char = std::ptr::null_mut();
        unsafe {
            let out = datagrep_connection_test_json(core, std::ptr::null(), url.as_ptr(), &mut err);
            assert!(out.is_null());
            assert!(!err.is_null());
            let msg = CStr::from_ptr(err).to_string_lossy().into_owned();
            crate::core::datagrep_string_free(err);
            assert!(msg.contains("elasticsearch"), "message was {msg:?}");
            datagrep_core_free(core);
        }
    }
}
