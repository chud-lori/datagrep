use std::cell::{Cell, RefCell};

use adw::prelude::*;
use adw::subclass::prelude::*;
use gtk::glib;

use crate::model::format;
use crate::model::history::{DateRange, HistoryEntry, HistoryFilter, HistoryStore, Outcome};
use crate::ui::StatusBar;

const EMPTY: &str = "Every statement datagrep runs is logged here automatically — the SQL, the \
                     connection, how long it took, and what came back.";
const NO_MATCH: &str = "No recorded query matches these filters.";
const RANGES: [(&str, DateRange); 4] = [
    ("All dates", DateRange::All),
    ("Today", DateRange::Day),
    ("Past week", DateRange::Week),
    ("Past month", DateRange::Month),
];
const OUTCOMES: [(&str, Option<Outcome>); 4] = [
    ("Any outcome", None),
    ("ok", Some(Outcome::Ok)),
    ("failed", Some(Outcome::Error)),
    ("cancelled", Some(Outcome::Cancelled)),
];

mod row_imp {
    use super::*;

    #[derive(Default)]
    pub struct HistoryRow {
        pub id: RefCell<String>,
        pub heading: RefCell<String>,
        pub statement: RefCell<String>,
        pub meta: RefCell<String>,
        pub outcome: Cell<Outcome>,
        pub dim: Cell<bool>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for HistoryRow {
        const NAME: &'static str = "DgHistoryRow";
        type Type = super::HistoryRow;
    }

    impl ObjectImpl for HistoryRow {}
}

glib::wrapper! {
    pub struct HistoryRow(ObjectSubclass<row_imp::HistoryRow>);
}

impl HistoryRow {
    fn day(title: &str, dim: bool) -> Self {
        let row: Self = glib::Object::new();
        *row.imp().heading.borrow_mut() = title.to_owned();
        row.imp().dim.set(dim);
        row
    }

    /// Every string the list draws is built here, never in the bind callback.
    fn entry(entry: &HistoryEntry) -> Self {
        let mut meta = vec![format::time_of_day(entry.started_at_ms)];
        meta.push(format::duration(entry.duration_ms));
        if entry.outcome == Outcome::Ok {
            if let Some(rows) = format::rows(entry.affected_rows.or(entry.row_count)) {
                meta.push(rows);
            }
        }
        meta.push(match entry.connection.is_empty() {
            true => "no connection".to_owned(),
            false => entry.connection.clone(),
        });
        if !entry.engine.is_empty() {
            meta.push(entry.engine.clone());
        }
        if entry.run_count > 1 {
            meta.push(format!("×{}", entry.run_count));
        }

        let row: Self = glib::Object::new();
        let imp = row.imp();
        *imp.id.borrow_mut() = entry.id.clone();
        *imp.statement.borrow_mut() = entry.one_line();
        *imp.meta.borrow_mut() = meta.join(" · ");
        imp.outcome.set(entry.outcome);
        row
    }

    fn id(&self) -> String {
        self.imp().id.borrow().clone()
    }

