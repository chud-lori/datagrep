use std::cell::RefCell;

use adw::prelude::*;
use adw::subclass::prelude::*;
use gtk::glib;

use crate::model::{pretty_json, ObjectDetail};
use crate::ui::StatusBar;

const LEGEND: &str = "NULL — present, and null\n\
                      (empty) — present, empty string\n\
                      — — ABSENT: not in the document at all\n\
                      {n fields} — nested: click to open here";

const NOTHING_SELECTED: &str =
    "Select a table, view, collection or key in the sidebar to see its structure.";

fn monospace_view() -> gtk::TextView {
    let view = gtk::TextView::new();
    view.set_editable(false);
    view.set_monospace(true);
    view.set_cursor_visible(false);
    view.set_wrap_mode(gtk::WrapMode::None);
    view.set_left_margin(8);
    view.set_right_margin(8);
    view.set_top_margin(6);
    view.set_bottom_margin(6);
    view
}

fn caption(text: &str) -> gtk::Label {
    let label = gtk::Label::new(Some(text));
    label.add_css_class("caption");
    label.add_css_class("dim-label");
    label.set_xalign(0.0);
    label.set_wrap(true);
    label
}

// Titles are engine-supplied names: markup off, so a `<` in one is a `<`.
fn detail_row(title: &str, subtitle: &str) -> adw::ActionRow {
    let row = adw::ActionRow::builder()
        .title(title)
        .subtitle(subtitle)
        .use_markup(false)
        .build();
    row.set_subtitle_lines(0);
    row.set_title_lines(0);
    row
}

fn section(title: &str) -> adw::ExpanderRow {
    let row = adw::ExpanderRow::builder()
        .title(title)
        .use_markup(false)
        .build();
    row.set_expanded(true);
    row
}

mod imp {
    use super::*;

    pub struct Inspector {
        pub status: RefCell<Option<StatusBar>>,
        pub cell_subtitle: gtk::Label,
        pub cell_text: gtk::TextView,
        pub cell_copy: gtk::Button,
        pub schema_slot: adw::Bin,
    }

    impl Default for Inspector {
        fn default() -> Self {
            Self {
                status: RefCell::new(None),
                cell_subtitle: caption("nothing selected"),
                cell_text: monospace_view(),
                cell_copy: gtk::Button::from_icon_name("edit-copy-symbolic"),
                schema_slot: adw::Bin::new(),
            }
        }
    }

    #[glib::object_subclass]
    impl ObjectSubclass for Inspector {
        const NAME: &'static str = "DgInspector";
        type Type = super::Inspector;
        type ParentType = adw::Bin;
    }

    impl ObjectImpl for Inspector {
        fn constructed(&self) {
            self.parent_constructed();

            let body = gtk::Box::new(gtk::Orientation::Vertical, 18);
            body.set_margin_top(12);
            body.set_margin_bottom(12);
            body.set_margin_start(12);
            body.set_margin_end(12);
            body.append(&self.cell_group());
            body.append(&self.schema_slot);

            self.obj().set_child(Some(
                &gtk::ScrolledWindow::builder()
                    .hscrollbar_policy(gtk::PolicyType::Never)
                    .vexpand(true)
                    .child(&body)
                    .build(),
            ));
            self.obj().show_schema("", "", "");
            self.obj().clear_cell();
        }
    }

    impl WidgetImpl for Inspector {}
    impl BinImpl for Inspector {}

    impl Inspector {
        fn cell_group(&self) -> adw::PreferencesGroup {
            let group = adw::PreferencesGroup::builder().title("Cell").build();

            self.cell_copy.add_css_class("flat");
            self.cell_copy.set_tooltip_text(Some("Copy this JSON"));
            self.cell_copy.set_valign(gtk::Align::Center);
            let inspector = self.obj().downgrade();
            self.cell_copy.connect_clicked(move |_| {
                if let Some(inspector) = inspector.upgrade() {
                    inspector.imp().copy_cell();
                }
            });
            group.set_header_suffix(Some(&self.cell_copy));

            let frame = gtk::Frame::new(None);
            frame.set_child(Some(
                &gtk::ScrolledWindow::builder()
                    .min_content_height(120)
                    .max_content_height(280)
                    .propagate_natural_height(true)
                    .child(&self.cell_text)
                    .build(),
            ));

            let box_ = gtk::Box::new(gtk::Orientation::Vertical, 6);
            box_.append(&self.cell_subtitle);
            box_.append(&frame);
            box_.append(&caption(LEGEND));
            group.add(&box_);
            group
        }

        fn copy_cell(&self) {
            let buffer = self.cell_text.buffer();
            let text = buffer.text(&buffer.start_iter(), &buffer.end_iter(), false);
            if text.is_empty() {
                return;
            }
            if let Some(display) = gtk::gdk::Display::default() {
                display.clipboard().set_text(&text);
            }
            self.say(&format!("copied {} characters of JSON", text.len()));
        }

