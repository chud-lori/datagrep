use std::time::Duration;

use adw::prelude::*;
use adw::subclass::prelude::*;
use gtk::{gio, glib};

use crate::editor::EditorPage;
use crate::engine;
use crate::model::Profile;
use crate::sql;
use crate::store::{SavedQueryRecord, SavedQueryStore};

mod imp {
    use std::cell::{Cell, OnceCell, RefCell};
    use std::sync::OnceLock;

    use glib::subclass::Signal;

    use super::*;

    #[derive(Default)]
    pub struct EditorTabs {
        pub tab_view: adw::TabView,
        pub tab_bar: OnceCell<adw::TabBar>,
        pub stack: gtk::Stack,
        pub welcome: OnceCell<adw::StatusPage>,
        pub saved_menu: gio::Menu,
        pub store: OnceCell<SavedQueryStore>,
        pub connections: RefCell<Vec<Profile>>,
        pub window_connection: RefCell<Option<String>>,
        pub menu_page: RefCell<Option<adw::TabPage>>,
        pub bind_action: OnceCell<gio::SimpleAction>,
        pub flush_source: RefCell<Option<glib::SourceId>>,
        pub restoring: Cell<bool>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for EditorTabs {
        const NAME: &'static str = "DgEditorTabs";
        type Type = super::EditorTabs;
        type ParentType = adw::Bin;
    }

    impl ObjectImpl for EditorTabs {
        fn signals() -> &'static [Signal] {
            static SIGNALS: OnceLock<Vec<Signal>> = OnceLock::new();
            SIGNALS.get_or_init(|| {
                vec![
                    // (profile name or "", sql) — profile already resolved by precedence.
                    Signal::builder("run-requested")
                        .param_types([String::static_type(), String::static_type()])
                        .build(),
                    Signal::builder("new-connection-requested").build(),
                    // (tab id or "", the connection that tab runs on or "")
                    Signal::builder("tab-activated")
                        .param_types([String::static_type(), String::static_type()])
                        .build(),
                    Signal::builder("tabs-closed").build(),
                ]
            })
        }

        fn constructed(&self) {
            self.parent_constructed();
            self.obj().build();
        }

        fn dispose(&self) {
            if let Some(source) = self.flush_source.take() {
                source.remove();
            }
        }
    }

    impl WidgetImpl for EditorTabs {}
    impl BinImpl for EditorTabs {}
}

glib::wrapper! {
    pub struct EditorTabs(ObjectSubclass<imp::EditorTabs>)
        @extends adw::Bin, gtk::Widget,
        @implements gtk::Accessible, gtk::Buildable, gtk::ConstraintTarget;
}

impl Default for EditorTabs {
    fn default() -> Self {
        Self::new()
    }
}

impl EditorTabs {
    pub fn new() -> Self {
        glib::Object::new()
    }

