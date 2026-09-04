use std::cell::RefCell;

use adw::prelude::*;
use adw::subclass::prelude::*;
use gtk::glib;

use crate::model::format::count as format_count;
use crate::model::{QueryState, QueryStatus, ResultModel};

fn format_elapsed(ms: u64) -> String {
    if ms < 1000 {
        format!("{ms} ms")
    } else {
        format!("{:.2} s", ms as f64 / 1000.0)
    }
}

/// A partial result must never print a count that looks final.
fn row_count_text(status: &QueryStatus) -> String {
    if let Some(affected) = status.affected_rows {
        return format!("{} rows affected", format_count(affected));
    }
    match status.state {
        QueryState::Capped => format!("first {} rows", format_count(status.rows_loaded)),
        QueryState::Streaming | QueryState::Parked => {
            format!("{} rows so far…", format_count(status.rows_loaded))
        }
        _ if !status.total_known => format!("≥ {} rows", format_count(status.rows_loaded)),
        _ => format!("{} rows", format_count(status.rows_loaded)),
    }
}

fn state_text(state: QueryState) -> &'static str {
    match state {
        QueryState::Streaming => "running",
        QueryState::Parked => "parked",
        QueryState::Capped => "capped",
        QueryState::Done => "done",
        QueryState::Cancelled => "cancelled",
        QueryState::Failed => "failed",
    }
}

fn chip() -> gtk::Label {
    let label = gtk::Label::new(None);
    label.add_css_class("caption");
    label.add_css_class("dim-label");
    label.set_xalign(0.0);
    label
}

mod imp {
    use super::*;
    use glib::subclass::Signal;
    use std::sync::OnceLock;

    #[derive(Default)]
    pub struct StatusBar {
        pub state: RefCell<Option<gtk::Label>>,
        pub rows: RefCell<Option<gtk::Label>>,
        pub elapsed: RefCell<Option<gtk::Label>>,
        pub message: RefCell<Option<gtk::Label>>,
        pub cancel: RefCell<Option<gtk::Button>>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for StatusBar {
        const NAME: &'static str = "DgStatusBar";
        type Type = super::StatusBar;
        type ParentType = adw::Bin;
    }

    impl ObjectImpl for StatusBar {
        fn signals() -> &'static [Signal] {
            static SIGNALS: OnceLock<Vec<Signal>> = OnceLock::new();
            SIGNALS.get_or_init(|| vec![Signal::builder("cancel-requested").build()])
        }

        fn constructed(&self) {
            self.parent_constructed();
            let (state, rows, elapsed) = (chip(), chip(), chip());
            let message = chip();
            message.set_hexpand(true);
            message.set_ellipsize(gtk::pango::EllipsizeMode::End);

            let cancel = gtk::Button::with_label("Cancel");
            cancel.add_css_class("flat");
            cancel.set_visible(false);
            let bar = self.obj().downgrade();
            cancel.connect_clicked(move |_| {
                if let Some(bar) = bar.upgrade() {
                    bar.emit_by_name::<()>("cancel-requested", &[]);
                }
            });

            let box_ = gtk::Box::new(gtk::Orientation::Horizontal, 12);
            box_.add_css_class("toolbar");
            for chip in [&state, &rows, &elapsed, &message] {
                box_.append(chip);
            }
            box_.append(&cancel);
            self.obj().set_child(Some(&box_));

            *self.state.borrow_mut() = Some(state);
            *self.rows.borrow_mut() = Some(rows);
            *self.elapsed.borrow_mut() = Some(elapsed);
            *self.message.borrow_mut() = Some(message);
            *self.cancel.borrow_mut() = Some(cancel);
        }
    }

    impl WidgetImpl for StatusBar {}
    impl BinImpl for StatusBar {}
}

