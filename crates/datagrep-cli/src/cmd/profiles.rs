//! `datagrep profiles` (ticket item 3): plain-text, git-committable connection
//! profiles. `add` splits any inline password
//! out of the parsed URL into a keychain [`datagrep_secrets::SecretRef`] before
//! the profile ever reaches [`datagrep_profiles::Store::create_profile`] — which
//! independently refuses a secret-shaped config key anyway
//! (`datagrep_profiles::secrets::validate_no_secrets`), so this is defense in
//! depth, not the only thing standing between a password and disk.

use std::path::Path;

use datagrep_api::ConfigValue;
use datagrep_secrets::SecretRef;

use crate::context::Context;
use crate::exit::CliError;

pub async fn list(ctx: &Context) -> Result<(), CliError> {
    let profiles = ctx.store.list_profiles(None).await?;
    if profiles.is_empty() {
        println!("(no profiles — see `datagrep profiles add`)");
        return Ok(());
    }
    for p in profiles {
        let secret = if p.secret_ref.is_some() {
            "secret"
        } else {
            "no-secret"
        };
        println!("{}\t{}\t{}", p.name, p.driver_id, secret);
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
            "could not tell which driver `{}` is for (expected postgres://... or sqlite://...)",
            datagrep_api::config::redact_url(url)
        ))
    })?;
    let mut config = driver
        .parse_url(url)
        .map_err(|e| CliError::usage(e.to_string()))?;

    let id = datagrep_profiles::new_id();
    let mut secret_ref = None;
    let schema = driver.config_schema();
    for field in schema.fields.iter().filter(|f| f.secret) {
        if let Some(ConfigValue::Str(value)) = config.values.remove(field.key.as_ref()) {
            if value.is_empty() {
                continue;
            }
            let reference = SecretRef::Keychain {
                service: "datagrep".to_string(),
                account: format!("{id}:{}", field.key),
            };
            ctx.secrets
                .store(&reference, datagrep_api::SecretString::new(value))
                .await?;
            secret_ref = Some(reference.to_string());
        }
    }

    let now = datagrep_profiles::now_ms();
    let profile = datagrep_profiles::Profile {
        id,
        folder_id: None,
        name: name.to_string(),
        driver_id: driver_id.to_string(),
        config,
        secret_ref,
        tunnel_id: None,
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
        datagrep_profiles::ImportStrategy::Replace
    } else {
        datagrep_profiles::ImportStrategy::Merge
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
        let ctx = crate::context::test_ctx();
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
            .starts_with("keychain:datagrep:"));
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
        let ctx = crate::context::test_ctx();
        add(&ctx, "pgtest2", "postgres://alice@localhost:5432/app")
            .await
            .expect("add should succeed");
        let profile = ctx.find_profile("pgtest2").await.unwrap();
        assert!(profile.secret_ref.is_none());
    }

    #[tokio::test]
    async fn add_sqlite_profile_has_no_secret_field_at_all() {
        let ctx = crate::context::test_ctx();
        add(&ctx, "sqtest", "sqlite:///tmp/does-not-need-to-exist.db")
            .await
            .expect("add should succeed");
        let profile = ctx.find_profile("sqtest").await.unwrap();
        assert!(profile.secret_ref.is_none());
        assert_eq!(profile.driver_id, "sqlite");
    }

    #[tokio::test]
    async fn add_rejects_a_duplicate_name() {
        let ctx = crate::context::test_ctx();
        add(&ctx, "dup", ":memory:").await.unwrap();
        let err = add(&ctx, "dup", ":memory:").await.unwrap_err();
        assert_eq!(err.kind, crate::exit::ExitKind::UsageError);
    }

    #[tokio::test]
    async fn add_rejects_an_unrecognized_url() {
        let ctx = crate::context::test_ctx();
        let err = add(&ctx, "bad", "mongodb://h/db").await.unwrap_err();
        assert_eq!(err.kind, crate::exit::ExitKind::UsageError);
    }

    #[tokio::test]
    async fn remove_deletes_the_profile() {
        let ctx = crate::context::test_ctx();
        add(&ctx, "gone", ":memory:").await.unwrap();
        remove(&ctx, "gone").await.unwrap();
        assert!(ctx.find_profile("gone").await.is_err());
    }

    #[tokio::test]
    async fn export_then_import_round_trips() {
        let ctx = crate::context::test_ctx();
        add(&ctx, "roundtrip", ":memory:").await.unwrap();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("profiles.toml");
        export(&ctx, Some(&path)).await.unwrap();

        let ctx2 = crate::context::test_ctx();
        import(&ctx2, &path, false, false).await.unwrap();
        assert!(ctx2.find_profile("roundtrip").await.is_ok());
    }

    // ---- secrets never reach disk, and never reach a log ------------------

    /// Every file under `dir`, recursively, with its raw bytes. Recursive and
    /// byte-level on purpose: the point is to look at whatever is actually
    /// there — SQLite's `-wal` and `-shm` siblings included — rather than at
    /// the files this crate happens to know it writes.
    fn files_under(dir: &std::path::Path) -> Vec<(std::path::PathBuf, Vec<u8>)> {
        let mut out = Vec::new();
        let entries = std::fs::read_dir(dir).expect("the config dir is readable");
        for entry in entries {
            let path = entry.expect("a readable dir entry").path();
            if path.is_dir() {
                out.extend(files_under(&path));
            } else {
                out.push((path.clone(), std::fs::read(&path).expect("a readable file")));
            }
        }
        out
    }

    fn contains(haystack: &[u8], needle: &[u8]) -> bool {
        haystack.windows(needle.len()).any(|w| w == needle)
    }

    /// The plaintext canary. Deliberately not a plausible password: if it ever
    /// shows up in a failure message, there is no ambiguity about where it came
    /// from, and it cannot collide with unrelated bytes in a SQLite page.
    const CANARY: &str = "PLAINTEXT-CANARY-do-not-persist-3f8a1c";

    /// Grep the whole config directory, not the files we expect to be clean.
    ///
    /// The existing coverage asserted on the *stdout* of `profiles show` and
    /// `profiles export`, which is a different claim: it proves the renderer
    /// masks the value, not that the value never reached storage. The profile
    /// DB, its write-ahead log, and an exported TOML are all separately capable
    /// of holding it.
    ///
    /// The first half of this test is a positive control, and it is the whole
    /// reason the test is shaped this way: the failure mode here is not a leak,
    /// it is an assertion that quietly stops running. If the password were
    /// never captured at all, every "the plaintext is absent" assertion below
    /// would pass while proving nothing.
    #[tokio::test]
    async fn a_password_never_reaches_any_file_in_the_config_dir() {
        let dir = tempfile::tempdir().expect("tempdir");
        let ctx = crate::context::test_ctx_at(&dir.path().join("profiles.db"));

        add(
            &ctx,
            "staging",
            &format!("postgres://alice:{CANARY}@localhost:5432/app"),
        )
        .await
        .expect("add");

        // Positive control: the secret really was captured and is really
        // retrievable, so the absence assertions below are about storage and
        // not about a no-op.
        let profile = ctx.find_profile("staging").await.expect("profile exists");
        let reference: datagrep_secrets::SecretRef = profile
            .secret_ref
            .as_deref()
            .expect("a secret_ref was recorded")
            .parse()
            .expect("the secret_ref parses");
        let resolved = ctx.secrets.resolve(&reference).await.expect("resolves");
        assert_eq!(
            resolved.expose(),
            CANARY,
            "the password must actually have been stored, or this test proves nothing"
        );

        // `profiles export` is meant to be committable, so it belongs in the
        // same sweep as the database itself.
        export(&ctx, Some(&dir.path().join("profiles.toml")))
            .await
            .expect("export");

        let files = files_under(dir.path());
        // Second positive control: if the store were still lazy and had written
        // nothing, an empty sweep would also "pass".
        assert!(
            files
                .iter()
                .any(|(p, b)| p.ends_with("profiles.db") && !b.is_empty()),
            "the profile store must have written a non-empty db; saw {:?}",
            files.iter().map(|(p, _)| p).collect::<Vec<_>>()
        );
        assert!(
            files.iter().any(|(p, _)| p.ends_with("profiles.toml")),
            "the export must have been written"
        );

        for (path, bytes) in &files {
            assert!(
                !contains(bytes, CANARY.as_bytes()),
                "{} holds the plaintext password ({} bytes scanned)",
                path.display(),
                bytes.len()
            );
        }
    }

    /// A `tracing` writer a test can read back. `tracing-subscriber` is already
    /// a normal dependency of this crate (`main.rs` builds the `--verbose`
    /// subscriber from it), so this needs no new dev-dependency.
    #[derive(Clone, Default)]
    struct CapturedLog(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

    impl CapturedLog {
        fn text(&self) -> String {
            let buf = self
                .0
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            String::from_utf8_lossy(&buf).into_owned()
        }
    }

    impl std::io::Write for CapturedLog {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .extend_from_slice(buf);
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

    /// Nothing on the secret path may log the value — at any level, including
    /// `trace`, and including inside an error.
    ///
    /// `trace` matters because it is the level someone turns on precisely when
    /// a connection is misbehaving, i.e. while looking at a profile that has a
    /// password in it. A redaction that only holds at `debug` is not a
    /// redaction.
    ///
    /// **Scope, stated rather than implied:** `with_default` installs the
    /// subscriber on this thread only, so this covers the secret handling that
    /// runs here — `profiles::add`, `Context::open_profile`, and
    /// `SecretResolver::resolve`, which is the whole path a password travels.
    /// The profile store's worker thread is not captured; it never sees a
    /// secret, because the secret is split out before `create_profile` is
    /// called and the store refuses secret-shaped keys anyway.
    #[test]
    fn no_secret_is_logged_at_any_level_including_trace() {
        let log = CapturedLog::default();
        let subscriber = tracing_subscriber::fmt()
            .with_writer(log.clone())
            .with_max_level(tracing::Level::TRACE)
            .with_ansi(false)
            .finish();

        let errors = tracing::subscriber::with_default(subscriber, || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("runtime");
            rt.block_on(async {
                let ctx = crate::context::test_ctx();
                let url = format!("postgres://alice:{CANARY}@localhost:5432/app");
                add(&ctx, "logtest", &url).await.expect("add");

                // Resolving is where the plaintext exists in this process at
                // all, so it is the call most likely to log it.
                let (_id, _p) = ctx.open_profile("logtest").await.expect("open");

                // Error paths too: a message that helpfully echoes the config
                // it failed on is the classic way a secret escapes.
                let mut errors = Vec::new();
                if let Err(e) = ctx.find_profile("no-such-profile").await {
                    errors.push(e.to_string());
                }
                if let Err(e) = add(&ctx, "logtest", &url).await {
                    errors.push(e.to_string());
                }
                if let Err(e) = add(&ctx, "bad", &format!("nonsense://u:{CANARY}@h/db")).await {
                    errors.push(e.to_string());
                }
                errors
            })
        });

        let text = log.text();
        // Positive control: the subscriber really was installed and really did
        // capture the secret path, so "no canary in the log" is a finding
        // rather than an empty buffer.
        assert!(
            text.contains("resolving secret ref"),
            "the trace subscriber captured nothing from the secret path; log was {text:?}"
        );
        assert!(
            !text.contains(CANARY),
            "the password reached the log at trace level:\n{text}"
        );
        assert!(!errors.is_empty(), "no error path was exercised");
        for err in &errors {
            assert!(
                !err.contains(CANARY),
                "an error message carries the password: {err}"
            );
        }
    }
}