    fn build(&self) {
        let imp = self.imp();
        imp.store
            .set(SavedQueryStore::new(SavedQueryStore::default_directory()))
            .ok()
            .unwrap();

        let tab_bar = adw::TabBar::new();
        tab_bar.set_view(Some(&imp.tab_view));
        tab_bar.set_autohide(false);

        let plus = adw::SplitButton::new();
        plus.set_icon_name("list-add-symbolic");
        plus.set_tooltip_text(Some(
            "New query tab (Ctrl+T) — the arrow reopens a saved query",
        ));
        plus.set_menu_model(Some(&imp.saved_menu));
        plus.connect_clicked(glib::clone!(
            #[weak(rename_to = tabs)]
            self,
            move |_| tabs.new_scratch_tab()
        ));
        tab_bar.set_end_action_widget(Some(&plus));

        let editors = gtk::Box::new(gtk::Orientation::Vertical, 0);
        editors.append(&tab_bar);
        editors.append(&imp.tab_view);
        imp.tab_view.set_vexpand(true);

        let welcome = adw::StatusPage::new();
        // .compact and no icon: the editor pane is short, and the buttons must fit in it.
        welcome.add_css_class("compact");
        welcome.set_title("No Editor Open");
        let actions = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        actions.set_halign(gtk::Align::Center);
        let new_editor = gtk::Button::with_label("New SQL Editor");
        new_editor.add_css_class("pill");
        new_editor.add_css_class("suggested-action");
        new_editor.set_action_name(Some("tabs.new"));
        let new_connection = gtk::Button::with_label("New Connection…");
        new_connection.add_css_class("pill");
        new_connection.connect_clicked(glib::clone!(
            #[weak(rename_to = tabs)]
            self,
            move |_| tabs.emit_by_name::<()>("new-connection-requested", &[])
        ));
        actions.append(&new_editor);
        actions.append(&new_connection);
        welcome.set_child(Some(&actions));

        imp.stack.add_named(&welcome, Some("welcome"));
        imp.stack.add_named(&editors, Some("editors"));
        self.set_child(Some(&imp.stack));
        imp.tab_bar.set(tab_bar).ok().unwrap();
        imp.welcome.set(welcome).ok().unwrap();

        self.install_actions();
        self.install_shortcuts();
        self.wire_tab_view();

        adw::StyleManager::default().connect_dark_notify(glib::clone!(
            #[weak(rename_to = tabs)]
            self,
            move |manager| {
                let dark = manager.is_dark();
                for editor in tabs.editors() {
                    editor.apply_scheme(dark);
                    tabs.update_page_chrome(&editor);
                }
            }
        ));

        self.restore();
        self.update_welcome();
        self.update_empty_state();
        self.rebuild_saved_menu();
    }

    fn install_actions(&self) {
        let group = gio::SimpleActionGroup::new();

        let new = gio::SimpleAction::new("new", None);
        new.connect_activate(glib::clone!(
            #[weak(rename_to = tabs)]
            self,
            move |_, _| tabs.new_scratch_tab()
        ));
        group.add_action(&new);

        let open = gio::SimpleAction::new("open-saved", Some(glib::VariantTy::STRING));
        open.connect_activate(glib::clone!(
            #[weak(rename_to = tabs)]
            self,
            move |_, param| {
                if let Some(id) = param.and_then(|v| v.str()) {
                    tabs.open_saved(id);
                }
            }
        ));
        group.add_action(&open);

        let save = gio::SimpleAction::new("save", None);
        save.connect_activate(glib::clone!(
            #[weak(rename_to = tabs)]
            self,
            move |_, _| tabs.save_target()
        ));
        group.add_action(&save);

        let close = gio::SimpleAction::new("close", None);
        close.connect_activate(glib::clone!(
            #[weak(rename_to = tabs)]
            self,
            move |_, _| {
                if let Some(page) = tabs.target_page() {
                    tabs.imp().tab_view.close_page(&page);
                }
            }
        ));
        group.add_action(&close);

        let bind = gio::SimpleAction::new_stateful(
            "bind",
            Some(glib::VariantTy::STRING),
            &"".to_variant(),
        );
        bind.connect_activate(glib::clone!(
            #[weak(rename_to = tabs)]
            self,
            move |action, param| {
                let name = param.and_then(|v| v.str()).unwrap_or_default();
                action.set_state(&name.to_variant());
                if let Some(page) = tabs.target_page() {
                    let editor = tabs.editor_of(&page);
                    editor.set_connection_binding((!name.is_empty()).then_some(name));
                    tabs.update_page_chrome(&editor);
                    tabs.announce_active();
                    tabs.schedule_flush();
                }
            }
        ));
        group.add_action(&bind);
        self.imp().bind_action.set(bind).ok().unwrap();

        self.insert_action_group("tabs", Some(&group));
    }

    fn install_shortcuts(&self) {
        let shortcuts = gtk::ShortcutController::new();
        shortcuts.set_scope(gtk::ShortcutScope::Global);
        for (trigger, action) in [
            ("<Control>t", "tabs.new"),
            ("<Control>w", "tabs.close"),
            ("<Control>s", "tabs.save"),
        ] {
            shortcuts.add_shortcut(gtk::Shortcut::new(
                gtk::ShortcutTrigger::parse_string(trigger),
                Some(gtk::NamedAction::new(action)),
            ));
        }
        self.add_controller(shortcuts);
    }

