//! `dbx profiles` (ticket item 3): plain-text, git-committable connection
//! profiles (design §4 killer feature #5). `add` splits any inline password
//! out of the parsed URL into a keychain [`dbx_secrets::SecretRef`] before
//! the profile ever reaches [`dbx_profiles::Store::create_profile`] — which
//! independently refuses a secret-shaped config key anyway
//! (`dbx_profiles::secrets::validate_no_secrets`), so this is defense in
//! depth, not the only thing standing between a password and disk.

use std::path::Path;

use dbx_api::ConfigValue;
use dbx_secrets::SecretRef;

use crate::context::Context;
use crate::exit::CliError;

pub async fn list(ctx: &Context) -> Result<(), CliError> {
    let profiles = ctx.store.list_profiles(None).await?;
    if profiles.is_empty() {
        println!("(no profiles — see `dbx profiles add`)");
        return Ok(());
    }
    for p in profiles {
        let secret = if p.secret_ref.is_some() {
            "secret"
        } else {
            "no-secret"
        };
        println!("{}\t{}\t{}\t{}", p.name, p.driver_id, p.env, secret);
    }
    Ok(())
}

pub async fn add(ctx: &Context, name: &str, url: &str) -> Result<(), CliError> {
    if ctx.find_profile(name).await.is_ok() {
        return Err(CliError::usage(format!(
            "a profile named `{name}` already exists"
        )));
    }
    let (driver_id, driver) = crate::drivers::driver_for_url(url).ok_or_else(|| {
        CliError::usage(format!(
            "could not tell which driver `{url}` is for (expected postgres://... or sqlite://...)"
        ))
    })?;
    let mut config = driver
        .parse_url(url)
        .map_err(|e| CliError::usage(e.to_string()))?;

    let id = dbx_profiles::new_id();
    let mut secret_ref = None;
    let schema = driver.config_schema();
    for field in schema.fields.iter().filter(|f| f.secret) {
        if let Some(ConfigValue::Str(value)) = config.values.remove(field.key.as_ref()) {
            if value.is_empty() {
                continue;
            }
            let reference = SecretRef::Keychain {
                service: "dbx".to_string(),
                account: format!("{id}:{}", field.key),
            };
            ctx.secrets
                .store(&reference, dbx_api::SecretString::new(value))
                .await?;
            secret_ref = Some(reference.to_string());
        }
    }

    let now = dbx_profiles::now_ms();
    let profile = dbx_profiles::Profile {
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
    };
    let has_secret = profile.secret_ref.is_some();
    ctx.store.create_profile(profile).await?;
    println!(
        "created profile `{name}` ({driver_id}{})",
        if has_secret {
            ", secret stored in the OS keychain"
        } else {
            ""
        }
    );
    Ok(())
}

pub async fn remove(ctx: &Context, name: &str) -> Result<(), CliError> {
    let profile = ctx.find_profile(name).await?;
    if let Some(secret_ref) = &profile.secret_ref {
        if let Ok(reference) = secret_ref.parse::<SecretRef>() {
            if reference.is_writable() {
                let _ = ctx.secrets.delete(&reference).await;
            }
        }
    }
    ctx.store.delete_profile(profile.id).await?;
    println!("removed profile `{name}`");
    Ok(())
}

pub async fn show(ctx: &Context, name: &str) -> Result<(), CliError> {
    let p = ctx.find_profile(name).await?;
    println!("name:       {}", p.name);
    println!("driver:     {}", p.driver_id);
    println!("env:        {}", p.env);
    println!("read_only:  {}", p.read_only);
    println!(
        "secret:     {}",
        if p.secret_ref.is_some() {
            "••••"
        } else {
            "(none)"
        }
    );
    println!("config:");
    for (k, v) in &p.config.values {
        println!("  {k} = {}", format_config_value(v));
    }
    Ok(())
}

pub async fn export(ctx: &Context, out: Option<&Path>) -> Result<(), CliError> {
    let toml = ctx.store.export_profiles().await?;
    match out {
        Some(path) => std::fs::write(path, toml)?,
        None => print!("{toml}"),
    }
    Ok(())
}

