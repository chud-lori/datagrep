use std::cell::{Cell, RefCell};

use adw::prelude::*;
use adw::subclass::prelude::*;
use gtk::glib;

use crate::model::mutation::{MutationOutcome, MutationReport, MutationRow};
use crate::model::{PendingEdits, StagedCounts};

/// The sentence that has to be read before the click, not after it.
pub fn commit_warning(count: u32, atomic: bool) -> String {
    if atomic {
        return format!(
            "This connection applies the batch atomically: either all {count} are written, or \
             none are."
        );
    }
    if count == 1 {
        return "The document is written on its own. If it fails, nothing is written and the edit \
                stays staged for another try."
            .to_owned();
    }
    let example = count.min(3);
    let before = example - 1;
    let before = if before == 1 {
        "one".to_owned()
    } else {
        before.to_string()
    };
    format!(
        "{count} documents will be written one by one, and there is no transaction: if #{example} \
         fails, the {before} before it stay written and nothing is rolled back. The report then \
         names every document — written, refused, or never attempted — and anything not written \
         stays staged."
    )
}

pub fn report_headline(report: &MutationReport) -> String {
    let mut parts = vec![format!("{} applied", report.applied)];
    if report.failed > 0 {
        parts.push(match report.conflicts {
            0 => format!("{} failed", report.failed),
            conflicts => format!("{} failed ({conflicts} a version conflict)", report.failed),
        });
    }
    if report.not_attempted > 0 {
        parts.push(format!(
            "{} never attempted, still staged",
            report.not_attempted
        ));
    }
    parts.join(" · ")
}

fn wrapped(text: &str) -> gtk::Label {
    let label = gtk::Label::new(Some(text));
    label.set_wrap(true);
    label.set_xalign(0.0);
    label
}

fn headline(counts: StagedCounts) -> String {
    match (counts.pending, counts.written) {
        (0, 1) => "1 document written — the grid still shows what was loaded".to_owned(),
        (0, n) => format!("{n} documents written — the grid still shows what was loaded"),
        (1, _) => "1 document edited, not yet written".to_owned(),
        (n, _) => format!("{n} documents edited, not yet written"),
    }
}

fn detail(counts: StagedCounts) -> String {
    let mut parts = Vec::new();
    if counts.updates > 0 {
        parts.push(format!("{} to update", counts.updates));
    }
    if counts.deletes > 0 {
        parts.push(format!("{} to delete", counts.deletes));
    }
    if counts.written > 0 {
        parts.push(format!("{} already written", counts.written));
    }
    match counts.conflicts {
        0 => {}
        1 => parts.push("1 changed on the server".to_owned()),
        n => parts.push(format!("{n} changed on the server")),
    }
    parts.join(" · ")
}

mod imp {
    use super::*;
    use glib::subclass::Signal;
    use std::sync::OnceLock;

    pub struct StagedEditsBar {
        pub edits: RefCell<Option<PendingEdits>>,
        pub headline: gtk::Label,
        pub detail: gtk::Label,
        pub discard: gtk::Button,
        pub resolve: gtk::Button,
        pub commit: gtk::Button,
        pub reload: gtk::Button,
        pub spinner: gtk::Spinner,
        pub committing: Cell<bool>,
        pub rereading: Cell<bool>,
    }

    impl Default for StagedEditsBar {
        fn default() -> Self {
            Self {
                edits: RefCell::new(None),
                headline: gtk::Label::new(None),
                detail: gtk::Label::new(None),
                discard: gtk::Button::with_label("Discard"),
                resolve: gtk::Button::new(),
                commit: gtk::Button::new(),
                reload: gtk::Button::with_label("Reload"),
                spinner: gtk::Spinner::new(),
                committing: Cell::new(false),
                rereading: Cell::new(false),
            }
        }
    }

    #[glib::object_subclass]
    impl ObjectSubclass for StagedEditsBar {
        const NAME: &'static str = "DgStagedEditsBar";
        type Type = super::StagedEditsBar;
        type ParentType = adw::Bin;
    }

    impl ObjectImpl for StagedEditsBar {
        fn signals() -> &'static [Signal] {
            static SIGNALS: OnceLock<Vec<Signal>> = OnceLock::new();
            SIGNALS.get_or_init(|| {
                [
                    "commit-requested",
                    "discard-requested",
                    "resolve-requested",
                    "reload-requested",
                ]
                .into_iter()
                .map(|name| Signal::builder(name).build())
                .collect()
            })
        }