    fn wire_tab_view(&self) {
        let view = &self.imp().tab_view;

        view.connect_close_page(glib::clone!(
            #[weak(rename_to = tabs)]
            self,
            #[upgrade_or]
            glib::Propagation::Proceed,
            move |view, page| tabs.on_close_page(view, page)
        ));

        view.connect_page_detached(glib::clone!(
            #[weak(rename_to = tabs)]
            self,
            move |_, _, _| {
                tabs.update_empty_state();
                tabs.rebuild_saved_menu();
                tabs.schedule_flush();
                tabs.emit_by_name::<()>("tabs-closed", &[]);
                tabs.announce_active();
            }
        ));
        view.connect_page_reordered(glib::clone!(
            #[weak(rename_to = tabs)]
            self,
            move |_, _, _| tabs.schedule_flush()
        ));
        view.connect_selected_page_notify(glib::clone!(
            #[weak(rename_to = tabs)]
            self,
            move |view| {
                if let Some(page) = view.selected_page() {
                    tabs.editor_of(&page).grab_editor_focus();
                }
                tabs.announce_active();
                tabs.schedule_flush();
            }
        ));
        view.connect_n_pages_notify(glib::clone!(
            #[weak(rename_to = tabs)]
            self,
            move |_| tabs.update_empty_state()
        ));

        view.connect_setup_menu(glib::clone!(
            #[weak(rename_to = tabs)]
            self,
            move |view, page| {
                tabs.imp().menu_page.replace(page.cloned());
                if let Some(page) = page {
                    tabs.build_page_menu(view, page);
                }
            }
        ));
    }

    // ---- pages -----------------------------------------------------------

    fn pages(&self) -> Vec<adw::TabPage> {
        self.imp()
            .tab_view
            .pages()
            .iter::<adw::TabPage>()
            .flatten()
            .collect()
    }

    fn editors(&self) -> Vec<EditorPage> {
        self.pages().iter().map(|p| self.editor_of(p)).collect()
    }

    fn editor_of(&self, page: &adw::TabPage) -> EditorPage {
        page.child().downcast().unwrap()
    }

    fn page_of(&self, editor: &EditorPage) -> adw::TabPage {
        self.imp().tab_view.page(editor)
    }

    fn target_page(&self) -> Option<adw::TabPage> {
        self.imp()
            .menu_page
            .borrow()
            .clone()
            .or_else(|| self.imp().tab_view.selected_page())
    }

    fn add_editor_page(&self, record: SavedQueryRecord, text: &str) -> adw::TabPage {
        let number = if record.is_scratch() {
            self.editors()
                .iter()
                .map(EditorPage::untitled_number)
                .max()
                .unwrap_or(0)
                + 1
        } else {
            0
        };
        let editor = EditorPage::new(record, text, number);

        editor.connect_local(
            "modified",
            false,
            glib::clone!(
                #[weak(rename_to = tabs)]
                self,
                #[upgrade_or]
                None,
                move |values| {
                    let editor = values[0].get::<EditorPage>().unwrap();
                    tabs.update_page_chrome(&editor);
                    tabs.schedule_flush();
                    None
                }
            ),
        );
        editor.connect_local(
            "run-requested",
            false,
            glib::clone!(
                #[weak(rename_to = tabs)]
                self,
                #[upgrade_or]
                None,
                move |values| {
                    let editor = values[0].get::<EditorPage>().unwrap();
                    let sql_text = values[1].get::<String>().unwrap();
                    let directive = values[2].get::<String>().unwrap();
                    tabs.run(&editor, &sql_text, &directive);
                    None
                }
            ),
        );

        let page = self.imp().tab_view.add_page(&editor, None);
        self.update_page_chrome(&editor);
        self.rebuild_saved_menu();
        self.schedule_flush();
        page
    }

    pub fn new_scratch_tab(&self) {
        let mut record = SavedQueryRecord::scratch();
        record.connection = self.imp().window_connection.borrow().clone();
        let page = self.add_editor_page(record, "");
        self.imp().tab_view.set_selected_page(&page);
        self.editor_of(&page).grab_editor_focus();
    }

