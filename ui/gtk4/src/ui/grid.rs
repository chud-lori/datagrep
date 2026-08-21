use std::cell::{Cell, RefCell};

use adw::prelude::*;
use adw::subclass::prelude::*;
use gtk::glib;

use crate::ffi::CellKind;
use crate::model::{ResultModel, ResultRow};

const NULL_TEXT: &str = "NULL";
const NAT_CHARS: u32 = 18;

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

fn cell_factory(model: &ResultModel, col: u32, numeric: bool) -> gtk::SignalListItemFactory {
    let factory = gtk::SignalListItemFactory::new();
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
        item.set_child(Some(&cell));
    });

    let model = model.clone();
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
        model.with_cell(row.index(), col, |kind, text| match kind {
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
                vec![Signal::builder("sort-requested")
                    .param_types([String::static_type(), bool::static_type()])
                    .build()]
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
                    Some(cell_factory(model, col, numeric)),
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
