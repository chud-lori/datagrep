use std::cell::{Cell, RefCell};

use adw::prelude::*;
use adw::subclass::prelude::*;
use gtk::glib;

use crate::ffi::CellKind;
use crate::model::{CellMark, MutationValue, ResultModel, ResultRow, StagedState};

const NULL_TEXT: &str = "NULL";
const NAT_CHARS: u32 = 18;
const STAGE_CLASSES: [&str; 4] = [
    "dg-staged",
    "dg-staged-delete",
    "dg-staged-written",
    "dg-staged-conflict",
];

/// One css class per staging state, so the tint is a stylesheet decision and not a hard-coded colour.
fn stage_class(mark: CellMark) -> Option<&'static str> {
    if !mark.staged {
        return None;
    }
    if mark.deleted {
        return Some("dg-staged-delete");
    }
    match mark.state {
        StagedState::Applied => mark.edited.then_some("dg-staged-written"),
        StagedState::Conflicted => mark.edited.then_some("dg-staged-conflict"),
        _ => mark.edited.then_some("dg-staged"),
    }
}

/// Into a caller-owned buffer: the gutter repaints on every scroll step.
fn decimal(mut value: u64, buf: &mut [u8; 20]) -> &str {
    let mut at = buf.len();
    loop {
        at -= 1;
        buf[at] = b'0' + (value % 10) as u8;
        value /= 10;
        if value == 0 {
            break;
        }
    }
    std::str::from_utf8(&buf[at..]).unwrap_or("")
}

fn is_numeric(ty: &str) -> bool {
    const TOKENS: [&str; 16] = [
        "int", "integer", "bigint", "smallint", "tinyint", "serial", "float", "double", "real",
        "decimal", "numeric", "number", "long", "short", "byte", "money",
    ];
    let ty = ty.to_ascii_lowercase();
    TOKENS.iter().any(|token| ty.contains(token))
}

fn paint_row_number(item: &gtk::ListItem, number: &gtk::Inscription) {
    let position = item.position();
    if position == gtk::INVALID_LIST_POSITION {
        number.set_text(None);
        return;
    }
    let mut buf = [0u8; 20];
    number.set_text(Some(decimal(u64::from(position) + 1, &mut buf)));
}

/// Renders `GtkListItem:position + 1` and reads nothing else — no route to result data.
fn gutter_factory() -> gtk::SignalListItemFactory {
    let factory = gtk::SignalListItemFactory::new();
    factory.connect_setup(|_, item| {
        let Some(item) = item.downcast_ref::<gtk::ListItem>() else {
            return;
        };
        let number = gtk::Inscription::builder().xalign(1.0).build();
        number.add_css_class("numeric");
        number.add_css_class("dim-label");
        item.set_child(Some(&number));
        item.set_activatable(false);
        item.set_selectable(false);
        item.connect_position_notify(move |item| paint_row_number(item, &number));
    });
    factory.connect_bind(|_, item| {
        let Some(item) = item.downcast_ref::<gtk::ListItem>() else {
            return;
        };
        if let Some(number) = item.child().and_downcast::<gtk::Inscription>() {
            paint_row_number(item, &number);
        }
    });
    factory
}

/// Hoisted out of the bind: a struck-through cell reuses this list rather than building one.
fn struck_through() -> gtk::pango::AttrList {
    let attributes = gtk::pango::AttrList::new();
    attributes.insert(gtk::pango::AttrInt::new_strikethrough(true));
    attributes
}