    fn is_day(&self) -> bool {
        self.imp().id.borrow().is_empty()
    }
}

fn dropdown(labels: &[&str], tooltip: &str) -> gtk::DropDown {
    let drop = gtk::DropDown::from_strings(labels);
    drop.set_tooltip_text(Some(tooltip));
    drop
}

fn caption(text: &str) -> gtk::Label {
    let label = gtk::Label::new(Some(text));
    label.add_css_class("caption");
    label.add_css_class("dim-label");
    label.set_xalign(0.0);
    label.set_wrap(true);
    label
}

fn action_button(icon: &str, tooltip: &str, action: &str) -> gtk::Button {
    let button = gtk::Button::from_icon_name(icon);
    button.set_tooltip_text(Some(tooltip));
    button.set_action_name(Some(action));
    button.add_css_class("flat");
    button
}

fn factory() -> gtk::SignalListItemFactory {
    let factory = gtk::SignalListItemFactory::new();
    factory.connect_setup(|_, item| {
        let Some(item) = item.downcast_ref::<gtk::ListItem>() else {
            return;
        };
        let heading = gtk::Label::new(None);
        heading.set_xalign(0.0);
        heading.set_wrap(true);

        let statement = gtk::Inscription::builder()
            .text_overflow(gtk::InscriptionOverflow::EllipsizeEnd)
            .hexpand(true)
            .build();
        statement.add_css_class("monospace");

        let outcome = gtk::Label::new(None);
        outcome.add_css_class("caption");
        let meta = gtk::Inscription::builder()
            .text_overflow(gtk::InscriptionOverflow::EllipsizeEnd)
            .hexpand(true)
            .build();
        meta.add_css_class("caption");
        meta.add_css_class("dim-label");

        let footer = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        footer.append(&outcome);
        footer.append(&meta);

        let entry = gtk::Box::new(gtk::Orientation::Vertical, 2);
        entry.append(&statement);
        entry.append(&footer);

        let row = gtk::Box::new(gtk::Orientation::Vertical, 0);
        row.append(&heading);
        row.append(&entry);
        item.set_child(Some(&row));
    });
    factory.connect_bind(|_, item| {
        let Some(item) = item.downcast_ref::<gtk::ListItem>() else {
            return;
        };
        let (Some(container), Some(row)) = (
            item.child().and_downcast::<gtk::Box>(),
            item.item().and_downcast::<HistoryRow>(),
        ) else {
            return;
        };
        let (Some(heading), Some(entry)) = (
            container.first_child().and_downcast::<gtk::Label>(),
            container.last_child().and_downcast::<gtk::Box>(),
        ) else {
            return;
        };
        let imp = row.imp();
        let is_day = row.is_day();
        heading.set_visible(is_day);
        entry.set_visible(!is_day);
        item.set_selectable(!is_day);
        item.set_activatable(!is_day);
        if is_day {
            heading.set_text(&imp.heading.borrow());
            let placeholder = imp.dim.get();
            heading.set_css_classes(match placeholder {
                true => &["dim-label", "caption"],
                false => &["heading"],
            });
            return;
        }
        if let Some(statement) = entry.first_child().and_downcast::<gtk::Inscription>() {
            statement.set_text(Some(&imp.statement.borrow()));
        }
        let Some(footer) = entry.last_child().and_downcast::<gtk::Box>() else {
            return;
        };
        if let Some(outcome) = footer.first_child().and_downcast::<gtk::Label>() {
            let kind = imp.outcome.get();
            outcome.set_text(kind.label());
            for class in ["success", "error", "warning"] {
                outcome.remove_css_class(class);
            }
            outcome.add_css_class(kind.css_class());
        }
        if let Some(meta) = footer.last_child().and_downcast::<gtk::Inscription>() {
            meta.set_text(Some(&imp.meta.borrow()));
        }
    });
    factory
}

mod imp {
    use super::*;
    use glib::subclass::Signal;
    use std::sync::OnceLock;

    pub struct HistoryPanel {
        pub store: RefCell<Option<HistoryStore>>,
        pub status: RefCell<Option<StatusBar>>,
        pub rows: gio::ListStore,
        pub selection: gtk::SingleSelection,
        pub list: gtk::ListView,
        pub search: gtk::SearchEntry,
        pub connections: gtk::DropDown,
        pub connection_names: RefCell<Vec<String>>,
        pub range: gtk::DropDown,
        pub outcome: gtk::DropDown,
        pub clear_filters: gtk::Button,
        pub count: gtk::Label,
        pub detail: gtk::Revealer,
        pub summary: gtk::Label,
        pub sql: gtk::TextView,
        pub error: gtk::Label,
        pub open: gtk::Button,
        pub retention: gtk::Label,
        pub clear: gtk::MenuButton,
        pub selected: RefCell<String>,
        pub refreshing: Cell<bool>,
    }

