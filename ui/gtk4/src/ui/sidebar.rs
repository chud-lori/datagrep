use std::cell::{Cell, RefCell};
use std::sync::Arc;

use adw::prelude::*;
use adw::subclass::prelude::*;
use gtk::glib;

use crate::ffi::Core;
use crate::model::Profile;
use crate::ui::SchemaTree;

mod entry_imp {
    use super::*;

    #[derive(Default)]
    pub struct ConnectionEntry {
        pub name: RefCell<String>,
        pub driver: RefCell<String>,
        pub read_only: Cell<bool>,
        pub color: RefCell<Option<String>>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for ConnectionEntry {
        const NAME: &'static str = "DgConnectionEntry";
        type Type = super::ConnectionEntry;
    }

    impl ObjectImpl for ConnectionEntry {}
}

glib::wrapper! {
    pub struct ConnectionEntry(ObjectSubclass<entry_imp::ConnectionEntry>);
}

impl ConnectionEntry {
    fn new(profile: &Profile) -> Self {
        let entry: Self = glib::Object::new();
        let imp = entry.imp();
        *imp.name.borrow_mut() = profile.name.clone();
        *imp.driver.borrow_mut() = profile.driver.clone();
        imp.read_only.set(profile.read_only);
        *imp.color.borrow_mut() = profile.color.clone();
        entry
    }

    pub fn name(&self) -> String {
        self.imp().name.borrow().clone()
    }

    pub fn driver(&self) -> String {
        self.imp().driver.borrow().clone()
    }

    pub fn read_only(&self) -> bool {
        self.imp().read_only.get()
    }

    pub fn color(&self) -> Option<String> {
        self.imp().color.borrow().clone()
    }
}

mod imp {
    use super::*;
    use glib::subclass::Signal;
    use std::sync::OnceLock;

    pub struct Sidebar {
        pub core: RefCell<Option<Arc<Core>>>,
        pub profiles: gio::ListStore,
        pub selection: gtk::SingleSelection,
        pub connections: gtk::ListView,
        pub schema: SchemaTree,
    }

    impl Default for Sidebar {
        fn default() -> Self {
            let profiles = gio::ListStore::new::<ConnectionEntry>();
            let selection = gtk::SingleSelection::new(Some(profiles.clone()));
            // Selecting dials the server, so nothing is picked for the user at startup.
            selection.set_autoselect(false);
            selection.set_can_unselect(true);
            selection.set_selected(gtk::INVALID_LIST_POSITION);
            Self {
                core: RefCell::new(None),
                profiles,
                connections: gtk::ListView::new(
                    Some(selection.clone()),
                    None::<gtk::ListItemFactory>,
                ),
                selection,
                schema: SchemaTree::new(),
            }
        }
    }

    #[glib::object_subclass]
    impl ObjectSubclass for Sidebar {
        const NAME: &'static str = "DgSidebar";
        type Type = super::Sidebar;
        type ParentType = adw::NavigationPage;
    }

    impl ObjectImpl for Sidebar {
        fn signals() -> &'static [Signal] {
            static SIGNALS: OnceLock<Vec<Signal>> = OnceLock::new();
            SIGNALS.get_or_init(|| {
                vec![
                    Signal::builder("connection-selected")
                        .param_types([String::static_type()])
                        .build(),
                    Signal::builder("object-activated")
                        .param_types([
                            String::static_type(),
                            String::static_type(),
                            String::static_type(),
                        ])
                        .build(),
                    Signal::builder("object-described")
                        .param_types([
                            String::static_type(),
                            String::static_type(),
                            String::static_type(),
                        ])
                        .build(),
                    Signal::builder("edit-requested")
                        .param_types([String::static_type()])
                        .build(),
                    Signal::builder("remove-requested")
                        .param_types([String::static_type()])
                        .build(),
                ]
            })
        }

