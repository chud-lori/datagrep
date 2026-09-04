mod conflict;
pub mod editing;
mod grid;
mod history;
mod inspector;
mod schema;
mod sidebar;
mod status_bar;
mod update_notice;
mod utility;
mod window;

pub use conflict::{ConflictDialog, ConflictDocument, ConflictField, ConflictReview};
pub use editing::StagedEditsBar;
pub use grid::ResultsGrid;
pub use history::HistoryPanel;
pub use inspector::Inspector;
pub use schema::SchemaTree;
pub use sidebar::Sidebar;
pub use status_bar::StatusBar;
pub use update_notice::UpdateNotice;
pub use utility::UtilityPane;
pub use window::Window;

use std::path::PathBuf;
use std::sync::Arc;

use adw::prelude::*;
use gtk::glib;

use std::cell::RefCell;
use std::rc::Rc;

use crate::appearance;
use crate::connection_dialog::ConnectionDialog;
use crate::ffi::Core;
use crate::model::update::UpdateCheck;
use crate::model::Profile;
use crate::tabs::EditorTabs;

pub const APP_ID: &str = "io.github.chud_lori.datagrep";

pub fn run() -> glib::ExitCode {
    let app = adw::Application::builder().application_id(APP_ID).build();
    app.connect_startup(|_| {
        load_style();
        appearance::apply_stored();
    });
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
    let utility = UtilityPane::mount(&window, history_dir());
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

    // A result belongs to the tab that ran it and to the connection it ran on.
    tabs.connect_local("tab-activated", false, {
        let window = window.downgrade();
        move |values| {
            let tab = values[1].get::<String>().unwrap_or_default();
            let connection = values[2].get::<String>().unwrap_or_default();
            if let Some(window) = window.upgrade() {
                window.set_active_tab(&tab, &connection);
            }
            None
        }
    });
    tabs.connect_local("tabs-closed", false, {
        let window = window.downgrade();
        let tabs = tabs.downgrade();
        move |_| {
            if let (Some(window), Some(tabs)) = (window.upgrade(), tabs.upgrade()) {
                window.forget_results(&tabs.live_ids());
            }
            None
        }
    });
    tabs.announce_active();

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

    // Adding, editing and removing a connection all end in the same reload.
    let reload = Rc::new({
        let core = core.clone();
        let profiles = profiles.clone();
        let tabs = tabs.downgrade();
        move |window: &Window, select: &str| {
            profiles.replace(load_profiles(&core));
            window.reload_connections();
            if !select.is_empty() {
                window.select_connection(select);
            }
            if let Some(tabs) = tabs.upgrade() {
                tabs.set_connections(&profiles.borrow());
            }
        }
    });

    let present_dialog = Rc::new({
        let reload = reload.clone();
        move |window: &Window, dialog: ConnectionDialog| {
            dialog.connect_local("saved", false, {
                let reload = reload.clone();
                let window = window.downgrade();
                move |values| {
                    let name = values[1].get::<String>().unwrap_or_default();
                    if let Some(window) = window.upgrade() {
                        reload(&window, &name);
                    }
                    None
                }
            });
            dialog.present(Some(window));
        }
    });

    let open_dialog = Rc::new({
        let core = core.clone();
        let present_dialog = present_dialog.clone();
        let window = window.downgrade();
        move || {
            if let Some(window) = window.upgrade() {
                present_dialog(&window, ConnectionDialog::for_new(core.clone()));
            }
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

    window.connect_edit_connection({
        let core = core.clone();
        let present_dialog = present_dialog.clone();
        move |window, name| {
            present_dialog(window, ConnectionDialog::for_editing(core.clone(), name));
        }
    });

    window.connect_remove_connection({
        let core = core.clone();
        let reload = reload.clone();
        move |window, name| confirm_remove(window, core.clone(), reload.clone(), name)
    });

    // A click on a table opens its rows in a tab of their own, through the
    // ordinary run path — the statement is the engine's, not the UI's.
    window.connect_object_activated({
        let core = core.clone();
        let tabs = tabs.clone();
        let profiles = profiles.clone();
        move |window, profile, path_json, name| {
            let driver = profiles
                .borrow()
                .iter()
                .find(|p| &p.name == profile)
                .map(|p| p.driver.clone())
                .unwrap_or_default();
            let database = profile_database(&core, profile);
            match crate::ffi::browse_statement(&driver, path_json, database.as_deref()) {
                Ok(sql) => tabs.open_browse(profile, name, &sql),
                Err(error) => window.status_bar().say(&error.0, true),
            }
        }
    });

    // "Open in Editor" puts the statement back in a tab; only Run re-runs it.
    utility.history().connect_open_requested({
        let tabs = tabs.clone();
        move |_, sql, connection| tabs.open_with_sql(sql, Some(connection))
    });

    window.connect_close_request({
        let tabs = tabs.clone();
        move |_| {
            tabs.persist_all();
            glib::Propagation::Proceed
        }
    });

    // AdwTabPage takes a GIcon, not a paintable, so tab marks re-resolve by signal.
    appearance::connect_changed({
        let tabs = tabs.clone();
        move |_| tabs.refresh_chrome()
    });

    let update = UpdateCheck::new();
    window
        .notice_slot()
        .set_child(Some(&UpdateNotice::new(&update)));
    window.connect_check_updates({
        let update = update.clone();
        move |_| update.check_now()
    });
    update.connect_check_finished({
        let window = window.downgrade();
        // Only check_now() reports here — the user asked and is watching.
        move |_, newer, failed| {
            let Some(window) = window.upgrade() else {
                return;
            };
            if failed {
                window.status_bar().say("update check failed", true);
            } else if !newer {
                let message = format!("datagrep {} is up to date", UpdateCheck::current_version());
                window.status_bar().say(&message, false);
            }
        }
    });
    update.check_on_launch_if_enabled();

    window.present();
    window
}

/// The database this connection opens to, read from the saved profile rather
/// than by dialling the server: the engine needs it to refuse a browse its
/// statement language cannot reach.
fn profile_database(core: &Core, profile: &str) -> Option<String> {
    let json = core.profile_json(profile).ok()?;
    let value: serde_json::Value = serde_json::from_str(&json).ok()?;
    value["config"]["database"]
        .as_str()
        .filter(|db| !db.is_empty())
        .map(str::to_owned)
}

/// Removing a connection also drops its saved secret, so it asks first and says so.
fn confirm_remove(
    window: &Window,
    core: Arc<Core>,
    reload: Rc<dyn Fn(&Window, &str)>,
    name: &str,
) {
    let dialog = adw::AlertDialog::new(
        Some(&format!("Remove ‘{name}’?")),
        Some(
            "datagrep forgets this connection and the password it saved in the keyring. \
             Nothing on the server is touched, and the queries you saved stay on disk.",
        ),
    );
    dialog.add_responses(&[("cancel", "Cancel"), ("remove", "Remove")]);
    dialog.set_response_appearance("remove", adw::ResponseAppearance::Destructive);
    dialog.set_default_response(Some("cancel"));
    dialog.set_close_response("cancel");
    let name = name.to_owned();
    dialog.choose(
        window,
        gio::Cancellable::NONE,
        glib::clone!(
            #[weak]
            window,
            move |response: glib::GString| {
                if response != "remove" {
                    return;
                }
                match core.remove_profile(&name) {
                    Ok(()) => reload(&window, ""),
                    Err(error) => window.status_bar().say(&error.0, true),
                }
            }
        ),
    );
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

/// Same filename the macOS app uses, so one `DATAGREP_CONFIG_DIR` serves both.
fn profiles_db_path() -> PathBuf {
    let dir = crate::store::support_dir();
    let _ = std::fs::create_dir_all(&dir);
    dir.join("profiles.sqlite")
}

/// The day files the other two front-ends read and write, byte for byte.
fn history_dir() -> PathBuf {
    crate::store::support_dir().join("history")
}