        pub(super) fn say(&self, message: &str) {
            if let Some(status) = self.status.borrow().as_ref() {
                status.say(message, false);
            }
        }
    }
}

glib::wrapper! {
    pub struct Inspector(ObjectSubclass<imp::Inspector>)
        @extends adw::Bin, gtk::Widget,
        @implements gtk::Accessible, gtk::Buildable, gtk::ConstraintTarget;
}

impl Default for Inspector {
    fn default() -> Self {
        Self::new()
    }
}

impl Inspector {
    pub fn new() -> Self {
        glib::Object::new()
    }

    pub fn set_status_bar(&self, status: &StatusBar) {
        *self.imp().status.borrow_mut() = Some(status.clone());
    }

    /// One describe payload, its failure, or the nothing-selected state.
    pub fn show_schema(&self, path_json: &str, detail_json: &str, error: &str) {
        let path: Vec<String> = serde_json::from_str(path_json).unwrap_or_default();
        let group = adw::PreferencesGroup::builder()
            .title(
                glib::markup_escape_text(path.last().map(String::as_str).unwrap_or("Schema"))
                    .as_str(),
            )
            .build();

        if path.is_empty() {
            group.set_description(Some(NOTHING_SELECTED));
            self.imp().schema_slot.set_child(Some(&group));
            return;
        }
        if !error.is_empty() {
            let parent = glib::markup_escape_text(&path[..path.len() - 1].join(" › "));
            group.set_description(Some(parent.as_str()));
            let row = detail_row("describe failed", error);
            row.add_css_class("error");
            group.add(&row);
            self.imp().schema_slot.set_child(Some(&group));
            return;
        }

        // No detail and no error is the state between the click and the answer.
        if detail_json.is_empty() {
            group.set_description(Some("reading this object…"));
            self.imp().schema_slot.set_child(Some(&group));
            return;
        }

        let detail = match ObjectDetail::parse(detail_json) {
            Ok(detail) => detail,
            Err(message) => {
                group.add(&detail_row("the object detail did not decode", &message));
                self.imp().schema_slot.set_child(Some(&group));
                return;
            }
        };

        let mut description = path[..path.len() - 1].join(" › ");
        let stats = detail.stats();
        if !stats.is_empty() {
            description = match description.is_empty() {
                true => stats,
                false => format!("{description}\n{stats}"),
            };
        }
        if let Some(comment) = detail.comment.as_deref().filter(|c| !c.is_empty()) {
            description.push('\n');
            description.push_str(comment);
        }
        group.set_description(Some(glib::markup_escape_text(&description).as_str()));

        // `[]` and null are two different sentences: none, and not reported.
        match detail.columns.as_deref() {
            Some([]) => group.add(&detail_row("Columns", "none")),
            Some(columns) => {
                let section = section(&format!("Columns ({})", columns.len()));
                for column in columns {
                    section.add_row(&detail_row(&column.name, &column.details()));
                }
                group.add(&section);
            }
            None => group.add(&detail_row("Columns", "not reported")),
        }
        match detail.indexes.as_deref() {
            Some([]) => group.add(&detail_row("Indexes", "none")),
            Some(indexes) => {
                let section = section(&format!("Indexes ({})", indexes.len()));
                for index in indexes {
                    let row = detail_row(&index.name, &index.details());
                    if let Some(definition) = index.definition.as_deref().filter(|d| !d.is_empty())
                    {
                        row.set_tooltip_text(Some(definition));
                    }
                    section.add_row(&row);
                }
                group.add(&section);
            }
            None => group.add(&detail_row("Indexes", "not reported")),
        }
        // Whatever else the driver reported, shown rather than dropped.
        if !detail.extra.is_empty() {
            let section = section("Extra");
            for (key, value) in &detail.extra {
                section.add_row(&detail_row(key, value));
            }
            group.add(&section);
        }

        self.imp().schema_slot.set_child(Some(&group));
    }

    pub fn show_cell(&self, row: u64, column: u32, name: &str, detail_json: &str, envelope: &str) {
        let imp = self.imp();
        let mut title = format!("row {} · column {}", row + 1, column + 1);
        if !name.is_empty() {
            title.push_str(" · ");
            title.push_str(name);
        }
        imp.cell_subtitle.set_text(&title);

        let value = match detail_json.is_empty() {
            true => "(no detail available)".to_owned(),
            false => pretty_json(detail_json),
        };
        let text = match envelope.is_empty() {
            true => value,
            false => format!(
                "// document\n{}\n\n// value\n{value}",
                pretty_json(envelope)
            ),
        };
        imp.cell_text.buffer().set_text(&text);
        imp.cell_copy.set_sensitive(true);
    }

    /// A new query invalidates every row and column the cell pane could be naming.
    pub fn clear_cell(&self) {
        let imp = self.imp();
        imp.cell_subtitle.set_text("nothing selected");
        imp.cell_text.buffer().set_text(
            "Click a cell in the grid to see its whole value — a {…} chip opens here on its own.",
        );
        imp.cell_copy.set_sensitive(false);
    }
}
