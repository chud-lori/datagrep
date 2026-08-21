use adw::prelude::*;
use adw::subclass::prelude::*;
use gtk::glib;
use sourceview5::prelude::*;

use crate::sql;
use crate::store::SavedQueryRecord;

mod imp {
    use std::cell::{Cell, OnceCell, RefCell};
    use std::sync::OnceLock;

    use glib::subclass::Signal;

    use super::*;

    #[derive(Default)]
    pub struct EditorPage {
        pub buffer: OnceCell<sourceview5::Buffer>,
        pub view: OnceCell<sourceview5::View>,
        pub record: RefCell<SavedQueryRecord>,
        pub untitled_number: Cell<u32>,
        pub dirty: Cell<bool>,
        pub pending_save: Cell<bool>,
        pub loading: Cell<bool>,
        pub skip_close_confirm: Cell<bool>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for EditorPage {
        const NAME: &'static str = "DgEditorPage";
        type Type = super::EditorPage;
        type ParentType = adw::Bin;
    }

    impl ObjectImpl for EditorPage {
        fn signals() -> &'static [Signal] {
            static SIGNALS: OnceLock<Vec<Signal>> = OnceLock::new();
            SIGNALS.get_or_init(|| {
                vec![
                    // (sql, `-- @connection` value or "") — precedence resolves upstream.
                    Signal::builder("run-requested")
                        .param_types([String::static_type(), String::static_type()])
                        .build(),
                    Signal::builder("modified").build(),
                ]
            })
        }

        fn constructed(&self) {
            self.parent_constructed();
            let obj = self.obj();

            let buffer = sourceview5::Buffer::new(None);
            buffer.set_highlight_syntax(true);
            if let Some(lang) = sourceview5::LanguageManager::default().language("sql") {
                buffer.set_language(Some(&lang));
            }
            buffer.connect_changed(glib::clone!(
                #[weak]
                obj,
                move |_| {
                    if obj.imp().loading.get() {
                        return;
                    }
                    obj.imp().dirty.set(true);
                    obj.imp().pending_save.set(true);
                    obj.emit_by_name::<()>("modified", &[]);
                }
            ));

            let view = sourceview5::View::with_buffer(&buffer);
            view.set_monospace(true);
            view.set_show_line_numbers(true);
            view.set_auto_indent(true);
            view.set_tab_width(4);
            view.set_highlight_current_line(true);
            view.set_left_margin(6);
            view.set_right_margin(6);
            view.set_top_margin(6);
            view.set_bottom_margin(6);

            let run = gtk::CallbackAction::new(glib::clone!(
                #[weak]
                obj,
                #[upgrade_or]
                glib::Propagation::Proceed,
                move |_, _| {
                    obj.run_statement();
                    glib::Propagation::Stop
                }
            ));
            let shortcuts = gtk::ShortcutController::new();
            shortcuts.add_shortcut(gtk::Shortcut::new(
                gtk::ShortcutTrigger::parse_string("<Control>Return|<Control>KP_Enter"),
                Some(run),
            ));
            view.add_controller(shortcuts);

            obj.set_child(Some(
                &gtk::ScrolledWindow::builder()
                    .child(&view)
                    .hexpand(true)
                    .vexpand(true)
                    .build(),
            ));
            self.buffer.set(buffer).unwrap();
            self.view.set(view).unwrap();
            obj.apply_scheme(adw::StyleManager::default().is_dark());
        }
    }

    impl WidgetImpl for EditorPage {}
    impl BinImpl for EditorPage {}
}

glib::wrapper! {
    pub struct EditorPage(ObjectSubclass<imp::EditorPage>)
        @extends adw::Bin, gtk::Widget,
        @implements gtk::Accessible, gtk::Buildable, gtk::ConstraintTarget;
}

impl EditorPage {
    pub fn new(record: SavedQueryRecord, text: &str, untitled_number: u32) -> Self {
        let page: Self = glib::Object::new();
        let imp = page.imp();
        imp.untitled_number.set(untitled_number);
        imp.dirty.set(record.is_dirty);
        imp.loading.set(true);
        let buffer = imp.buffer.get().unwrap();
        buffer.set_text(text);
        let max = buffer.char_count();
        let start = buffer.iter_at_offset((record.cursor_location as i32).clamp(0, max));
        let end = buffer
            .iter_at_offset(((record.cursor_location + record.cursor_length) as i32).clamp(0, max));
        buffer.select_range(&start, &end);
        imp.loading.set(false);
        imp.record.replace(record);
        page
    }

