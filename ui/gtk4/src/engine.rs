use std::cell::RefCell;
use std::collections::HashMap;

use gtk::{gio, glib};

struct Art {
    svg: &'static str,
    light_fill: &'static str,
    dark_fill: &'static str,
}

// One artwork source for every platform (ui/linux references the same SVGs, same dark fills).
fn art_for(key: &str) -> Option<Art> {
    macro_rules! art {
        ($file:literal, $light:literal, $dark:literal) => {
            Some(Art {
                svg: include_str!(concat!(
                    "../../macos/Sources/DatagrepKit/Resources/EngineIcons/",
                    $file
                )),
                light_fill: $light,
                dark_fill: $dark,
            })
        };
    }
    match key {
        "postgres" => art!("postgresql.svg", "#4169E1", "#7D9EF5"),
        "mysql" => art!("mysql.svg", "#4479A1", "#7FB3D5"),
        "sqlite" => art!("sqlite.svg", "#003B57", "#4D9BC4"),
        "redis" => art!("redis.svg", "#FF4438", "#FF6B5E"),
        "mongo" => art!("mongodb.svg", "#47A248", "#6FD070"),
        _ => None,
    }
}

/// The shared spelling-folding table — EngineStyle.canonicalID / dg::canonicalDriverId.
pub fn canonical_driver_id(id: &str) -> Option<&'static str> {
    let s = id.to_lowercase();
    if s.starts_with("postgres") || s == "pg" || s == "psql" {
        Some("postgres")
    } else if s.starts_with("mysql") || s.starts_with("maria") {
        Some("mysql")
    } else if s.starts_with("sqlite") {
        Some("sqlite")
    } else if s.starts_with("redis") {
        Some("redis")
    } else if s.starts_with("mongo") {
        Some("mongo")
    } else if s.starts_with("elastic") || s.starts_with("opensearch") {
        Some("elasticsearch")
    } else {
        None
    }
}

pub fn display_name(driver_id: &str) -> String {
    match canonical_driver_id(driver_id) {
        Some("postgres") => "PostgreSQL".into(),
        Some("mysql") => "MySQL".into(),
        Some("sqlite") => "SQLite".into(),
        Some("redis") => "Redis".into(),
        Some("mongo") => "MongoDB".into(),
        Some("elasticsearch") => "Elasticsearch".into(),
        _ => driver_id.into(),
    }
}

/// Brand mark as a `GIcon`; the magnifier is Elasticsearch everywhere, and an unknown driver draws no mark at all.
pub fn icon(driver_id: &str, dark: bool) -> Option<gio::Icon> {
    let key = canonical_driver_id(driver_id)?;
    if key == "elasticsearch" {
        return Some(gio::ThemedIcon::new("system-search-symbolic").into());
    }
    thread_local! {
        static CACHE: RefCell<HashMap<(&'static str, bool), gio::Icon>> =
            RefCell::new(HashMap::new());
    }
    CACHE.with(|cache| {
        if let Some(icon) = cache.borrow().get(&(key, dark)) {
            return Some(icon.clone());
        }
        let a = art_for(key)?;
        // The fill sits once on the <svg> root; a failed replace leaves the light art.
        let svg = if dark {
            a.svg.replacen(
                &format!("fill=\"{}\"", a.light_fill),
                &format!("fill=\"{}\"", a.dark_fill),
                1,
            )
        } else {
            a.svg.to_string()
        };
        let icon: gio::Icon = gio::BytesIcon::new(&glib::Bytes::from_owned(svg)).into();
        cache.borrow_mut().insert((key, dark), icon.clone());
        Some(icon)
    })
}

pub const MARKER_NAMES: [&str; 7] = [
    "red", "orange", "yellow", "green", "blue", "purple", "graphite",
];

/// GNOME-palette values for the connection colour marker.
pub fn marker_hex(name: &str) -> Option<&'static str> {
    match name {
        "red" => Some("#e01b24"),
        "orange" => Some("#ff7800"),
        "yellow" => Some("#f5c211"),
        "green" => Some("#2ec27e"),
        "blue" => Some("#3584e4"),
        "purple" => Some("#9141ac"),
        "graphite" | "gray" | "grey" => Some("#77767b"),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stored_driver_spellings_fold_to_one_engine() {
        assert_eq!(canonical_driver_id("postgresql"), Some("postgres"));
        assert_eq!(canonical_driver_id("mariadb"), Some("mysql"));
        assert_eq!(canonical_driver_id("mongodb"), Some("mongo"));
        assert_eq!(canonical_driver_id("opensearch"), Some("elasticsearch"));
        assert_eq!(canonical_driver_id("db2"), None);
    }

    #[test]
    fn display_name_falls_back_to_the_stored_id() {
        assert_eq!(display_name("postgresql"), "PostgreSQL");
        assert_eq!(display_name("db2"), "db2");
    }

    #[test]
    fn every_marker_name_has_a_swatch_colour() {
        for name in MARKER_NAMES {
            assert!(marker_hex(name).is_some(), "{name}");
        }
    }
}
