use std::sync::Arc;

use adw::prelude::*;
use adw::subclass::prelude::*;
use gtk::{gio, glib};
use serde_json::{json, Value};

use crate::engine;
use crate::ffi::Core;

struct Engine {
    id: &'static str,
    scheme: &'static str,
    aliases: &'static [&'static str],
    tls_scheme: Option<&'static str>,
    default_port: Option<u16>,
    file_based: bool,
    database_label: &'static str,
}

// Kept in step with datagrep-ffi/src/drivers.rs: an engine the build cannot route would fail on Add.
const ENGINES: [Engine; 6] = [
    Engine {
        id: "postgres",
        scheme: "postgres://",
        aliases: &["postgresql://"],
        tls_scheme: None,
        default_port: Some(5432),
        file_based: false,
        database_label: "Database",
    },
    Engine {
        id: "mysql",
        scheme: "mysql://",
        aliases: &["mariadb://"],
        tls_scheme: None,
        default_port: Some(3306),
        file_based: false,
        database_label: "Database",
    },
    Engine {
        id: "sqlite",
        scheme: "sqlite://",
        aliases: &[],
        tls_scheme: None,
        default_port: None,
        file_based: true,
        database_label: "File",
    },
    Engine {
        id: "redis",
        scheme: "redis://",
        aliases: &["rediss://"],
        tls_scheme: None,
        default_port: Some(6379),
        file_based: false,
        database_label: "Database index",
    },
    Engine {
        id: "mongo",
        scheme: "mongodb://",
        aliases: &["mongodb+srv://"],
        tls_scheme: None,
        default_port: Some(27017),
        file_based: false,
        database_label: "Database",
    },
    Engine {
        id: "elasticsearch",
        scheme: "http://",
        aliases: &["elasticsearch://"],
        tls_scheme: Some("https://"),
        default_port: Some(9200),
        file_based: false,
        database_label: "Default index",
    },
];

fn engine_by_id(id: &str) -> Option<&'static Engine> {
    let key = engine::canonical_driver_id(id)?;
    ENGINES.iter().find(|e| e.id == key)
}

#[derive(Debug, Default, Clone, PartialEq)]
struct Fields {
    engine_id: String,
    host: String,
    port: String,
    database: String,
    username: String,
    password: String,
    file_path: String,
    tls: bool,
    extras: String,
}

// Unreserved set only (A-Za-z0-9-._~), matching the macOS encoder, so URLs round-trip through the CLI.
fn percent_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Ok(v) = u8::from_str_radix(&s[i + 1..i + 3], 16) {
                out.push(v);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn build_url(f: &Fields, include_password: bool) -> String {
    let Some(e) = engine_by_id(&f.engine_id) else {
        return String::new();
    };
    if e.file_based {
        let path = f.file_path.trim();
        if path.is_empty() {
            return String::new();
        }
        if path == ":memory:" {
            return path.to_string();
        }
        return if path.starts_with('/') {
            format!("{}{path}", e.scheme)
        } else {
            format!("{}/{path}", e.scheme)
        };
    }
    let host = f.host.trim();
    if host.is_empty() {
        return String::new();
    }
    let mut out = match e.tls_scheme {
        Some(tls) if f.tls => tls.to_string(),
        _ => e.scheme.to_string(),
    };
    let user = f.username.trim();
    if !user.is_empty() {
        out.push_str(&percent_encode(user));
        if include_password && !f.password.is_empty() {
            out.push(':');
            out.push_str(&percent_encode(&f.password));
        }
        out.push('@');
    }
    // An IPv6 literal keeps its brackets, or the port ':' reads as part of the address.
    if host.contains(':') && !host.starts_with('[') {
        out.push_str(&format!("[{host}]"));
    } else {
        out.push_str(host);
    }
    let port = f
        .port
        .trim()
        .parse::<u16>()
        .ok()
        .filter(|p| *p > 0)
        .or(e.default_port);
    if let Some(port) = port {
        out.push_str(&format!(":{port}"));
    }
    let db = f.database.trim();
    if !db.is_empty() {
        out.push('/');
        out.push_str(db);
    }
    let extras = f.extras.trim();
    if !extras.is_empty() {
        out.push('?');
        out.push_str(extras);
    }
    out
}

// DatagrepKit.ConnectionURL port; `engine_id` stays empty for a half-typed scheme so the caller keeps its fields.
fn parse_url(url: &str) -> Fields {
    let mut f = Fields::default();
    let trimmed = url.trim();
    let lower = trimmed.to_lowercase();
    if lower == ":memory:" {
        f.engine_id = "sqlite".into();
        f.file_path = ":memory:".into();
        return f;
    }

    let mut engine: Option<&Engine> = None;
    'outer: for e in &ENGINES {
        let mut schemes = vec![e.scheme];
        schemes.extend(e.aliases);
        if let Some(tls) = e.tls_scheme {
            schemes.push(tls);
        }
        for scheme in schemes {
            if lower.starts_with(scheme) {
                engine = Some(e);
                break 'outer;
            }
        }
    }
    let Some(engine) = engine else {
        return f;
    };
    f.engine_id = engine.id.to_string();

    let Some(scheme_end) = trimmed.find("://") else {
        return f;
    };
    let scheme = format!("{}://", trimmed[..scheme_end].to_lowercase());
    f.tls = engine.tls_scheme == Some(scheme.as_str());
    let mut rest = &trimmed[scheme_end + 3..];

    if engine.file_based {
        f.file_path = rest.to_string();
        return f;
    }
    if let Some(q) = rest.find('?') {
        f.extras = rest[q + 1..].to_string();
        rest = &rest[..q];
    }
    // First '/', so an Elasticsearch proxy prefix containing a slash stays whole.
    if let Some(slash) = rest.find('/') {
        f.database = percent_decode(&rest[slash + 1..]);
        rest = &rest[..slash];
    }
    // Last '@': a password may legally contain one.
    if let Some(at) = rest.rfind('@') {
        let userinfo = &rest[..at];
        rest = &rest[at + 1..];
        match userinfo.split_once(':') {
            Some((user, password)) => {
                f.username = percent_decode(user);
                f.password = percent_decode(password);
            }
            None => f.username = percent_decode(userinfo),
        }
    }
    if let Some(stripped) = rest.strip_prefix('[') {
        if let Some(close) = stripped.find(']') {
            f.host = stripped[..close].to_string();
            if let Some(port) = stripped[close + 1..].strip_prefix(':') {
                f.port = port.to_string();
            }
        }
    } else if let Some((host, port)) = rest.rsplit_once(':') {
        f.host = host.to_string();
        f.port = port.to_string();
    } else {
        f.host = rest.to_string();
    }
    f
}

