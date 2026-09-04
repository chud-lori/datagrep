use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::Arc;

use adw::prelude::*;
use adw::subclass::prelude::*;
use gtk::glib;

use crate::appearance;
use crate::ffi::Core;
use crate::model::mutation::{
    document_address_batch_json, mutation_batch_json, MutationReport, ServerDocument,
};
use crate::model::update::UpdateCheck;
use crate::model::{ParkedResult, ResultModel, StagedDocument};
use crate::sql::Derived;
use crate::ui::conflict::{ConflictDialog, ConflictReview};
use crate::ui::editing::{commit_warning, confirm, report_dialog, report_headline};
use crate::ui::{ResultsGrid, Sidebar, StagedEditsBar, StatusBar};

mod imp {
    use super::*;
    use glib::subclass::Signal;
    use std::sync::OnceLock;

    pub struct Window {
        pub core: RefCell<Option<Arc<Core>>>,
        pub model: ResultModel,
        pub grid: ResultsGrid,
        pub sidebar: Sidebar,
        pub status: StatusBar,
        pub staged: StagedEditsBar,
        pub committing: Cell<bool>,
        pub rereading: Cell<bool>,
        // The connection the loaded result came from, which the selected one may no longer be.
        pub ran_profile: RefCell<String>,
        pub review: RefCell<ConflictReview>,
        pub title: adw::WindowTitle,
        pub navigation: adw::NavigationSplitView,
        pub utility: adw::OverlaySplitView,
        pub editor_slot: adw::Bin,
        pub utility_slot: adw::Bin,
        pub notice_slot: adw::Bin,
        pub derived: RefCell<Derived>,
        pub run_profile: RefCell<String>,
        // A result belongs to the editor tab that ran it and to the connection it ran on.
        pub results: RefCell<HashMap<String, TabResult>>,
        pub result_tab: RefCell<String>,
        pub active_tab: RefCell<String>,
        pub active_connection: RefCell<String>,
    }

    /// One tab's result off screen, with the clauses and the line about it.
    pub struct TabResult {
        pub parked: ParkedResult,
        pub profile: String,
        pub derived: Derived,
        pub sort: Option<(String, bool)>,
        pub message: String,
        pub is_error: bool,
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
                staged: StagedEditsBar::new(),
                committing: Cell::new(false),
                rereading: Cell::new(false),
                ran_profile: RefCell::new(String::new()),
                review: RefCell::new(ConflictReview::default()),
                title: adw::WindowTitle::new("datagrep", ""),
                navigation: adw::NavigationSplitView::new(),
                utility: adw::OverlaySplitView::new(),
                editor_slot: adw::Bin::new(),
                utility_slot: adw::Bin::new(),
                notice_slot: adw::Bin::new(),
                derived: RefCell::new(Derived::default()),
                run_profile: RefCell::new(String::new()),
                results: RefCell::new(HashMap::new()),
                result_tab: RefCell::new(String::new()),
                active_tab: RefCell::new(String::new()),
                active_connection: RefCell::new(String::new()),
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
                    Signal::builder("check-updates").build(),
                    Signal::builder("object-activated")
                        .param_types([
                            String::static_type(),
                            String::static_type(),
                            String::static_type(),
                        ])
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
            toolbar.add_top_bar(&self.notice_slot);
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

    #[derive(Clone, Copy)]
    pub enum Action {
        Commit,
        Discard,
        Resolve,
        Reload,
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

            header.pack_end(&primary_menu());

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

            // The bar annotates the rows it is about, so it sits directly over them.
            let results = gtk::Box::new(gtk::Orientation::Vertical, 0);
            results.append(&self.staged);
            results.append(&self.grid);

            let workspace = gtk::Paned::new(gtk::Orientation::Vertical);
            workspace.set_shrink_start_child(false);
            workspace.set_shrink_end_child(false);
            workspace.set_start_child(Some(&self.editor_slot));
            workspace.set_end_child(Some(&results));
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
            self.staged.bind(&self.model.edits());
            self.wire_editing();