        fn constructed(&self) {
            self.parent_constructed();
            let obj = self.obj();
            obj.set_title("Connections");
            obj.set_tag(Some("connections"));

            let header = adw::HeaderBar::new();
            header.add_css_class("flat");
            let add = gtk::Button::from_icon_name("list-add-symbolic");
            add.set_tooltip_text(Some("New Connection"));
            add.set_action_name(Some("win.new-connection"));
            header.pack_start(&add);

            self.connections.add_css_class("navigation-sidebar");
            self.connections.set_factory(Some(&connection_factory()));
            self.install_actions();

            // The Qt gesture for the same thing, and what a double-click means everywhere else.
            let sidebar = self.obj().downgrade();
            self.connections.connect_activate(move |_, _| {
                if let Some(sidebar) = sidebar.upgrade() {
                    sidebar.imp().request("edit-requested");
                }
            });

            let sidebar = self.obj().downgrade();
            self.selection
                .connect_selected_item_notify(move |selection| {
                    if let Some(sidebar) = sidebar.upgrade() {
                        sidebar.imp().on_connection_selected(selection);
                    }
                });

            let sidebar = self.obj().downgrade();
            self.schema.connect_object_activated(move |_, path_json, name| {
                if let Some(sidebar) = sidebar.upgrade() {
                    let profile = sidebar.selected_connection().unwrap_or_default();
                    sidebar.emit_by_name::<()>(
                        "object-activated",
                        &[&profile, &path_json, &name],
                    );
                }
            });

            let sidebar = self.obj().downgrade();
            self.schema
                .connect_object_described(move |_, path, detail, error| {
                    if let Some(sidebar) = sidebar.upgrade() {
                        sidebar.emit_by_name::<()>("object-described", &[&path, &detail, &error]);
                    }
                });

            let connections = gtk::ScrolledWindow::builder()
                .hscrollbar_policy(gtk::PolicyType::Never)
                .propagate_natural_height(true)
                .max_content_height(220)
                .child(&self.connections)
                .build();

            let body = gtk::Box::new(gtk::Orientation::Vertical, 0);
            body.append(&connections);
            body.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
            body.append(&self.schema);

            let toolbar = adw::ToolbarView::new();
            toolbar.add_top_bar(&header);
            toolbar.set_content(Some(&body));
            obj.set_child(Some(&toolbar));
        }
    }

    impl WidgetImpl for Sidebar {}
    impl NavigationPageImpl for Sidebar {}

    impl Sidebar {
        fn install_actions(&self) {
            let actions = gio::SimpleActionGroup::new();
            for (name, signal) in [("edit", "edit-requested"), ("remove", "remove-requested")] {
                let action = gio::SimpleAction::new(name, None);
                let sidebar = self.obj().downgrade();
                action.connect_activate(move |_, _| {
                    if let Some(sidebar) = sidebar.upgrade() {
                        sidebar.imp().request(signal);
                    }
                });
                actions.add_action(&action);
            }
            self.obj()
                .insert_action_group("connections", Some(&actions));
        }

        /// Both actions read the selection, so nothing can act on a row nobody picked.
        pub(super) fn request(&self, signal: &str) {
            if let Some(name) = self.obj().selected_connection().filter(|n| !n.is_empty()) {
                self.obj().emit_by_name::<()>(signal, &[&name]);
            }
        }

        fn on_connection_selected(&self, selection: &gtk::SingleSelection) {
            let name = selection
                .selected_item()
                .and_downcast::<ConnectionEntry>()
                .map(|entry| entry.name())
                .unwrap_or_default();
            self.schema.show_profile(&name);
            self.obj()
                .emit_by_name::<()>("connection-selected", &[&name]);
        }
    }
}

/// The per-row menu: the only way to edit or remove a connection, so it is on
/// the selected row rather than behind a right-click alone.
fn row_menu() -> gio::Menu {
    let menu = gio::Menu::new();
    menu.append(Some("Edit Connection…"), Some("connections.edit"));
    menu.append(Some("Remove Connection…"), Some("connections.remove"));
    menu
}

