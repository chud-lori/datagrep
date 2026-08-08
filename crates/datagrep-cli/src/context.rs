//! Shared runtime state for every subcommand: the engine, the local profile
//! store, and the secret resolver.
//!
//! Constructing a [`Context`] touches no socket and opens no file:
//! `CoreApi::new()` is plain data structures, `register_drivers` is a
//! hashmap insert per driver (`datagrep_core::registry` docs: "nothing is
//! constructed until first use"), and `datagrep_profiles::Store::open` is
//! documented lazy — its worker thread and SQLite file only come alive on
//! the first real call. This is what keeps `datagrep --help` / `datagrep profiles
//! list` near-instant.

use std::sync::{Arc, Mutex};

use datagrep_api::ConfigValue;
use datagrep_core::CoreApi;
use datagrep_profiles::Store;
use datagrep_secrets::{SecretRef, SecretResolver};

use crate::exit::CliError;

pub struct Context {
    pub core: CoreApi,
    pub store: Store,
    pub secrets: SecretResolver,
    /// The query currently streaming, if any — so a Ctrl-C handler running
    /// concurrently (see `main.rs`) can cancel *that* query specifically and
    /// report the real `CancelReport` rather than just killing the process
    /// blind — the stop button has to tell the truth about what it did.
    current_query: Mutex<Option<datagrep_core::QueryId>>,
}

impl Context {
    pub fn new() -> Self {
        Self::with_store(Store::open(crate::paths::profiles_db_path()))
    }

    /// A context over an explicit [`Store`] — what every test in this crate
    /// uses (with [`Store::open_in_memory`]) instead of [`Context::new`],
    /// which points at the developer's real `~/.config/datagrep/profiles.db`.
    /// `cargo test` runs tests in parallel threads of one process, so two
    /// tests both defaulting to that shared file race on the same on-disk
    /// migration (`CREATE TABLE folder` colliding with itself) — an
    /// in-memory store per `Context` sidesteps that entirely rather than
    /// serializing tests or juggling a process-global `DATAGREP_CONFIG_DIR`.
    pub fn with_store(store: Store) -> Self {
        let core = CoreApi::new();
        crate::drivers::register_drivers(&core);
        Self {
            core,
            store,
            secrets: SecretResolver::new(),
            current_query: Mutex::new(None),
        }
    }

    pub fn set_current_query(&self, qid: Option<datagrep_core::QueryId>) {
        *self
            .current_query
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = qid;
    }

    pub fn current_query(&self) -> Option<datagrep_core::QueryId> {
        *self
            .current_query
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Find a saved profile by name.
    pub async fn find_profile(&self, name: &str) -> Result<datagrep_profiles::Profile, CliError> {
        let profiles = self.store.list_profiles(None).await?;
        profiles
            .into_iter()
            .find(|p| p.name == name)
            .ok_or_else(|| {
                CliError::usage(format!(
                    "no profile named `{name}` (see `datagrep profiles list`)"
                ))
            })
    }

    /// Resolve a saved profile's secret (if any) and register it with
    /// `CoreApi`, ready to connect.
    ///
    /// **CoreApi gap, worked around here** (see `README.md` "CoreApi gaps"):
    /// `datagrep_core::session::Session::acquire` always builds
    /// `ResolvedConfig::without_secrets(self.config.clone())` — there is no
    /// seam for a frontend to hand a resolved secret to a running session.
    /// Until that lands, this crate resolves the secret itself and folds it
    /// back into the plaintext `ConnectionConfig` handed to
    /// `CoreApi::add_profile_full`, which is the only way a driver in this
    /// build ever sees it (both `datagrep-drv-postgres` and, if it grows a secret
    /// field, `datagrep-drv-sqlite` fall back to reading the field straight out of
    /// `ConnectionConfig.values` when `ResolvedConfig.secrets` is empty). The
    /// on-disk profile never holds the secret (`datagrep_profiles::secrets`
    /// rejects it structurally), but the resolved value does sit in an
    /// un-zeroized `String` inside `ConnectionConfig.values` for the life of
    /// this process's `CoreApi::Profile` — weaker than the `SecretString`
    /// guarantee (zeroize-on-drop, redacted `Debug`) it started as.
    pub async fn open_profile(
        &self,
        name: &str,
    ) -> Result<(datagrep_core::ProfileId, datagrep_profiles::Profile), CliError> {
        let profile = self.find_profile(name).await?;
        let mut config = profile.config.clone();

        if let Some(secret_ref) = &profile.secret_ref {
            let reference: SecretRef = secret_ref
                .parse()
                .map_err(|e: datagrep_secrets::SecretError| CliError::usage(e.to_string()))?;
            let secret = self.secrets.resolve(&reference).await?;
            if let Some(driver) = crate::drivers::driver_for(&profile.driver_id) {
                let schema = driver.config_schema();
                if let Some(field) = schema.fields.iter().find(|f| f.secret) {
                    config.values.insert(
                        field.key.to_string(),
                        ConfigValue::Str(secret.expose().to_string()),
                    );
                }
            }
        }

        let core_profile = datagrep_core::Profile {
            id: datagrep_core::ProfileId(0), // overwritten by add_profile_full
            name: Arc::from(profile.name.as_str()),
            driver: Arc::from(profile.driver_id.as_str()),
            config,
            env: map_env(profile.env),
            read_only: profile.read_only,
        };
        let id = self.core.add_profile_full(core_profile).await;
        let _ = self.store.touch_profile_last_used(profile.id.clone()).await;
        Ok((id, profile))
    }
}

impl Default for Context {
    fn default() -> Self {
        Self::new()
    }
}

fn map_env(env: datagrep_profiles::Env) -> datagrep_core::api::Env {
    match env {
        datagrep_profiles::Env::Dev => datagrep_core::api::Env::Dev,
        datagrep_profiles::Env::Staging => datagrep_core::api::Env::Staging,
        datagrep_profiles::Env::Prod => datagrep_core::api::Env::Prod,
    }
}

/// A [`Context`] whose profile store **and** secret store are both in memory.
///
/// The secrets half is the point. [`SecretResolver::new`] talks to the OS
/// credential store, which fails outright on a bare Linux CI runner — there is
/// no Secret Service on the session bus — and, worse, quietly *succeeds* on a
/// developer's Mac, leaving a junk credential in their login keychain on every
/// run. Those accumulate silently; a real machine had 30+ before this existed.
/// No test in this crate asserts anything about the OS store itself.
#[cfg(test)]
pub fn test_ctx() -> Context {
    let mut ctx = Context::with_store(Store::open_in_memory());
    ctx.secrets = datagrep_secrets::SecretResolver::in_memory();
    ctx
}

/// [`test_ctx`] over a real file, for the one claim an in-memory store cannot
/// support: that a password never lands on disk. Give it a path inside a
/// `tempfile::tempdir()` and the whole directory — db, `-wal`, `-shm`, any
/// export written beside them — becomes greppable.
///
/// Secrets stay in memory for exactly the reasons [`test_ctx`] gives; a test
/// that reaches the OS keychain to prove something about disk would be trading
/// one leak for another.
#[cfg(test)]
pub fn test_ctx_at(profiles_db: &std::path::Path) -> Context {
    let mut ctx = Context::with_store(Store::open(profiles_db));
    ctx.secrets = datagrep_secrets::SecretResolver::in_memory();
    ctx
}