pub async fn import(
    ctx: &Context,
    file: &Path,
    merge: bool,
    replace: bool,
) -> Result<(), CliError> {
    if merge && replace {
        return Err(CliError::usage("pass only one of --merge or --replace"));
    }
    let strategy = if replace {
        dbx_profiles::ImportStrategy::Replace
    } else {
        dbx_profiles::ImportStrategy::Merge
    };
    let toml = std::fs::read_to_string(file)
        .map_err(|e| CliError::usage(format!("could not read {}: {e}", file.display())))?;
    let summary = ctx.store.import_profiles(toml, strategy).await?;
    println!(
        "imported: {} profile(s), {} folder(s), {} tunnel(s){}",
        summary.profiles_upserted,
        summary.folders_upserted,
        summary.tunnels_upserted,
        if replace {
            format!(
                " (removed {} profile(s), {} folder(s), {} tunnel(s) not in the file)",
                summary.profiles_removed, summary.folders_removed, summary.tunnels_removed
            )
        } else {
            String::new()
        }
    );
    Ok(())
}

fn format_config_value(v: &ConfigValue) -> String {
    match v {
        ConfigValue::Str(s) => s.clone(),
        ConfigValue::Num(n) => n.to_string(),
        ConfigValue::Bool(b) => b.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn add_with_inline_password_stores_no_password_and_a_secret_ref() {
        let ctx = Context::with_store(dbx_profiles::Store::open_in_memory());
        add(
            &ctx,
            "pgtest",
            "postgres://alice:hunter2@localhost:5432/app",
        )
        .await
        .expect("add should succeed");

        let profile = ctx.find_profile("pgtest").await.expect("profile exists");
        assert!(
            !profile.config.values.contains_key("password"),
            "the plaintext password must not be stored in the profile"
        );
        assert!(
            profile.secret_ref.is_some(),
            "a secret_ref must be recorded"
        );
        assert!(profile
            .secret_ref
            .as_deref()
            .unwrap()
            .starts_with("keychain:dbx:"));
        assert_eq!(
            profile.config.values.get("user"),
            Some(&ConfigValue::Str("alice".to_string()))
        );

        // Cleanup: this test wrote a real keychain entry.
        let reference: SecretRef = profile.secret_ref.as_deref().unwrap().parse().unwrap();
        let _ = ctx.secrets.delete(&reference).await;
    }

    #[tokio::test]
    async fn add_without_password_has_no_secret_ref() {
        let ctx = Context::with_store(dbx_profiles::Store::open_in_memory());
        add(&ctx, "pgtest2", "postgres://alice@localhost:5432/app")
            .await
            .expect("add should succeed");
        let profile = ctx.find_profile("pgtest2").await.unwrap();
        assert!(profile.secret_ref.is_none());
    }

    #[tokio::test]
    async fn add_sqlite_profile_has_no_secret_field_at_all() {
        let ctx = Context::with_store(dbx_profiles::Store::open_in_memory());
        add(&ctx, "sqtest", "sqlite:///tmp/does-not-need-to-exist.db")
            .await
            .expect("add should succeed");
        let profile = ctx.find_profile("sqtest").await.unwrap();
        assert!(profile.secret_ref.is_none());
        assert_eq!(profile.driver_id, "sqlite");
    }

    #[tokio::test]
    async fn add_rejects_a_duplicate_name() {
        let ctx = Context::with_store(dbx_profiles::Store::open_in_memory());
        add(&ctx, "dup", ":memory:").await.unwrap();
        let err = add(&ctx, "dup", ":memory:").await.unwrap_err();
        assert_eq!(err.kind, crate::exit::ExitKind::UsageError);
    }

    #[tokio::test]
    async fn add_rejects_an_unrecognized_url() {
        let ctx = Context::with_store(dbx_profiles::Store::open_in_memory());
        let err = add(&ctx, "bad", "mongodb://h/db").await.unwrap_err();
        assert_eq!(err.kind, crate::exit::ExitKind::UsageError);
    }

    #[tokio::test]
    async fn remove_deletes_the_profile() {
        let ctx = Context::with_store(dbx_profiles::Store::open_in_memory());
        add(&ctx, "gone", ":memory:").await.unwrap();
        remove(&ctx, "gone").await.unwrap();
        assert!(ctx.find_profile("gone").await.is_err());
    }

    #[tokio::test]
    async fn export_then_import_round_trips() {
        let ctx = Context::with_store(dbx_profiles::Store::open_in_memory());
        add(&ctx, "roundtrip", ":memory:").await.unwrap();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("profiles.toml");
        export(&ctx, Some(&path)).await.unwrap();

        let ctx2 = Context::with_store(dbx_profiles::Store::open_in_memory());
        import(&ctx2, &path, false, false).await.unwrap();
        assert!(ctx2.find_profile("roundtrip").await.is_ok());
    }
}