// The ABI masks a stored secret to "••••"; it must never be pasted into a URL.
fn config_str(config: &Value, key: &str) -> String {
    match config.get(key) {
        Some(Value::String(s)) if s != "••••" => s.clone(),
        Some(Value::Number(n)) => n.to_string(),
        Some(Value::Bool(b)) => b.to_string(),
        _ => String::new(),
    }
}

fn fields_from_config(driver: &str, config: &Value) -> Fields {
    let mut f = Fields::default();
    let Some(e) = engine_by_id(driver) else {
        return f;
    };
    f.engine_id = e.id.to_string();
    if e.file_based {
        f.file_path = config_str(config, "path");
        return f;
    }
    f.host = config_str(config, "host");
    if f.host.is_empty() {
        f.host = config_str(config, "hosts")
            .split(',')
            .next()
            .unwrap_or("")
            .trim()
            .to_string();
    }
    f.port = config_str(config, "port");
    f.username = config_str(config, "user");
    if f.username.is_empty() {
        f.username = config_str(config, "username");
    }
    f.database = config_str(config, "database");
    if f.database.is_empty() {
        f.database = config_str(config, "db");
    }
    if f.database.is_empty() {
        f.database = config_str(config, "index");
    }
    if e.tls_scheme.is_some() {
        let tls = config_str(config, "tls");
        f.tls = tls == "true" || tls == "require";
    }
    f
}

const KEYCHAIN_NEW: &str = "The password is moved into the system keychain before the connection \
is written; it never reaches disk in plain text and is never shown in the URL below.";
const KEYCHAIN_STORED: &str = "A password is saved in the system keychain. Leave this blank to \
keep it — datagrep never reads it back into the window.";

mod imp {
    use std::cell::{Cell, OnceCell, RefCell};
    use std::sync::OnceLock;

    use glib::subclass::Signal;

    use super::*;

    pub struct Widgets {
        pub engine_row: adw::ComboRow,
        pub name_row: adw::EntryRow,
        pub host_row: adw::EntryRow,
        pub port_row: adw::SpinRow,
        pub file_row: adw::EntryRow,
        pub database_row: adw::EntryRow,
        pub auth_group: adw::PreferencesGroup,
        pub username_row: adw::EntryRow,
        pub password_row: adw::PasswordEntryRow,
        pub tls_row: adw::SwitchRow,
        pub url_row: adw::EntryRow,
        pub test_row: adw::ActionRow,
        pub swatches: Vec<(String, gtk::CheckButton)>,
        pub limit_row: adw::SpinRow,
        pub idle_row: adw::SpinRow,
        pub read_only_row: adw::SwitchRow,
        pub confirm_row: adw::SwitchRow,
        pub enforcement_row: adw::ActionRow,
        pub save_button: gtk::Button,
        pub error_label: gtk::Label,
    }

    #[derive(Default)]
    pub struct ConnectionDialog {
        pub core: OnceCell<Arc<Core>>,
        pub widgets: OnceCell<Widgets>,
        pub editing: Cell<bool>,
        pub syncing: Cell<bool>,
        pub testing: Cell<bool>,
        pub original_name: RefCell<String>,
        pub original_url_no_password: RefCell<String>,
        pub have_original: Cell<bool>,
        pub orig_read_only: Cell<bool>,
        pub orig_confirm_writes: Cell<bool>,
        pub orig_color: RefCell<String>,
        pub orig_auto_limit: Cell<i64>,
        pub orig_idle_timeout: Cell<i64>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for ConnectionDialog {
        const NAME: &'static str = "DgConnectionDialog";
        type Type = super::ConnectionDialog;
        type ParentType = adw::Dialog;
    }

