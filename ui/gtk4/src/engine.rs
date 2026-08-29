use std::cell::RefCell;
use std::collections::HashMap;

use adw::prelude::*;
use gtk::subclass::prelude::ObjectSubclassIsExt;
use gtk::{gdk, gio, glib};

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

// The Qt magnifier and cylinder, on the same 16-grid proportions, as SVG source.
const ELASTIC_TINT: &str = "#00BFB3";

fn magnifier_svg(tint: &str) -> String {
    format!(
        "<svg xmlns=\"http://www.w3.org/2000/svg\" viewBox=\"0 0 16 16\">\
         <g fill=\"none\" stroke=\"{tint}\" stroke-width=\"1.92\" stroke-linecap=\"round\">\
         <circle cx=\"6.72\" cy=\"6.72\" r=\"4.48\"/>\
         <path d=\"M10.24 10.24L14.08 14.08\"/></g></svg>"
    )
}

fn cylinder_svg(dark: bool) -> String {
    let tint = if dark { "#c0bfbc" } else { "#5e5c64" };
    format!(
        "<svg xmlns=\"http://www.w3.org/2000/svg\" viewBox=\"0 0 16 16\"><g fill=\"{tint}\">\
         <ellipse cx=\"8\" cy=\"2.88\" rx=\"4.8\" ry=\"1.92\"/>\
         <rect x=\"3.2\" y=\"2.88\" width=\"9.6\" height=\"10.24\"/>\
         <ellipse cx=\"8\" cy=\"13.12\" rx=\"4.8\" ry=\"1.92\"/></g></svg>"
    )
}

// Cache key for anything canonical folding does not recognise.
const UNKNOWN: &str = "unknown";

fn svg_source(key: &str, dark: bool) -> String {
    if key == "elasticsearch" {
        return magnifier_svg(ELASTIC_TINT);
    }
    match art_for(key) {
        // The fill sits once on the <svg> root; a failed replace leaves the light art.
        Some(a) if dark => a.svg.replacen(
            &format!("fill=\"{}\"", a.light_fill),
            &format!("fill=\"{}\"", a.dark_fill),
            1,
        ),
        Some(a) => a.svg.to_string(),
        None => cylinder_svg(dark),
    }
}

/// Brand mark as a `GIcon`; the magnifier is Elasticsearch everywhere, an unknown driver a tinted cylinder — never a blank.
pub fn icon(driver_id: &str, dark: bool) -> gio::Icon {
    let key = canonical_driver_id(driver_id).unwrap_or(UNKNOWN);
    thread_local! {
        static CACHE: RefCell<HashMap<(&'static str, bool), gio::Icon>> =
            RefCell::new(HashMap::new());
    }
    CACHE.with(|cache| {
        if let Some(icon) = cache.borrow().get(&(key, dark)) {
            return icon.clone();
        }
        let bytes = glib::Bytes::from_owned(svg_source(key, dark));
        let icon: gio::Icon = gio::BytesIcon::new(&bytes).into();
        cache.borrow_mut().insert((key, dark), icon.clone());
        icon
    })
}

mod paintable_imp {
    use super::*;
    use gtk::gdk_pixbuf::PixbufLoader;
    use gtk::subclass::prelude::*;

    #[derive(Default)]
    pub struct EnginePaintable {
        pub key: RefCell<&'static str>,
        pub textures: RefCell<HashMap<bool, gdk::Texture>>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for EnginePaintable {
        const NAME: &'static str = "DgEnginePaintable";
        type Type = super::EnginePaintable;
        type Interfaces = (gdk::Paintable,);
    }

    impl ObjectImpl for EnginePaintable {}

    impl PaintableImpl for EnginePaintable {
        fn flags(&self) -> gdk::PaintableFlags {
            gdk::PaintableFlags::SIZE
        }

        fn intrinsic_width(&self) -> i32 {
            16
        }

        fn intrinsic_height(&self) -> i32 {
            16
        }

        // The Qt-icon-engine equivalent: light or dark is decided here, per paint.
        fn snapshot(&self, snapshot: &gdk::Snapshot, width: f64, height: f64) {
            let dark = adw::StyleManager::default().is_dark();
            if let Some(texture) = self.texture(dark) {
                texture.snapshot(snapshot, width, height);
            }
        }
    }

    impl EnginePaintable {
        fn texture(&self, dark: bool) -> Option<gdk::Texture> {
            if let Some(texture) = self.textures.borrow().get(&dark) {
                return Some(texture.clone());
            }
            let svg = svg_source(*self.key.borrow(), dark);
            // 64px so a 16px image stays crisp on 2x-4x displays.
            let loader = PixbufLoader::new();
            loader.connect_size_prepared(|loader, _, _| loader.set_size(64, 64));
            loader.write(svg.as_bytes()).ok()?;
            loader.close().ok()?;
            let texture = gdk::Texture::for_pixbuf(&loader.pixbuf()?);
            self.textures.borrow_mut().insert(dark, texture.clone());
            Some(texture)
        }
    }
}

glib::wrapper! {
    pub struct EnginePaintable(ObjectSubclass<paintable_imp::EnginePaintable>)
        @implements gdk::Paintable;
}

/// Palette-aware brand mark for `GtkImage`; repaints itself on every effective palette change.
pub fn paintable(driver_id: &str) -> gdk::Paintable {
    let key = canonical_driver_id(driver_id).unwrap_or(UNKNOWN);
    thread_local! {
        static CACHE: RefCell<HashMap<&'static str, EnginePaintable>> =
            RefCell::new(HashMap::new());
    }
    CACHE.with(|cache| {
        if let Some(paintable) = cache.borrow().get(key) {
            return paintable.clone().upcast();
        }
        let paintable: EnginePaintable = glib::Object::new();
        *paintable.imp().key.borrow_mut() = key;
        let weak = paintable.downgrade();
        adw::StyleManager::default().connect_dark_notify(move |_| {
            if let Some(paintable) = weak.upgrade() {
                paintable.invalidate_contents();
            }
        });
        cache.borrow_mut().insert(key, paintable.clone());
        paintable.upcast()
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
    fn no_driver_is_ever_blank() {
        assert!(svg_source("elasticsearch", false).contains(ELASTIC_TINT));
        assert!(svg_source("db2-or-whatever", false).contains("ellipse"));
        assert_ne!(svg_source("unknown", false), svg_source("unknown", true));
        assert!(svg_source("postgres", true).contains("#7D9EF5"));
    }

    #[test]
    fn every_marker_name_has_a_swatch_colour() {
        for name in MARKER_NAMES {
            assert!(marker_hex(name).is_some(), "{name}");
        }
    }
}
