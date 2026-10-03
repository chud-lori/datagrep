// Runs the real Oracle sidecar when DATAGREP_SIDECAR_DIR holds a build of it; skipped otherwise.
use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use datagrep_api::config::{ConfigValue, ConnectionConfig, ResolvedConfig};
use datagrep_api::driver::{ConnectCtx, Driver};
use datagrep_api::DbError;
use datagrep_drv_sidecar::{SidecarDriver, ORACLE};

fn built() -> bool {
    std::env::var_os("DATAGREP_SIDECAR_DIR")
        .map(|d| std::path::Path::new(&d).join(ORACLE.program).is_file())
        .unwrap_or(false)
}

#[tokio::test]
async fn the_oracle_sidecar_agrees_with_its_manifest_and_fails_a_dead_server_cleanly() {
    if !built() {
        eprintln!("skipped: no {} in DATAGREP_SIDECAR_DIR", ORACLE.program);
        return;
    }
    let driver = SidecarDriver::new(&ORACLE);
    let mut cfg = driver
        .parse_url("oracle://scott:never-shown-77@127.0.0.1:1/FREEPDB1")
        .unwrap();
    cfg.driver = Arc::from("oracle");
    let err = tokio::time::timeout(
        Duration::from_secs(15),
        driver.connect(&ResolvedConfig::without_secrets(cfg), ConnectCtx::default()),
    )
    .await
    .expect("connect hung")
    .err()
    .expect("nothing listens on port 1");
    // A handshake mismatch would be a Protocol error; reaching the dial proves it passed.
    assert!(matches!(err, DbError::Connect(_)), "{err:?}");
    assert!(!err.to_string().contains("never-shown-77"), "{err}");
}

#[tokio::test]
async fn a_missing_engine_binary_is_a_connect_error_naming_where_it_looked() {
    if built() {
        return;
    }
    let mut values = BTreeMap::new();
    values.insert("host".into(), ConfigValue::Str("h".into()));
    let cfg = ResolvedConfig::without_secrets(ConnectionConfig {
        driver: Arc::from("oracle"),
        values,
    });
    let err = SidecarDriver::new(&ORACLE)
        .connect(&cfg, ConnectCtx::default())
        .await
        .err()
        .unwrap();
    assert!(
        matches!(&err, DbError::Connect(m) if m.contains("not installed") && m.contains(ORACLE.program)),
        "{err:?}"
    );
}
