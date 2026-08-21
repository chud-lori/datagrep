use std::cell::RefCell;
use std::rc::Rc;

use adw::prelude::*;
use adw::subclass::prelude::*;
use gtk::glib;

use crate::ffi::Core;
use crate::model::ResultModel;
use crate::sql::Derived;
use crate::ui::{ResultsGrid, Sidebar, StatusBar};

mod imp {
    use super::*;
    use glib::subclass::Signal;
    use std::sync::OnceLock;

    pub struct Window {
        pub core: RefCell<Option<Rc<Core>>>,
        pub model: ResultModel,
        pub grid: ResultsGrid,
        pub sidebar: Sidebar,
        pub status: StatusBar,
        pub title: adw::WindowTitle,
        pub navigation: adw::NavigationSplitView,
        pub utility: adw::OverlaySplitView,
        pub editor_slot: adw::Bin,
        pub utility_slot: adw::Bin,
        pub derived: RefCell<Derived>,
    }

    impl Default for Window {
        fn default() -> Self {
            let model = ResultModel::new();
            Self {
                core: RefCell::new(None),
                grid: ResultsGrid::new(&model),
                model,
                sidebar: Sidebar::new(),
                status: StatusBar::new(),
                title: adw::WindowTitle::new("datagrep", ""),
                navigation: adw::NavigationSplitView::new(),
                utility: adw::OverlaySplitView::new(),
                editor_slot: adw::Bin::new(),
                utility_slot: adw::Bin::new(),
                derived: RefCell::new(Derived::default()),
            }
        }
    }

    #[glib::object_subclass]
    impl ObjectSubclass for Window {
        const NAME: &'static str = "DgWindow";
        type Type = super::Window;
        type ParentType = adw::ApplicationWindow;
    }

    impl ObjectImpl for Window {
        fn signals() -> &'static [Signal] {
            static SIGNALS: OnceLock<Vec<Signal>> = OnceLock::new();
            SIGNALS.get_or_init(|| {
                vec![
                    Signal::builder("new-connection").build(),
                    Signal::builder("object-activated")
                        .param_types([String::static_type(), String::static_type()])
                        .build(),
                    Signal::builder("object-described")
                        .param_types([
                            String::static_type(),
                            String::static_type(),
                            String::static_type(),
                        ])
                        .build(),
                    Signal::builder("run-started")
                        .param_types([
                            String::static_type(),
                            String::static_type(),
                            String::static_type(),
                        ])
                        .build(),
                    Signal::builder("run-failed")
                        .param_types([String::static_type()])
                        .build(),
                ]
            })
        }

