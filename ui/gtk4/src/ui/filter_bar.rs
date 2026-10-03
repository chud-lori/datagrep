use std::cell::RefCell;

use adw::prelude::*;
use adw::subclass::prelude::*;
use gtk::glib;

use crate::model::{FilterOperator, RowFilter};

/// One condition's widgets; its column list may carry a name the result no longer has.
pub struct ConditionRow {
    container: gtk::Box,
    columns: Vec<String>,
    column: gtk::DropDown,
    op: gtk::DropDown,
    value: gtk::Entry,
}

mod imp {
    use super::*;
    use glib::subclass::Signal;
    use std::sync::OnceLock;

    pub struct FilterBar {
        pub rows_box: gtk::Box,
        pub rows: RefCell<Vec<ConditionRow>>,
        pub columns: RefCell<Vec<String>>,
        pub operators: RefCell<Vec<FilterOperator>>,
    }

    impl Default for FilterBar {
        fn default() -> Self {
            Self {
                rows_box: gtk::Box::new(gtk::Orientation::Vertical, 4),
                rows: RefCell::new(Vec::new()),
                columns: RefCell::new(Vec::new()),
                operators: RefCell::new(Vec::new()),
            }
        }
    }

    #[glib::object_subclass]
    impl ObjectSubclass for FilterBar {
        const NAME: &'static str = "DgFilterBar";
        type Type = super::FilterBar;
        type ParentType = adw::Bin;
    }

    impl ObjectImpl for FilterBar {
        fn signals() -> &'static [Signal] {
            static SIGNALS: OnceLock<Vec<Signal>> = OnceLock::new();
            SIGNALS.get_or_init(|| {
                ["apply-requested", "clear-requested", "close-requested"]
                    .into_iter()
                    .map(|name| Signal::builder(name).build())
                    .collect()
            })
        }

        fn constructed(&self) {
            self.parent_constructed();

            let add = gtk::Button::from_icon_name("list-add-symbolic");
            add.add_css_class("flat");
            add.set_tooltip_text(Some("Add a condition (every condition must hold)"));
            let bar = self.obj().downgrade();
            add.connect_clicked(move |_| {
                if let Some(bar) = bar.upgrade() {
                    bar.add_condition(None);
                }
            });

            let spacer = gtk::Box::new(gtk::Orientation::Horizontal, 0);
            spacer.set_hexpand(true);
            let clear = gtk::Button::with_label("Clear");
            clear.set_tooltip_text(Some("Remove every filter and the header sort"));
            let apply = gtk::Button::with_label("Apply");
            apply.add_css_class("suggested-action");
            apply.set_tooltip_text(Some(
                "Re-run the statement with these conditions as its WHERE",
            ));
            let close = gtk::Button::from_icon_name("window-close-symbolic");
            close.add_css_class("flat");
            close.set_tooltip_text(Some("Close the filter bar and drop its filters"));

            let footer = gtk::Box::new(gtk::Orientation::Horizontal, 6);
            footer.append(&add);
            footer.append(&spacer);
            for (button, signal) in [
                (&clear, "clear-requested"),
                (&apply, "apply-requested"),
                (&close, "close-requested"),
            ] {
                footer.append(button);
                let (bar, signal) = (self.obj().downgrade(), signal.to_owned());
                button.connect_clicked(move |_| {
                    if let Some(bar) = bar.upgrade() {
                        bar.emit_by_name::<()>(&signal, &[]);
                    }
                });
            }

            let body = gtk::Box::new(gtk::Orientation::Vertical, 4);
            body.add_css_class("toolbar");
            body.append(&self.rows_box);
            body.append(&footer);
            self.obj().set_child(Some(&body));
            self.obj().set_visible(false);
        }
    }

    impl WidgetImpl for FilterBar {}
    impl BinImpl for FilterBar {}
}

glib::wrapper! {
    /// Column / operator / value conditions over the grid; Apply re-runs the statement with their WHERE.
    pub struct FilterBar(ObjectSubclass<imp::FilterBar>)
        @extends adw::Bin, gtk::Widget,
        @implements gtk::Accessible, gtk::Buildable, gtk::ConstraintTarget;
}

impl Default for FilterBar {
    fn default() -> Self {
        Self::new()
    }
}

impl FilterBar {
    pub fn new() -> Self {
        glib::Object::new()
    }