    impl Default for HistoryPanel {
        fn default() -> Self {
            let rows = gio::ListStore::new::<HistoryRow>();
            let selection = gtk::SingleSelection::new(Some(rows.clone()));
            selection.set_autoselect(false);
            selection.set_can_unselect(true);
            selection.set_selected(gtk::INVALID_LIST_POSITION);
            let sql = gtk::TextView::new();
            sql.set_editable(false);
            sql.set_monospace(true);
            sql.set_cursor_visible(false);
            Self {
                store: RefCell::new(None),
                status: RefCell::new(None),
                list: gtk::ListView::new(Some(selection.clone()), Some(factory())),
                rows,
                selection,
                search: gtk::SearchEntry::new(),
                connections: dropdown(
                    &["All connections"],
                    "Filter by the connection a statement was run against",
                ),
                connection_names: RefCell::new(Vec::new()),
                range: dropdown(
                    &RANGES.map(|(label, _)| label),
                    "Filter by when the statement ran",
                ),
                outcome: dropdown(
                    &OUTCOMES.map(|(label, _)| label),
                    "Filter by outcome — ok, failed or cancelled",
                ),
                clear_filters: gtk::Button::with_label("Clear"),
                count: caption(""),
                detail: gtk::Revealer::new(),
                summary: caption(""),
                sql,
                error: caption(""),
                open: action_button(
                    "document-edit-symbolic",
                    "Put this statement in the editor, on the connection it ran against",
                    "history.open",
                ),
                retention: caption(""),
                clear: gtk::MenuButton::new(),
                selected: RefCell::new(String::new()),
                refreshing: Cell::new(false),
            }
        }
    }

    #[glib::object_subclass]
    impl ObjectSubclass for HistoryPanel {
        const NAME: &'static str = "DgHistoryPanel";
        type Type = super::HistoryPanel;
        type ParentType = adw::Bin;
    }

    impl ObjectImpl for HistoryPanel {
        fn signals() -> &'static [Signal] {
            static SIGNALS: OnceLock<Vec<Signal>> = OnceLock::new();
            SIGNALS.get_or_init(|| {
                vec![
                    Signal::builder("rerun-requested")
                        .param_types([String::static_type(), String::static_type()])
                        .build(),
                    Signal::builder("open-requested")
                        .param_types([String::static_type(), String::static_type()])
                        .build(),
                ]
            })
        }

        fn constructed(&self) {
            self.parent_constructed();
            self.install_actions();
            self.list.add_css_class("dg-history");

            let body = gtk::Box::new(gtk::Orientation::Vertical, 6);
            body.set_margin_top(6);
            body.set_margin_bottom(6);
            body.set_margin_start(6);
            body.set_margin_end(6);
            body.append(&self.filter_bar());
            body.append(
                &gtk::ScrolledWindow::builder()
                    .hscrollbar_policy(gtk::PolicyType::Never)
                    .vexpand(true)
                    .child(&self.list)
                    .build(),
            );
            body.append(self.detail_strip());
            body.append(&self.footer());
            self.obj().set_child(Some(&body));

            let panel = self.obj().downgrade();
            self.selection.connect_selected_item_notify(move |_| {
                if let Some(panel) = panel.upgrade() {
                    panel.imp().on_selected();
                }
            });
            // Double-click is the Qt gesture for "put it back in the editor".
            let panel = self.obj().downgrade();
            self.list.connect_activate(move |_, _| {
                if let Some(panel) = panel.upgrade() {
                    panel.imp().request("open-requested");
                }
            });
        }
    }

    impl WidgetImpl for HistoryPanel {}
    impl BinImpl for HistoryPanel {}