glib::wrapper! {
    pub struct StatusBar(ObjectSubclass<imp::StatusBar>)
        @extends adw::Bin, gtk::Widget,
        @implements gtk::Accessible, gtk::Buildable, gtk::ConstraintTarget;
}

impl Default for StatusBar {
    fn default() -> Self {
        Self::new()
    }
}

impl StatusBar {
    pub fn new() -> Self {
        glib::Object::new()
    }

    pub fn bind(&self, model: &ResultModel) {
        let bar = self.downgrade();
        model.connect_status_changed(move |model| {
            if let Some(bar) = bar.upgrade() {
                model.with_status(|status| bar.render(status));
            }
        });
        model.with_status(|status| self.render(status));
    }

    /// Free-text line: errors, hints, the sentence a click just produced.
    pub fn say(&self, message: &str, error: bool) {
        let Some(label) = self.imp().message.borrow().clone() else {
            return;
        };
        label.set_text(message);
        if error {
            label.add_css_class("error");
        } else {
            label.remove_css_class("error");
        }
    }

    /// What `say` last put on the line, so a tab switch can put it back.
    pub fn spoken(&self) -> (String, bool) {
        match self.imp().message.borrow().as_ref() {
            Some(label) => (label.text().to_string(), label.has_css_class("error")),
            None => (String::new(), false),
        }
    }

    fn render(&self, status: &QueryStatus) {
        let imp = self.imp();
        if let Some(label) = imp.state.borrow().as_ref() {
            label.set_text(state_text(status.state));
        }
        if let Some(label) = imp.rows.borrow().as_ref() {
            label.set_text(&row_count_text(status));
        }
        if let Some(label) = imp.elapsed.borrow().as_ref() {
            label.set_text(&format_elapsed(status.elapsed_ms));
        }
        if let Some(button) = imp.cancel.borrow().as_ref() {
            button.set_visible(status.is_streaming());
        }
        match (&status.safety, &status.error) {
            // The ladder refused and nothing was sent; a raw challenge id helps nobody.
            (Some(refusal), _) => self.say(
                &format!("{} Run it again to be asked.", refusal.body()),
                true,
            ),
            (None, Some(error)) => self.say(error, true),
            _ => {}
        }
    }

    pub fn connect_cancel_requested<F: Fn(&Self) + 'static>(&self, f: F) -> glib::SignalHandlerId {
        self.connect_local("cancel-requested", false, move |values| {
            let bar = values[0].get::<Self>().expect("the signal carries the bar");
            f(&bar);
            None
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn status(json: &str) -> QueryStatus {
        QueryStatus::parse(json)
    }

    #[test]
    fn a_capped_result_never_prints_a_final_looking_count() {
        assert_eq!(
            row_count_text(&status(r#"{"state":"capped","rows_loaded":12000}"#)),
            "first 12,000 rows"
        );
    }

    #[test]
    fn a_streaming_result_says_so_far() {
        assert_eq!(
            row_count_text(&status(r#"{"state":"streaming","rows_loaded":512}"#)),
            "512 rows so far…"
        );
    }

    #[test]
    fn a_finished_result_with_no_known_total_stays_a_lower_bound() {
        assert_eq!(
            row_count_text(&status(
                r#"{"state":"done","rows_loaded":7,"total_known":false}"#
            )),
            "≥ 7 rows"
        );
        assert_eq!(
            row_count_text(&status(
                r#"{"state":"done","rows_loaded":7,"total_known":true}"#
            )),
            "7 rows"
        );
    }

    #[test]
    fn a_write_reports_affected_rows_rather_than_a_fetched_count() {
        assert_eq!(
            row_count_text(&status(
                r#"{"state":"done","rows_loaded":0,"affected_rows":3}"#
            )),
            "3 rows affected"
        );
    }

    #[test]
    fn elapsed_switches_unit_at_a_second() {
        assert_eq!(format_elapsed(999), "999 ms");
        assert_eq!(format_elapsed(1500), "1.50 s");
    }
}