    /// The result's columns and the engine's operators; existing conditions are kept.
    pub fn set_choices(&self, columns: Vec<String>, operators: Vec<FilterOperator>) {
        let kept = self.filters();
        *self.imp().columns.borrow_mut() = columns;
        *self.imp().operators.borrow_mut() = operators;
        self.set_filters(&kept);
    }

    pub fn has_operators(&self) -> bool {
        !self.imp().operators.borrow().is_empty()
    }

    pub fn set_filters(&self, filters: &[RowFilter]) {
        let imp = self.imp();
        for row in imp.rows.borrow_mut().drain(..) {
            imp.rows_box.remove(&row.container);
        }
        for filter in filters {
            self.add_condition(Some(filter));
        }
    }

    /// Conditions with a column; a row whose column list is empty says nothing.
    pub fn filters(&self) -> Vec<RowFilter> {
        let operators = self.imp().operators.borrow();
        self.imp()
            .rows
            .borrow()
            .iter()
            .filter_map(|row| {
                let column = row.columns.get(row.column.selected() as usize)?.clone();
                let op = operators.get(row.op.selected() as usize)?.op.clone();
                Some(RowFilter {
                    column,
                    op,
                    value: row.value.text().to_string(),
                })
            })
            .collect()
    }

    pub fn is_empty(&self) -> bool {
        self.imp().rows.borrow().is_empty()
    }

    pub fn add_condition(&self, filter: Option<&RowFilter>) {
        let imp = self.imp();
        let operators = imp.operators.borrow().clone();
        let mut columns = imp.columns.borrow().clone();
        if let Some(name) = filter.map(|f| &f.column) {
            if !columns.contains(name) {
                columns.push(name.clone());
            }
        }

        let names: Vec<&str> = columns.iter().map(String::as_str).collect();
        let column = gtk::DropDown::from_strings(&names);
        column.set_enable_search(true);
        column.set_tooltip_text(Some("Column"));
        let labels: Vec<&str> = operators.iter().map(|o| o.label.as_str()).collect();
        let op = gtk::DropDown::from_strings(&labels);
        op.set_tooltip_text(Some("Operator"));
        let value = gtk::Entry::new();
        value.set_placeholder_text(Some("value"));
        value.set_hexpand(true);
        let remove = gtk::Button::from_icon_name("list-remove-symbolic");
        remove.add_css_class("flat");
        remove.set_tooltip_text(Some("Remove this condition"));

        if let Some(filter) = filter {
            if let Some(i) = columns.iter().position(|c| *c == filter.column) {
                column.set_selected(i as u32);
            }
            if let Some(i) = operators.iter().position(|o| o.op == filter.op) {
                op.set_selected(i as u32);
            }
            value.set_text(&filter.value);
        }
        let needs_value = move |selected: u32| {
            operators
                .get(selected as usize)
                .map_or(true, |o| o.needs_value)
        };
        value.set_visible(needs_value(op.selected()));
        let shown = value.clone();
        op.connect_selected_notify(move |op| shown.set_visible(needs_value(op.selected())));

        let bar = self.downgrade();
        value.connect_activate(move |_| {
            if let Some(bar) = bar.upgrade() {
                bar.emit_by_name::<()>("apply-requested", &[]);
            }
        });

        let container = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        container.append(&column);
        container.append(&op);
        container.append(&value);
        container.append(&remove);
        imp.rows_box.append(&container);

        let (bar, target) = (self.downgrade(), container.clone());
        remove.connect_clicked(move |_| {
            if let Some(bar) = bar.upgrade() {
                let imp = bar.imp();
                imp.rows.borrow_mut().retain(|row| row.container != target);
                imp.rows_box.remove(&target);
            }
        });
        imp.rows.borrow_mut().push(ConditionRow {
            container,
            columns,
            column,
            op,
            value,
        });
    }

    pub fn connect_apply_requested<F: Fn(&Self) + 'static>(&self, f: F) -> glib::SignalHandlerId {
        self.connect_signal("apply-requested", f)
    }

    pub fn connect_clear_requested<F: Fn(&Self) + 'static>(&self, f: F) -> glib::SignalHandlerId {
        self.connect_signal("clear-requested", f)
    }

    pub fn connect_close_requested<F: Fn(&Self) + 'static>(&self, f: F) -> glib::SignalHandlerId {
        self.connect_signal("close-requested", f)
    }

    fn connect_signal<F: Fn(&Self) + 'static>(&self, name: &str, f: F) -> glib::SignalHandlerId {
        self.connect_local(name, false, move |values| {
            let bar = values[0].get::<Self>().expect("the signal carries the bar");
            f(&bar);
            None
        })
    }
}
