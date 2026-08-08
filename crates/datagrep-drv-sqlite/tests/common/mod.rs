//! Shared connect helper for the integration tests. Every test uses only
//! the public `datagrep-api` seam — no `pub(crate)` access — so these are honest
//! end-to-end exercises of the driver as any consumer above `datagrep-api` would
//! use it.

use std::collections::BTreeMap;
use std::sync::Arc;

use datagrep_api::{ConfigValue, ConnectCtx, Connection, ConnectionConfig, Driver, ResolvedConfig};
use datagrep_drv_sqlite::SqliteDriver;

#[allow(dead_code)]
pub async fn connect_memory() -> Box<dyn Connection> {
    connect_with(":memory:", false).await
}

#[allow(dead_code)]
pub async fn connect_with(path: &str, read_only: bool) -> Box<dyn Connection> {
    let mut values = BTreeMap::new();
    values.insert("path".to_string(), ConfigValue::Str(path.to_string()));
    values.insert("read_only".to_string(), ConfigValue::Bool(read_only));
    let cfg = ConnectionConfig {
        driver: Arc::from("sqlite"),
        values,
    };
    let resolved = ResolvedConfig::without_secrets(cfg);
    SqliteDriver::new()
        .connect(&resolved, ConnectCtx::default())
        .await
        .expect("connect should succeed")
}