    /// One catalog object's rows, in a tab of their own. A second click on the
    /// same object focuses its tab and touches neither the buffer nor the server.
    pub fn open_browse(&self, connection: &str, subject: &str, sql: &str) {
        if let Some(editor) = self.editors().iter().find(|e| {
            e.subject().as_deref() == Some(subject)
                && e.connection_binding().as_deref() == Some(connection)
        }) {
            self.imp().tab_view.set_selected_page(&self.page_of(editor));
            editor.grab_editor_focus();
            return;
        }
        let mut record = SavedQueryRecord::scratch();
        record.connection = Some(connection.to_string());
        record.subject = Some(subject.to_string());
        let page = self.add_editor_page(record, "");
        let editor = self.editor_of(&page);
        editor.set_text_unmodified(sql);
        self.imp().tab_view.set_selected_page(&page);
        editor.run_statement();
    }

    /// A statement out of history, in a tab of its own rather than over what is open.
    pub fn open_with_sql(&self, sql: &str, connection: Option<&str>) {
        let mut record = SavedQueryRecord::scratch();
        record.connection = connection
            .filter(|c| !c.is_empty())
            .map(str::to_string)
            .or_else(|| self.imp().window_connection.borrow().clone());
        let page = self.add_editor_page(record, "");
        let editor = self.editor_of(&page);
        editor.set_text_unmodified(sql);
        self.imp().tab_view.set_selected_page(&page);
        editor.grab_editor_focus();
    }

    pub fn open_saved(&self, id: &str) {
        if let Some(editor) = self.editors().iter().find(|e| e.id() == id) {
            self.imp().tab_view.set_selected_page(&self.page_of(editor));
            return;
        }
        let store = self.store();
        let Some(record) = store.all_records().into_iter().find(|r| r.id == id) else {
            return;
        };
        let text = store.text(&record).unwrap_or_default();
        let page = self.add_editor_page(record, &text);
        self.imp().tab_view.set_selected_page(&page);
    }

    pub fn active_editor(&self) -> Option<EditorPage> {
        self.imp()
            .tab_view
            .selected_page()
            .map(|p| self.editor_of(&p))
    }

    // ---- run: the one precedence rule ------------------------------------

    /// Where this editor's statements go when no `-- @connection` overrides it.
    fn target_of(&self, editor: &EditorPage, directive: Option<&str>) -> String {
        let binding = editor.connection_binding();
        let window = self.imp().window_connection.borrow().clone();
        sql::effective_connection(directive, binding.as_deref(), window.as_deref())
            .unwrap_or_default()
            .to_string()
    }

    fn run(&self, editor: &EditorPage, sql_text: &str, directive: &str) {
        let profile = self.target_of(editor, (!directive.is_empty()).then_some(directive));
        self.emit_by_name::<()>("run-requested", &[&profile, &sql_text.to_string()]);
    }

    /// The tab in front and the connection it runs on — one signal for both, so
    /// a result can never be shown under a connection that did not produce it.
    pub fn announce_active(&self) {
        let (id, connection) = match self.active_editor() {
            Some(editor) => (editor.id(), self.target_of(&editor, None)),
            None => (String::new(), String::new()),
        };
        self.emit_by_name::<()>("tab-activated", &[&id, &connection]);
    }

    /// Every tab still open, so a closed one's result can be freed.
    pub fn live_ids(&self) -> Vec<String> {
        self.editors().iter().map(EditorPage::id).collect()
    }

    // ---- close -----------------------------------------------------------

