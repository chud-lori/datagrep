//! Driver registration (ticket: "structure driver registration so adding
//! mongo/redis later is one line each").
//!
//! `CoreApi::register_driver` is documented as "a hashmap insert — nothing is
//! constructed until first use" (`dbx-core::api`), so registering every
//! compiled-in driver unconditionally costs nothing at cold start (design P1)
//! — the `SqliteDriver`/`PostgresDriver` structs themselves aren't built
//! until a profile actually needs one.

use std::sync::Arc;

use dbx_core::CoreApi;

/// Register every driver this build was compiled with. Touch ONLY this
/// function to add an engine: `mongo`/`redis` each need one more
/// `core.register_driver(...)` line here plus their crate in `Cargo.toml`'s
/// `[dependencies]` — nothing else in this crate names a concrete driver
/// (design §3, crate rules: "no `if driver_id == …` above `dbx-api`").
pub fn register_drivers(core: &CoreApi) {
    core.register_driver("sqlite", || Arc::new(dbx_drv_sqlite::SqliteDriver::new()));
    core.register_driver("postgres", || {
        Arc::new(dbx_drv_postgres::PostgresDriver::new())
    });
    // core.register_driver("mongo", || Arc::new(dbx_drv_mongo::MongoDriver::new()));
    // core.register_driver("redis", || Arc::new(dbx_drv_redis::RedisDriver::new()));
}

/// Stable registry ids this build knows about, for `dbx doctor` and for
/// validating a profile's `driver_id` before we ever try to connect.
pub fn known_driver_ids() -> &'static [&'static str] {
    &["sqlite", "postgres"]
}

/// A standalone `Driver` instance by registry id, for the operations
/// `CoreApi` does not expose a façade for: `parse_url`/`config_schema` (used
/// by `dbx profiles add`/`show`/`doctor`). `CoreApi::register_driver` only
/// ever hands the registry a *constructor* (`dbx_core::registry`: "nothing
/// is constructed until first use"), and `CoreApi` has no method to fetch a
/// driver back out — so this is a second, deliberately tiny, one-line-per-
/// driver mapping alongside [`register_drivers`], not a reach-around into
/// driver internals (both crates are already direct dependencies per this
/// crate's `Cargo.toml`, exactly the ones `register_drivers` names).
pub fn driver_for(id: &str) -> Option<Arc<dyn dbx_api::Driver>> {
    match id {
        "sqlite" => Some(Arc::new(dbx_drv_sqlite::SqliteDriver::new())),
        "postgres" => Some(Arc::new(dbx_drv_postgres::PostgresDriver::new())),
        _ => None,
    }
}

/// Guess a profile's driver from a pasted connection URL (`dbx profiles add`).
/// One line per driver, same spirit as [`register_drivers`].
pub fn driver_for_url(url: &str) -> Option<(&'static str, Arc<dyn dbx_api::Driver>)> {
    if url == ":memory:" || url.starts_with("sqlite://") {
        Some(("sqlite", driver_for("sqlite")?))
    } else if url.starts_with("postgres://") || url.starts_with("postgresql://") {
        Some(("postgres", driver_for("postgres")?))
    } else {
        None
    }
}

/// The SQL dialect (hence [`dbx_lang::Language`]) a profile's driver speaks.
/// Static per driver id rather than read from a live connection's
/// `Capabilities`, so `dbx query` can split/classify a script before
/// deciding whether it even needs to connect anywhere (only sqlite/postgres
/// — both SQL — exist in this build; mongo/redis will need a real entry here
/// when they land, not a fallback).
pub fn language_for_driver(id: &str) -> Option<dbx_api::LanguageId> {
    match id {
        "sqlite" => Some(dbx_api::LanguageId::Sql(dbx_api::SqlDialect::Sqlite)),
        "postgres" => Some(dbx_api::LanguageId::Sql(dbx_api::SqlDialect::Postgres)),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn driver_for_url_detects_both_drivers() {
        assert_eq!(
            driver_for_url("sqlite:///tmp/x.db").map(|(id, _)| id),
            Some("sqlite")
        );
        assert_eq!(driver_for_url(":memory:").map(|(id, _)| id), Some("sqlite"));
        assert_eq!(
            driver_for_url("postgres://u@h/db").map(|(id, _)| id),
            Some("postgres")
        );
        assert_eq!(
            driver_for_url("postgresql://u@h/db").map(|(id, _)| id),
            Some("postgres")
        );
        assert!(driver_for_url("mongodb://h/db").is_none());
    }

    #[test]
    fn driver_for_and_language_for_driver_agree_with_known_driver_ids() {
        for id in known_driver_ids() {
            assert!(driver_for(id).is_some(), "{id} should construct");
            assert!(
                language_for_driver(id).is_some(),
                "{id} should have a language"
            );
        }
        assert!(driver_for("mongo").is_none());
        assert!(language_for_driver("redis").is_none());
    }
}
