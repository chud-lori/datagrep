use std::cell::{Cell, RefCell};
use std::future::Future;
use std::pin::Pin;
use std::rc::Rc;
use std::sync::Arc;

use gtk::prelude::*;
use gtk::subclass::prelude::*;
use gtk::{gio, glib};
use serde::Deserialize;
use sourceview5::subclass::prelude::*;

use crate::ffi::Core;

/// The engine and connection the editor's text is completed against, asked fresh each time.
pub type Target = Rc<dyn Fn() -> Option<(Arc<Core>, String)>>;

#[derive(Debug, Clone, Default, Deserialize)]
pub struct Item {
    pub label: String,
    pub insert: String,
    pub kind: String,
    pub detail: Option<String>,
}

#[derive(Debug, Deserialize)]
struct Answer {
    prefix: String,
    items: Vec<Item>,
    error: Option<String>,
}

mod imp {
    use super::*;

    #[derive(Default)]
    pub struct Proposal {
        pub item: RefCell<Item>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for Proposal {
        const NAME: &'static str = "DgCompletionProposal";
        type Type = super::Proposal;
        type Interfaces = (sourceview5::CompletionProposal,);
    }

    impl ObjectImpl for Proposal {}
    impl CompletionProposalImpl for Proposal {}

    #[derive(Default)]
    pub struct Provider {
        pub target: RefCell<Option<Target>>,
        /// Char offset the accepted text replaces from: the caret minus what was typed.
        pub start: Cell<i32>,
        pub typed: Rc<RefCell<String>>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for Provider {
        const NAME: &'static str = "DgSqlCompletion";
        type Type = super::Provider;
        type Interfaces = (sourceview5::CompletionProvider,);
    }

    impl ObjectImpl for Provider {}

    impl CompletionProviderImpl for Provider {
        fn title(&self) -> Option<glib::GString> {
            Some("Schema".into())
        }

        fn is_trigger(&self, _iter: &gtk::TextIter, c: char) -> bool {
            c == '.'
        }

        fn populate_future(
            &self,
            context: &sourceview5::CompletionContext,
        ) -> Pin<Box<dyn Future<Output = Result<gio::ListModel, glib::Error>>>> {
            let target = self.target.borrow().as_ref().and_then(|f| f());
            let (Some((core, profile)), Some(buffer)) = (target, context.buffer()) else {
                return Box::pin(async { Ok(gio::ListStore::new::<super::Proposal>().upcast()) });
            };
            let caret = buffer.iter_at_mark(&buffer.get_insert());
            let text = buffer
                .text(&buffer.start_iter(), &buffer.end_iter(), true)
                .to_string();
            let caret_byte = buffer.text(&buffer.start_iter(), &caret, true).len();
            let caret_char = caret.offset();
            let provider = self.obj().downgrade();
            let typed = self.typed.clone();
            Box::pin(async move {
                let answered =
                    gio::spawn_blocking(move || core.complete_json(&profile, &text, caret_byte))
                        .await
                        .map_err(|_| failed("completion did not finish"))?
                        .map_err(|e| failed(&e.0))?;
                let answer: Answer =
                    serde_json::from_str(&answered).map_err(|e| failed(&e.to_string()))?;
                if let Some(error) = &answer.error {
                    glib::g_warning!("datagrep", "completion without the catalog: {error}");
                }
                let store = gio::ListStore::new::<super::Proposal>();
                for item in answer.items {
                    let proposal: super::Proposal = glib::Object::new();
                    proposal.imp().item.replace(item);
                    store.append(&proposal);
                }
                if let Some(provider) = provider.upgrade() {
                    let start = caret_char - answer.prefix.chars().count() as i32;
                    provider.imp().start.set(start);
                }
                typed.replace(answer.prefix.to_lowercase());
                Ok(super::filtered(store, typed))
            })
        }

        fn refilter(&self, context: &sourceview5::CompletionContext, model: &gio::ListModel) {
            let (Some(buffer), Some(filtered)) = (
                context.buffer(),
                model.downcast_ref::<gtk::FilterListModel>(),
            ) else {
                return;
            };
            let start = buffer.iter_at_offset(self.start.get());
            let caret = buffer.iter_at_mark(&buffer.get_insert());
            let typed = buffer.text(&start, &caret, true).to_lowercase();
            self.typed.replace(typed);
            if let Some(filter) = filtered.filter() {
                filter.changed(gtk::FilterChange::Different);
            }
        }

        fn display(
            &self,
            context: &sourceview5::CompletionContext,
            proposal: &sourceview5::CompletionProposal,
            cell: &sourceview5::CompletionCell,
        ) {
            let Some(proposal) = proposal.downcast_ref::<super::Proposal>() else {
                return;
            };
            let item = proposal.imp().item.borrow();
            match cell.column() {
                sourceview5::CompletionColumn::TypedText => {
                    let word = context.word().to_lowercase();
                    match sourceview5::Completion::fuzzy_highlight(&item.label, &word) {
                        Some(attrs) => cell.set_text_with_attributes(&item.label, &attrs),
                        None => cell.set_text(Some(&item.label)),
                    }
                }
                sourceview5::CompletionColumn::After => {
                    cell.set_margin_start(12);
                    cell.add_css_class("dim-label");
                    cell.set_text(Some(item.detail.as_deref().unwrap_or(&item.kind)))
                }
                _ => cell.set_text(None),
            }
        }

        fn activate(
            &self,
            context: &sourceview5::CompletionContext,
            proposal: &sourceview5::CompletionProposal,
        ) {
            let (Some(buffer), Some(proposal)) =
                (context.buffer(), proposal.downcast_ref::<super::Proposal>())
            else {
                return;
            };
            let insert = proposal.imp().item.borrow().insert.clone();
            let mut start = buffer.iter_at_offset(self.start.get());
            let mut caret = buffer.iter_at_mark(&buffer.get_insert());
            buffer.begin_user_action();
            buffer.delete(&mut start, &mut caret);
            buffer.insert(&mut start, &insert);
            buffer.end_user_action();
        }
    }
}

glib::wrapper! {
    pub struct Proposal(ObjectSubclass<imp::Proposal>)
        @implements sourceview5::CompletionProposal;
}

glib::wrapper! {
    pub struct Provider(ObjectSubclass<imp::Provider>)
        @implements sourceview5::CompletionProvider;
}

impl Default for Provider {
    fn default() -> Self {
        glib::Object::new()
    }
}

impl Provider {
    pub fn set_target(&self, target: Target) {
        self.imp().target.replace(Some(target));
    }
}

fn failed(message: &str) -> glib::Error {
    glib::Error::new(gio::IOErrorEnum::Failed, message)
}

// The engine ranked the first answer; typing on narrows it to labels holding the typed letters in order.
fn filtered(store: gio::ListStore, typed: Rc<RefCell<String>>) -> gio::ListModel {
    let filter = gtk::CustomFilter::new(move |object| {
        let Some(proposal) = object.downcast_ref::<Proposal>() else {
            return false;
        };
        let label = proposal.imp().item.borrow().label.to_lowercase();
        let mut rest = label.chars();
        typed
            .borrow()
            .chars()
            .all(|c| rest.by_ref().any(|l| l == c))
    });
    gtk::FilterListModel::new(Some(store), Some(filter)).upcast()
}