    impl ObjectImpl for ConnectionDialog {
        fn signals() -> &'static [Signal] {
            static SIGNALS: OnceLock<Vec<Signal>> = OnceLock::new();
            SIGNALS.get_or_init(|| {
                vec![Signal::builder("saved")
                    .param_types([String::static_type()])
                    .build()]
            })
        }

        fn constructed(&self) {
            self.parent_constructed();
            self.obj().build();
        }
    }

    impl WidgetImpl for ConnectionDialog {}
    impl AdwDialogImpl for ConnectionDialog {}
}

glib::wrapper! {
    pub struct ConnectionDialog(ObjectSubclass<imp::ConnectionDialog>)
        @extends adw::Dialog, gtk::Widget,
        @implements gtk::Accessible, gtk::Buildable, gtk::ConstraintTarget;
}

impl ConnectionDialog {
    pub fn for_new(core: Arc<Core>) -> Self {
        let dialog: Self = glib::Object::new();
        dialog.imp().core.set(core).ok().unwrap();
        dialog.set_title("New Connection");
        let w = dialog.widgets();
        w.save_button.set_label("Add");
        w.enforcement_row.set_visible(false);
        dialog.reshape_for_engine();
        dialog.render_url_from_fields();
        dialog
    }

    pub fn for_editing(core: Arc<Core>, name: &str) -> Self {
        let dialog: Self = glib::Object::new();
        dialog.imp().core.set(core).ok().unwrap();
        dialog.set_title("Edit Connection");
        dialog.imp().editing.set(true);
        dialog.imp().original_name.replace(name.to_string());
        dialog.widgets().save_button.set_label("Save");
        dialog.seed_for_edit(name);
        dialog
    }

    fn widgets(&self) -> &imp::Widgets {
        self.imp().widgets.get().unwrap()
    }

    fn core(&self) -> Arc<Core> {
        self.imp().core.get().unwrap().clone()
    }

    // ---- construction ----------------------------------------------------

