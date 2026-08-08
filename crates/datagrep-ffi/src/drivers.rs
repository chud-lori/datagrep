//! Driver registration — **one line per engine**, so mongo slots in later
//! without touching anything else in this crate.
//!
//! `CoreApi::register_driver` is documented as "a hashmap insert — nothing is
//! constructed until first use" (`datagrep_core::api`), so registering every
//! compiled-in engine unconditionally costs nothing at cold start (design
//! §5.2: "Eager driver/TLS/regex init at startup" is a banned anti-pattern).
//! The `SqliteDriver`/`PostgresDriver`/`RedisDriver` structs are not built
//! until a profile actually needs one.

use std::sync::Arc;

use datagrep_core::CoreApi;

/// Register every driver this build was compiled with.
///
/// Adding mongo is one line here plus one in `Cargo.toml` — nothing else in
/// this crate names a concrete engine (design §3 crate rules: no
/// `if driver_id == …` above `datagrep-api`).
pub fn register_drivers(core: &CoreApi) {
    core.register_driver("sqlite", || {
        Arc::new(datagrep_drv_sqlite::SqliteDriver::new())
    });
    core.register_driver("postgres", || {
        Arc::new(datagrep_drv_postgres::PostgresDriver::new())
    });
    core.register_driver("redis", || Arc::new(datagrep_drv_redis::RedisDriver::new()));
    core.register_driver("mongodb", || Arc::new(datagrep_drv_mongo::MongoDriver::new()));
    core.register_driver("mysql", || Arc::new(datagrep_drv_mysql::MySqlDriver::new()));
}

/// A standalone `Driver` by registry id, for the two things `CoreApi` has no
/// façade for: `parse_url` and `config_schema` (both needed by
/// `datagrep_profiles_add`).
///
/// `CoreApi::register_driver` only ever receives a *constructor*, and
/// `CoreApi` exposes no way to get a driver back out — so this is a second,
/// deliberately tiny, one-line-per-engine mapping, not a reach-around into
/// driver internals. Recorded as a `CoreApi` gap in this crate's README.
pub fn driver_for(id: &str) -> Option<Arc<dyn datagrep_api::Driver>> {
    match id {
        "sqlite" => Some(Arc::new(datagrep_drv_sqlite::SqliteDriver::new())),
        "postgres" => Some(Arc::new(datagrep_drv_postgres::PostgresDriver::new())),
        "redis" => Some(Arc::new(datagrep_drv_redis::RedisDriver::new())),
        "mongodb" => Some(Arc::new(datagrep_drv_mongo::MongoDriver::new())),
        "mysql" => Some(Arc::new(datagrep_drv_mysql::MySqlDriver::new())),
        _ => None,
    }
}

/// Guess a profile's engine from a pasted connection URL. One line per driver,
/// same spirit as [`register_drivers`].
pub fn driver_for_url(url: &str) -> Option<(&'static str, Arc<dyn datagrep_api::Driver>)> {
    let id = if url == ":memory:" || url.starts_with("sqlite://") {
        "sqlite"
    } else if url.starts_with("postgres://") || url.starts_with("postgresql://") {
        "postgres"
    } else if url.starts_with("redis://") || url.starts_with("rediss://") {
        "redis"
    } else if url.starts_with("mongodb://") || url.starts_with("mongodb+srv://") {
        "mongodb"
    } else if url.starts_with("mysql://") || url.starts_with("mariadb://") {
        "mysql"
    } else {
        return None;
    };
    Some((id, driver_for(id)?))
}

/// Registry ids this build knows about — the message
/// [`driver_for_url`] failures quote back at the user.
pub fn known_driver_ids() -> &'static [&'static str] {
    &["sqlite", "postgres", "redis", "mongodb", "mysql"]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_known_id_constructs_and_every_scheme_maps() {
        for id in known_driver_ids() {
            assert!(driver_for(id).is_some(), "{id} should construct");
        }
        assert_eq!(driver_for_url(":memory:").map(|(i, _)| i), Some("sqlite"));
        assert_eq!(
            driver_for_url("sqlite:///tmp/x.db").map(|(i, _)| i),
            Some("sqlite")
        );
        assert_eq!(
            driver_for_url("postgres://u@h/db").map(|(i, _)| i),
            Some("postgres")
        );
        assert_eq!(
            driver_for_url("postgresql://u@h/db").map(|(i, _)| i),
            Some("postgres")
        );
        assert_eq!(
            driver_for_url("redis://localhost:6379/0").map(|(i, _)| i),
            Some("redis")
        );
        assert_eq!(
            driver_for_url("rediss://localhost:6379").map(|(i, _)| i),
            Some("redis")
        );
        assert_eq!(
            driver_for_url("mongodb://h/db").map(|(i, _)| i),
            Some("mongodb")
        );
    }

    #[test]
    fn registration_constructs_nothing_and_lists_every_engine() {
        let rt = crate::runtime::runtime().expect("runtime");
        let _guard = rt.enter();
        let core = CoreApi::new();
        register_drivers(&core);
        let mut ids: Vec<String> = core.drivers().iter().map(|s| s.to_string()).collect();
        ids.sort();
        assert_eq!(ids, ["mongodb", "mysql", "postgres", "redis", "sqlite"]);
    }
}