    impl HistoryPanel {
        fn install_actions(&self) {
            let actions = gio::SimpleActionGroup::new();
            for (name, handler) in [
                ("copy", 0u8),
                ("open", 1),
                ("rerun", 2),
                ("remove", 3),
                ("retention", 4),
                ("clear-all", 5),
                ("clear-connection", 6),
            ] {
                let action = gio::SimpleAction::new(name, None);
                let panel = self.obj().downgrade();
                action.connect_activate(move |_, _| {
                    let Some(panel) = panel.upgrade() else {
                        return;
                    };
                    let imp = panel.imp();
                    match handler {
                        0 => imp.copy(),
                        1 => imp.request("open-requested"),
                        2 => imp.request("rerun-requested"),
                        3 => imp.remove(),
                        4 => imp.edit_retention(),
                        5 => imp.clear(None),
                        _ => imp.clear(Some(imp.filter().connection).filter(|c| !c.is_empty())),
                    }
                });
                actions.add_action(&action);
            }
            self.obj().insert_action_group("history", Some(&actions));

            let shortcut = gtk::Shortcut::new(
                gtk::ShortcutTrigger::parse_string("Delete"),
                Some(gtk::NamedAction::new("history.remove")),
            );
            let controller = gtk::ShortcutController::new();
            controller.set_scope(gtk::ShortcutScope::Local);
            controller.add_shortcut(shortcut);
            self.list.add_controller(controller);
        }

        fn filter_bar(&self) -> gtk::Box {
            self.search
                .set_placeholder_text(Some("Search SQL and error text"));
            let panel = self.obj().downgrade();
            self.search.connect_search_changed(move |_| {
                if let Some(panel) = panel.upgrade() {
                    panel.refresh();
                }
            });
            for drop in [&self.connections, &self.range, &self.outcome] {
                drop.set_hexpand(true);
                let panel = self.obj().downgrade();
                drop.connect_selected_notify(move |_| {
                    if let Some(panel) = panel.upgrade() {
                        panel.refresh();
                    }
                });
            }

            self.clear_filters.add_css_class("flat");
            self.clear_filters
                .set_tooltip_text(Some("Remove every filter"));
            let panel = self.obj().downgrade();
            self.clear_filters.connect_clicked(move |_| {
                let Some(panel) = panel.upgrade() else {
                    return;
                };
                let imp = panel.imp();
                imp.search.set_text("");
                for drop in [&imp.connections, &imp.range, &imp.outcome] {
                    drop.set_selected(0);
                }
            });

            let filters = gtk::Box::new(gtk::Orientation::Horizontal, 6);
            filters.append(&self.connections);
            filters.append(&self.range);
            filters.append(&self.outcome);

            let counted = gtk::Box::new(gtk::Orientation::Horizontal, 6);
            self.count.set_hexpand(true);
            counted.append(&self.count);
            counted.append(&self.clear_filters);

            let bar = gtk::Box::new(gtk::Orientation::Vertical, 6);
            bar.append(&self.search);
            bar.append(&filters);
            bar.append(&counted);
            bar
        }

        fn detail_strip(&self) -> &gtk::Revealer {
            // Hidden until wired: an unmounted editor offers no "Open in Editor".
            self.open.set_visible(false);
            let buttons = gtk::Box::new(gtk::Orientation::Horizontal, 0);
            buttons.add_css_class("linked");
            buttons.append(&action_button(
                "edit-copy-symbolic",
                "Copy SQL",
                "history.copy",
            ));
            buttons.append(&self.open);
            buttons.append(&action_button(
                "media-playback-start-symbolic",
                "Run this statement again now",
                "history.rerun",
            ));
            buttons.append(&action_button(
                "user-trash-symbolic",
                "Remove from history",
                "history.remove",
            ));

            self.summary.set_hexpand(true);
            let header = gtk::Box::new(gtk::Orientation::Horizontal, 6);
            header.append(&self.summary);
            header.append(&buttons);

            let frame = gtk::Frame::new(None);
            frame.set_child(Some(
                &gtk::ScrolledWindow::builder()
                    .max_content_height(140)
                    .propagate_natural_height(true)
                    .child(&self.sql)
                    .build(),
            ));

            self.error.remove_css_class("dim-label");
            self.error.add_css_class("error");
            self.error.set_visible(false);

            let body = gtk::Box::new(gtk::Orientation::Vertical, 6);
            body.append(&header);
            body.append(&frame);
            body.append(&self.error);
            self.detail.set_child(Some(&body));
            &self.detail
        }