            let appearance = gio::SimpleAction::new_stateful(
                "appearance",
                Some(glib::VariantTy::STRING),
                &appearance::mode().as_str().to_variant(),
            );
            appearance.connect_activate(|action, parameter| {
                if let Some(value) = parameter.and_then(|p| p.str()) {
                    action.set_state(&value.to_variant());
                    appearance::set_mode(appearance::Mode::parse(value));
                }
            });
            self.obj().add_action(&appearance);

            let launch_check = gio::SimpleAction::new_stateful(
                "update-check-on-launch",
                None,
                &UpdateCheck::check_on_launch_enabled().to_variant(),
            );
            launch_check.connect_activate(|action, _| {
                let on = !action.state().and_then(|s| s.get::<bool>()).unwrap_or(true);
                action.set_state(&on.to_variant());
                UpdateCheck::set_check_on_launch_enabled(on);
            });
            self.obj().add_action(&launch_check);

            let check_updates = gio::SimpleAction::new("check-updates", None);
            let window = self.obj().downgrade();
            check_updates.connect_activate(move |_, _| {
                if let Some(window) = window.upgrade() {
                    window.emit_by_name::<()>("check-updates", &[]);
                }
            });
            self.obj().add_action(&check_updates);

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
                .connect_object_activated(move |_, profile, path, name| {
                    if let Some(window) = window.upgrade() {
                        window.emit_by_name::<()>("object-activated", &[&profile, &path, &name]);
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

        fn wire_editing(&self) {
            let window = self.obj().downgrade();
            self.grid.connect_edit_refused(move |_, message| {
                if let Some(window) = window.upgrade() {
                    window.imp().status.say(message, true);
                }
            });
            let window = self.obj().downgrade();
            self.grid.connect_copied(move |_, message| {
                if let Some(window) = window.upgrade() {
                    window.imp().status.say(message, false);
                }
            });
            for (signal, action) in [
                ("commit-requested", Action::Commit),
                ("discard-requested", Action::Discard),
                ("resolve-requested", Action::Resolve),
                ("reload-requested", Action::Reload),
            ] {
                let window = self.obj().downgrade();
                self.staged.connect_request(signal, move |_| {
                    if let Some(window) = window.upgrade() {
                        window.imp().on_staged_action(action);
                    }
                });
            }
        }

        fn on_staged_action(&self, action: Action) {
            match action {
                Action::Commit => self.commit_staged_edits(),
                Action::Discard => self.discard_staged_edits(),
                Action::Resolve => self.review_conflicts(),
                Action::Reload => self.execute(),
            }
        }

        /// The one destructive step. Everything before it is staging.
        fn commit_staged_edits(&self) {
            let pending = self.model.edits().pending();
            let profile = self.ran_profile.borrow().clone();
            if self.committing.get() || pending.is_empty() || profile.is_empty() {
                return;
            }
            // The veto is re-read here, not only when a cell was typed into.
            let Some(editable) = self.model.editable() else {
                self.status.say(
                    "this result is no longer editable on this connection, so nothing was sent",
                    true,
                );
                return;
            };
            let count = pending.len() as u32;
            let heading = match count {
                1 => format!("Commit 1 document edit to `{profile}`?"),
                n => format!("Commit {n} document edits to `{profile}`?"),
            };
            let label = match count {
                1 => "Commit 1 Document".to_owned(),
                n => format!("Commit {n} Documents"),
            };
            let window = self.obj().downgrade();
            let dialog = confirm(
                &heading,
                &commit_warning(count, editable.atomic_batch),
                &label,
                move || {
                    if let Some(window) = window.upgrade() {
                        window.imp().send_mutations(&pending, &profile);
                    }
                },
            );
            dialog.present(Some(self.obj().as_ref()));
        }

        fn send_mutations(&self, pending: &[StagedDocument], profile: &str) {
            let Some(core) = self.core.borrow().clone() else {
                return;
            };
            self.committing.set(true);
            self.staged.set_committing(true);
            self.status.say(
                &format!("committing {} document(s) to {profile}…", pending.len()),
                false,
            );
            let ids: Vec<String> = pending.iter().map(|d| d.id.clone()).collect();
            let rows: Vec<u64> = pending.iter().map(|d| d.row).collect();
            let batch = mutation_batch_json(
                &pending
                    .iter()
                    .map(StagedDocument::mutation)
                    .collect::<Vec<_>>(),
            );
            let (window, core, profile) =
                (self.obj().downgrade(), (*core).clone(), profile.to_owned());
            glib::spawn_future_local(async move {
                let sent = gio::spawn_blocking(move || core.mutate_json(&profile, &batch)).await;
                if let Some(window) = window.upgrade() {
                    window.imp().finish_commit(sent.ok(), &ids, &rows);
                }
            });
        }

        fn finish_commit(
            &self,
            sent: Option<Result<String, crate::ffi::Error>>,
            ids: &[String],
            rows: &[u64],
        ) {
            self.committing.set(false);
            self.staged.set_committing(false);
            let report = match sent {
                // The batch never ran: nothing was written, and everything stays staged.
                Some(Err(error)) => return self.status.say(&error.0, true),
                None => return self.status.say("the commit did not finish", true),
                Some(Ok(json)) => match MutationReport::decode(&json) {
                    Ok(report) => report,
                    Err(why) => return self.status.say(&why, true),
                },
            };
            let lined_up = self.model.edits().apply(&report, ids);
            self.model.refresh_staged_rows(rows);
            self.status.say(
                &match lined_up {
                    true => report_headline(&report),
                    false => format!(
                        "the engine reported {} outcome(s) for {} document(s), so datagrep cannot \
                         say which is which — read the report, and re-run the statement to see \
                         what was written",
                        report.rows.len(),
                        ids.len()
                    ),
                },
                !report.is_clean() || !lined_up,
            );
            let window = self.obj().downgrade();
            let dialog = report_dialog(&report, move || {
                if let Some(window) = window.upgrade() {
                    window.imp().review_conflicts();
                }
            });
            dialog.present(Some(self.obj().as_ref()));
        }

        fn review_conflicts(&self) {
            let conflicted = self.model.edits().conflicted();
            let profile = self.ran_profile.borrow().clone();
            if self.committing.get() || self.rereading.get() || conflicted.is_empty() {
                return;
            }
            let (Some(core), false) = (self.core.borrow().clone(), profile.is_empty()) else {
                return;
            };
            if self.model.editable().is_none() {
                self.status.say(
                    "this result no longer says how its documents are identified, so datagrep \
                     cannot read them back — re-run the statement",
                    true,
                );
                return;
            }
            self.rereading.set(true);
            self.staged.set_rereading(true);
            self.status.say("reading what the server holds now…", false);
            let addresses = document_address_batch_json(
                &conflicted
                    .iter()
                    .map(StagedDocument::address)
                    .collect::<Vec<_>>(),
            );
            let (window, core) = (self.obj().downgrade(), (*core).clone());
            glib::spawn_future_local(async move {
                let read =
                    gio::spawn_blocking(move || core.reread_documents_json(&profile, &addresses))
                        .await;
                if let Some(window) = window.upgrade() {
                    window.imp().finish_reread(read.ok(), &conflicted);
                }
            });
        }

        fn finish_reread(
            &self,
            read: Option<Result<String, crate::ffi::Error>>,
            conflicted: &[StagedDocument],
        ) {
            self.rereading.set(false);
            self.staged.set_rereading(false);
            let server = match read {
                Some(Err(error)) => return self.status.say(&error.0, true),
                None => return self.status.say("the re-read did not finish", true),
                Some(Ok(json)) => match ServerDocument::decode_all(&json) {
                    Ok(documents) => documents,
                    Err(why) => return self.status.say(&why, true),
                },
            };
            // By position, like the report: a list that does not line up is not guessed at.
            if server.len() != conflicted.len() {
                self.status.say(
                    &format!(
                        "the engine answered for {} of {} documents, so datagrep cannot say which \
                         answer belongs to which — re-run the statement",
                        server.len(),
                        conflicted.len()
                    ),
                    true,
                );
                return;
            }
            let Some(editable) = self.model.editable() else {
                return;
            };
            *self.review.borrow_mut() = ConflictReview::build(conflicted, &server, &editable);
            self.status.say(
                &format!("{} conflict(s) to resolve", conflicted.len()),
                false,
            );
            self.present_conflict_review();
        }

        /// A rebase is staged again, not written: the commit button stays the only thing that writes.
        fn present_conflict_review(&self) {
            let dialog: Rc<RefCell<Option<ConflictDialog>>> = Rc::default();
            let window = self.obj().downgrade();
            let owner = Rc::clone(&dialog);
            let review = ConflictDialog::new(&self.review.borrow(), move |id, rebase| {
                let Some(window) = window.upgrade() else {
                    return;
                };
                if window.imp().resolve_conflict(id, rebase) {
                    if let Some(dialog) = owner.borrow().as_ref() {
                        dialog.resolved(id);
                    }
                }
            });
            review.present(self.obj().as_ref());
            *dialog.borrow_mut() = Some(review);
        }

        fn resolve_conflict(&self, id: &str, rebase: bool) -> bool {
            let edits = self.model.edits();
            if !rebase {
                if let Some(row) = edits.discard_by_id(id) {
                    self.model.refresh_staged_rows(&[row]);
                }
                self.status
                    .say("edit discarded — the server's version is untouched", false);
                return true;
            }
            let guard = self
                .review
                .borrow()
                .document(id)
                .filter(|document| document.can_rebase)
                .map(|document| document.rebase_guard.clone());
            let Some(guard) = guard else {
                self.status.say(
                    "the server did not return a version for this document, so the edit could \
                     only be re-sent unguarded — which would overwrite whatever is there now",
                    true,
                );
                return false;
            };
            if let Some(row) = edits.rebase(id, guard) {
                self.model.refresh_staged_rows(&[row]);
            }
            self.status.say(
                "re-applied onto the current version — still staged, and still not written. \
                 Commit to write it.",
                false,
            );
            true
        }

        fn discard_staged_edits(&self) {
            let edits = self.model.edits();
            let rows = edits.rows();
            if rows.is_empty() {
                return;
            }
            let window = self.obj().downgrade();
            let dialog = confirm(
                &format!(
                    "Discard {} staged document edit(s)?",
                    edits.counts().pending
                ),
                "Nothing has been written yet, and nothing will be. The values you typed are lost.",
                "Discard",
                move || {
                    let Some(window) = window.upgrade() else {
                        return;
                    };
                    let imp = window.imp();
                    imp.model.edits().discard_all();
                    imp.model.refresh_staged_rows(&rows);
                    imp.status.say("staged edits discarded", false);
                },
            );
            dialog.present(Some(self.obj().as_ref()));
        }

        /// Park the visible result under its own tab, so switching back restores it.
        fn park_visible(&self) {
            let tab = self.result_tab.borrow().clone();
            let Some(parked) = (!tab.is_empty()).then(|| self.model.park()).flatten() else {
                return;
            };
            let (message, is_error) = self.status.spoken();
            self.results.borrow_mut().insert(
                tab,
                TabResult {
                    parked,
                    profile: self.ran_profile.borrow().clone(),
                    derived: self.derived.borrow().clone(),
                    sort: self.grid.sort_indicator(),
                    message,
                    is_error,
                },
            );
        }

        /// Back to "no result in this tab yet": nothing is attributed to anything.
        fn clear_visible(&self) {
            let had = !self.result_tab.borrow().is_empty();
            self.model.reset();
            self.result_tab.borrow_mut().clear();
            self.ran_profile.borrow_mut().clear();
            *self.derived.borrow_mut() = Derived::default();
            self.grid.clear_sort_indicator();
            self.status
                .say(if had { "no result in this tab yet" } else { "" }, false);
        }

        /// Put `tab`'s result on screen — and only if this connection produced it.
        pub(super) fn sync_visible(&self) {
            let (tab, connection) = (
                self.active_tab.borrow().clone(),
                self.active_connection.borrow().clone(),
            );
            if *self.result_tab.borrow() == tab && *self.ran_profile.borrow() == connection {
                return;
            }
            self.park_visible();
            self.clear_visible();
            let restorable = self
                .results
                .borrow()
                .get(&tab)
                .is_some_and(|saved| saved.profile == connection && !connection.is_empty());
            if !restorable {
                return;
            }
            let Some(saved) = self.results.borrow_mut().remove(&tab) else {
                return;
            };
            *self.ran_profile.borrow_mut() = saved.profile;
            *self.derived.borrow_mut() = saved.derived;
            *self.result_tab.borrow_mut() = tab;
            self.grid.set_sort_indicator(saved.sort);
            self.model.adopt(saved.parked);
            self.status.say(&saved.message, saved.is_error);
        }

        /// The saved profile's own flag; unknown reads as read-only.
        fn profile_refuses_writes(&self, profile: &str) -> bool {
            let Some(core) = self.core.borrow().clone() else {
                return true;
            };
            core.profile_json(profile)
                .ok()
                .and_then(|json| serde_json::from_str::<serde_json::Value>(&json).ok())
                .and_then(|profile| profile["read_only"].as_bool())
                .unwrap_or(true)
        }

        pub(super) fn execute(&self) {
            let Some(core) = self.core.borrow().clone() else {
                return;
            };
            // The profile the statement resolved to, not whatever the sidebar shows now.
            let profile = self.run_profile.borrow().clone();
            if profile.is_empty() {
                self.status.say("pick a connection first", true);
                return;
            }
            let sql = self.derived.borrow().sql();
            if sql.trim().is_empty() {
                return;
            }
            // Announced before the engine is asked, so a statement refused was never a run.
            let driver = self.derived.borrow().driver().to_owned();
            // Set before the result exists, and read from the connection the
            // statement resolved to rather than whatever the sidebar shows.
            self.model
                .set_allows_editing(!self.profile_refuses_writes(&profile));
            let obj = self.obj();
            obj.emit_by_name::<()>("run-started", &[&profile, &driver, &sql]);
            match core.query(&profile, &sql) {
                Ok(query) => {
                    self.status.say("", false);
                    // The run is what this tab is pointed at now, directive included.
                    *self.active_connection.borrow_mut() = profile.clone();
                    *self.ran_profile.borrow_mut() = profile;
                    *self.result_tab.borrow_mut() = self.active_tab.borrow().clone();
                    self.model.set_query(query);
                }
                Err(error) => {
                    // Nothing ran, so nothing is owned: the previous handle goes too.
                    self.model.reset();
                    self.result_tab.borrow_mut().clear();
                    self.ran_profile.borrow_mut().clear();
                    self.status.say(&error.0, true);
                    obj.emit_by_name::<()>("run-failed", &[&error.0]);
                }
            }
        }
    }
}

fn primary_menu() -> gtk::MenuButton {
    let appearance = gio::Menu::new();
    for (label, value) in [
        ("Follow System", "system"),
        ("Light", "light"),
        ("Dark", "dark"),
    ] {
        let item = gio::MenuItem::new(Some(label), None);
        item.set_action_and_target_value(Some("win.appearance"), Some(&value.to_variant()));
        appearance.append_item(&item);
    }
    let updates = gio::Menu::new();
    updates.append(Some("Check for Updates…"), Some("win.check-updates"));
    updates.append(
        Some("Check for Updates at Launch"),
        Some("win.update-check-on-launch"),
    );
    let menu = gio::Menu::new();
    menu.append_submenu(Some("Appearance"), &appearance);
    menu.append_section(None, &updates);
    gtk::MenuButton::builder()
        .icon_name("open-menu-symbolic")
        .menu_model(&menu)
        .tooltip_text("Main Menu")
        .build()
}

glib::wrapper! {
    pub struct Window(ObjectSubclass<imp::Window>)
        @extends adw::ApplicationWindow, gtk::ApplicationWindow, gtk::Window, gtk::Widget,
        @implements gio::ActionGroup, gio::ActionMap, gtk::Accessible, gtk::Buildable,
                    gtk::ConstraintTarget, gtk::Native, gtk::Root, gtk::ShortcutManager;
}

impl Window {
    pub fn new(app: &adw::Application, core: Arc<Core>) -> Self {
        let window: Self = glib::Object::builder().property("application", app).build();
        let imp = window.imp();
        imp.sidebar.set_core(core.clone());
        *imp.core.borrow_mut() = Some(core);
        window
    }

    /// The one run path, so the derived clauses cannot be bypassed by where the SQL came from.
    pub fn run(&self, sql: &str) {
        self.run_on(
            &self.imp().sidebar.selected_connection().unwrap_or_default(),
            &self.imp().sidebar.selected_driver().unwrap_or_default(),
            sql,
        );
    }

    /// The editor's entry: `profile` already resolved by directive > binding > window precedence.
    pub fn run_on(&self, profile: &str, driver: &str, sql: &str) {
        let imp = self.imp();
        imp.run_profile.replace(profile.to_string());
        imp.derived.borrow_mut().ask(sql, driver);
        imp.grid.clear_sort_indicator();
        imp.execute();
    }

    /// The tab in front and its connection; a result of neither goes off screen.
    pub fn set_active_tab(&self, tab: &str, connection: &str) {
        let imp = self.imp();
        imp.active_tab.replace(tab.to_string());
        imp.active_connection.replace(connection.to_string());
        imp.sync_visible();
    }

    /// Closed tabs free their result, and with it the engine-side result store.
    pub fn forget_results(&self, live: &[String]) {
        self.imp()
            .results
            .borrow_mut()
            .retain(|tab, _| live.iter().any(|id| id == tab));
    }

    pub fn selected_connection(&self) -> Option<String> {
        self.imp().sidebar.selected_connection()
    }

    pub fn connect_connection_selected<F: Fn(&Self, &str) + 'static>(
        &self,
        f: F,
    ) -> glib::SignalHandlerId {
        let window = self.downgrade();
        self.imp()
            .sidebar
            .connect_connection_selected(move |_, name| {
                if let Some(window) = window.upgrade() {
                    f(&window, name);
                }
            })
    }

    /// The only route to a connection's colour, read-only flag and enforcement.
    pub fn connect_edit_connection<F: Fn(&Self, &str) + 'static>(
        &self,
        f: F,
    ) -> glib::SignalHandlerId {
        self.forward_from_sidebar("edit-requested", f)
    }

