//! The connection-safety claims, per driver, tested where they can be tested.
//!
//! This crate is the one place every driver is linked together, which makes it
//! the only place a cross-driver claim can be checked in one pass. Two of the
//! three claims in the issue are decidable without a server, and one is not:
//!
//! | claim | testable offline? |
//! |---|---|
//! | TLS default, and no silent downgrade | **yes** — `parse_url` and the connect-time refusal are pure |
//! | read-only refuses a write | **partly** — the client-side layer, yes; the server-side layer needs a server |
//! | `Enforcement` is reported truthfully | **partly** — the mapping, yes; what a driver returns, no |
//!
//! Where a live server is genuinely required, this file says so in place
//! rather than asserting something weaker and letting it read as coverage.
//! The per-driver server-side assertions live in each driver's own
//! `tests/integration.rs` behind `#[ignore]` and a `DATAGREP_TEST_*` env var;
//! only `datagrep-drv-sqlite` can run its read-only test unattended.

use datagrep_api::config::{ConfigValue, ConnectionConfig};
use datagrep_ffi::drivers::{driver_for, known_driver_ids};
use datagrep_ffi::query::refuse_writes;

/// The `tls`-ish config key each driver records its transport decision under,
/// and the value that means plaintext. `None` = the driver has no TLS field at
/// all, which is its own claim and is checked separately below.
fn tls_key(driver_id: &str) -> Option<(&'static str, ConfigValue)> {
    match driver_id {
        "postgres" => Some(("tls", ConfigValue::Str("disable".to_string()))),
        "redis" | "mongodb" | "elasticsearch" => Some(("tls", ConfigValue::Bool(false))),
        // sqlite is a local file — there is no transport to secure.
        // mysql compiles without TLS support at all.
        _ => None,
    }
}

fn parse(driver_id: &str, url: &str) -> Result<ConnectionConfig, String> {
    let driver = driver_for(driver_id).unwrap_or_else(|| panic!("{driver_id} is registered"));
    driver.parse_url(url).map_err(|e| e.to_string())
}

// ---- TLS defaults ------------------------------------------------------

/// Every driver that has a TLS setting must *have a default* for it, and that
/// default must be a value — never absent. An absent key is the dangerous
/// shape: whoever reads it later picks their own default, and the two ends
/// disagree about whether the connection is encrypted.
#[test]
fn every_driver_declares_an_explicit_tls_default() {
    for id in known_driver_ids() {
        let driver = driver_for(id).expect("registered");
        let schema = driver.config_schema();
        let Some((key, plaintext)) = tls_key(id) else {
            assert!(
                !schema.fields.iter().any(|f| f.key.as_ref() == "tls"),
                "{id} has a tls field but this test does not know its plaintext value — \
                 add it to `tls_key` rather than leaving the driver unchecked"
            );
            continue;
        };
        let field = schema
            .fields
            .iter()
            .find(|f| f.key.as_ref() == key)
            .unwrap_or_else(|| panic!("{id} must declare a `{key}` field"));
        let default = field
            .default
            .as_ref()
            .unwrap_or_else(|| panic!("{id}.{key} must have an explicit default, not None"));
        // The default being plaintext is not what is under test — it is a
        // documented posture for this build, and `datagrep-drv-elasticsearch`
        // is the only driver with working TLS. What is under test is that the
        // default is *stated*, so it cannot drift silently.
        assert_eq!(
            default, &plaintext,
            "{id}.{key} default changed; if that is intended, update this test \
             deliberately — it exists to make the change visible"
        );
    }
}

/// **The claim that matters: a downgrade to plaintext is never silent.**
///
/// A URL that explicitly asks for TLS must not come back configured for
/// plaintext. It may fail — three of these drivers have no TLS implementation
/// and refuse rather than pretend — but it may not quietly succeed unencrypted.
///
/// Postgres failed this until recently: `parse_url` read `sslmode` out of the
/// parsed libpq config and then never used it, so `?sslmode=require` produced
/// `tls=disable` and connected in the clear with nothing on screen.
#[test]
fn an_explicit_tls_request_is_never_silently_downgraded() {
    // (driver, URL asking for TLS, the config key/value that would mean
    // "plaintext" and therefore a silent downgrade)
    let cases = [
        ("postgres", "postgres://u:p@h:5432/db?sslmode=require"),
        ("redis", "rediss://h:6380"),
        ("mongodb", "mongodb://u:p@h:27017/db?tls=true"),
        ("elasticsearch", "https://es.example.com:9243"),
    ];

    for (id, url) in cases {
        let (key, plaintext) = tls_key(id).expect("these four all have a tls key");
        match parse(id, url) {
            Ok(config) => {
                let got = config.values.get(key);
                assert_ne!(
                    got,
                    Some(&plaintext),
                    "{id}: `{url}` asked for TLS and parsed to plaintext ({key}={plaintext:?}) — \
                     that is a silent downgrade"
                );
                assert!(
                    got.is_some(),
                    "{id}: `{url}` left `{key}` unset, so whoever reads it next picks the default"
                );
            }
            // Refusing outright is also honest: the user finds out.
            Err(e) => assert!(
                !e.is_empty(),
                "{id}: `{url}` was rejected with an empty message"
            ),
        }
    }
}