        fn footer(&self) -> gtk::Box {
            self.retention.set_hexpand(true);
            let edit = gtk::Button::with_label("Retention…");
            edit.add_css_class("flat");
            edit.set_tooltip_text(Some(
                "Choose how many queries, and how many days, of history to keep",
            ));
            edit.set_action_name(Some("history.retention"));

            self.clear.set_label("Clear…");
            self.clear.add_css_class("flat");

            let footer = gtk::Box::new(gtk::Orientation::Horizontal, 6);
            footer.append(&self.retention);
            footer.append(&edit);
            footer.append(&self.clear);
            footer
        }

        pub(super) fn filter(&self) -> HistoryFilter {
            let index = self.connections.selected() as usize;
            HistoryFilter {
                text: self.search.text().to_string(),
                connection: match index {
                    0 => String::new(),
                    _ => self
                        .connection_names
                        .borrow()
                        .get(index - 1)
                        .cloned()
                        .unwrap_or_default(),
                },
                range: RANGES
                    .get(self.range.selected() as usize)
                    .map(|(_, range)| *range)
                    .unwrap_or_default(),
                outcome: OUTCOMES
                    .get(self.outcome.selected() as usize)
                    .and_then(|(_, outcome)| *outcome),
            }
        }

        fn selected_entry(&self) -> Option<HistoryEntry> {
            let store = self.store.borrow().clone()?;
            store.entry(&self.selected.borrow())
        }

        fn on_selected(&self) {
            if self.refreshing.get() {
                return;
            }
            let id = self
                .selection
                .selected_item()
                .and_downcast::<HistoryRow>()
                .map(|row| row.id())
                .unwrap_or_default();
            *self.selected.borrow_mut() = id;
            self.show_detail();
        }

        pub(super) fn show_detail(&self) {
            let Some(entry) = self.selected_entry() else {
                self.detail.set_reveal_child(false);
                return;
            };
            let mut parts = Vec::new();
            if !entry.connection.is_empty() {
                parts.push(match entry.engine.is_empty() {
                    true => entry.connection.clone(),
                    false => format!("{} · {}", entry.connection, entry.engine),
                });
            }
            let now = crate::model::history::now_ms();
            parts.push(format!(
                "{} {}",
                format::day_title(&entry.day_key(), now),
                format::time_of_day(entry.started_at_ms)
            ));
            parts.push(format::duration(entry.duration_ms));
            if entry.outcome == Outcome::Ok {
                if let Some(rows) = format::rows(entry.affected_rows.or(entry.row_count)) {
                    parts.push(rows);
                }
            } else {
                parts.push(entry.outcome.label().to_owned());
            }
            if entry.run_count > 1 {
                parts.push(format!("run {}×", entry.run_count));
            }
            self.summary.set_text(&parts.join("  ·  "));
            self.sql.buffer().set_text(&entry.sql);
            self.error.set_visible(!entry.error.is_empty());
            self.error.set_text(&entry.error);
            self.detail.set_reveal_child(true);
        }

        fn request(&self, signal: &str) {
            let Some(entry) = self.selected_entry() else {
                return;
            };
            self.obj()
                .emit_by_name::<()>(signal, &[&entry.sql, &entry.connection]);
        }

        fn copy(&self) {
            let Some(entry) = self.selected_entry() else {
                return;
            };
            if let Some(display) = gtk::gdk::Display::default() {
                display.clipboard().set_text(&entry.sql);
            }
            self.say(&format!("copied {} characters of SQL", entry.sql.len()));
        }

        fn remove(&self) {
            let (Some(store), Some(entry)) = (self.store.borrow().clone(), self.selected_entry())
            else {
                return;
            };
            self.selected.borrow_mut().clear();
            store.remove(&entry.id);
        }