fn connection_factory() -> gtk::SignalListItemFactory {
    let factory = gtk::SignalListItemFactory::new();
    factory.connect_setup(|_, item| {
        let Some(item) = item.downcast_ref::<gtk::ListItem>() else {
            return;
        };
        let text = gtk::Box::new(gtk::Orientation::Vertical, 0);
        text.set_hexpand(true);
        text.append(&gtk::Inscription::builder().xalign(0.0).build());
        let driver = gtk::Inscription::builder().xalign(0.0).build();
        driver.add_css_class("caption");
        driver.add_css_class("dim-label");
        text.append(&driver);

        let marker = gtk::Box::new(gtk::Orientation::Vertical, 0);
        marker.add_css_class("dg-marker");
        marker.set_vexpand(true);
        marker.set_visible(false);

        let lock = gtk::Image::from_icon_name("changes-prevent-symbolic");
        lock.set_tooltip_text(Some("read-only"));

        let menu = gtk::MenuButton::new();
        menu.set_icon_name("view-more-symbolic");
        menu.set_valign(gtk::Align::Center);
        menu.add_css_class("flat");
        menu.set_tooltip_text(Some("Connection actions"));
        menu.set_menu_model(Some(&row_menu()));
        // Only on the row the actions read, so the menu can never mean another connection.
        menu.set_visible(item.is_selected());
        item.connect_selected_notify(glib::clone!(
            #[weak]
            menu,
            move |item| menu.set_visible(item.is_selected())
        ));

        let row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        row.append(&marker);
        row.append(&gtk::Image::new());
        row.append(&text);
        row.append(&lock);
        row.append(&menu);
        item.set_child(Some(&row));
    });
    factory.connect_bind(|_, item| {
        let Some(item) = item.downcast_ref::<gtk::ListItem>() else {
            return;
        };
        let (Some(row), Some(entry)) = (
            item.child().and_downcast::<gtk::Box>(),
            item.item().and_downcast::<ConnectionEntry>(),
        ) else {
            return;
        };
        let imp = entry.imp();
        let marker = row.first_child();
        if let Some(marker) = marker.as_ref() {
            for name in crate::engine::MARKER_NAMES {
                marker.remove_css_class(&format!("dg-marker-{name}"));
            }
            match imp.color.borrow().as_deref().filter(|c| !c.is_empty()) {
                Some(color) => {
                    marker.add_css_class(&format!("dg-marker-{color}"));
                    marker.set_tooltip_text(Some(&format!("Marked connection ({color})")));
                    marker.set_visible(true);
                }
                None => marker.set_visible(false),
            }
        }
        if let Some(mark) = marker
            .as_ref()
            .and_then(|m| m.next_sibling())
            .and_downcast::<gtk::Image>()
        {
            mark.set_paintable(Some(&crate::engine::paintable(&imp.driver.borrow())));
        }
        if let Some(text) = row
            .first_child()
            .and_then(|c| c.next_sibling())
            .and_then(|c| c.next_sibling())
            .and_downcast::<gtk::Box>()
        {
            if let Some(name) = text.first_child().and_downcast::<gtk::Inscription>() {
                name.set_text(Some(&imp.name.borrow()));
            }
            if let Some(driver) = text.last_child().and_downcast::<gtk::Inscription>() {
                driver.set_text(Some(&crate::engine::display_name(&imp.driver.borrow())));
            }
        }
        if let Some(lock) = row
            .last_child()
            .and_then(|c| c.prev_sibling())
            .and_downcast::<gtk::Image>()
        {
            lock.set_visible(imp.read_only.get());
        }
    });
    factory
}

glib::wrapper! {
    pub struct Sidebar(ObjectSubclass<imp::Sidebar>)
        @extends adw::NavigationPage, gtk::Widget,
        @implements gtk::Accessible, gtk::Buildable, gtk::ConstraintTarget;
}

