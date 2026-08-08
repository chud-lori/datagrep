//! `datagrep doctor` (ticket item 6): resolved config paths, registered drivers
//! with `Capabilities` decoded to human-readable names, whether a profile's
//! secret resolves, and a connection round-trip time.

use crate::cli::DoctorArgs;
use crate::context::Context;
use crate::exit::CliError;

pub async fn run(ctx: &Context, args: &DoctorArgs) -> Result<(), CliError> {
    println!("config dir:  {}", crate::paths::config_dir().display());
    println!(
        "profiles db: {}",
        crate::paths::profiles_db_path().display()
    );
    println!();

    println!("drivers:");
    let mut registered: Vec<_> = ctx.core.drivers().iter().map(|id| id.to_string()).collect();
    registered.sort();
    for id in &registered {
        match crate::drivers::driver_for(id) {
            Some(driver) => println!("  {id}: {}", describe_caps(&driver.capabilities().flags)),
            None => println!("  {id}: (registered, but not constructible from datagrep-cli — bug)"),
        }
    }
    // Cross-check against the build's own stable list (`known_driver_ids`):
    // a mismatch here means `register_drivers` and `known_driver_ids` in
    // `drivers.rs` drifted apart, which is exactly the kind of thing a
    // health check exists to catch before a user does.
    for id in crate::drivers::known_driver_ids() {
        if !registered.iter().any(|r| r == id) {
            println!("  {id}: KNOWN but not registered — drivers.rs is out of sync");
        }
    }

    let profiles = ctx.store.list_profiles(None).await?;
    println!();
    println!("profiles: {} configured", profiles.len());

    if let Some(name) = &args.profile {
        let profile = ctx.find_profile(name).await?;
        print!("  {name}: secret ");
        match &profile.secret_ref {
            None => println!("(none)"),
            Some(secret_ref) => match secret_ref.parse::<datagrep_secrets::SecretRef>() {
                Err(e) => println!("INVALID ({e})"),
                Ok(reference) => match ctx.secrets.resolve(&reference).await {
                    Ok(_) => println!("resolves ok ({})", reference.scheme()),
                    Err(e) => println!("FAILED to resolve: {e}"),
                },
            },
        }

        let started = std::time::Instant::now();
        match ctx.open_profile(name).await {
            Err(e) => println!("  {name}: connect FAILED before dialing: {e}"),
            Ok((id, _)) => match ctx.core.connect(id).await {
                Ok(_) => println!("  {name}: connect ok ({:?})", started.elapsed()),
                Err(e) => println!("  {name}: connect FAILED: {e}"),
            },
        }
    }

    Ok(())
}

fn describe_caps(flags: &datagrep_api::Caps) -> String {
    use datagrep_api::Caps;
    const NAMED: &[(Caps, &str)] = &[
        (Caps::TRANSACTIONS, "transactions"),
        (Caps::DDL, "ddl"),
        (Caps::EXPLAIN, "explain"),
        (Caps::EDITABLE_RESULTS, "editable-results"),
        (Caps::SERVER_CANCEL, "server-cancel"),
        (Caps::EXACT_COUNT_CHEAP, "exact-count-cheap"),
        (Caps::RANDOM_ACCESS_PAGE, "random-access-page"),
        (Caps::SCHEMA_DECLARED, "schema-declared"),
        (Caps::KEY_ENUMERATION, "key-enumeration"),
        (Caps::READ_ONLY_SESSION, "read-only-session"),
        (Caps::NESTED_TRANSACTIONS, "nested-transactions"),
        (Caps::EXPLAIN_ANALYZE, "explain-analyze"),
        (Caps::MULTI_STATEMENT, "multi-statement"),
        (Caps::POSITIONAL_PARAMS, "positional-params"),
        (Caps::NAMED_PARAMS, "named-params"),
        (Caps::EXPORT_STREAMING, "export-streaming"),
        (Caps::EXPRESSION_FILTER, "expression-filter"),
    ];
    let names: Vec<&str> = NAMED
        .iter()
        .filter(|(flag, _)| flags.contains(*flag))
        .map(|(_, name)| *name)
        .collect();
    if names.is_empty() {
        "(no capability flags)".to_string()
    } else {
        names.join(", ")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn doctor_is_clean_with_zero_profiles() {
        let ctx = crate::context::test_ctx();
        let args = DoctorArgs { profile: None };
        run(&ctx, &args).await.expect("doctor should not error");
    }

    #[test]
    fn describe_caps_lists_known_flags() {
        let flags = datagrep_api::Caps::TRANSACTIONS | datagrep_api::Caps::DDL;
        let s = describe_caps(&flags);
        assert!(s.contains("transactions"));
        assert!(s.contains("ddl"));
        assert!(!s.contains("server-cancel"));
    }

    #[test]
    fn describe_caps_handles_empty_flags() {
        assert_eq!(
            describe_caps(&datagrep_api::Caps::empty()),
            "(no capability flags)"
        );
    }
}
