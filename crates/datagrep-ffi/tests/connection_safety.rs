use datagrep_api::config::{ConfigValue, ConnectionConfig};
use datagrep_ffi::drivers::{driver_for, known_driver_ids};
use datagrep_ffi::query::refuse_writes;

fn tls_key(driver_id: &str) -> Option<(&'static str, ConfigValue)> {
    match driver_id {
        "postgres" => Some(("tls", ConfigValue::Str("disable".to_string()))),
        "redis" | "mongodb" | "elasticsearch" => Some(("tls", ConfigValue::Bool(false))),
        _ => None,
    }
}

fn parse(driver_id: &str, url: &str) -> Result<ConnectionConfig, String> {
    let driver = driver_for(driver_id).unwrap_or_else(|| panic!("{driver_id} is registered"));
    driver.parse_url(url).map_err(|e| e.to_string())
}

// ---- TLS defaults ------------------------------------------------------

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
        assert_eq!(
            default, &plaintext,
            "{id}.{key} default changed; if that is intended, update this test \
             deliberately — it exists to make the change visible"
        );
    }
}

#[test]
fn an_explicit_tls_request_is_never_silently_downgraded() {
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
        assert!(
            !text.contains("refused") && !text.contains("timed out"),
            "{id}: the driver dialled before refusing the TLS request: {err}"
        );
    }
}

// ---- read-only ---------------------------------------------------------

#[test]
fn the_client_side_guard_refuses_a_write_for_every_classifiable_engine() {
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

#[test]
fn no_enforcement_short_of_server_is_ever_reported_as_server() {
    use datagrep_api::Enforcement;
    use datagrep_ffi::core::read_only_json;

    for id in known_driver_ids() {
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

        assert!(read_only_json(false, id, Some(Enforcement::Server)).is_null());
    }
}

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
