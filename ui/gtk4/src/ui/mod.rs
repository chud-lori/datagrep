mod grid;
mod schema;
mod sidebar;
mod status_bar;
mod window;

pub use grid::ResultsGrid;
pub use schema::SchemaTree;
pub use sidebar::Sidebar;
pub use status_bar::StatusBar;
pub use window::Window;

use std::path::PathBuf;
use std::sync::Arc;

use adw::prelude::*;
use gtk::glib;

use std::cell::RefCell;
use std::rc::Rc;

use crate::connection_dialog::ConnectionDialog;
use crate::ffi::Core;
use crate::model::Profile;
use crate::tabs::EditorTabs;

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
            mount(app, Arc::new(core));
        }
        Err(error) => refuse_to_start(app, &error.0),
    }
}

/// The full wiring — window, editor tabs, dialog, run path — in one place.
pub fn mount(app: &adw::Application, core: Arc<Core>) -> Window {
    let window = Window::new(app, core.clone());
    let tabs = EditorTabs::new();
    window.editor_slot().set_child(Some(&tabs));

    let profiles = Rc::new(RefCell::new(load_profiles(&core)));
    tabs.set_connections(&profiles.borrow());
    match tabs.restored_window_connection() {
        Some(name) if window.select_connection(&name) => {}
        _ => tabs.set_window_connection(window.selected_connection().as_deref()),
    }

    window.connect_connection_selected({
        let tabs = tabs.clone();
        move |_, name| tabs.set_window_connection(Some(name))
    });

    tabs.connect_local("run-requested", false, {
        let window = window.downgrade();
        let profiles = profiles.clone();
        move |values| {
            let profile = values[1].get::<String>().unwrap_or_default();
            let sql = values[2].get::<String>().unwrap_or_default();
            if let Some(window) = window.upgrade() {
                let driver = profiles
                    .borrow()
                    .iter()
                    .find(|p| p.name == profile)
                    .map(|p| p.driver.clone())
                    .unwrap_or_default();
                window.run_on(&profile, &driver, &sql);
            }
            None
        }
    });

    let open_dialog = Rc::new({
        let core = core.clone();
        let profiles = profiles.clone();
        let window = window.downgrade();
        let tabs = tabs.downgrade();
        move || {
            let Some(window) = window.upgrade() else {
                return;
            };
            let dialog = ConnectionDialog::for_new(core.clone());
            dialog.connect_local("saved", false, {
                let core = core.clone();
                let profiles = profiles.clone();
                let window = window.downgrade();
                let tabs = tabs.clone();
                move |values| {
                    let name = values[1].get::<String>().unwrap_or_default();
                    profiles.replace(load_profiles(&core));
                    if let Some(window) = window.upgrade() {
                        window.reload_connections();
                        window.select_connection(&name);
                    }
                    if let Some(tabs) = tabs.upgrade() {
                        tabs.set_connections(&profiles.borrow());
                    }
                    None
                }
            });
            dialog.present(Some(&window));
        }
    });
    window.connect_new_connection({
        let open_dialog = open_dialog.clone();
        move |_| open_dialog()
    });
    tabs.connect_local("new-connection-requested", false, {
        let open_dialog = open_dialog.clone();
        move |_| {
            open_dialog();
            None
        }
    });

    window.connect_close_request({
        let tabs = tabs.clone();
        move |_| {
            tabs.persist_all();
            glib::Propagation::Proceed
        }
    });

    window.present();
    window
}

fn load_profiles(core: &Core) -> Vec<Profile> {
    core.profiles_list_json()
        .ok()
        .and_then(|json| Profile::parse_list(&json).ok())
        .unwrap_or_default()
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

fn load_style() {
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

/// Same filename the macOS app uses, so one `DATAGREP_CONFIG_DIR` serves both.
fn profiles_db_path() -> PathBuf {
    let dir = crate::store::support_dir();
    let _ = std::fs::create_dir_all(&dir);
    dir.join("profiles.sqlite")
}
