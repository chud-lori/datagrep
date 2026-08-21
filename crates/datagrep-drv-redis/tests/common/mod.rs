use datagrep_api::{ConnectCtx, Connection, Driver, ResolvedConfig};
use datagrep_drv_redis::RedisDriver;

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

#[allow(dead_code)]
pub async fn flush(mgr: &mut redis::aio::ConnectionManager) {
    let _: () = redis::cmd("FLUSHDB")
        .query_async(mgr)
        .await
        .expect("FLUSHDB failed");
}

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
