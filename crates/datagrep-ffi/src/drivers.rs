use std::sync::Arc;

use datagrep_core::CoreApi;

pub fn register_drivers(core: &CoreApi) {
    core.register_driver("sqlite", || {
        Arc::new(datagrep_drv_sqlite::SqliteDriver::new())
    });
    core.register_driver("postgres", || {
        Arc::new(datagrep_drv_postgres::PostgresDriver::new())
    });
    core.register_driver("redis", || Arc::new(datagrep_drv_redis::RedisDriver::new()));
    core.register_driver("mongodb", || {
        Arc::new(datagrep_drv_mongo::MongoDriver::new())
    });
    core.register_driver("mysql", || Arc::new(datagrep_drv_mysql::MySqlDriver::new()));
    core.register_driver("elasticsearch", || {
        Arc::new(datagrep_drv_elasticsearch::ElasticsearchDriver::new())
    });
}

pub fn driver_for(id: &str) -> Option<Arc<dyn datagrep_api::Driver>> {
    match id {
        "sqlite" => Some(Arc::new(datagrep_drv_sqlite::SqliteDriver::new())),
        "postgres" => Some(Arc::new(datagrep_drv_postgres::PostgresDriver::new())),
        "redis" => Some(Arc::new(datagrep_drv_redis::RedisDriver::new())),
        "mongodb" => Some(Arc::new(datagrep_drv_mongo::MongoDriver::new())),
        "mysql" => Some(Arc::new(datagrep_drv_mysql::MySqlDriver::new())),
        "elasticsearch" => Some(Arc::new(
            datagrep_drv_elasticsearch::ElasticsearchDriver::new(),
        )),
        _ => None,
    }
}

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
    } else if url.starts_with("elasticsearch://")
        || url.starts_with("http://")
        || url.starts_with("https://")
    {
        "elasticsearch"
    } else {
        return None;
    };
    Some((id, driver_for(id)?))
}

pub fn known_driver_ids() -> &'static [&'static str] {
    &[
        "sqlite",
        "postgres",
        "redis",
        "mongodb",
        "mysql",
        "elasticsearch",
    ]
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
        for url in [
            "elasticsearch://localhost:9200",
            "http://localhost:9200",
            "https://es.example.com:9243",
        ] {
            assert_eq!(
                driver_for_url(url).map(|(i, _)| i),
                Some("elasticsearch"),
                "{url}"
            );
            let (_, driver) = driver_for_url(url).expect("routed");
            assert!(driver.parse_url(url).is_ok(), "{url}");
        }
    }

    #[test]
    fn registration_constructs_nothing_and_lists_every_engine() {
        let rt = crate::runtime::runtime().expect("runtime");
        let _guard = rt.enter();
        let core = CoreApi::new();
        register_drivers(&core);
        let mut ids: Vec<String> = core.drivers().iter().map(|s| s.to_string()).collect();
        ids.sort();
        assert_eq!(
            ids,
            [
                "elasticsearch",
                "mongodb",
                "mysql",
                "postgres",
                "redis",
                "sqlite"
            ]
        );
    }
}