        fn constructed(&self) {
            self.parent_constructed();
            self.headline.add_css_class("heading");
            self.headline.set_xalign(0.0);
            self.detail.add_css_class("caption");
            self.detail.add_css_class("dim-label");
            self.detail.set_xalign(0.0);

            let text = gtk::Box::new(gtk::Orientation::Vertical, 1);
            text.set_hexpand(true);
            text.set_valign(gtk::Align::Center);
            text.append(&self.headline);
            text.append(&self.detail);

            self.commit.add_css_class("destructive-action");
            self.resolve.add_css_class("suggested-action");

            let bar = gtk::Box::new(gtk::Orientation::Horizontal, 8);
            bar.add_css_class("toolbar");
            bar.add_css_class("dg-staged-bar");
            bar.append(&self.spinner);
            bar.append(&text);
            for button in [&self.discard, &self.resolve, &self.commit, &self.reload] {
                bar.append(button);
            }
            self.obj().set_child(Some(&bar));
            self.obj().set_visible(false);

            for (button, signal) in [
                (&self.discard, "discard-requested"),
                (&self.resolve, "resolve-requested"),
                (&self.commit, "commit-requested"),
                (&self.reload, "reload-requested"),
            ] {
                let (bar, signal) = (self.obj().downgrade(), signal.to_owned());
                button.connect_clicked(move |_| {
                    if let Some(bar) = bar.upgrade() {
                        bar.emit_by_name::<()>(&signal, &[]);
                    }
                });
            }
        }
    }

    impl WidgetImpl for StagedEditsBar {}
    impl BinImpl for StagedEditsBar {}
}

glib::wrapper! {
    /// Everything staged and unwritten, and the only button that writes.
    pub struct StagedEditsBar(ObjectSubclass<imp::StagedEditsBar>)
        @extends adw::Bin, gtk::Widget,
        @implements gtk::Accessible, gtk::Buildable, gtk::ConstraintTarget;
}

impl Default for StagedEditsBar {
    fn default() -> Self {
        Self::new()
    }
}

impl StagedEditsBar {
    pub fn new() -> Self {
        glib::Object::new()
    }

    pub fn bind(&self, edits: &PendingEdits) {
        *self.imp().edits.borrow_mut() = Some(edits.clone());
        let bar = self.downgrade();
        edits.connect_staging_changed(move |_| {
            if let Some(bar) = bar.upgrade() {
                bar.refresh();
            }
        });
        self.refresh();
    }

    pub fn set_committing(&self, committing: bool) {
        self.imp().committing.set(committing);
        self.refresh();
    }

    pub fn set_rereading(&self, rereading: bool) {
        self.imp().rereading.set(rereading);
        self.refresh();
    }

    pub fn refresh(&self) {
        let imp = self.imp();
        let Some(edits) = imp.edits.borrow().clone() else {
            return;
        };
        if edits.is_empty() {
            self.set_visible(false);
            imp.spinner.set_spinning(false);
            return;
        }
        let counts = edits.counts();
        imp.headline.set_text(&headline(counts));
        let detail = detail(counts);
        imp.detail.set_text(&detail);
        imp.detail.set_visible(!detail.is_empty());

        let busy = imp.committing.get();
        imp.spinner.set_visible(busy);
        imp.spinner.set_spinning(busy);
        if busy {
            imp.headline.set_text("committing…");
            imp.detail.set_visible(false);
        }
        let idle = !busy && counts.pending > 0;
        imp.discard.set_visible(idle);
        imp.commit.set_visible(idle);
        imp.resolve.set_visible(idle && counts.conflicts > 0);
        imp.reload.set_visible(!busy && counts.pending == 0);
        if idle {
            imp.commit.set_label(&match counts.pending {
                1 => "Commit 1…".to_owned(),
                n => format!("Commit {n}…"),
            });
            imp.resolve.set_label(&match counts.conflicts {
                1 => "Resolve 1 Conflict…".to_owned(),
                n => format!("Resolve {n} Conflicts…"),
            });
            imp.resolve.set_sensitive(!imp.rereading.get());
        }
        self.set_visible(true);
    }

    pub fn connect_request<F: Fn(&Self) + 'static>(
        &self,
        signal: &str,
        f: F,
    ) -> glib::SignalHandlerId {
        self.connect_local(signal, false, move |values| {
            let bar = values[0].get::<Self>().expect("the signal carries the bar");
            f(&bar);
            None
        })
    }
}

