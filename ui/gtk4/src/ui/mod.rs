mod grid;
mod history;
mod inspector;
mod schema;
mod sidebar;
mod status_bar;
mod utility;
mod window;

pub use grid::ResultsGrid;
pub use history::HistoryPanel;
pub use inspector::Inspector;
pub use schema::SchemaTree;
pub use sidebar::Sidebar;
pub use status_bar::StatusBar;
pub use utility::UtilityPane;
pub use window::Window;

use std::path::PathBuf;
use std::rc::Rc;

use adw::prelude::*;
use gtk::glib;

use crate::ffi::Core;

pub const APP_ID: &str = "io.github.chud_lori.datagrep";

pub fn run() -> glib::ExitCode {
    let app = adw::Application::builder().application_id(APP_ID).build();
    app.connect_startup(|_| load_style());
    app.connect_activate(|app| match app.active_window() {
        Some(window) => window.present(),
        None => open(app),
    });
    app.run()
}

fn open(app: &adw::Application) {
    match Core::open(&profiles_db_path().to_string_lossy()) {
        Ok(core) => {
            let window = Window::new(app, Rc::new(core));
            UtilityPane::mount(&window, history_dir());
            window.present();
        }
        Err(error) => refuse_to_start(app, &error.0),
    }
}

/// Failing to open the profile store is the whole story, not a silent exit.
fn refuse_to_start(app: &adw::Application, reason: &str) {
    let page = adw::StatusPage::builder()
        .icon_name("dialog-error-symbolic")
        .title("datagrep cannot open its connection store")
        .description(reason)
        .build();
    let toolbar = adw::ToolbarView::new();
    toolbar.add_top_bar(&adw::HeaderBar::new());
    toolbar.set_content(Some(&page));
    let window = adw::ApplicationWindow::builder()
        .application(app)
        .default_width(600)
        .default_height(400)
        .content(&toolbar)
        .build();
    window.present();
}

/// Also loaded by `examples/preview.rs`, so a render shows what a launch shows.
pub fn load_style() {
    let Some(display) = gtk::gdk::Display::default() else {
        return;
    };
    let provider = gtk::CssProvider::new();
    provider.load_from_string(include_str!("style.css"));
    gtk::style_context_add_provider_for_display(
        &display,
        &provider,
        gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
    );
}

/// The directory the macOS and Qt apps use, so one `DATAGREP_CONFIG_DIR` serves all three.
fn config_dir() -> PathBuf {
    let dir = match std::env::var("DATAGREP_CONFIG_DIR") {
        Ok(value) if !value.is_empty() => expand_tilde(&value),
        _ => glib::user_data_dir().join("datagrep"),
    };
    let _ = std::fs::create_dir_all(&dir);
    dir
}

fn profiles_db_path() -> PathBuf {
    config_dir().join("profiles.sqlite")
}

/// The day files the other two front-ends read and write, byte for byte.
fn history_dir() -> PathBuf {
    config_dir().join("history")
}

// A leading ~ arrives unexpanded when the variable comes from a launcher.
fn expand_tilde(path: &str) -> PathBuf {
    match path.strip_prefix('~') {
        Some("") => glib::home_dir(),
        Some(rest) if rest.starts_with('/') => glib::home_dir().join(&rest[1..]),
        _ => PathBuf::from(path),
    }
}