    pub fn connect_remove_connection<F: Fn(&Self, &str) + 'static>(
        &self,
        f: F,
    ) -> glib::SignalHandlerId {
        self.forward_from_sidebar("remove-requested", f)
    }

    fn forward_from_sidebar<F: Fn(&Self, &str) + 'static>(
        &self,
        signal: &str,
        f: F,
    ) -> glib::SignalHandlerId {
        let window = self.downgrade();
        self.imp()
            .sidebar
            .connect_local(signal, false, move |values| {
                let name = values[1].get::<String>().unwrap_or_default();
                if let Some(window) = window.upgrade() {
                    f(&window, &name);
                }
                None
            })
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

    /// Everything staged and unwritten, and the only button that writes.
    pub fn staged_bar(&self) -> StagedEditsBar {
        self.imp().staged.clone()
    }

    /// Where the SQL editor mounts.
    pub fn editor_slot(&self) -> adw::Bin {
        self.imp().editor_slot.clone()
    }

    /// Where the inspector / history pane mounts.
    pub fn utility_slot(&self) -> adw::Bin {
        self.imp().utility_slot.clone()
    }

    /// Where the update notice mounts, under the header bar.
    pub fn notice_slot(&self) -> adw::Bin {
        self.imp().notice_slot.clone()
    }

    pub fn connect_check_updates<F: Fn(&Self) + 'static>(&self, f: F) -> glib::SignalHandlerId {
        self.connect_local("check-updates", false, move |values| {
            let window = values[0]
                .get::<Self>()
                .expect("the signal carries the window");
            f(&window);
            None
        })
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

    /// A catalog object was activated: its connection, its path and its leaf name.
    pub fn connect_object_activated<F: Fn(&Self, &str, &str, &str) + 'static>(
        &self,
        f: F,
    ) -> glib::SignalHandlerId {
        self.connect_local("object-activated", false, move |values| {
            let window = values[0]
                .get::<Self>()
                .expect("the signal carries the window");
            let profile = values[1].get::<String>().unwrap_or_default();
            let path = values[2].get::<String>().unwrap_or_default();
            let name = values[3].get::<String>().unwrap_or_default();
            f(&window, &profile, &path, &name);
            None
        })
    }
}