    fn on_close_page(&self, view: &adw::TabView, page: &adw::TabPage) -> glib::Propagation {
        let editor = self.editor_of(page);
        if editor.skip_close_confirm() {
            return glib::Propagation::Proceed;
        }
        if !editor.is_scratch() {
            // A named tab's .sql stays on disk; the "+" menu is the way back to it.
            self.store().save(&editor.snapshot_record(), &editor.text());
            return glib::Propagation::Proceed;
        }
        if editor.text().trim().is_empty() {
            self.store().delete(&editor.snapshot_record());
            return glib::Propagation::Proceed;
        }
        // A browse buffer nobody typed into holds nothing a click cannot regenerate.
        if editor.subject().is_some() && !editor.is_dirty() {
            self.store().delete(&editor.snapshot_record());
            return glib::Propagation::Proceed;
        }
        // The ONLY action that destroys typed SQL, so the only one that confirms.
        let dialog = adw::AlertDialog::new(
            Some("Discard This Scratch Tab?"),
            Some("Its SQL has never been named or saved. Closing the tab discards it permanently."),
        );
        dialog.add_responses(&[("cancel", "Keep Editing"), ("discard", "Discard")]);
        dialog.set_response_appearance("discard", adw::ResponseAppearance::Destructive);
        dialog.set_default_response(Some("cancel"));
        dialog.set_close_response("cancel");
        dialog.choose(
            self,
            gio::Cancellable::NONE,
            glib::clone!(
                #[weak(rename_to = tabs)]
                self,
                #[weak]
                view,
                #[weak]
                page,
                move |response: glib::GString| {
                    let discard = response == "discard";
                    if discard {
                        let editor = tabs.editor_of(&page);
                        tabs.store().delete(&editor.snapshot_record());
                    }
                    view.close_page_finish(&page, discard);
                }
            ),
        );
        glib::Propagation::Stop
    }

    // ---- naming / saving -------------------------------------------------

    fn save_target(&self) {
        let Some(page) = self.target_page() else {
            return;
        };
        let editor = self.editor_of(&page);
        if !editor.is_scratch() {
            self.store().save(&editor.snapshot_record(), &editor.text());
            editor.mark_saved();
            return;
        }
        let dialog = adw::AlertDialog::new(
            Some("Name This Query"),
            Some("Named queries keep their .sql on disk after the tab closes; the \"+\" menu reopens them."),
        );
        let entry = gtk::Entry::new();
        entry.set_placeholder_text(Some("a name for this query"));
        entry.set_activates_default(true);
        dialog.set_extra_child(Some(&entry));
        dialog.add_responses(&[("cancel", "Cancel"), ("save", "Save")]);
        dialog.set_response_appearance("save", adw::ResponseAppearance::Suggested);
        dialog.set_default_response(Some("save"));
        dialog.set_close_response("cancel");
        dialog.choose(
            self,
            gio::Cancellable::NONE,
            glib::clone!(
                #[weak(rename_to = tabs)]
                self,
                #[weak]
                editor,
                #[weak]
                entry,
                move |response: glib::GString| {
                    let name = entry.text().trim().to_string();
                    if response != "save" || name.is_empty() {
                        return;
                    }
                    // New basename first, old pair dropped after — never the reverse.
                    let old = editor.snapshot_record();
                    editor.set_name(&name);
                    let store = tabs.store();
                    store.save(&editor.snapshot_record(), &editor.text());
                    if old.basename() != editor.snapshot_record().basename() {
                        store.delete(&old);
                    }
                    editor.mark_saved();
                    tabs.rebuild_saved_menu();
                    tabs.schedule_flush();
                }
            ),
        );
    }

    // ---- chrome ----------------------------------------------------------

    fn connection_info(&self, name: &str) -> Option<Profile> {
        self.imp()
            .connections
            .borrow()
            .iter()
            .find(|c| c.name == name)
            .cloned()
    }