        fn clear(&self, connection: Option<String>) {
            let Some(store) = self.store.borrow().clone() else {
                return;
            };
            self.selected.borrow_mut().clear();
            store.clear(connection.as_deref());
            self.say(
                match connection {
                    Some(name) => format!("history cleared for {name}"),
                    None => "query history cleared".to_owned(),
                }
                .as_str(),
            );
        }

        fn edit_retention(&self) {
            let Some(store) = self.store.borrow().clone() else {
                return;
            };
            let current = store.retention();
            let entries = adw::SpinRow::with_range(100.0, 1_000_000.0, 100.0);
            entries.set_title("Entries (newest kept)");
            entries.set_value(f64::from(current.max_entries));
            let days = adw::SpinRow::with_range(1.0, 3650.0, 1.0);
            days.set_title("Days (older entries dropped)");
            days.set_value(f64::from(current.max_days));

            let reset = gtk::Button::with_label("Reset to defaults");
            reset.add_css_class("flat");
            let (spin_entries, spin_days) = (entries.clone(), days.clone());
            reset.connect_clicked(move |_| {
                let defaults = crate::model::Retention::default();
                spin_entries.set_value(f64::from(defaults.max_entries));
                spin_days.set_value(f64::from(defaults.max_days));
            });

            let group = adw::PreferencesGroup::new();
            group.set_header_suffix(Some(&reset));
            group.add(&entries);
            group.add(&days);

            let dialog = adw::MessageDialog::new(
                self.obj().root().and_downcast::<gtk::Window>().as_ref(),
                Some("How much history to keep"),
                Some(&format!(
                    "datagrep keeps whichever limit is reached first. Entries are stored as one \
                     plain JSON-lines file per day in {}, so nothing here is locked away.",
                    store.directory().display()
                )),
            );
            dialog.set_extra_child(Some(&group));
            dialog.add_response("cancel", "Cancel");
            dialog.add_response("apply", "Apply");
            dialog.set_response_appearance("apply", adw::ResponseAppearance::Suggested);
            dialog.set_default_response(Some("apply"));
            dialog.set_close_response("cancel");
            dialog.connect_response(None, move |_, response| {
                if response != "apply" {
                    return;
                }
                store.set_retention(crate::model::Retention::clamped(
                    entries.value() as u32,
                    days.value() as u32,
                ));
            });
            dialog.present();
        }

        fn say(&self, message: &str) {
            if let Some(status) = self.status.borrow().as_ref() {
                status.say(message, false);
            }
        }

        pub(super) fn rebuild_connection_filter(&self, names: Vec<String>) {
            if *self.connection_names.borrow() == names {
                return;
            }
            let current = self.filter().connection;
            let mut labels: Vec<&str> = vec!["All connections"];
            labels.extend(names.iter().map(String::as_str));
            self.connections
                .set_model(Some(&gtk::StringList::new(&labels)));
            let restored = names.iter().position(|name| *name == current);
            self.connections.set_selected(match restored {
                Some(index) => index as u32 + 1,
                None => 0,
            });
            *self.connection_names.borrow_mut() = names;
        }

        pub(super) fn rebuild_clear_menu(&self, connection: &str) {
            let menu = gio::Menu::new();
            if !connection.is_empty() {
                menu.append(
                    Some(&format!("Clear History for ‘{connection}’")),
                    Some("history.clear-connection"),
                );
            }
            menu.append(Some("Clear All History"), Some("history.clear-all"));
            self.clear.set_menu_model(Some(&menu));
        }
    }
}

glib::wrapper! {
    pub struct HistoryPanel(ObjectSubclass<imp::HistoryPanel>)
        @extends adw::Bin, gtk::Widget,
        @implements gtk::Accessible, gtk::Buildable, gtk::ConstraintTarget;
}

impl Default for HistoryPanel {
    fn default() -> Self {
        Self::new()
    }
}

impl HistoryPanel {
    pub fn new() -> Self {
        glib::Object::new()
    }