    fn build(&self) {
        ensure_swatch_css();
        self.set_content_width(620);

        let header = adw::HeaderBar::new();
        header.set_show_start_title_buttons(false);
        header.set_show_end_title_buttons(false);
        let cancel = gtk::Button::with_label("Cancel");
        cancel.connect_clicked(glib::clone!(
            #[weak(rename_to = dialog)]
            self,
            move |_| {
                dialog.close();
            }
        ));
        header.pack_start(&cancel);
        let save_button = gtk::Button::with_label("Add");
        save_button.add_css_class("suggested-action");
        save_button.connect_clicked(glib::clone!(
            #[weak(rename_to = dialog)]
            self,
            move |_| dialog.on_accept()
        ));
        header.pack_end(&save_button);

        let page = adw::PreferencesPage::new();

        let connection_group = adw::PreferencesGroup::new();
        connection_group.set_title("Connection");

        let engine_row = adw::ComboRow::new();
        engine_row.set_title("Engine");
        let labels: Vec<String> = ENGINES.iter().map(|e| engine::display_name(e.id)).collect();
        let label_refs: Vec<&str> = labels.iter().map(String::as_str).collect();
        engine_row.set_model(Some(&gtk::StringList::new(&label_refs)));
        engine_row.set_factory(Some(&engine_factory()));
        connection_group.add(&engine_row);

        let name_row = adw::EntryRow::new();
        name_row.set_title("Name");
        connection_group.add(&name_row);
        let host_row = adw::EntryRow::new();
        host_row.set_title("Host");
        connection_group.add(&host_row);
        let port_row = adw::SpinRow::with_range(0.0, 65535.0, 1.0);
        port_row.set_title("Port");
        connection_group.add(&port_row);
        let file_row = adw::EntryRow::new();
        file_row.set_title("File");
        let browse = gtk::Button::from_icon_name("document-open-symbolic");
        browse.set_valign(gtk::Align::Center);
        browse.add_css_class("flat");
        browse.set_tooltip_text(Some("Choose a SQLite database file"));
        browse.connect_clicked(glib::clone!(
            #[weak(rename_to = dialog)]
            self,
            move |_| dialog.on_browse_file()
        ));
        file_row.add_suffix(&browse);
        connection_group.add(&file_row);
        let database_row = adw::EntryRow::new();
        database_row.set_title("Database");
        connection_group.add(&database_row);
        let tls_row = adw::SwitchRow::new();
        tls_row.set_title("Use TLS (https)");
        connection_group.add(&tls_row);
        page.add(&connection_group);

        let auth_group = adw::PreferencesGroup::new();
        auth_group.set_title("Authentication");
        auth_group.set_description(Some(KEYCHAIN_NEW));
        let username_row = adw::EntryRow::new();
        username_row.set_title("Username");
        auth_group.add(&username_row);
        let password_row = adw::PasswordEntryRow::new();
        password_row.set_title("Password");
        auth_group.add(&password_row);
        page.add(&auth_group);

        let url_group = adw::PreferencesGroup::new();
        let url_row = adw::EntryRow::new();
        url_row.set_title("Connection URL");
        url_row.add_css_class("monospace");
        url_group.add(&url_row);
        let test_row = adw::ActionRow::new();
        test_row.set_title("Test Connection");
        test_row.set_subtitle("Opens one connection with these settings and reports what answers; nothing is saved by testing.");
        test_row.set_activatable(true);
        test_row.add_prefix(&gtk::Image::from_icon_name(
            "network-transmit-receive-symbolic",
        ));
        test_row.connect_activated(glib::clone!(
            #[weak(rename_to = dialog)]
            self,
            move |_| dialog.on_test_connection()
        ));
        url_group.add(&test_row);
        page.add(&url_group);

        let marker_group = adw::PreferencesGroup::new();
        marker_group.set_title("Colour Marker");
        marker_group.set_description(Some(
            "Marks this connection everywhere it appears. The colour is a caution \
             stripe, not decoration — every marked surface also says so in words.",
        ));
        let swatch_box = gtk::Box::new(gtk::Orientation::Horizontal, 10);
        swatch_box.set_margin_top(6);
        let mut swatches: Vec<(String, gtk::CheckButton)> = Vec::new();
        let none = gtk::CheckButton::new();
        none.add_css_class("marker-swatch");
        none.add_css_class("marker-none");
        none.set_tooltip_text(Some("No marker"));
        none.set_active(true);
        swatch_box.append(&none);
        swatches.push((String::new(), none.clone()));
        for name in engine::MARKER_NAMES {
            let check = gtk::CheckButton::new();
            check.set_group(Some(&none));
            check.add_css_class("marker-swatch");
            check.add_css_class("marker-colored");
            check.add_css_class(&format!("marker-{name}"));
            let mut tip = name.to_string();
            if let Some(first) = tip.get_mut(0..1) {
                first.make_ascii_uppercase();
            }
            check.set_tooltip_text(Some(&tip));
            swatch_box.append(&check);
            swatches.push((name.to_string(), check));
        }
        marker_group.add(&swatch_box);
        page.add(&marker_group);

        let limits_group = adw::PreferencesGroup::new();
        limits_group.set_title("Limits");
        let limit_row = adw::SpinRow::with_range(0.0, 1_000_000_000.0, 100.0);
        limit_row.set_title("Row limit");
        limit_row.set_subtitle("Rows fetched before datagrep stops on its own; 0 means no limit");
        limits_group.add(&limit_row);
        let idle_row = adw::SpinRow::with_range(0.0, 86_400.0, 30.0);
        idle_row.set_title("Idle timeout");
        idle_row.set_subtitle("Seconds before an unused connection is dropped; 0 means never");
        limits_group.add(&idle_row);
        page.add(&limits_group);

        let safety_group = adw::PreferencesGroup::new();
        safety_group.set_title("Safety");
        let read_only_row = adw::SwitchRow::new();
        read_only_row.set_title("Read-only");
        read_only_row.set_subtitle(
            "Refuses writes on this connection even when the database account is allowed to make them",
        );
        safety_group.add(&read_only_row);
        let confirm_row = adw::SwitchRow::new();
        confirm_row.set_title("Ask before running a write");
        confirm_row.set_subtitle("Shows a confirmation before INSERT / UPDATE / DELETE / DROP");
        safety_group.add(&confirm_row);
        let enforcement_row = adw::ActionRow::new();
        enforcement_row.set_title("Check read-only enforcement");
        enforcement_row.set_subtitle(
            "Asks the engine which protection is actually in force — server, client, or none",
        );
        enforcement_row.set_activatable(true);
        enforcement_row.connect_activated(glib::clone!(
            #[weak(rename_to = dialog)]
            self,
            move |_| dialog.on_check_enforcement()
        ));
        safety_group.add(&enforcement_row);
        page.add(&safety_group);

        let error_label = gtk::Label::new(None);
        error_label.set_wrap(true);
        error_label.set_selectable(true);
        error_label.add_css_class("error");
        error_label.set_margin_start(18);
        error_label.set_margin_end(18);
        error_label.set_margin_top(6);
        error_label.set_visible(false);

        let content = gtk::Box::new(gtk::Orientation::Vertical, 0);
        content.append(&error_label);
        page.set_vexpand(true);
        content.append(&page);

        let toolbar = adw::ToolbarView::new();
        toolbar.add_top_bar(&header);
        toolbar.set_content(Some(&content));
        self.set_child(Some(&toolbar));

        engine_row.connect_selected_notify(glib::clone!(
            #[weak(rename_to = dialog)]
            self,
            move |_| {
                dialog.reshape_for_engine();
                if !dialog.imp().syncing.get() {
                    dialog.render_url_from_fields();
                }
            }
        ));
        for row in [&host_row, &database_row, &username_row, &file_row] {
            row.connect_changed(glib::clone!(
                #[weak(rename_to = dialog)]
                self,
                move |_| dialog.on_field_edited()
            ));
        }
        port_row.connect_value_notify(glib::clone!(
            #[weak(rename_to = dialog)]
            self,
            move |_| dialog.on_field_edited()
        ));
        tls_row.connect_active_notify(glib::clone!(
            #[weak(rename_to = dialog)]
            self,
            move |_| dialog.on_field_edited()
        ));
        url_row.connect_changed(glib::clone!(
            #[weak(rename_to = dialog)]
            self,
            move |_| dialog.on_url_edited()
        ));
        read_only_row.connect_active_notify(glib::clone!(
            #[weak(rename_to = dialog)]
            self,
            move |row| dialog.on_read_only_toggled(row.is_active())
        ));

        self.imp()
            .widgets
            .set(imp::Widgets {
                engine_row,
                name_row,
                host_row,
                port_row,
                file_row,
                database_row,
                auth_group,
                username_row,
                password_row,
                tls_row,
                url_row,
                test_row,
                swatches,
                limit_row,
                idle_row,
                read_only_row,
                confirm_row,
                enforcement_row,
                save_button,
                error_label,
            })
            .ok()
            .unwrap();
    }