fn row_line(row: &MutationRow) -> (String, String) {
    match row.outcome {
        MutationOutcome::Applied => {
            let mut detail = "written".to_owned();
            if let Some(seq) = row.seq_no {
                detail.push_str(&format!(" · now at _seq_no {seq}"));
            }
            if row.forced_refresh {
                detail.push_str(
                    " · the server forced an immediate refresh rather than waiting for one",
                );
            }
            ("✓".to_owned(), detail)
        }
        MutationOutcome::NotAttempted => (
            "…".to_owned(),
            "never attempted — the batch stopped before it, so this is still staged".to_owned(),
        ),
        MutationOutcome::Failed if row.conflict => (
            "⑂".to_owned(),
            "version conflict — this document changed on the server after you loaded it, so \
             nothing was written"
                .to_owned(),
        ),
        MutationOutcome::Failed => (
            "✗".to_owned(),
            match row.error.is_empty() {
                true => "the write failed".to_owned(),
                false => row.error.clone(),
            },
        ),
    }
}

fn report_subtitle(report: &MutationReport) -> String {
    let mut text = format!("{} applied", report.applied);
    if report.failed > 0 {
        text.push_str(&format!(" · {} failed", report.failed));
    }
    if report.not_attempted > 0 {
        text.push_str(&format!(
            " · {} never attempted. The ones that were never attempted are still staged — nothing \
             was written for them, and nothing was lost.",
            report.not_attempted
        ));
    }
    if report.conflicts > 0 {
        text.push_str(
            " A version conflict means the document changed on the server after you loaded it, so \
             the write was refused rather than overwriting someone else's change. What you typed \
             is still staged — resolve it to see what changed.",
        );
    }
    text
}

/// The commit report, with the one button that leads on to the conflict review.
pub fn report_dialog(report: &MutationReport, on_resolve: impl Fn() + 'static) -> adw::Dialog {
    let list = gtk::Box::new(gtk::Orientation::Vertical, 8);
    list.set_margin_top(4);
    list.set_margin_bottom(4);
    for notice in &report.notices {
        let mark = if notice.is_warning() { "⚠" } else { "ⓘ" };
        let code = match notice.code.is_empty() {
            true => String::new(),
            false => format!("  [{}]", notice.code),
        };
        list.append(&wrapped(&format!("{mark} {}{code}", notice.message)));
    }
    if !report.notices.is_empty() {
        list.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
    }
    for row in &report.rows {
        let (mark, detail) = row_line(row);
        let line = wrapped(&format!(
            "{mark}  {}  {}/{}\n     {detail}",
            row.op, row.index, row.document_id
        ));
        line.add_css_class("monospace");
        list.append(&line);
    }

    let title = match (report.is_clean(), report.applied) {
        (true, 1) => "1 document written".to_owned(),
        (true, n) => format!("{n} documents written"),
        _ => "The batch stopped part way through".to_owned(),
    };
    let footer = match (report.conflicts > 0, report.is_clean()) {
        (true, _) => "Reads each conflicted document back and shows what changed.",
        (false, false) => "Re-run the statement to see what the server holds now.",
        _ => "",
    };

    let resolve = (report.conflicts > 0).then(|| {
        let button = gtk::Button::with_label("Resolve Conflicts…");
        button.add_css_class("suggested-action");
        button
    });
    let dialog = shell(
        Shell {
            dialog_title: "Commit report",
            title: &title,
            subtitle: &report_subtitle(report),
            footer,
            size: (560, 480),
        },
        &list,
        resolve.as_ref(),
    );
    if let Some(button) = resolve {
        let dialog = dialog.clone();
        button.connect_clicked(move |_| {
            on_resolve();
            dialog.close();
        });
    }
    dialog
}

/// One scrolled list under a heading, closed by Done — the shape both review dialogs take.
pub(crate) struct Shell<'a> {
    pub dialog_title: &'a str,
    pub title: &'a str,
    pub subtitle: &'a str,
    pub footer: &'a str,
    pub size: (i32, i32),
}

pub(crate) fn shell(
    spec: Shell<'_>,
    list: &impl IsA<gtk::Widget>,
    action: Option<&gtk::Button>,
) -> adw::Dialog {
    let Shell {
        dialog_title,
        title,
        subtitle,
        footer,
        size: (width, height),
    } = spec;
    let heading = gtk::Label::new(Some(title));
    heading.add_css_class("title-4");
    heading.set_xalign(0.0);

    let body = gtk::Box::new(gtk::Orientation::Vertical, 8);
    body.set_margin_top(12);
    body.set_margin_bottom(12);
    body.set_margin_start(16);
    body.set_margin_end(16);
    body.append(&heading);
    body.append(&wrapped(subtitle));

    let scroller = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .vexpand(true)
        .child(list)
        .build();
    body.append(&scroller);
    if !footer.is_empty() {
        let note = wrapped(footer);
        note.add_css_class("dim-label");
        body.append(&note);
    }

    let buttons = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    buttons.set_halign(gtk::Align::End);
    if let Some(action) = action {
        buttons.append(action);
    }
    let done = gtk::Button::with_label("Done");
    buttons.append(&done);
    body.append(&buttons);

    let view = adw::ToolbarView::new();
    view.add_top_bar(&adw::HeaderBar::new());
    view.set_content(Some(&body));

    let dialog = adw::Dialog::builder()
        .title(dialog_title)
        .content_width(width)
        .content_height(height)
        .child(&view)
        .build();
    let closing = dialog.clone();
    done.connect_clicked(move |_| {
        closing.close();
    });
    dialog
}

