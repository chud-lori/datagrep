use std::sync::Arc;

use adw::prelude::*;
use adw::subclass::prelude::*;
use gtk::{gio, glib};
use serde_json::{json, Value};

use crate::engine;
use crate::ffi::Core;
use crate::model::SafetyLevel;

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

const SSH_AUTH: [(&str, &str); 3] = [
    ("agent", "SSH Agent"),
    ("key", "Key File"),
    ("password", "Password"),
];

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
        pub ssh_group: adw::PreferencesGroup,
        pub ssh_row: adw::ExpanderRow,
        pub ssh_host_row: adw::EntryRow,
        pub ssh_port_row: adw::SpinRow,
        pub ssh_user_row: adw::EntryRow,
        pub ssh_auth_row: adw::ComboRow,
        pub ssh_key_row: adw::EntryRow,
        pub ssh_secret_row: adw::PasswordEntryRow,
        pub url_row: adw::EntryRow,
        pub test_row: adw::ActionRow,
        pub swatches: Vec<(String, gtk::CheckButton)>,
        pub limit_row: adw::SpinRow,
        pub idle_row: adw::SpinRow,
        pub read_only_row: adw::SwitchRow,
        pub safety_row: adw::ComboRow,
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
        pub orig_safety: Cell<SafetyLevel>,
        pub orig_color: RefCell<String>,
        pub orig_auto_limit: Cell<i64>,
        pub orig_idle_timeout: Cell<i64>,
        pub orig_ssh: RefCell<Value>,
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
        dialog.reshape_for_ssh_auth();
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

        let ssh_group = adw::PreferencesGroup::new();
        ssh_group.set_title("SSH Tunnel");
        let ssh_row = adw::ExpanderRow::new();
        ssh_row.set_title("Connect over SSH");
        ssh_row.set_subtitle("Reach the database host and port above from an SSH server");
        ssh_row.set_show_enable_switch(true);
        ssh_row.set_enable_expansion(false);
        let ssh_host_row = adw::EntryRow::new();
        ssh_host_row.set_title("SSH Host");
        ssh_row.add_row(&ssh_host_row);
        let ssh_port_row = adw::SpinRow::with_range(1.0, 65535.0, 1.0);
        ssh_port_row.set_title("SSH Port");
        ssh_port_row.set_value(22.0);
        ssh_row.add_row(&ssh_port_row);
        let ssh_user_row = adw::EntryRow::new();
        ssh_user_row.set_title("SSH User");
        ssh_row.add_row(&ssh_user_row);
        let ssh_auth_row = adw::ComboRow::new();
        ssh_auth_row.set_title("Sign In With");
        let auth_titles: Vec<&str> = SSH_AUTH.iter().map(|(_, title)| *title).collect();
        ssh_auth_row.set_model(Some(&gtk::StringList::new(&auth_titles)));
        ssh_row.add_row(&ssh_auth_row);
        let ssh_key_row = adw::EntryRow::new();
        ssh_key_row.set_title("Key File");
        let browse_key = gtk::Button::from_icon_name("document-open-symbolic");
        browse_key.set_valign(gtk::Align::Center);
        browse_key.add_css_class("flat");
        browse_key.set_tooltip_text(Some("Choose an SSH private key"));
        browse_key.connect_clicked(glib::clone!(
            #[weak(rename_to = dialog)]
            self,
            move |_| dialog.on_browse_key()
        ));
        ssh_key_row.add_suffix(&browse_key);
        ssh_row.add_row(&ssh_key_row);
        let ssh_secret_row = adw::PasswordEntryRow::new();
        ssh_row.add_row(&ssh_secret_row);
        ssh_group.add(&ssh_row);
        ssh_group.set_description(Some(
            "The secret is kept in the system keychain. A host datagrep has not seen \
             before shows its key fingerprint for you to confirm first.",
        ));
        page.add(&ssh_group);
        ssh_auth_row.connect_selected_notify(glib::clone!(
            #[weak(rename_to = dialog)]
            self,
            move |_| dialog.reshape_for_ssh_auth()
        ));

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
        // Read-only REFUSES writes; the ladder GATES statements — the two compose, so neither hides the other.
        let read_only_row = adw::SwitchRow::new();
        read_only_row.set_title("Read-only");
        read_only_row.set_subtitle(
            "Refuses writes on this connection even when the database account is allowed to make them",
        );
        safety_group.add(&read_only_row);
        let safety_row = adw::ComboRow::new();
        safety_row.set_title("Safety level");
        let titles: Vec<&str> = SafetyLevel::ALL.iter().map(|l| l.title()).collect();
        safety_row.set_model(Some(&gtk::StringList::new(&titles)));
        safety_row.set_subtitle(SafetyLevel::Silent.blurb());
        safety_row.connect_selected_notify(|row| {
            row.set_subtitle(level_at(row.selected()).blurb());
        });
        safety_group.add(&safety_row);
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
                ssh_group,
                ssh_row,
                ssh_host_row,
                ssh_port_row,
                ssh_user_row,
                ssh_auth_row,
                ssh_key_row,
                ssh_secret_row,
                url_row,
                test_row,
                swatches,
                limit_row,
                idle_row,
                read_only_row,
                safety_row,
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
        w.ssh_group.set_visible(!file);
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

    fn ssh_auth(&self) -> &'static str {
        let idx = self.widgets().ssh_auth_row.selected() as usize;
        SSH_AUTH[idx.min(SSH_AUTH.len() - 1)].0
    }

    fn reshape_for_ssh_auth(&self) {
        let w = self.widgets();
        let auth = self.ssh_auth();
        w.ssh_key_row.set_visible(auth == "key");
        w.ssh_secret_row.set_visible(auth != "agent");
        let saved = self.imp().orig_ssh.borrow()["auth"].as_str() == Some(auth)
            && self.imp().orig_ssh.borrow()["has_secret"].as_bool() == Some(true);
        let title = if auth == "key" {
            "Key Passphrase"
        } else {
            "SSH Password"
        };
        w.ssh_secret_row.set_title(&if saved {
            format!("{title} (saved)")
        } else {
            title.to_string()
        });
    }

    // The tunnel the form describes, without its secret; None when the connection is direct.
    fn ssh_from_ui(&self) -> Option<Value> {
        let w = self.widgets();
        if !w.ssh_row.enables_expansion() || self.current_engine().file_based {
            return None;
        }
        let auth = self.ssh_auth();
        let mut ssh = json!({
            "host": w.ssh_host_row.text().trim(),
            "port": w.ssh_port_row.value() as u16,
            "user": w.ssh_user_row.text().trim(),
            "auth": auth,
        });
        if auth == "key" {
            ssh["key_path"] = json!(w.ssh_key_row.text().trim());
        }
        Some(ssh)
    }

    // What the add, update and test calls take for "ssh": an object (with any typed secret) or null.
    fn ssh_option(&self) -> Value {
        let Some(mut ssh) = self.ssh_from_ui() else {
            return Value::Null;
        };
        let secret = self.widgets().ssh_secret_row.text();
        if ssh["auth"] != "agent" && !secret.is_empty() {
            ssh["secret"] = json!(secret.as_str());
        }
        ssh
    }

    fn ssh_changed(&self) -> bool {
        let current = self.ssh_from_ui().unwrap_or(Value::Null);
        let orig = self.imp().orig_ssh.borrow();
        let same = |key: &str| current[key] == orig[key];
        let key_path_same = current["auth"] != "key" || same("key_path");
        let unchanged = (current.is_null() && orig.is_null())
            || (!current.is_null()
                && same("host")
                && same("port")
                && same("user")
                && same("auth")
                && key_path_same);
        !unchanged || !self.widgets().ssh_secret_row.text().is_empty()
    }

    fn apply_ssh_to_ui(&self, ssh: &Value) {
        let w = self.widgets();
        w.ssh_row.set_enable_expansion(!ssh.is_null());
        w.ssh_row.set_expanded(!ssh.is_null());
        w.ssh_host_row
            .set_text(ssh["host"].as_str().unwrap_or_default());
        w.ssh_port_row
            .set_value(ssh["port"].as_u64().unwrap_or(22) as f64);
        w.ssh_user_row
            .set_text(ssh["user"].as_str().unwrap_or_default());
        let auth = ssh["auth"].as_str().unwrap_or("agent");
        let idx = SSH_AUTH.iter().position(|(id, _)| *id == auth).unwrap_or(0);
        w.ssh_auth_row.set_selected(idx as u32);
        w.ssh_key_row
            .set_text(ssh["key_path"].as_str().unwrap_or_default());
        self.reshape_for_ssh_auth();
    }

    fn on_browse_key(&self) {
        let chooser = gtk::FileDialog::new();
        chooser.set_title("Choose an SSH private key");
        if let Some(home) = glib::home_dir().to_str() {
            chooser.set_initial_folder(Some(&gio::File::for_path(format!("{home}/.ssh"))));
        }
        let parent = self.root().and_downcast::<gtk::Window>();
        chooser.open(
            parent.as_ref(),
            gio::Cancellable::NONE,
            glib::clone!(
                #[weak(rename_to = dialog)]
                self,
                move |result| {
                    if let Some(path) = result.ok().and_then(|f| f.path()) {
                        dialog
                            .widgets()
                            .ssh_key_row
                            .set_text(&path.to_string_lossy());
                    }
                }
            ),
        );
    }

    // Shows the SSH host's key and asks before trusting a new one; `then` runs once it is trusted.
    // Unless `required`, an unreachable host does not block saving: the key is checked on connect.
    fn verify_host_key(&self, required: bool, fail: fn(&Self, &str), then: fn(&Self)) {
        let Some(ssh) = self.ssh_from_ui() else {
            return then(self);
        };
        let host = ssh["host"].as_str().unwrap_or_default().to_string();
        let port = ssh["port"].as_u64().unwrap_or(22) as u16;
        let core = self.core();
        let (tx, rx) = async_channel::bounded(1);
        std::thread::spawn(move || {
            let _ = tx.send_blocking(core.ssh_host_key_json(&host, port));
        });
        glib::spawn_future_local(glib::clone!(
            #[weak(rename_to = dialog)]
            self,
            async move {
                let Ok(review) = rx.recv().await else {
                    return;
                };
                let review: Value = match review {
                    Ok(json) => serde_json::from_str(&json).unwrap_or_default(),
                    Err(e) if required => return fail(&dialog, &e.0),
                    Err(_) => return then(&dialog),
                };
                dialog.on_host_key_reviewed(review, fail, then);
            }
        ));
    }

    fn on_host_key_reviewed(&self, review: Value, fail: fn(&Self, &str), then: fn(&Self)) {
        let text = |key: &str| review[key].as_str().unwrap_or_default().to_string();
        let (host, fingerprint) = (text("host"), text("fingerprint"));
        let port = review["port"].as_u64().unwrap_or(22) as u16;
        match review["status"].as_str() {
            Some("trusted") => then(self),
            Some("changed") => {
                let alert = adw::AlertDialog::new(
                    Some("SSH Host Key Changed"),
                    Some(&format!(
                        "Trusted: {}\nOffered: {fingerprint}\n\nSomeone may be intercepting the \
                         connection, or the server was reinstalled. datagrep will not connect. \
                         If the change is expected, confirm the new fingerprint with the server's \
                         administrator and remove the old entry from {}.",
                        text("expected"),
                        text("known_hosts")
                    )),
                );
                alert.add_responses(&[("close", "Close")]);
                alert.present(Some(self));
                fail(
                    self,
                    &format!(
                        "The SSH host key of {host}:{port} has changed, so nothing was sent to it."
                    ),
                );
            }
            _ => {
                let alert = adw::AlertDialog::new(
                    Some("Trust This SSH Host?"),
                    Some(&format!(
                        "datagrep has not connected to {host}:{port} before. It offered this {} key:\n\n\
                         {fingerprint}\n\nCompare it with the fingerprint the server's \
                         administrator gives you. Trusting it saves it to {}; a different key later \
                         will be refused.",
                        text("algorithm"),
                        text("known_hosts")
                    )),
                );
                alert.add_responses(&[("cancel", "Cancel"), ("trust", "Trust and Continue")]);
                alert.set_default_response(Some("cancel"));
                alert.set_close_response("cancel");
                alert.choose(
                    self,
                    gio::Cancellable::NONE,
                    glib::clone!(
                        #[weak(rename_to = dialog)]
                        self,
                        move |response: glib::GString| {
                            if response != "trust" {
                                return fail(
                                    &dialog,
                                    &format!("The SSH host key was not trusted, so nothing was sent to {host}."),
                                );
                            }
                            match dialog.core().ssh_trust_host_key(&host, port, &fingerprint) {
                                Ok(()) => then(&dialog),
                                Err(e) => fail(&dialog, &e.0),
                            }
                        }
                    ),
                );
            }
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

    fn current_safety(&self) -> SafetyLevel {
        level_at(self.widgets().safety_row.selected())
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
        if self.imp().testing.get() {
            return;
        }
        self.imp().testing.set(true);
        self.widgets()
            .test_row
            .set_subtitle("Checking the SSH host…");
        self.verify_host_key(
            true,
            |dialog, message| {
                dialog.imp().testing.set(false);
                dialog.widgets().test_row.set_subtitle(&format!(
                    "Could not connect: {}",
                    glib::markup_escape_text(message)
                ));
            },
            Self::run_test_connection,
        );
    }

    fn run_test_connection(&self) {
        let imp = self.imp();
        let w = self.widgets();
        let url = build_url(&self.fields_from_ui(), true);
        let unchanged = imp.editing.get()
            && imp.have_original.get()
            && w.password_row.text().is_empty()
            && w.url_row.text().trim() == imp.original_url_no_password.borrow().as_str();
        // The saved profile lends its keychain secrets; an edited URL replaces its config.
        let name = if imp.editing.get() {
            imp.original_name.borrow().clone()
        } else {
            String::new()
        };
        let url = if unchanged { String::new() } else { url };
        if name.is_empty() && url.is_empty() {
            imp.testing.set(false);
            w.test_row
                .set_subtitle("Complete the connection details first.");
            return;
        }
        w.test_row.set_subtitle("Connecting…");

        let options = json!({ "ssh": self.ssh_option() }).to_string();
        let core = self.core();
        let (tx, rx) = async_channel::bounded(1);
        std::thread::spawn(move || {
            let _ = tx.send_blocking(core.test_connection_json(&name, &url, &options));
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
        imp.orig_safety.set(SafetyLevel::from(
            o["safety"].as_str().unwrap_or("silent").to_string(),
        ));
        imp.orig_color
            .replace(o["color"].as_str().unwrap_or_default().to_string());
        imp.orig_auto_limit
            .set(o["auto_limit"].as_i64().unwrap_or(0));
        imp.orig_idle_timeout
            .set(o["idle_timeout_s"].as_i64().unwrap_or(0));
        let has_secret = o["has_secret"].as_bool().unwrap_or(false);
        let driver = o["driver"].as_str().unwrap_or_default();
        imp.orig_ssh.replace(o["ssh"].clone());

        imp.syncing.set(true);
        let fields = fields_from_config(driver, &o["config"]);
        self.apply_fields_to_ui(&fields);
        self.set_color(&imp.orig_color.borrow());
        w.read_only_row.set_active(imp.orig_read_only.get());
        w.safety_row
            .set_selected(level_index(imp.orig_safety.get()));
        w.limit_row.set_value(imp.orig_auto_limit.get() as f64);
        w.idle_row.set_value(imp.orig_idle_timeout.get() as f64);
        self.apply_ssh_to_ui(&o["ssh"]);
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
            "safety": self.current_safety().as_str(),
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
        let ssh = self.ssh_option();
        if !ssh.is_null() {
            o["ssh"] = ssh;
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
        if self.current_safety() != imp.orig_safety.get() {
            p.insert("safety".into(), json!(self.current_safety().as_str()));
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
        if self.ssh_changed() {
            p.insert("ssh".into(), self.ssh_option());
        }
        Value::Object(p).to_string()
    }

    fn on_accept(&self) {
        if self.widgets().name_row.text().trim().is_empty() {
            self.show_error("A name is required.");
            return;
        }
        if self.ssh_from_ui().is_some() && (!self.imp().editing.get() || self.ssh_changed()) {
            self.verify_host_key(
                false,
                |dialog, message| dialog.show_error(message),
                Self::save,
            );
        } else {
            self.save();
        }
    }

    fn save(&self) {
        let imp = self.imp();
        let name = self.widgets().name_row.text().trim().to_string();
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

fn level_at(index: u32) -> SafetyLevel {
    SafetyLevel::ALL
        .get(index as usize)
        .copied()
        .unwrap_or_default()
}

fn level_index(level: SafetyLevel) -> u32 {
    SafetyLevel::ALL
        .iter()
        .position(|l| *l == level)
        .unwrap_or(0) as u32
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