    // ---- field <-> URL sync ----------------------------------------------

    fn current_engine(&self) -> &'static Engine {
        let idx = self.widgets().engine_row.selected() as usize;
        &ENGINES[idx.min(ENGINES.len() - 1)]
    }

    fn reshape_for_engine(&self) {
        let w = self.widgets();
        let e = self.current_engine();
        let file = e.file_based;
        w.host_row.set_visible(!file);
        w.port_row.set_visible(!file);
        w.auth_group.set_visible(!file);
        w.file_row.set_visible(file);
        w.database_row.set_visible(!file);
        w.database_row.set_title(e.database_label);
        w.tls_row.set_visible(e.tls_scheme.is_some());
        if e.tls_scheme.is_none() {
            w.tls_row.set_active(false);
        }
        if !file && !self.imp().syncing.get() {
            w.port_row.set_value(e.default_port.unwrap_or(0) as f64);
        }
    }

    fn fields_from_ui(&self) -> Fields {
        let w = self.widgets();
        Fields {
            engine_id: self.current_engine().id.to_string(),
            host: w.host_row.text().to_string(),
            port: (w.port_row.value() as i64).to_string(),
            database: w.database_row.text().to_string(),
            username: w.username_row.text().to_string(),
            password: w.password_row.text().to_string(),
            file_path: w.file_row.text().to_string(),
            tls: w.tls_row.is_active(),
            extras: String::new(),
        }
    }

    fn apply_fields_to_ui(&self, f: &Fields) {
        let w = self.widgets();
        if let Some(idx) = ENGINES.iter().position(|e| e.id == f.engine_id) {
            w.engine_row.set_selected(idx as u32);
        }
        self.reshape_for_engine();
        w.host_row.set_text(&f.host);
        if let Ok(port) = f.port.trim().parse::<u16>() {
            w.port_row.set_value(port as f64);
        }
        w.database_row.set_text(&f.database);
        w.username_row.set_text(&f.username);
        w.file_row.set_text(&f.file_path);
        w.tls_row.set_active(f.tls);
        // A password lifted from a pasted URL goes into the secure field only.
        if !f.password.is_empty() {
            w.password_row.set_text(&f.password);
        }
    }

    fn render_url_from_fields(&self) {
        let imp = self.imp();
        imp.syncing.set(true);
        self.widgets()
            .url_row
            .set_text(&build_url(&self.fields_from_ui(), false));
        imp.syncing.set(false);
    }

    fn on_field_edited(&self) {
        if !self.imp().syncing.get() {
            self.render_url_from_fields();
        }
    }

    fn on_url_edited(&self) {
        let imp = self.imp();
        if imp.syncing.get() {
            return;
        }
        let f = parse_url(&self.widgets().url_row.text());
        if f.engine_id.is_empty() {
            return;
        }
        imp.syncing.set(true);
        let had_password = !f.password.is_empty();
        self.apply_fields_to_ui(&f);
        imp.syncing.set(false);
        if had_password {
            // Re-render so the visible box never shows the password.
            self.render_url_from_fields();
        }
    }

    fn on_read_only_toggled(&self, on: bool) {
        let w = self.widgets();
        w.confirm_row.set_sensitive(!on);
        w.confirm_row.set_subtitle(if on {
            "Not needed while read-only is on — writes are refused"
        } else {
            "Shows a confirmation before INSERT / UPDATE / DELETE / DROP"
        });
    }

    fn on_browse_file(&self) {
        let chooser = gtk::FileDialog::new();
        chooser.set_title("Choose a SQLite database file");
        let parent = self.root().and_downcast::<gtk::Window>();
        chooser.open(
            parent.as_ref(),
            gio::Cancellable::NONE,
            glib::clone!(
                #[weak(rename_to = dialog)]
                self,
                move |result| {
                    if let Ok(file) = result {
                        if let Some(path) = file.path() {
                            dialog.widgets().file_row.set_text(&path.to_string_lossy());
                        }
                    }
                }
            ),
        );
    }

    fn current_color(&self) -> Option<String> {
        self.widgets()
            .swatches
            .iter()
            .find(|(_, check)| check.is_active())
            .map(|(name, _)| name.clone())
            .filter(|name| !name.is_empty())
    }

    fn set_color(&self, color: &str) {
        for (name, check) in &self.widgets().swatches {
            check.set_active(name == color);
        }
    }

    fn show_error(&self, text: &str) {
        let label = &self.widgets().error_label;
        label.set_text(text);
        label.set_visible(!text.is_empty());
    }

    // ---- test / enforcement ----------------------------------------------

    fn on_test_connection(&self) {
        let imp = self.imp();
        if imp.testing.get() {
            return;
        }
        let w = self.widgets();
        let url = build_url(&self.fields_from_ui(), true);
        let unchanged = imp.editing.get()
            && imp.have_original.get()
            && w.password_row.text().is_empty()
            && w.url_row.text().trim() == imp.original_url_no_password.borrow().as_str();
        let name = if unchanged {
            imp.original_name.borrow().clone()
        } else {
            String::new()
        };
        if name.is_empty() && url.is_empty() {
            w.test_row
                .set_subtitle("Complete the connection details first.");
            return;
        }
        imp.testing.set(true);
        w.test_row.set_subtitle("Connecting…");

        let core = self.core();
        let (tx, rx) = async_channel::bounded(1);
        std::thread::spawn(move || {
            let _ = tx.send_blocking(core.test_connection_json(&name, &url));
        });
        glib::spawn_future_local(glib::clone!(
            #[weak(rename_to = dialog)]
            self,
            async move {
                if let Ok(result) = rx.recv().await {
                    dialog.on_test_finished(result);
                }
            }
        ));
    }

    fn on_test_finished(&self, result: Result<String, crate::ffi::Error>) {
        self.imp().testing.set(false);
        let w = self.widgets();
        let json = match result {
            Ok(json) => json,
            Err(e) => {
                w.test_row.set_subtitle(&format!(
                    "Could not connect: {}",
                    glib::markup_escape_text(&e.0)
                ));
                return;
            }
        };
        let o: Value = serde_json::from_str(&json).unwrap_or_default();
        let driver = o["driver"].as_str().unwrap_or_default();
        let product = o["product"].as_str().unwrap_or_default();
        let version = o["version"].as_str().unwrap_or_default();
        let elapsed = o["elapsed_ms"].as_u64().unwrap_or_default();
        let mut what = if product.is_empty() {
            engine::display_name(driver)
        } else {
            product.to_string()
        };
        if !version.is_empty() && version.to_lowercase() != "unknown" {
            what.push(' ');
            what.push_str(version);
        }
        let details: Vec<String> = o["details"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|pair| {
                let pair = pair.as_array()?;
                Some(format!(
                    "{}: {}",
                    pair.first()?.as_str()?,
                    pair.get(1)?.as_str()?
                ))
            })
            .collect();
        let second_line = if details.is_empty() {
            "The engine accepted the connection and it was closed again — nothing was saved by testing.".to_string()
        } else {
            details.join(" · ")
        };
        w.test_row.set_subtitle(&glib::markup_escape_text(&format!(
            "Connected to {what} in {elapsed} ms\n{second_line}"
        )));
    }

    fn on_check_enforcement(&self) {
        let imp = self.imp();
        if !imp.editing.get() {
            return;
        }
        let name = imp.original_name.borrow().clone();
        let row = &self.widgets().enforcement_row;
        let json = match self.core().connection_info_json(&name) {
            Ok(json) => json,
            Err(e) => {
                row.set_subtitle(&glib::markup_escape_text(&e.0));
                return;
            }
        };
        let o: Value = serde_json::from_str(&json).unwrap_or_default();
        let text = match &o["read_only"] {
            Value::Null => {
                "This connection is writeable — no read-only protection is in force.".to_string()
            }
            ro => match ro["enforcement"].as_str().unwrap_or_default() {
                "server" => {
                    if ro["server_confirmed"].as_bool().unwrap_or(false) {
                        "Read-only enforced by the server — the engine opened this session \
                         read-only and will refuse a write itself."
                            .to_string()
                    } else {
                        "Read-only reported by the server, but not yet confirmed on a live \
                         session."
                            .to_string()
                    }
                }
                "client" => "Read-only blocked by datagrep only — statements classified as \
                             writes are refused before dispatch. The server would still accept \
                             a write from anything that bypasses datagrep."
                    .to_string(),
                _ => "No read-only enforcement is available for this engine — datagrep can \
                      refuse writes it sends, but nothing else is protected."
                    .to_string(),
            },
        };
        row.set_subtitle(&glib::markup_escape_text(&text));
    }

    // ---- seeding + accept ------------------------------------------------

    fn seed_for_edit(&self, name: &str) {
        let imp = self.imp();
        let w = self.widgets();
        w.name_row.set_text(name);
        let json = match self.core().profile_json(name) {
            Ok(json) => json,
            Err(e) => {
                self.show_error(&format!("Could not read this connection back: {}", e.0));
                self.reshape_for_engine();
                return;
            }
        };
        let o: Value = serde_json::from_str(&json).unwrap_or_default();
        imp.orig_read_only
            .set(o["read_only"].as_bool().unwrap_or(false));
        imp.orig_confirm_writes
            .set(o["confirm_writes"].as_bool().unwrap_or(false));
        imp.orig_color
            .replace(o["color"].as_str().unwrap_or_default().to_string());
        imp.orig_auto_limit
            .set(o["auto_limit"].as_i64().unwrap_or(0));
        imp.orig_idle_timeout
            .set(o["idle_timeout_s"].as_i64().unwrap_or(0));
        let has_secret = o["has_secret"].as_bool().unwrap_or(false);
        let driver = o["driver"].as_str().unwrap_or_default();

        imp.syncing.set(true);
        let fields = fields_from_config(driver, &o["config"]);
        self.apply_fields_to_ui(&fields);
        self.set_color(&imp.orig_color.borrow());
        w.read_only_row.set_active(imp.orig_read_only.get());
        w.confirm_row.set_active(imp.orig_confirm_writes.get());
        w.limit_row.set_value(imp.orig_auto_limit.get() as f64);
        w.idle_row.set_value(imp.orig_idle_timeout.get() as f64);
        self.on_read_only_toggled(imp.orig_read_only.get());
        let url = build_url(&self.fields_from_ui(), false);
        w.url_row.set_text(&url);
        imp.syncing.set(false);

        imp.original_url_no_password.replace(url);
        imp.have_original.set(true);
        if has_secret {
            w.password_row.set_title("Password (saved)");
            w.auth_group.set_description(Some(KEYCHAIN_STORED));
        }
    }

    fn options_json(&self) -> String {
        let w = self.widgets();
        let mut o = json!({
            "read_only": w.read_only_row.is_active(),
            "confirm_writes": w.confirm_row.is_active(),
        });
        let limit = w.limit_row.value() as i64;
        if limit > 0 {
            o["auto_limit"] = json!(limit);
        }
        let idle = w.idle_row.value() as i64;
        if idle > 0 {
            o["idle_timeout_s"] = json!(idle);
        }
        if let Some(color) = self.current_color() {
            o["color"] = json!(color);
        }
        o.to_string()
    }

    // Only the keys that actually moved: absent = leave alone, null = clear.
    fn patch_json(&self) -> String {
        let imp = self.imp();
        let w = self.widgets();
        let mut p = serde_json::Map::new();

        let name = w.name_row.text().trim().to_string();
        if name != *imp.original_name.borrow() {
            p.insert("name".into(), json!(name));
        }
        let current_url = build_url(&self.fields_from_ui(), false);
        let typed_password = !w.password_row.text().is_empty();
        let url_changed = imp.have_original.get()
            && current_url != *imp.original_url_no_password.borrow()
            && !current_url.is_empty();
        if typed_password || url_changed {
            let url = if typed_password {
                build_url(&self.fields_from_ui(), true)
            } else {
                current_url
            };
            p.insert("url".into(), json!(url));
        }
        if w.read_only_row.is_active() != imp.orig_read_only.get() {
            p.insert("read_only".into(), json!(w.read_only_row.is_active()));
        }
        if w.confirm_row.is_active() != imp.orig_confirm_writes.get() {
            p.insert("confirm_writes".into(), json!(w.confirm_row.is_active()));
        }
        let color = self.current_color().unwrap_or_default();
        if color != *imp.orig_color.borrow() {
            p.insert(
                "color".into(),
                if color.is_empty() {
                    Value::Null
                } else {
                    json!(color)
                },
            );
        }
        let limit = w.limit_row.value() as i64;
        if limit != imp.orig_auto_limit.get() {
            p.insert(
                "auto_limit".into(),
                if limit == 0 {
                    Value::Null
                } else {
                    json!(limit)
                },
            );
        }
        let idle = w.idle_row.value() as i64;
        if idle != imp.orig_idle_timeout.get() {
            p.insert(
                "idle_timeout_s".into(),
                if idle == 0 { Value::Null } else { json!(idle) },
            );
        }
        Value::Object(p).to_string()
    }

    fn on_accept(&self) {
        let imp = self.imp();
        let name = self.widgets().name_row.text().trim().to_string();
        if name.is_empty() {
            self.show_error("A name is required.");
            return;
        }
        if !imp.editing.get() {
            let url = build_url(&self.fields_from_ui(), true);
            if url.is_empty() {
                self.show_error("A host (or, for SQLite, a file) is required.");
                return;
            }
            if let Err(e) = self
                .core()
                .add_profile_json(&name, &url, &self.options_json())
            {
                self.show_error(&e.0);
                return;
            }
        } else {
            let patch = self.patch_json();
            if patch != "{}" {
                let original = imp.original_name.borrow().clone();
                if let Err(e) = self.core().update_profile(&original, &patch) {
                    self.show_error(&e.0);
                    return;
                }
            }
        }
        self.emit_by_name::<()>("saved", &[&name]);
        self.close();
    }
}

