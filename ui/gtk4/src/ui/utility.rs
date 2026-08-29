use std::path::PathBuf;

use adw::prelude::*;
use adw::subclass::prelude::*;
use gtk::glib;

use crate::ffi::CellKind;
use crate::model::HistoryStore;
use crate::ui::{HistoryPanel, Inspector, Window};

mod imp {
    use super::*;

    pub struct UtilityPane {
        pub stack: adw::ViewStack,
        pub inspector: Inspector,
        pub history: HistoryPanel,
    }

    impl Default for UtilityPane {
        fn default() -> Self {
            Self {
                stack: adw::ViewStack::new(),
                inspector: Inspector::new(),
                history: HistoryPanel::new(),
            }
        }
    }

    #[glib::object_subclass]
    impl ObjectSubclass for UtilityPane {
        const NAME: &'static str = "DgUtilityPane";
        type Type = super::UtilityPane;
        type ParentType = adw::Bin;
    }

    impl ObjectImpl for UtilityPane {
        fn constructed(&self) {
            self.parent_constructed();
            self.stack.add_titled_with_icon(
                &self.inspector,
                Some("inspector"),
                "Inspector",
                "view-list-symbolic",
            );
            self.stack.add_titled_with_icon(
                &self.history,
                Some("history"),
                "History",
                "document-open-recent-symbolic",
            );
            self.stack.set_vexpand(true);

            let switcher = adw::ViewSwitcher::builder()
                .stack(&self.stack)
                .policy(adw::ViewSwitcherPolicy::Wide)
                .build();
            let bar = gtk::Box::new(gtk::Orientation::Horizontal, 0);
            bar.add_css_class("toolbar");
            bar.set_halign(gtk::Align::Center);
            bar.append(&switcher);

            let body = gtk::Box::new(gtk::Orientation::Vertical, 0);
            body.append(&bar);
            body.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
            body.append(&self.stack);
            self.obj().set_child(Some(&body));
        }
    }

    impl WidgetImpl for UtilityPane {}
    impl BinImpl for UtilityPane {}
}

glib::wrapper! {
    pub struct UtilityPane(ObjectSubclass<imp::UtilityPane>)
        @extends adw::Bin, gtk::Widget,
        @implements gtk::Accessible, gtk::Buildable, gtk::ConstraintTarget;
}

impl UtilityPane {
    /// Fills `Window::utility_slot` and wires both panels to the window they inspect.
    pub fn mount(window: &Window, history_dir: PathBuf) -> Self {
        let pane: Self = glib::Object::new();
        let imp = pane.imp();
        let status = window.status_bar();
        imp.inspector.set_status_bar(&status);
        imp.history.set_status_bar(&status);

        let store = HistoryStore::new(history_dir);
        imp.history.bind(&store);

        let inspector = imp.inspector.downgrade();
        window.connect_object_described(move |_, path, detail, error| {
            if let Some(inspector) = inspector.upgrade() {
                inspector.show_schema(path, detail, error);
            }
        });

        let (owner, watched) = (pane.downgrade(), window.downgrade());
        window.grid().connect_cell_selected(move |_, row, column| {
            let (Some(pane), Some(window)) = (owner.upgrade(), watched.upgrade()) else {
                return;
            };
            pane.show_cell(&window, row, column);
        });

        // Recorded before the engine is asked, so a refused run has an entry to fail into.
        let (log, inspector) = (store.clone(), imp.inspector.downgrade());
        window.connect_run_started(move |_, profile, driver, sql| {
            log.execution_started(sql, profile, driver);
            if let Some(inspector) = inspector.upgrade() {
                inspector.clear_cell();
            }
        });
        let log = store.clone();
        window.connect_run_failed(move |_, message| log.execution_failed_to_start(message));
        let log = store.clone();
        window.model().connect_status_changed(move |model| {
            model.with_status(|status| log.execution_progressed(status));
        });

        // A replay is a run like any other: same path, same guards, same prompt.
        let watched = window.downgrade();
        imp.history
            .connect_rerun_requested(move |_, sql, connection| {
                let Some(window) = watched.upgrade() else {
                    return;
                };
                if !connection.is_empty() && !window.select_connection(connection) {
                    window.status_bar().say(
                        &format!("connection ‘{connection}’ no longer exists — not run"),
                        true,
                    );
                    return;
                }
                window.run(sql);
            });

        store.load();
        window.utility_slot().set_child(Some(&pane));
        pane
    }

    pub fn inspector(&self) -> Inspector {
        self.imp().inspector.clone()
    }

    pub fn history(&self) -> HistoryPanel {
        self.imp().history.clone()
    }

    pub fn show_page(&self, name: &str) {
        self.imp().stack.set_visible_child_name(name);
    }

    fn show_cell(&self, window: &Window, row: u64, column: u32) {
        let model = window.model();
        let kind = model.with_cell(row, column, |kind, _, _| kind);
        if kind == CellKind::Pending {
            return; // skeleton row — nothing truthful to show yet
        }
        let name = model.column(column).map(|c| c.name).unwrap_or_default();
        self.imp().inspector.show_cell(
            row,
            column,
            &name,
            &model.cell_detail_json(row, column).unwrap_or_default(),
            &model.envelope_json(row).unwrap_or_default(),
        );
        // Only a nested cell raises the pane: that click unambiguously asks to see inside.
        if kind == CellKind::Nested {
            self.show_page("inspector");
            window.reveal_utility();
        }
    }
}