fn cell_factory(
    grid: &ResultsGrid,
    model: &ResultModel,
    col: u32,
    numeric: bool,
) -> gtk::SignalListItemFactory {
    let factory = gtk::SignalListItemFactory::new();
    let owner = grid.downgrade();
    factory.connect_setup(move |_, item| {
        let Some(item) = item.downcast_ref::<gtk::ListItem>() else {
            return;
        };
        let cell = gtk::Inscription::builder()
            .xalign(if numeric { 1.0 } else { 0.0 })
            .text_overflow(gtk::InscriptionOverflow::EllipsizeEnd)
            .nat_chars(NAT_CHARS)
            .build();
        if numeric {
            cell.add_css_class("numeric");
        }
        // Set up once per recycled widget, and weak both ways: the controller
        // outlives nothing it points at.
        let click = gtk::GestureClick::new();
        let (clicked, watched, anchor) = (owner.clone(), item.downgrade(), cell.downgrade());
        click.connect_pressed(move |_, presses, _, _| {
            let (Some(grid), Some(item), Some(anchor)) =
                (clicked.upgrade(), watched.upgrade(), anchor.upgrade())
            else {
                return;
            };
            let Some(row) = item.item().and_downcast::<ResultRow>() else {
                return;
            };
            grid.emit_by_name::<()>("cell-selected", &[&row.index(), &col]);
            if presses == 2 {
                grid.edit_cell(anchor.upcast_ref(), row.index(), col);
            }
        });
        cell.add_controller(click);

        let menu = gtk::GestureClick::new();
        menu.set_button(gtk::gdk::BUTTON_SECONDARY);
        let (raised, watched, anchor) = (owner.clone(), item.downgrade(), cell.downgrade());
        menu.connect_pressed(move |_, _, x, y| {
            let (Some(grid), Some(item), Some(anchor)) =
                (raised.upgrade(), watched.upgrade(), anchor.upgrade())
            else {
                return;
            };
            if let Some(row) = item.item().and_downcast::<ResultRow>() {
                grid.open_row_menu(anchor.upcast_ref(), row.index(), col, x, y);
            }
        });
        cell.add_controller(menu);
        item.set_child(Some(&cell));
    });

    let (model, struck) = (model.clone(), struck_through());
    factory.connect_bind(move |_, item| {
        let Some(item) = item.downcast_ref::<gtk::ListItem>() else {
            return;
        };
        let (Some(cell), Some(row)) = (
            item.child().and_downcast::<gtk::Inscription>(),
            item.item().and_downcast::<ResultRow>(),
        ) else {
            return;
        };
        model.with_cell(row.index(), col, |kind, text, mark| {
            for class in STAGE_CLASSES {
                cell.remove_css_class(class);
            }
            if let Some(class) = stage_class(mark) {
                cell.add_css_class(class);
            }
            // CSS text-decoration does not reach a GtkInscription's own text.
            cell.set_attributes(mark.deleted.then_some(&struck));
            // A staged value is data, however chrome the cell it was typed over was.
            match kind {
                _ if mark.edited => {
                    cell.remove_css_class("dim-label");
                    cell.set_text(Some(text));
                }
                CellKind::Value => {
                    cell.remove_css_class("dim-label");
                    cell.set_text(Some(text));
                }
                // Nested cells arrive as a summary ("{3 fields}") — chrome, whatever stands behind it.
                CellKind::Nested => {
                    cell.add_css_class("dim-label");
                    cell.set_text(Some(text));
                }
                CellKind::Null => {
                    cell.add_css_class("dim-label");
                    cell.set_text(Some(NULL_TEXT));
                }
                CellKind::Absent | CellKind::Pending => {
                    cell.add_css_class("dim-label");
                    cell.set_text(None);
                }
            }
        });
    });
    factory
}

mod imp {
    use super::*;
    use glib::subclass::Signal;
    use std::sync::OnceLock;

    pub struct ResultsGrid {
        pub model: RefCell<Option<ResultModel>>,
        pub view: gtk::ColumnView,
        pub gutter: gtk::ListView,
        pub header_height: gtk::SizeGroup,
        pub placeholder: adw::StatusPage,
        // Answers Equal for every pair, so it could not reorder this model even if asked to.
        pub inert_sorter: gtk::CustomSorter,
        pub built: Cell<u32>,
        pub sort: RefCell<Option<(String, bool)>>,
        pub restoring: Cell<bool>,
        // The cell the row menu was raised on, so "Edit Cell…" opens over it.
        pub menu_anchor: RefCell<Option<gtk::Widget>>,
    }

    impl Default for ResultsGrid {
        fn default() -> Self {
            Self {
                model: RefCell::new(None),
                view: gtk::ColumnView::new(None::<gtk::SelectionModel>),
                gutter: gtk::ListView::new(
                    None::<gtk::SelectionModel>,
                    None::<gtk::ListItemFactory>,
                ),
                header_height: gtk::SizeGroup::new(gtk::SizeGroupMode::Vertical),
                placeholder: adw::StatusPage::new(),
                inert_sorter: gtk::CustomSorter::new(|_, _| gtk::Ordering::Equal),
                built: Cell::new(0),
                sort: RefCell::new(None),
                restoring: Cell::new(false),
                menu_anchor: RefCell::new(None),
            }
        }
    }