    fn update_page_chrome(&self, editor: &EditorPage) {
        let page = self.page_of(editor);
        page.set_title(&editor.display_title());
        page.set_needs_attention(editor.is_dirty());

        let binding = editor.connection_binding();
        let window = self.imp().window_connection.borrow().clone();
        let effective = binding.clone().or(window);
        let info = effective.as_deref().and_then(|n| self.connection_info(n));
        let dark = adw::StyleManager::default().is_dark();

        page.set_icon(
            info.as_ref()
                .map(|i| engine::icon(&i.driver, dark))
                .as_ref(),
        );

        let mut tooltip = glib::markup_escape_text(&editor.display_title()).to_string();
        match &effective {
            Some(name) => {
                let how = if binding.is_some() {
                    "bound to"
                } else {
                    "following the window connection,"
                };
                tooltip.push_str(&format!(" — {how} {}", glib::markup_escape_text(name)));
            }
            None => tooltip.push_str(" — no connection selected"),
        }
        if editor.is_dirty() {
            tooltip.push_str("\nUnsaved changes");
        }

        // Colour is never the only channel: marked/read-only also gets an icon and words.
        let (indicator, indicator_tip) = match &info {
            Some(i) if i.color.is_some() => {
                let colour = i.color.clone().unwrap_or_default();
                let mut tip = format!(
                    "Marked connection ({colour}): {}",
                    glib::markup_escape_text(&i.name)
                );
                if i.read_only {
                    tip.push_str(" — read-only");
                }
                (Some("emblem-important-symbolic"), tip)
            }
            Some(i) if i.read_only => (
                Some("changes-prevent-symbolic"),
                format!(
                    "Read-only connection: {} refuses writes",
                    glib::markup_escape_text(&i.name)
                ),
            ),
            _ => (None, String::new()),
        };
        match indicator {
            Some(icon_name) => {
                page.set_indicator_icon(Some(&gio::ThemedIcon::new(icon_name)));
                page.set_indicator_tooltip(&indicator_tip);
                tooltip.push('\n');
                tooltip.push_str(&indicator_tip);
            }
            None => page.set_indicator_icon(gio::Icon::NONE),
        }
        page.set_tooltip(&tooltip);
    }

    // ---- menus -----------------------------------------------------------

    fn rebuild_saved_menu(&self) {
        let menu = &self.imp().saved_menu;
        menu.remove_all();
        let open: Vec<String> = self.editors().iter().map(EditorPage::id).collect();
        let section = gio::Menu::new();
        for record in self.store().all_records() {
            let Some(name) = record.name.clone().filter(|n| !n.is_empty()) else {
                continue;
            };
            if open.contains(&record.id) {
                continue;
            }
            let item = gio::MenuItem::new(Some(&name), None);
            item.set_action_and_target_value(
                Some("tabs.open-saved"),
                Some(&record.id.to_variant()),
            );
            section.append_item(&item);
        }
        if section.n_items() == 0 {
            section.append(Some("No Saved Queries"), Some("tabs.unavailable"));
        }
        menu.append_section(Some("Saved Queries"), &section);
    }

    fn build_page_menu(&self, view: &adw::TabView, page: &adw::TabPage) {
        let editor = self.editor_of(page);
        if let Some(bind) = self.imp().bind_action.get() {
            bind.set_state(&editor.connection_binding().unwrap_or_default().to_variant());
        }

        let menu = gio::Menu::new();
        let run_against = gio::Menu::new();
        let follow = gio::MenuItem::new(Some("Window Connection"), None);
        follow.set_action_and_target_value(Some("tabs.bind"), Some(&"".to_variant()));
        run_against.append_item(&follow);
        for conn in self.imp().connections.borrow().iter() {
            let label = format!("{}  ·  {}", conn.name, engine::display_name(&conn.driver));
            let item = gio::MenuItem::new(Some(&label), None);
            item.set_action_and_target_value(Some("tabs.bind"), Some(&conn.name.to_variant()));
            run_against.append_item(&item);
        }
        menu.append_submenu(Some("Run Against"), &run_against);

        let section = gio::Menu::new();
        let save_label = if editor.is_scratch() {
            "Name…"
        } else {
            "Save"
        };
        section.append(Some(save_label), Some("tabs.save"));
        section.append(Some("Close"), Some("tabs.close"));
        menu.append_section(None, &section);

        view.set_menu_model(Some(&menu));
    }

    // ---- state from the shell --------------------------------------------

