//! Shared helpers for the `#[ignore]`d integration suite. Every test in this
//! directory talks to a **real** Redis server — see `../README.md` for how
//! to start one, and why every test calls [`flush`] first.
//!
//! Two connection kinds are used deliberately:
//! - [`connect`] goes through the public `datagrep-api` seam (`datagrep_drv_redis::RedisDriver`)
//!   — every test exercises the driver exactly as any consumer above
//!   `datagrep-api` would, no `pub(crate)` access.
//! - [`raw_connection`] is a plain `redis` crate connection, used only to
//!   set the stage (bulk seeding) or audit from outside (`INFO
//!   commandstats`) — never to exercise the thing under test.

use datagrep_api::{ConnectCtx, Connection, Driver, ResolvedConfig};
use datagrep_drv_redis::RedisDriver;

/// `DATAGREP_TEST_REDIS`, defaulting to a local disposable instance.
#[allow(dead_code)]
pub fn test_url() -> String {
    std::env::var("DATAGREP_TEST_REDIS").unwrap_or_else(|_| "redis://localhost:6379".to_string())
}

#[allow(dead_code)]
pub async fn connect() -> Box<dyn Connection> {
    let driver = RedisDriver::new();
    let cfg = driver
        .parse_url(&test_url())
        .expect("parse DATAGREP_TEST_REDIS url");
    let resolved = ResolvedConfig::without_secrets(cfg);
    driver
        .connect(&resolved, ConnectCtx::default())
        .await
        .expect(
            "connect to the integration test Redis failed — is it running? \
         see tests/README.md (`docker run -p 6379:6379 redis`)",
        )
}

#[allow(dead_code)]
pub async fn raw_connection() -> redis::aio::ConnectionManager {
    let client = redis::Client::open(test_url()).expect("invalid DATAGREP_TEST_REDIS url");
    client
        .get_connection_manager()
        .await
        .expect("raw connect to the integration test Redis failed")
}

/// Wipes the whole test database. **Every** integration test starts with
/// this — `DATAGREP_TEST_REDIS` must point at a disposable instance
/// (`tests/README.md` says so loudly; never point this at anything real).
#[allow(dead_code)]
pub async fn flush(mgr: &mut redis::aio::ConnectionManager) {
    let _: () = redis::cmd("FLUSHDB")
        .query_async(mgr)
        .await
        .expect("FLUSHDB failed");
}

/// Seed `n` plain string keys (`{prefix}{i}` -> `v{i}`) via pipelined
/// `SET`s, chunked so a 50k-key seed doesn't build one giant pipeline.
#[allow(dead_code)]
pub async fn seed_keys(mgr: &mut redis::aio::ConnectionManager, prefix: &str, n: u32) {
    const CHUNK: u32 = 2000;
    let mut start = 0;
    while start < n {
        let end = (start + CHUNK).min(n);
        let mut pipe = redis::pipe();
        for i in start..end {
            pipe.cmd("SET")
                .arg(format!("{prefix}{i}"))
                .arg(format!("v{i}"))
                .ignore();
        }
        let _: () = pipe
            .query_async(mgr)
            .await
            .expect("seed SET pipeline failed");
        start = end;
    }
}

/// Seed one HASH key with `n` fields (`f{i}` -> `v{i}`) via pipelined
/// `HSET`s, chunked the same way — used for the "a huge hash must page
/// rather than come back whole" contract at a 100k-field scale.
#[allow(dead_code)]
pub async fn seed_hash(mgr: &mut redis::aio::ConnectionManager, key: &str, n: u32) {
    const CHUNK: u32 = 2000;
    let mut start = 0;
    while start < n {
        let end = (start + CHUNK).min(n);
        let mut pipe = redis::pipe();
        for i in start..end {
            pipe.cmd("HSET")
                .arg(key)
                .arg(format!("f{i}"))
                .arg(format!("v{i}"))
                .ignore();
        }
        let _: () = pipe
            .query_async(mgr)
            .await
            .expect("seed HSET pipeline failed");
        start = end;
    }
}

/// Parse one `cmdstat_<name>:calls=<N>,...` line out of `INFO commandstats`
/// — a real, external way to prove a command was (or was never) sent to the
/// server, stronger than trying to intercept the wire protocol from a test.
#[allow(dead_code)]
pub async fn command_call_count(mgr: &mut redis::aio::ConnectionManager, name: &str) -> u64 {
    let info: String = redis::cmd("INFO")
        .arg("commandstats")
        .query_async(mgr)
        .await
        .expect("INFO commandstats failed");
    let needle = format!("cmdstat_{}:calls=", name.to_lowercase());
    for line in info.lines() {
        if let Some(rest) = line.strip_prefix(needle.as_str()) {
            let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
            return digits.parse().unwrap_or(0);
        }
    }
    0
}