/// Cancel is the default response: nothing is written by pressing return on a dialog nobody read.
pub fn confirm(
    heading: &str,
    body: &str,
    confirm_label: &str,
    on_confirm: impl Fn() + 'static,
) -> adw::AlertDialog {
    let dialog = adw::AlertDialog::new(Some(heading), Some(body));
    dialog.add_response("cancel", "Cancel");
    dialog.add_response("confirm", confirm_label);
    dialog.set_response_appearance("confirm", adw::ResponseAppearance::Destructive);
    dialog.set_default_response(Some("cancel"));
    dialog.set_close_response("cancel");
    dialog.connect_response(None, move |_, response| {
        if response == "confirm" {
            on_confirm();
        }
    });
    dialog
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_multi_document_commit_says_serial_and_untransacted_before_the_click() {
        let warning = commit_warning(5, false);
        assert!(warning.contains("one by one"), "{warning}");
        assert!(warning.contains("there is no transaction"), "{warning}");
        assert!(
            warning.contains("if #3 fails, the 2 before it stay written"),
            "{warning}"
        );
        assert!(
            warning.contains("anything not written stays staged"),
            "{warning}"
        );
    }

    #[test]
    fn two_documents_name_the_one_that_would_stay_written() {
        assert!(commit_warning(2, false).contains("if #2 fails, the one before it stay written"));
    }

    #[test]
    fn an_atomic_connection_is_told_the_truth_about_itself_instead() {
        let warning = commit_warning(5, true);
        assert!(
            warning.contains("either all 5 are written, or none are"),
            "{warning}"
        );
        assert!(!warning.contains("no transaction"), "{warning}");
    }

    #[test]
    fn a_single_document_promises_only_that_it_stays_staged() {
        let warning = commit_warning(1, false);
        assert!(warning.contains("written on its own"), "{warning}");
        assert!(
            warning.contains("stays staged for another try"),
            "{warning}"
        );
    }

    #[test]
    fn the_bar_never_calls_a_partly_written_batch_finished() {
        let counts = StagedCounts {
            pending: 2,
            written: 1,
            updates: 2,
            deletes: 0,
            conflicts: 1,
        };
        assert_eq!(headline(counts), "2 documents edited, not yet written");
        assert_eq!(
            detail(counts),
            "2 to update · 1 already written · 1 changed on the server"
        );
    }

    #[test]
    fn a_fully_written_batch_says_the_grid_is_stale_rather_than_wrong() {
        let counts = StagedCounts {
            written: 3,
            ..StagedCounts::default()
        };
        assert_eq!(
            headline(counts),
            "3 documents written — the grid still shows what was loaded"
        );
    }

    #[test]
    fn the_headline_after_a_commit_counts_conflicts_inside_the_failures() {
        let report = MutationReport {
            applied: 1,
            failed: 2,
            not_attempted: 1,
            conflicts: 2,
            ..MutationReport::default()
        };
        assert_eq!(
            report_headline(&report),
            "1 applied · 2 failed (2 a version conflict) · 1 never attempted, still staged"
        );
    }

    #[test]
    fn a_not_attempted_row_says_it_is_still_staged() {
        let (mark, detail) = row_line(&MutationRow {
            outcome: MutationOutcome::NotAttempted,
            ..MutationRow::default()
        });
        assert_eq!(mark, "…");
        assert!(detail.contains("still staged"), "{detail}");
    }

    #[test]
    fn an_applied_row_names_the_version_it_now_sits_at() {
        let (_, detail) = row_line(&MutationRow {
            outcome: MutationOutcome::Applied,
            seq_no: Some(42),
            forced_refresh: true,
            ..MutationRow::default()
        });
        assert!(detail.contains("now at _seq_no 42"), "{detail}");
        assert!(detail.contains("forced an immediate refresh"), "{detail}");
    }
}