/// The two drivers with a TLS *field* but no TLS *implementation* must refuse
/// at connect time, not fall back. Checked through `connect`, because that is
/// where the fallback would live — and it needs no network to prove: the
/// refusal happens before any socket is opened.
#[test]
fn a_driver_without_tls_refuses_rather_than_connecting_in_the_clear() {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime");

    for (id, url) in [
        ("postgres", "postgres://u:p@127.0.0.1:1/db?sslmode=require"),
        ("redis", "rediss://127.0.0.1:1"),
    ] {
        let config = parse(id, url).unwrap_or_else(|e| panic!("{id}: {e}"));
        let driver = driver_for(id).expect("registered");
        let resolved = datagrep_api::config::ResolvedConfig::without_secrets(config);
        let err = rt
            .block_on(driver.connect(&resolved, datagrep_api::driver::ConnectCtx::default()))
            .err()
            .unwrap_or_else(|| panic!("{id}: a TLS URL must not connect at all here"));
        let text = err.to_string().to_lowercase();
        assert!(
            text.contains("tls"),
            "{id}: the refusal must name TLS so the user knows why, got {err}"
        );
        // Port 1 has nothing on it, so a message about a refused connection
        // would mean the driver got as far as dialling — i.e. it accepted the
        // TLS request and was about to speak plaintext.
        assert!(
            !text.contains("refused") && !text.contains("timed out"),
            "{id}: the driver dialled before refusing the TLS request: {err}"
        );
    }
}

// ---- read-only ---------------------------------------------------------

/// Layer 2 of the read-only guard — the client-side classifier — refuses a
/// write for every engine this build can classify, *before* anything is
/// dispatched. This is the layer that carries Redis and Mongo, which have no
/// server-side per-session read-only switch at all.
///
/// It is also the layer that can be tested without a server, which is exactly
/// why it is worth pinning: the server-side half (layer 1) is only reachable
/// with a live database and lives behind `#[ignore]` in each driver's own
/// integration suite.
#[test]
fn the_client_side_guard_refuses_a_write_for_every_classifiable_engine() {
    // One genuinely destructive statement per engine's own language, through
    // the guard the FFI actually calls rather than a re-derivation of it.
    let writes = [
        ("postgres", "DELETE FROM users"),
        ("postgres", "DROP TABLE users"),
        ("mysql", "DROP TABLE users"),
        ("mysql", "INSERT INTO users VALUES (1)"),
        ("sqlite", "UPDATE users SET admin = 1"),
        ("redis", "FLUSHALL"),
        ("redis", "DEL k"),
        ("mongodb", "db.users.deleteMany({})"),
    ];
    for (id, sql) in writes {
        let refused = refuse_writes("prod", id, sql);
        let msg = refused.expect_err(&format!(
            "{id}: `{sql}` was allowed through the read-only guard"
        ));
        assert!(
            msg.contains("prod"),
            "{id}: the refusal must name the profile that refused, got {msg}"
        );
    }

    // And the mirror: a plain read must still run, or read-only mode is broken
    // rather than merely strict.
    for (id, sql) in [
        ("postgres", "SELECT 1"),
        ("mysql", "SELECT 1"),
        ("sqlite", "SELECT * FROM t"),
        ("redis", "GET k"),
        ("mongodb", "db.users.find({})"),
    ] {
        assert!(
            refuse_writes("prod", id, sql).is_ok(),
            "{id}: `{sql}` is a read but the guard refused it"
        );
    }
}

/// **`Enforcement` must never be reported as stronger than it is.**
///
/// This is the badge the user relaxes on the strength of, so the mapping from
/// what a driver reported to what the UI is told gets its own test. The one
/// rule with teeth: `"server"` is reachable *only* from a live connection that
/// actually returned `Enforcement::Server`. Never from a guess, never from an
/// earlier connection, never from "the profile says read-only".
#[test]
fn no_enforcement_short_of_server_is_ever_reported_as_server() {
    use datagrep_api::Enforcement;
    use datagrep_ffi::core::read_only_json;

    for id in known_driver_ids() {
        // A read-only profile that has not connected, or whose driver admitted
        // client-side-only enforcement, must never claim the server.
        for reported in [None, Some(Enforcement::Client), Some(Enforcement::None)] {
            let json = read_only_json(true, id, reported);
            assert_ne!(
                json["enforcement"], "server",
                "{id} claimed server enforcement from {reported:?}"
            );
            assert_eq!(
                json["server_confirmed"], false,
                "{id} claimed server_confirmed from {reported:?}"
            );
        }
        // Only a live `Server` answer earns the strong claim.
        let json = read_only_json(true, id, Some(Enforcement::Server));
        assert_eq!(json["enforcement"], "server", "{id}");
        assert_eq!(json["server_confirmed"], true, "{id}");

        // A writeable profile says nothing at all rather than "none", so the
        // UI cannot render a read-only badge for a connection that has none.
        assert!(read_only_json(false, id, Some(Enforcement::Server)).is_null());
    }
}

/// Every engine this build ships must be classifiable, or its read-only mode
/// silently degrades to nothing: `refuse_writes` passes a statement straight
/// through when `language_for_driver` returns `None`.
#[test]
fn no_registered_driver_is_left_without_a_client_side_classifier() {
    for id in known_driver_ids() {
        assert!(
            datagrep_ffi::query::language_for_driver(id).is_some(),
            "{id} has no language, so a read-only profile on it would refuse nothing \
             while still being badged read-only"
        );
    }
}