    pub fn id(&self) -> String {
        self.imp().record.borrow().id.clone()
    }

    pub fn name(&self) -> Option<String> {
        self.imp().record.borrow().name.clone()
    }

    pub fn set_name(&self, name: &str) {
        self.imp().record.borrow_mut().name = Some(name.to_string());
    }

    pub fn connection_binding(&self) -> Option<String> {
        self.imp()
            .record
            .borrow()
            .connection
            .clone()
            .filter(|c| !c.is_empty())
    }

    pub fn set_connection_binding(&self, connection: Option<&str>) {
        self.imp().record.borrow_mut().connection = connection.map(str::to_string);
        self.imp().pending_save.set(true);
    }

    pub fn untitled_number(&self) -> u32 {
        self.imp().untitled_number.get()
    }

    pub fn is_scratch(&self) -> bool {
        self.imp().record.borrow().is_scratch()
    }

    pub fn is_dirty(&self) -> bool {
        self.imp().dirty.get()
    }

    pub fn mark_saved(&self) {
        self.imp().dirty.set(false);
        self.emit_by_name::<()>("modified", &[]);
    }

    pub fn take_pending_save(&self) -> bool {
        self.imp().pending_save.replace(false)
    }

    pub fn display_title(&self) -> String {
        match self.name() {
            Some(name) if !name.is_empty() => name,
            _ => match self.imp().untitled_number.get() {
                0 => "Untitled".to_string(),
                n => format!("Untitled {n}"),
            },
        }
    }

    pub fn set_text(&self, text: &str) {
        self.buffer().set_text(text);
    }

    pub fn text(&self) -> String {
        let buffer = self.buffer();
        buffer
            .text(&buffer.start_iter(), &buffer.end_iter(), true)
            .to_string()
    }

    /// The record as it should hit disk right now: current caret, current dirty flag.
    pub fn snapshot_record(&self) -> SavedQueryRecord {
        let buffer = self.buffer();
        let (start, end) = buffer.selection_bounds().unwrap_or_else(|| {
            let at = buffer.iter_at_mark(&buffer.get_insert());
            (at, at)
        });
        let mut record = self.imp().record.borrow().clone();
        record.cursor_location = start.offset() as i64;
        record.cursor_length = (end.offset() - start.offset()) as i64;
        record.is_dirty = self.imp().dirty.get();
        record
    }

    pub fn statement_under_cursor(&self) -> Option<sql::Block> {
        let buffer = self.buffer();
        let caret = buffer.iter_at_mark(&buffer.get_insert()).offset() as usize;
        sql::block_at(&self.text(), caret)
    }

    pub fn run_statement(&self) {
        if let Some(block) = self.statement_under_cursor() {
            let directive = block.directives.connection.unwrap_or_default();
            self.emit_by_name::<()>("run-requested", &[&block.text, &directive]);
        }
    }

    pub fn apply_scheme(&self, dark: bool) {
        let id = if dark { "Adwaita-dark" } else { "Adwaita" };
        let manager = sourceview5::StyleSchemeManager::default();
        // Adwaita ships with GtkSourceView >= 5.4; Classic is the never-absent fallback.
        if let Some(scheme) = manager
            .scheme(id)
            .or_else(|| manager.scheme(if dark { "classic-dark" } else { "classic" }))
        {
            self.buffer().set_style_scheme(Some(&scheme));
        }
    }

    pub fn grab_editor_focus(&self) {
        if let Some(view) = self.imp().view.get() {
            view.grab_focus();
        }
    }

    pub(crate) fn set_skip_close_confirm(&self) {
        self.imp().skip_close_confirm.set(true);
    }

    pub(crate) fn skip_close_confirm(&self) -> bool {
        self.imp().skip_close_confirm.get()
    }

    fn buffer(&self) -> &sourceview5::Buffer {
        self.imp().buffer.get().unwrap()
    }
}