        fn constructed(&self) {
            self.parent_constructed();
            let obj = self.obj();
            obj.set_default_size(1280, 800);
            obj.set_title(Some("datagrep"));

            let toolbar = adw::ToolbarView::new();
            toolbar.add_top_bar(&self.header());
            toolbar.set_content(Some(&self.navigation));
            toolbar.add_bottom_bar(&self.status);
            obj.set_content(Some(&toolbar));

            self.build_panes();
            self.add_breakpoints();
            self.wire();
            for slot in [&self.editor_slot, &self.utility_slot] {
                slot.set_visible(false);
                slot.connect_child_notify(|slot| slot.set_visible(slot.child().is_some()));
            }
        }
    }

    impl WidgetImpl for Window {}
    impl WindowImpl for Window {}
    impl ApplicationWindowImpl for Window {}
    impl AdwApplicationWindowImpl for Window {}

    impl Window {
        fn header(&self) -> adw::HeaderBar {
            let header = adw::HeaderBar::new();
            header.set_title_widget(Some(&self.title));

            // This header bar sits above the split view, so collapsed needs its own way back.
            let back = gtk::Button::from_icon_name("go-previous-symbolic");
            back.set_tooltip_text(Some("Connections"));
            back.set_visible(false);
            let navigation = self.navigation.clone();
            back.connect_clicked(move |_| navigation.set_show_content(false));
            header.pack_start(&back);
            let refresh = {
                let navigation = self.navigation.clone();
                let back = back.clone();
                move |_: &adw::NavigationSplitView| {
                    back.set_visible(navigation.is_collapsed() && navigation.shows_content());
                }
            };
            self.navigation.connect_collapsed_notify(refresh.clone());
            self.navigation.connect_show_content_notify(refresh);

            let utility_toggle = gtk::ToggleButton::new();
            utility_toggle.set_icon_name("sidebar-show-right-symbolic");
            utility_toggle.set_tooltip_text(Some("Inspector"));
            self.utility_slot
                .bind_property("child", &utility_toggle, "sensitive")
                .transform_to(|_, child: Option<gtk::Widget>| Some(child.is_some()))
                .sync_create()
                .build();
            self.utility
                .bind_property("show-sidebar", &utility_toggle, "active")
                .bidirectional()
                .sync_create()
                .build();
            header.pack_end(&utility_toggle);
            header
        }

        fn build_panes(&self) {
            self.utility.set_sidebar_position(gtk::PackType::End);
            self.utility.set_max_sidebar_width(420.0);
            self.utility.set_show_sidebar(false);
            self.utility.set_sidebar(Some(&self.utility_slot));

            let workspace = gtk::Paned::new(gtk::Orientation::Vertical);
            workspace.set_shrink_start_child(false);
            workspace.set_shrink_end_child(false);
            workspace.set_start_child(Some(&self.editor_slot));
            workspace.set_end_child(Some(&self.grid));
            workspace.set_position(260);
            self.utility.set_content(Some(&workspace));

            let content = adw::NavigationPage::new(&self.utility, "Workbench");
            content.set_tag(Some("workbench"));
            self.navigation.set_sidebar(Some(&self.sidebar));
            self.navigation.set_content(Some(&content));
            self.navigation.set_min_sidebar_width(260.0);
            self.navigation.set_max_sidebar_width(340.0);
            self.navigation.set_sidebar_width_fraction(0.22);
        }

        fn add_breakpoints(&self) {
            let obj = self.obj();
            if let Ok(condition) = adw::BreakpointCondition::parse("max-width: 1000sp") {
                let breakpoint = adw::Breakpoint::new(condition);
                breakpoint.add_setter(&self.utility, "collapsed", Some(&true.into()));
                obj.add_breakpoint(breakpoint);
            }
            if let Ok(condition) = adw::BreakpointCondition::parse("max-width: 560sp") {
                let breakpoint = adw::Breakpoint::new(condition);
                breakpoint.add_setter(&self.navigation, "collapsed", Some(&true.into()));
                obj.add_breakpoint(breakpoint);
            }
        }

        fn wire(&self) {
            self.status.bind(&self.model);

            let new_connection = gio::SimpleAction::new("new-connection", None);
            let window = self.obj().downgrade();
            new_connection.connect_activate(move |_, _| {
                if let Some(window) = window.upgrade() {
                    window.emit_by_name::<()>("new-connection", &[]);
                }
            });
            self.obj().add_action(&new_connection);

            let window = self.obj().downgrade();
            self.sidebar.connect_connection_selected(move |_, name| {
                if let Some(window) = window.upgrade() {
                    window.imp().title.set_subtitle(name);
                }
            });

            let window = self.obj().downgrade();
            self.sidebar
                .connect_object_described(move |_, path, detail, error| {
                    if let Some(window) = window.upgrade() {
                        window.emit_by_name::<()>("object-described", &[&path, &detail, &error]);
                    }
                });

            let window = self.obj().downgrade();
            self.sidebar
                .connect_object_activated(move |_, profile, path| {
                    if let Some(window) = window.upgrade() {
                        window.emit_by_name::<()>("object-activated", &[&profile, &path]);
                    }
                });

            // A header click is a new statement, not a re-ordering of what is loaded.
            let window = self.obj().downgrade();
            self.grid
                .connect_sort_requested(move |_, column, ascending| {
                    let Some(window) = window.upgrade() else {
                        return;
                    };
                    let imp = window.imp();
                    if imp.derived.borrow().base().is_empty() {
                        return;
                    }
                    imp.derived.borrow_mut().sort_by(column, ascending);
                    imp.execute();
                });

            let window = self.obj().downgrade();
            self.status.connect_cancel_requested(move |bar| {
                if let Some(window) = window.upgrade() {
                    if let Some(outcome) = window.imp().model.cancel() {
                        bar.say(&outcome, false);
                    }
                }
            });
        }

        pub(super) fn execute(&self) {
            let Some(core) = self.core.borrow().clone() else {
                return;
            };
            let profile = self.sidebar.selected_connection().unwrap_or_default();
            if profile.is_empty() {
                self.status.say("pick a connection first", true);
                return;
            }
            let sql = self.derived.borrow().sql();
            if sql.trim().is_empty() {
                return;
            }
            // Announced before the engine is asked, so a statement refused was never a run.
            let driver = self.sidebar.selected_driver().unwrap_or_default();
            let obj = self.obj();
            obj.emit_by_name::<()>("run-started", &[&profile, &driver, &sql]);
            match core.query(&profile, &sql) {
                Ok(query) => {
                    self.status.say("", false);
                    self.model.set_query(query);
                }
                Err(error) => {
                    self.model.reset();
                    self.status.say(&error.0, true);
                    obj.emit_by_name::<()>("run-failed", &[&error.0]);
                }
            }
        }
    }
}