impl Default for Sidebar {
    fn default() -> Self {
        Self::new()
    }
}

impl Sidebar {
    pub fn new() -> Self {
        glib::Object::new()
    }

    pub fn set_core(&self, core: Arc<Core>) {
        self.imp().schema.set_core(core.clone());
        *self.imp().core.borrow_mut() = Some(core);
        self.reload();
    }

    /// Re-read the profile store, keeping the selected connection if it survived.
    pub fn reload(&self) {
        let imp = self.imp();
        let Some(core) = imp.core.borrow().clone() else {
            return;
        };
        let selected = self.selected_connection();
        let profiles = core
            .profiles_list_json()
            .map_err(|e| e.0)
            .and_then(|json| Profile::parse_list(&json))
            .unwrap_or_default();

        imp.profiles.remove_all();
        for profile in &profiles {
            imp.profiles.append(&ConnectionEntry::new(profile));
        }
        let restored = selected.and_then(|name| profiles.iter().position(|p| p.name == name));
        imp.selection.set_selected(match restored {
            Some(index) => index as u32,
            None => gtk::INVALID_LIST_POSITION,
        });
    }

    pub fn selected_connection(&self) -> Option<String> {
        self.imp()
            .selection
            .selected_item()
            .and_downcast::<ConnectionEntry>()
            .map(|entry| entry.name())
    }

    /// Select a connection by name, as an `@connection` directive would.
    pub fn select(&self, name: &str) -> bool {
        let imp = self.imp();
        for index in 0..imp.profiles.n_items() {
            let matched = imp
                .profiles
                .item(index)
                .and_downcast::<ConnectionEntry>()
                .is_some_and(|entry| entry.name() == name);
            if matched {
                imp.selection.set_selected(index);
                return true;
            }
        }
        false
    }

    /// Whether the selected connection refuses writes — the window's veto on editing.
    pub fn selected_read_only(&self) -> bool {
        self.imp()
            .selection
            .selected_item()
            .and_downcast::<ConnectionEntry>()
            .is_some_and(|entry| entry.read_only())
    }

    /// The engine behind the selected connection — what identifier quoting turns on.
    pub fn selected_driver(&self) -> Option<String> {
        self.imp()
            .selection
            .selected_item()
            .and_downcast::<ConnectionEntry>()
            .map(|entry| entry.driver())
    }

    pub fn connect_connection_selected<F: Fn(&Self, &str) + 'static>(
        &self,
        f: F,
    ) -> glib::SignalHandlerId {
        self.connect_local("connection-selected", false, move |values| {
            let sidebar = values[0]
                .get::<Self>()
                .expect("the signal carries the sidebar");
            let name = values[1].get::<String>().unwrap_or_default();
            f(&sidebar, &name);
            None
        })
    }

    pub fn connect_object_described<F: Fn(&Self, &str, &str, &str) + 'static>(
        &self,
        f: F,
    ) -> glib::SignalHandlerId {
        self.connect_local("object-described", false, move |values| {
            let sidebar = values[0]
                .get::<Self>()
                .expect("the signal carries the sidebar");
            let path = values[1].get::<String>().unwrap_or_default();
            let detail = values[2].get::<String>().unwrap_or_default();
            let error = values[3].get::<String>().unwrap_or_default();
            f(&sidebar, &path, &detail, &error);
            None
        })
    }

    pub fn connect_object_activated<F: Fn(&Self, &str, &str, &str) + 'static>(
        &self,
        f: F,
    ) -> glib::SignalHandlerId {
        self.connect_local("object-activated", false, move |values| {
            let sidebar = values[0]
                .get::<Self>()
                .expect("the signal carries the sidebar");
            let profile = values[1].get::<String>().unwrap_or_default();
            let path = values[2].get::<String>().unwrap_or_default();
            let name = values[3].get::<String>().unwrap_or_default();
            f(&sidebar, &profile, &path, &name);
            None
        })
    }
}
