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
    current_query: Mutex<Option<datagrep_core::QueryId>>,
}

impl Context {
    pub fn new() -> Self {
        Self::with_store(Store::open(crate::paths::profiles_db_path()))
    }

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
            read_only: profile.read_only,
            safety: profile.safety,
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

#[cfg(test)]
pub fn test_ctx() -> Context {
    let mut ctx = Context::with_store(Store::open_in_memory());
    ctx.secrets = datagrep_secrets::SecretResolver::in_memory();
    ctx
}

#[cfg(test)]
pub fn test_ctx_at(profiles_db: &std::path::Path) -> Context {
    let mut ctx = Context::with_store(Store::open(profiles_db));
    ctx.secrets = datagrep_secrets::SecretResolver::in_memory();
    ctx
}