glib::wrapper! {
    pub struct Window(ObjectSubclass<imp::Window>)
        @extends adw::ApplicationWindow, gtk::ApplicationWindow, gtk::Window, gtk::Widget,
        @implements gio::ActionGroup, gio::ActionMap, gtk::Accessible, gtk::Buildable,
                    gtk::ConstraintTarget, gtk::Native, gtk::Root, gtk::ShortcutManager;
}

impl Window {
    pub fn new(app: &adw::Application, core: Rc<Core>) -> Self {
        let window: Self = glib::Object::builder().property("application", app).build();
        let imp = window.imp();
        imp.sidebar.set_core(core.clone());
        *imp.core.borrow_mut() = Some(core);
        window
    }

    /// The one run path, so the derived clauses cannot be bypassed by where the SQL came from.
    pub fn run(&self, sql: &str) {
        let imp = self.imp();
        imp.derived.borrow_mut().ask(
            sql,
            &self.imp().sidebar.selected_driver().unwrap_or_default(),
        );
        imp.grid.clear_sort_indicator();
        imp.execute();
    }

    pub fn model(&self) -> ResultModel {
        self.imp().model.clone()
    }

    pub fn grid(&self) -> ResultsGrid {
        self.imp().grid.clone()
    }

    pub fn status_bar(&self) -> StatusBar {
        self.imp().status.clone()
    }

    /// Where the SQL editor mounts.
    pub fn editor_slot(&self) -> adw::Bin {
        self.imp().editor_slot.clone()
    }

    /// Where the inspector / history pane mounts.
    pub fn utility_slot(&self) -> adw::Bin {
        self.imp().utility_slot.clone()
    }

    /// Slide the utility pane out, for the one click that unambiguously asks for it.
    pub fn reveal_utility(&self) {
        self.imp().utility.set_show_sidebar(true);
    }

    pub fn select_connection(&self, name: &str) -> bool {
        self.imp().sidebar.select(name)
    }

    pub fn reload_connections(&self) {
        self.imp().sidebar.reload();
    }

    pub fn connect_new_connection<F: Fn(&Self) + 'static>(&self, f: F) -> glib::SignalHandlerId {
        self.connect_local("new-connection", false, move |values| {
            let window = values[0]
                .get::<Self>()
                .expect("the signal carries the window");
            f(&window);
            None
        })
    }

    /// The selected catalog object's describe payload, or its failure.
    pub fn connect_object_described<F: Fn(&Self, &str, &str, &str) + 'static>(
        &self,
        f: F,
    ) -> glib::SignalHandlerId {
        self.connect_local("object-described", false, move |values| {
            let window = values[0]
                .get::<Self>()
                .expect("the signal carries the window");
            let path = values[1].get::<String>().unwrap_or_default();
            let detail = values[2].get::<String>().unwrap_or_default();
            let error = values[3].get::<String>().unwrap_or_default();
            f(&window, &path, &detail, &error);
            None
        })
    }

    /// About to be sent: the connection, its engine, and the SQL as `run` will send it.
    pub fn connect_run_started<F: Fn(&Self, &str, &str, &str) + 'static>(
        &self,
        f: F,
    ) -> glib::SignalHandlerId {
        self.connect_local("run-started", false, move |values| {
            let window = values[0]
                .get::<Self>()
                .expect("the signal carries the window");
            let profile = values[1].get::<String>().unwrap_or_default();
            let driver = values[2].get::<String>().unwrap_or_default();
            let sql = values[3].get::<String>().unwrap_or_default();
            f(&window, &profile, &driver, &sql);
            None
        })
    }

    /// The run never got a query handle, so no status tick will ever report it.
    pub fn connect_run_failed<F: Fn(&Self, &str) + 'static>(&self, f: F) -> glib::SignalHandlerId {
        self.connect_local("run-failed", false, move |values| {
            let window = values[0]
                .get::<Self>()
                .expect("the signal carries the window");
            let message = values[1].get::<String>().unwrap_or_default();
            f(&window, &message);
            None
        })
    }

    pub fn connect_object_activated<F: Fn(&Self, &str, &str) + 'static>(
        &self,
        f: F,
    ) -> glib::SignalHandlerId {
        self.connect_local("object-activated", false, move |values| {
            let window = values[0]
                .get::<Self>()
                .expect("the signal carries the window");
            let profile = values[1].get::<String>().unwrap_or_default();
            let path = values[2].get::<String>().unwrap_or_default();
            f(&window, &profile, &path);
            None
        })
    }
}