    #[glib::object_subclass]
    impl ObjectSubclass for ResultsGrid {
        const NAME: &'static str = "DgResultsGrid";
        type Type = super::ResultsGrid;
        type ParentType = adw::Bin;
    }

    impl ObjectImpl for ResultsGrid {
        fn signals() -> &'static [Signal] {
            static SIGNALS: OnceLock<Vec<Signal>> = OnceLock::new();
            SIGNALS.get_or_init(|| {
                vec![
                    Signal::builder("sort-requested")
                        .param_types([String::static_type(), bool::static_type()])
                        .build(),
                    Signal::builder("cell-selected")
                        .param_types([u64::static_type(), u32::static_type()])
                        .build(),
                    Signal::builder("edit-refused")
                        .param_types([String::static_type()])
                        .build(),
                    Signal::builder("copied")
                        .param_types([String::static_type()])
                        .build(),
                ]
            })
        }

        fn constructed(&self) {
            self.parent_constructed();
            self.view.add_css_class("data-table");
            self.view.set_reorderable(false);
            self.view.set_show_column_separators(true);

            let scroller = gtk::ScrolledWindow::builder()
                .hscrollbar_policy(gtk::PolicyType::Automatic)
                .vscrollbar_policy(gtk::PolicyType::Automatic)
                .hexpand(true)
                .vexpand(true)
                .child(&self.view)
                .build();

            self.gutter.set_factory(Some(&gutter_factory()));
            self.gutter.add_css_class("dg-gutter");

            // The grid's vadjustment, its own scroller: tracks rows, outside anything horizontal.
            let gutter_scroller = gtk::ScrolledWindow::builder()
                .hscrollbar_policy(gtk::PolicyType::Never)
                .vscrollbar_policy(gtk::PolicyType::External)
                .vexpand(true)
                .vadjustment(&scroller.vadjustment())
                .child(&self.gutter)
                .build();

            // The header eats the top of the grid's scroller; the spacer equalises both viewports.
            let spacer = adw::Bin::new();
            spacer.add_css_class("dg-gutter-head");
            self.header_height.add_widget(&spacer);
            if let Some(header) = self.view.first_child() {
                self.header_height.add_widget(&header);
            }

            let gutter_column = gtk::Box::new(gtk::Orientation::Vertical, 0);
            gutter_column.add_css_class("dg-gutter-column");
            gutter_column.append(&spacer);
            gutter_column.append(&gutter_scroller);

            let body = gtk::Box::new(gtk::Orientation::Horizontal, 0);
            body.append(&gutter_column);
            body.append(&scroller);

            self.placeholder.set_icon_name(Some("view-list-symbolic"));
            self.placeholder.add_css_class("background");

            let overlay = gtk::Overlay::new();
            overlay.set_child(Some(&body));
            overlay.add_overlay(&self.placeholder);
            self.obj().set_child(Some(&overlay));

            self.add_editing_actions();

            if let Some(sorter) = self.view.sorter().and_downcast::<gtk::ColumnViewSorter>() {
                let grid = self.obj().downgrade();
                sorter.connect_changed(move |sorter, _| {
                    if let Some(grid) = grid.upgrade() {
                        grid.imp().on_header_clicked(sorter);
                    }
                });
            }
        }
    }

    impl WidgetImpl for ResultsGrid {}
    impl BinImpl for ResultsGrid {}

    impl ResultsGrid {
        /// The row menu's three items, reachable from any cell below this widget.
        fn add_editing_actions(&self) {
            let group = gio::SimpleActionGroup::new();

            let edit = gio::SimpleAction::new("edit", Some(&<(u64, u32)>::static_variant_type()));
            let grid = self.obj().downgrade();
            edit.connect_activate(move |_, target| {
                let (Some(grid), Some((row, col))) =
                    (grid.upgrade(), target.and_then(|t| t.get::<(u64, u32)>()))
                else {
                    return;
                };
                let anchor = grid.imp().menu_anchor.borrow().clone();
                // The menu popover is still down this turn; the editor needs its own.
                glib::idle_add_local_once(move || {
                    if let Some(anchor) = anchor {
                        grid.edit_cell(&anchor, row, col);
                    }
                });
            });
            group.add_action(&edit);

            for (name, delete) in [("delete", true), ("discard", false)] {
                let action = gio::SimpleAction::new(name, Some(&u64::static_variant_type()));
                let grid = self.obj().downgrade();
                action.connect_activate(move |_, target| {
                    let (Some(grid), Some(row)) =
                        (grid.upgrade(), target.and_then(|t| t.get::<u64>()))
                    else {
                        return;
                    };
                    grid.imp().stage_row(row, delete);
                });
                group.add_action(&action);
            }
            let copy_cell =
                gio::SimpleAction::new("copy-cell", Some(&<(u64, u32)>::static_variant_type()));
            let grid = self.obj().downgrade();
            copy_cell.connect_activate(move |_, target| {
                let (Some(grid), Some((row, col))) =
                    (grid.upgrade(), target.and_then(|t| t.get::<(u64, u32)>()))
                else {
                    return;
                };
                let text = grid.imp().model_text(|model| model.cell_text(row, col));
                grid.imp().copy(&text, "cell copied");
            });
            group.add_action(&copy_cell);

            let copy_row = gio::SimpleAction::new("copy-row", Some(&u64::static_variant_type()));
            let grid = self.obj().downgrade();
            copy_row.connect_activate(move |_, target| {
                let (Some(grid), Some(row)) = (grid.upgrade(), target.and_then(|t| t.get::<u64>()))
                else {
                    return;
                };
                let text = grid.imp().model_text(|model| model.row_text(row));
                grid.imp().copy(&text, "row copied");
            });
            group.add_action(&copy_row);

            let copy_selection = gio::SimpleAction::new("copy-selection", None);
            let grid = self.obj().downgrade();
            copy_selection.connect_activate(move |_, _| {
                if let Some(grid) = grid.upgrade() {
                    grid.imp().copy_selection();
                }
            });
            group.add_action(&copy_selection);

            self.obj().insert_action_group("results", Some(&group));

            let shortcuts = gtk::ShortcutController::new();
            shortcuts.set_scope(gtk::ShortcutScope::Local);
            shortcuts.add_shortcut(gtk::Shortcut::new(
                gtk::ShortcutTrigger::parse_string("<Control>c"),
                Some(gtk::NamedAction::new("results.copy-selection")),
            ));
            self.view.add_controller(shortcuts);
        }

        fn model_text(&self, read: impl Fn(&ResultModel) -> String) -> String {
            match self.model.borrow().as_ref() {
                Some(model) => read(model),
                None => String::new(),
            }
        }

        /// Every selected row, tab-separated like the Qt grid's Ctrl+C. The row
        /// numbers live in their own view, so they cannot be in this text.
        fn copy_selection(&self) {
            let Some(model) = self.model.borrow().clone() else {
                return;
            };
            let Some(selection) = self.view.model().map(|model| model.selection()) else {
                return;
            };
            let mut lines = Vec::new();
            for i in 0..selection.size() {
                lines.push(model.row_text(u64::from(selection.nth(i as u32))));
            }
            if lines.is_empty() {
                return self.obj().emit_by_name::<()>(
                    "copied",
                    &[&"select a row first — nothing was copied".to_string()],
                );
            }
            let said = match lines.len() {
                1 => "1 row copied".to_owned(),
                n => format!("{n} rows copied"),
            };
            self.copy(&lines.join("\n"), &said);
        }

        fn copy(&self, text: &str, said: &str) {
            let Some(display) = gtk::gdk::Display::default() else {
                return;
            };
            display.clipboard().set_text(text);
            self.obj()
                .emit_by_name::<()>("copied", &[&said.to_string()]);
        }

        fn stage_row(&self, row: u64, delete: bool) {
            let Some(model) = self.model.borrow().clone() else {
                return;
            };
            if !delete {
                model.discard_staged_row(row);
                return;
            }
            if let Err(why) = model.stage_delete(row) {
                self.obj().emit_by_name::<()>("edit-refused", &[&why]);
            }
        }

        pub(super) fn bind(&self, model: &ResultModel) {
            self.view
                .set_model(Some(&gtk::MultiSelection::new(Some(model.clone()))));
            self.gutter
                .set_model(Some(&gtk::NoSelection::new(Some(model.clone()))));
            *self.model.borrow_mut() = Some(model.clone());

            let grid = self.obj().downgrade();
            model.connect_columns_changed(move |model| {
                if let Some(grid) = grid.upgrade() {
                    grid.imp().rebuild_columns(model);
                }
            });
            let grid = self.obj().downgrade();
            model.connect_status_changed(move |model| {
                if let Some(grid) = grid.upgrade() {
                    grid.imp().refresh_placeholder(model);
                }
            });
            self.rebuild_columns(model);
        }

        /// Columns only grow rightwards, so a schema delta appends and set widths survive.
        fn rebuild_columns(&self, model: &ResultModel) {
            let wanted = model.column_count();
            if wanted < self.built.get() {
                while self.view.columns().n_items() > 0 {
                    if let Some(column) = self.view.columns().item(0).and_downcast() {
                        self.view.remove_column(&column);
                    }
                }
                self.built.set(0);
            }
            for col in self.built.get()..wanted {
                let Some(spec) = model.column(col) else {
                    break;
                };
                let numeric = is_numeric(&spec.ty);
                let column = gtk::ColumnViewColumn::new(
                    Some(&spec.name),
                    Some(cell_factory(&self.obj(), model, col, numeric)),
                );
                column.set_resizable(true);
                // Display state only: it makes the header clickable; the click re-issues SQL.
                column.set_sorter(Some(&self.inert_sorter));
                self.view.append_column(&column);
            }
            self.built.set(wanted);
            self.restore_indicator();
            self.refresh_placeholder(model);
        }

        fn on_header_clicked(&self, sorter: &gtk::ColumnViewSorter) {
            if self.restoring.get() {
                return;
            }
            let Some(name) = sorter.primary_sort_column().and_then(|c| c.title()) else {
                return;
            };
            let ascending = sorter.primary_sort_order() == gtk::SortType::Ascending;
            *self.sort.borrow_mut() = Some((name.to_string(), ascending));
            self.obj()
                .emit_by_name::<()>("sort-requested", &[&name.as_str(), &ascending]);
        }

        fn restore_indicator(&self) {
            let Some((name, ascending)) = self.sort.borrow().clone() else {
                return;
            };
            let columns = self.view.columns();
            let found = (0..columns.n_items())
                .filter_map(|i| columns.item(i).and_downcast::<gtk::ColumnViewColumn>())
                .find(|column| column.title().is_some_and(|t| t == name));
            let Some(column) = found else {
                return;
            };
            self.restoring.set(true);
            self.view.sort_by_column(
                Some(&column),
                if ascending {
                    gtk::SortType::Ascending
                } else {
                    gtk::SortType::Descending
                },
            );
            self.restoring.set(false);
        }

        fn refresh_placeholder(&self, model: &ResultModel) {
            let empty = model.column_count() == 0;
            self.placeholder.set_visible(empty);
            if !empty {
                return;
            }
            let ran = model.with_status(|s| s.state.is_terminal() || s.elapsed_ms > 0);
            if ran {
                self.placeholder.set_title("No rows returned");
                self.placeholder
                    .set_description(Some("The statement finished without a result set."));
            } else {
                self.placeholder.set_title("Nothing loaded yet");
                self.placeholder
                    .set_description(Some("Pick a connection and run a statement."));
            }
        }
    }
}