    pub fn set_status_bar(&self, status: &StatusBar) {
        *self.imp().status.borrow_mut() = Some(status.clone());
    }

    pub fn bind(&self, store: &HistoryStore) {
        *self.imp().store.borrow_mut() = Some(store.clone());
        let panel = self.downgrade();
        store.connect_changed(move |_| {
            if let Some(panel) = panel.upgrade() {
                panel.refresh();
            }
        });
        self.refresh();
    }

    /// One pass over the log: filtered, day-headed and counted in the same walk.
    pub fn refresh(&self) {
        let imp = self.imp();
        let Some(store) = imp.store.borrow().clone() else {
            return;
        };
        imp.refreshing.set(true);
        imp.rebuild_connection_filter(store.connections());

        let now = crate::model::history::now_ms();
        let filter = imp.filter();
        let prepared = filter.prepare(now);
        let selected = imp.selected.borrow().clone();
        let mut restore = gtk::INVALID_LIST_POSITION;
        let mut rows: Vec<HistoryRow> = Vec::new();
        let (total, shown) = store.with_entries(|entries| {
            let mut day = String::new();
            let mut shown = 0;
            for entry in entries.iter().filter(|entry| prepared.matches(entry)) {
                let key = entry.day_key();
                if key != day {
                    rows.push(HistoryRow::day(&format::day_title(&key, now), false));
                    day = key;
                }
                if entry.id == selected {
                    restore = rows.len() as u32;
                }
                rows.push(HistoryRow::entry(entry));
                shown += 1;
            }
            (entries.len(), shown)
        });

        imp.rows.splice(0, imp.rows.n_items(), &rows);
        imp.selection.set_selected(restore);
        if restore == gtk::INVALID_LIST_POSITION {
            imp.selected.borrow_mut().clear();
        }

        imp.count.set_text(&match (total, filter.is_empty()) {
            (0, _) => "nothing has been run yet".to_owned(),
            (_, false) => format!(
                "{} of {} queries",
                format::count(shown as u64),
                format::count(total as u64)
            ),
            (1, true) => "1 query".to_owned(),
            (_, true) => format!("{} queries", format::count(total as u64)),
        });
        imp.clear_filters.set_visible(!filter.is_empty());
        imp.clear.set_sensitive(total > 0);
        imp.rebuild_clear_menu(&filter.connection);
        imp.retention
            .set_text(&retention_summary(store.retention()));

        if rows.is_empty() {
            imp.rows.append(&HistoryRow::day(
                match total {
                    0 => EMPTY,
                    _ => NO_MATCH,
                },
                true,
            ));
        }
        imp.refreshing.set(false);
        imp.show_detail();
    }

    /// Run this statement again — always through the window's one run path.
    pub fn connect_rerun_requested<F: Fn(&Self, &str, &str) + 'static>(
        &self,
        f: F,
    ) -> glib::SignalHandlerId {
        self.connect_entry("rerun-requested", f)
    }

    /// Wiring this shows the button; an editor that is not mounted offers none.
    pub fn connect_open_requested<F: Fn(&Self, &str, &str) + 'static>(
        &self,
        f: F,
    ) -> glib::SignalHandlerId {
        self.imp().open.set_visible(true);
        self.connect_entry("open-requested", f)
    }

    fn connect_entry<F: Fn(&Self, &str, &str) + 'static>(
        &self,
        signal: &str,
        f: F,
    ) -> glib::SignalHandlerId {
        self.connect_local(signal, false, move |values| {
            let panel = values[0]
                .get::<Self>()
                .expect("the signal carries the panel");
            let sql = values[1].get::<String>().unwrap_or_default();
            let connection = values[2].get::<String>().unwrap_or_default();
            f(&panel, &sql, &connection);
            None
        })
    }
}

/// Retention is stated, never a silent cap.
pub fn retention_summary(retention: crate::model::Retention) -> String {
    format!(
        "keeping the last {} queries, up to {} days",
        format::count(u64::from(retention.max_entries)),
        retention.max_days
    )
}