    pub fn set_connections(&self, connections: &[Profile]) {
        self.imp().connections.replace(connections.to_vec());
        // Prune only against an authoritative, non-empty list; the files stay
        if !connections.is_empty() {
            for editor in self.editors() {
                if let Some(bound) = editor.connection_binding() {
                    if !connections.iter().any(|c| c.name == bound) {
                        self.store().save(&editor.snapshot_record(), &editor.text());
                        editor.set_skip_close_confirm();
                        self.imp().tab_view.close_page(&self.page_of(&editor));
                    }
                }
            }
        }
        for editor in self.editors() {
            self.update_page_chrome(&editor);
        }
    }

    /// Re-resolve every page's icon against the palette now in effect.
    pub fn refresh_chrome(&self) {
        for editor in self.editors() {
            self.update_page_chrome(&editor);
        }
    }

    pub fn set_window_connection(&self, connection: Option<&str>) {
        self.imp()
            .window_connection
            .replace(connection.map(str::to_string));
        for editor in self.editors() {
            if editor.connection_binding().is_none() {
                self.update_page_chrome(&editor);
            }
        }
        self.update_welcome();
        self.announce_active();
        self.schedule_flush();
    }

    /// What the last session's NEW tabs were created for — seeds the shell's sidebar selection.
    pub fn restored_window_connection(&self) -> Option<String> {
        self.imp().window_connection.borrow().clone()
    }

    // ---- persistence ------------------------------------------------------

    fn store(&self) -> &SavedQueryStore {
        self.imp().store.get().unwrap()
    }

    fn restore(&self) {
        self.imp().restoring.set(true);
        let loaded = self.store().load();
        self.imp()
            .window_connection
            .replace(loaded.session.active_connection.clone());
        for tab in &loaded.tabs {
            self.add_editor_page(tab.record.clone(), &tab.text);
        }
        if let Some(active) = &loaded.session.active_id {
            if let Some(editor) = self.editors().iter().find(|e| &e.id() == active) {
                self.imp().tab_view.set_selected_page(&self.page_of(editor));
            }
        }
        self.imp().restoring.set(false);
    }

    fn schedule_flush(&self) {
        if self.imp().restoring.get() || self.imp().flush_source.borrow().is_some() {
            return;
        }
        let source = glib::timeout_add_local_once(
            Duration::from_millis(700),
            glib::clone!(
                #[weak(rename_to = tabs)]
                self,
                move || {
                    tabs.imp().flush_source.take();
                    tabs.flush(false);
                }
            ),
        );
        self.imp().flush_source.replace(Some(source));
    }

    fn flush(&self, everything: bool) {
        let store = self.store();
        for editor in self.editors() {
            if editor.take_pending_save() || everything {
                store.save(&editor.snapshot_record(), &editor.text());
            }
        }
        store.save_session(&self.session_snapshot());
    }

    fn session_snapshot(&self) -> crate::store::EditorSession {
        crate::store::EditorSession {
            order: self.editors().iter().map(EditorPage::id).collect(),
            active_id: self.active_editor().map(|e| e.id()),
            active_connection: self.imp().window_connection.borrow().clone(),
        }
    }

    /// Everything to disk now, cursor positions included; the shell calls this on window close.
    pub fn persist_all(&self) {
        if let Some(source) = self.imp().flush_source.take() {
            source.remove();
        }
        self.flush(true);
    }

    // ---- empty state ------------------------------------------------------

    fn update_empty_state(&self) {
        let empty = self.imp().tab_view.n_pages() == 0;
        self.imp()
            .stack
            .set_visible_child_name(if empty { "welcome" } else { "editors" });
    }

    fn update_welcome(&self) {
        let Some(welcome) = self.imp().welcome.get() else {
            return;
        };
        let description = match &*self.imp().window_connection.borrow() {
            Some(conn) => format!(
                "Ctrl+T opens a new SQL editor for {}. Every editor stays in this bar, \
                 whatever connection it targets, and Ctrl+Return runs the statement under the caret.",
                glib::markup_escape_text(conn)
            ),
            None => "Add a connection, or pick one in the sidebar, then Ctrl+T opens an editor. \
                     Every editor stays in this bar, whatever connection it targets."
                .to_string(),
        };
        welcome.set_description(Some(&description));
    }
}