glib::wrapper! {
    pub struct ResultsGrid(ObjectSubclass<imp::ResultsGrid>)
        @extends adw::Bin, gtk::Widget,
        @implements gtk::Accessible, gtk::Buildable, gtk::ConstraintTarget;
}

impl ResultsGrid {
    pub fn new(model: &ResultModel) -> Self {
        let grid: Self = glib::Object::new();
        grid.imp().bind(model);
        grid
    }

    /// A header click: the column and the direction the re-issued statement should carry.
    pub fn connect_sort_requested<F: Fn(&Self, &str, bool) + 'static>(
        &self,
        f: F,
    ) -> glib::SignalHandlerId {
        self.connect_local("sort-requested", false, move |values| {
            let grid = values[0]
                .get::<Self>()
                .expect("the signal carries the grid");
            let column = values[1].get::<String>().unwrap_or_default();
            let ascending = values[2].get::<bool>().unwrap_or(true);
            f(&grid, &column, ascending);
            None
        })
    }

    /// A refused staging attempt, in the words the status bar shows.
    /// What a copy just put on the clipboard, for the status line.
    pub fn connect_copied<F: Fn(&Self, &str) + 'static>(&self, f: F) -> glib::SignalHandlerId {
        self.connect_local("copied", false, move |values| {
            let grid = values[0]
                .get::<Self>()
                .expect("the signal carries the grid");
            let message = values[1].get::<String>().unwrap_or_default();
            f(&grid, &message);
            None
        })
    }

    pub fn connect_edit_refused<F: Fn(&Self, &str) + 'static>(
        &self,
        f: F,
    ) -> glib::SignalHandlerId {
        self.connect_local("edit-refused", false, move |values| {
            let grid = values[0]
                .get::<Self>()
                .expect("the signal carries the grid");
            let message = values[1].get::<String>().unwrap_or_default();
            f(&grid, &message);
            None
        })
    }

    /// A cell click: the result row and the column, for the inspector to open.
    pub fn connect_cell_selected<F: Fn(&Self, u64, u32) + 'static>(
        &self,
        f: F,
    ) -> glib::SignalHandlerId {
        self.connect_local("cell-selected", false, move |values| {
            let grid = values[0]
                .get::<Self>()
                .expect("the signal carries the grid");
            let row = values[1].get::<u64>().unwrap_or_default();
            let column = values[2].get::<u32>().unwrap_or_default();
            f(&grid, row, column);
            None
        })
    }

    /// The editor is a popover on the cell itself, so what is being retyped stays in view.
    pub fn edit_cell(&self, anchor: &gtk::Widget, row: u64, col: u32) {
        let Some(model) = self.imp().model.borrow().clone() else {
            return;
        };
        if !model.is_editable_cell(row, col) {
            return;
        }
        let Some(field) = model.field_name(row, col) else {
            self.emit_by_name::<()>(
                "edit-refused",
                &[&"this column is not one of the fields the row was read under".to_owned()],
            );
            return;
        };
        let loaded = model.loaded_value(row, col);
        let entry = gtk::Entry::new();
        entry.set_text(&model.with_cell(row, col, |_, text, mark| {
            match mark.edited {
                true => text.to_owned(),
                false => loaded
                    .as_ref()
                    .map(MutationValue::display)
                    .unwrap_or_default(),
            }
        }));
        entry.set_activates_default(true);

        let title = gtk::Label::new(Some(&field));
        title.add_css_class("heading");
        title.add_css_class("monospace");
        title.set_xalign(0.0);
        let hint = gtk::Label::new(Some(&format!(
            "holds {}",
            loaded.as_ref().map_or("empty", MutationValue::type_name)
        )));
        hint.add_css_class("caption");
        hint.add_css_class("dim-label");
        hint.set_xalign(0.0);
        let why_not = gtk::Label::new(None);
        why_not.add_css_class("error");
        why_not.add_css_class("caption");
        why_not.set_wrap(true);
        why_not.set_xalign(0.0);
        why_not.set_visible(false);

        let cancel = gtk::Button::with_label("Cancel");
        let stage = gtk::Button::with_label("Stage Edit");
        stage.add_css_class("suggested-action");
        let buttons = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        buttons.set_halign(gtk::Align::End);
        buttons.append(&cancel);
        buttons.append(&stage);

        let body = gtk::Box::new(gtk::Orientation::Vertical, 6);
        body.set_margin_top(6);
        body.set_margin_bottom(6);
        body.set_margin_start(6);
        body.set_margin_end(6);
        for child in [
            title.upcast_ref::<gtk::Widget>(),
            entry.upcast_ref(),
            hint.upcast_ref(),
            why_not.upcast_ref(),
        ] {
            body.append(child);
        }
        body.append(&buttons);

        let popover = gtk::Popover::builder().child(&body).build();
        popover.set_parent(anchor);
        popover.connect_closed(|popover| popover.unparent());

        let commit = {
            let (grid, popover, entry, why_not) = (
                self.downgrade(),
                popover.clone(),
                entry.clone(),
                why_not.clone(),
            );
            move || {
                let (Some(grid), Some(model)) = (grid.upgrade(), Some(model.clone())) else {
                    return;
                };
                match model.stage_edit(row, col, &entry.text()) {
                    Ok(()) => popover.popdown(),
                    Err(why) => {
                        why_not.set_text(&why);
                        why_not.set_visible(true);
                        grid.emit_by_name::<()>("edit-refused", &[&why]);
                    }
                }
            }
        };
        let activate = commit.clone();
        entry.connect_activate(move |_| activate());
        stage.connect_clicked(move |_| commit());
        let closing = popover.clone();
        cancel.connect_clicked(move |_| closing.popdown());

        popover.popup();
        entry.grab_focus();
    }

    /// The same four choices the macOS and Qt grids offer, and no "retry as written".
    pub fn open_row_menu(&self, anchor: &gtk::Widget, row: u64, col: u32, x: f64, y: f64) {
        let Some(model) = self.imp().model.borrow().clone() else {
            return;
        };
        if model.column_count() == 0 {
            return;
        }
        let edits = model.edits();
        let menu = gio::Menu::new();
        // Targets are built as typed variants: a detailed name would infer (ii), not (tu).
        let item = |label: &str, action: &str, target: glib::Variant| {
            let item = gio::MenuItem::new(Some(label), None);
            item.set_action_and_target_value(Some(action), Some(&target));
            item
        };

        // Reading is never gated on the result being writable.
        let copy = gio::Menu::new();
        copy.append_item(&item(
            "Copy Cell",
            "results.copy-cell",
            (row, col).to_variant(),
        ));
        copy.append_item(&item("Copy Row", "results.copy-row", row.to_variant()));
        copy.append(Some("Copy Selected Rows"), Some("results.copy-selection"));
        menu.append_section(None, &copy);

        if model.editable().is_some() {
            let editing = gio::Menu::new();
            if model.is_editable_cell(row, col) {
                editing.append_item(&item("Edit Cell…", "results.edit", (row, col).to_variant()));
            }
            if edits.is_deleted(row) {
                editing.append_item(&item(
                    "Keep This Document",
                    "results.discard",
                    row.to_variant(),
                ));
            } else {
                editing.append_item(&item("Delete Document", "results.delete", row.to_variant()));
                if edits.is_staged(row) {
                    editing.append_item(&item(
                        "Discard Staged Changes",
                        "results.discard",
                        row.to_variant(),
                    ));
                }
            }
            menu.append_section(None, &editing);
        }
        *self.imp().menu_anchor.borrow_mut() = Some(anchor.clone());
        let popover = gtk::PopoverMenu::from_model(Some(&menu));
        popover.set_has_arrow(false);
        popover.set_halign(gtk::Align::Start);
        popover.set_pointing_to(Some(&gtk::gdk::Rectangle::new(x as i32, y as i32, 1, 1)));
        popover.set_parent(anchor);
        popover.connect_closed(|popover| popover.unparent());
        popover.popup();
    }

    /// The header arrow's column and direction, parked with its result.
    pub fn sort_indicator(&self) -> Option<(String, bool)> {
        self.imp().sort.borrow().clone()
    }

    /// Set before the columns rebuild: that rebuild is what re-draws the arrow.
    pub fn set_sort_indicator(&self, sort: Option<(String, bool)>) {
        *self.imp().sort.borrow_mut() = sort;
    }

    pub fn clear_sort_indicator(&self) {
        let imp = self.imp();
        *imp.sort.borrow_mut() = None;
        imp.restoring.set(true);
        imp.view
            .sort_by_column(None::<&gtk::ColumnViewColumn>, gtk::SortType::Ascending);
        imp.restoring.set(false);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn row_numbers_format_without_allocating() {
        let mut buf = [0u8; 20];
        assert_eq!(decimal(1, &mut buf), "1");
        assert_eq!(decimal(0, &mut buf), "0");
        assert_eq!(decimal(4821, &mut buf), "4821");
        assert_eq!(decimal(u64::MAX, &mut buf), "18446744073709551615");
    }

    #[test]
    fn numeric_columns_are_recognised_by_the_same_tokens_the_other_grids_use() {
        assert!(is_numeric("int8"));
        assert!(is_numeric("BIGINT"));
        assert!(is_numeric("numeric(10,2)"));
        assert!(!is_numeric("text"));
        assert!(!is_numeric("timestamptz"));
    }
}