fn engine_factory() -> gtk::SignalListItemFactory {
    let factory = gtk::SignalListItemFactory::new();
    factory.connect_setup(|_, item| {
        let item: &gtk::ListItem = item.downcast_ref().unwrap();
        let row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        row.append(&gtk::Image::new());
        row.append(&gtk::Label::new(None));
        item.set_child(Some(&row));
    });
    factory.connect_bind(|_, item| {
        let item: &gtk::ListItem = item.downcast_ref().unwrap();
        let Some(engine) = ENGINES.get(item.position() as usize) else {
            return;
        };
        let row: gtk::Box = item.child().and_downcast().unwrap();
        let image: gtk::Image = row.first_child().and_downcast().unwrap();
        let label: gtk::Label = row.last_child().and_downcast().unwrap();
        image.set_paintable(Some(&engine::paintable(engine.id)));
        label.set_text(&engine::display_name(engine.id));
    });
    factory
}

fn ensure_swatch_css() {
    use std::sync::Once;
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        let mut css = String::from(
            "checkbutton.marker-swatch radio { min-width: 18px; min-height: 18px; \
             border-radius: 9999px; -gtk-icon-source: none; }\n\
             checkbutton.marker-colored radio:checked { -gtk-icon-source: \
             -gtk-icontheme('object-select-symbolic'); color: white; }\n\
             checkbutton.marker-none radio { background: none; \
             border: 2px solid alpha(currentColor, 0.35); }\n\
             checkbutton.marker-none radio:checked { background: none; \
             -gtk-icon-source: -gtk-icontheme('object-select-symbolic'); }\n",
        );
        for name in engine::MARKER_NAMES {
            let hex = engine::marker_hex(name).unwrap_or("#888888");
            css.push_str(&format!(
                "checkbutton.marker-{name} radio {{ background: {hex}; border-color: {hex}; }}\n"
            ));
        }
        let provider = gtk::CssProvider::new();
        provider.load_from_string(&css);
        if let Some(display) = gtk::gdk::Display::default() {
            gtk::style_context_add_provider_for_display(
                &display,
                &provider,
                gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
            );
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fields(engine: &str) -> Fields {
        Fields {
            engine_id: engine.into(),
            host: "db.internal".into(),
            port: "0".into(),
            database: "app".into(),
            username: "svc".into(),
            password: "p@ss:word".into(),
            ..Fields::default()
        }
    }

    #[test]
    fn the_visible_url_never_carries_the_password() {
        let f = fields("postgres");
        assert_eq!(build_url(&f, false), "postgres://svc@db.internal:5432/app");
        assert_eq!(
            build_url(&f, true),
            "postgres://svc:p%40ss%3Aword@db.internal:5432/app"
        );
    }

    #[test]
    fn urls_round_trip_through_parse() {
        let f = parse_url("postgres://svc:p%40ss%3Aword@db.internal:5433/app");
        assert_eq!(f.engine_id, "postgres");
        assert_eq!(f.host, "db.internal");
        assert_eq!(f.port, "5433");
        assert_eq!(f.username, "svc");
        assert_eq!(f.password, "p@ss:word");
        assert_eq!(f.database, "app");
    }

    #[test]
    fn sqlite_paths_and_memory_are_urls_too() {
        let f = Fields {
            engine_id: "sqlite".into(),
            file_path: "/home/me/data.db".into(),
            ..Fields::default()
        };
        assert_eq!(build_url(&f, true), "sqlite:///home/me/data.db");
        let mem = parse_url(":memory:");
        assert_eq!(mem.engine_id, "sqlite");
        assert_eq!(mem.file_path, ":memory:");
    }

    #[test]
    fn ipv6_hosts_keep_their_brackets() {
        let f = Fields {
            engine_id: "redis".into(),
            host: "::1".into(),
            port: "6380".into(),
            ..Fields::default()
        };
        assert_eq!(build_url(&f, true), "redis://[::1]:6380");
        let parsed = parse_url("redis://[::1]:6380");
        assert_eq!(parsed.host, "::1");
        assert_eq!(parsed.port, "6380");
    }

    #[test]
    fn the_tls_scheme_is_elasticsearch_https() {
        let mut f = fields("elasticsearch");
        f.tls = true;
        f.password.clear();
        assert!(build_url(&f, true).starts_with("https://"));
        assert!(parse_url("https://es.internal:9200").tls);
        assert!(!parse_url("http://es.internal:9200").tls);
    }

    #[test]
    fn an_unknown_scheme_keeps_the_current_engine() {
        assert!(parse_url("bogus://x").engine_id.is_empty());
        assert!(parse_url("postg").engine_id.is_empty());
    }

    #[test]
    fn masked_secrets_never_reach_a_rebuilt_url() {
        let config = json!({"host": "h", "user": "u", "password": "••••", "database": "d"});
        let f = fields_from_config("postgres", &config);
        assert!(f.password.is_empty());
        assert_eq!(f.host, "h");
    }
}
