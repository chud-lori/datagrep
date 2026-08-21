use std::cell::{Cell, RefCell};
use std::rc::Rc;

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
        entry
    }

    pub fn name(&self) -> String {
        self.imp().name.borrow().clone()
    }

    pub fn driver(&self) -> String {
        self.imp().driver.borrow().clone()
    }
}

mod imp {
    use super::*;
    use glib::subclass::Signal;
    use std::sync::OnceLock;

    pub struct Sidebar {
        pub core: RefCell<Option<Rc<Core>>>,
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
                        .param_types([String::static_type(), String::static_type()])
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

            let sidebar = self.obj().downgrade();
            self.selection
                .connect_selected_item_notify(move |selection| {
                    if let Some(sidebar) = sidebar.upgrade() {
                        sidebar.imp().on_connection_selected(selection);
                    }
                });

            let sidebar = self.obj().downgrade();
            self.schema.connect_object_activated(move |_, path_json| {
                if let Some(sidebar) = sidebar.upgrade() {
                    let profile = sidebar.selected_connection().unwrap_or_default();
                    sidebar.emit_by_name::<()>("object-activated", &[&profile, &path_json]);
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

        let lock = gtk::Image::from_icon_name("changes-prevent-symbolic");
        lock.set_tooltip_text(Some("read-only"));

        let row = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        row.append(&text);
        row.append(&lock);
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
        if let Some(text) = row.first_child().and_downcast::<gtk::Box>() {
            if let Some(name) = text.first_child().and_downcast::<gtk::Inscription>() {
                name.set_text(Some(&imp.name.borrow()));
            }
            if let Some(driver) = text.last_child().and_downcast::<gtk::Inscription>() {
                driver.set_text(Some(&imp.driver.borrow()));
            }
        }
        if let Some(lock) = row.last_child() {
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

    pub fn set_core(&self, core: Rc<Core>) {
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

    pub fn connect_object_activated<F: Fn(&Self, &str, &str) + 'static>(
        &self,
        f: F,
    ) -> glib::SignalHandlerId {
        self.connect_local("object-activated", false, move |values| {
            let sidebar = values[0]
                .get::<Self>()
                .expect("the signal carries the sidebar");
            let profile = values[1].get::<String>().unwrap_or_default();
            let path = values[2].get::<String>().unwrap_or_default();
            f(&sidebar, &profile, &path);
            None
        })
    }
}
