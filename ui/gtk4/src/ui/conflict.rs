use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use adw::prelude::*;

use crate::model::mutation::{
    EditableResult, FieldValue, MutationValue, ServerDocument, ServerValue,
};
use crate::model::StagedDocument;
use crate::ui::editing::{shell, Shell};

type Choose = Rc<dyn Fn(&str, bool)>;

/// One edited field's three readings.
#[derive(Debug, Clone)]
pub struct ConflictField {
    pub name: String,
    pub loaded: Option<MutationValue>,
    pub server: ServerValue,
    pub typed: MutationValue,
}

impl ConflictField {
    pub fn moved_underneath(&self) -> bool {
        match &self.server {
            ServerValue::Value(now) => self.loaded.as_ref() != Some(now),
            ServerValue::Nested(_) => true,
            ServerValue::Missing => self.loaded.is_some(),
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct ConflictDocument {
    pub id: String,
    pub title: String,
    pub fields: Vec<ConflictField>,
    pub is_delete: bool,
    pub rebase_guard: Vec<FieldValue>,
    pub can_rebase: bool,
    /// No longer on the server: somebody deleted it.
    pub gone: bool,
    pub error: String,
}

impl ConflictDocument {
    pub fn contested(&self) -> usize {
        self.fields.iter().filter(|f| f.moved_underneath()).count()
    }
}

/// Every conflicted document from one commit, with what the server holds now.
#[derive(Debug, Clone, Default)]
pub struct ConflictReview {
    pub documents: Vec<ConflictDocument>,
}

impl ConflictReview {
    pub fn build(
        conflicted: &[StagedDocument],
        server: &[ServerDocument],
        editable: &EditableResult,
    ) -> Self {
        let documents = conflicted
            .iter()
            .zip(server)
            .map(|(staged, now)| {
                let fields = staged
                    .sets
                    .iter()
                    .map(|set| ConflictField {
                        name: set.field.clone(),
                        loaded: set.loaded.clone(),
                        server: now
                            .fields
                            .get(&set.field)
                            .cloned()
                            .unwrap_or(ServerValue::Missing),
                        typed: set.value.clone(),
                    })
                    .collect();
                // A partly returned guard could only be re-sent unguarded, so it is no guard.
                let rebase_guard: Option<Vec<FieldValue>> = editable
                    .guard
                    .iter()
                    .map(|field| {
                        now.envelope
                            .get(field)
                            .and_then(ServerValue::mutation_value)
                            .map(|value| FieldValue {
                                field: field.clone(),
                                value: value.clone(),
                            })
                    })
                    .collect();
                let rebase_guard = rebase_guard
                    .filter(|guard| now.found && !guard.is_empty())
                    .unwrap_or_default();
                ConflictDocument {
                    id: staged.id.clone(),
                    title: staged.title(),
                    fields,
                    is_delete: staged.is_delete,
                    can_rebase: !rebase_guard.is_empty(),
                    rebase_guard,
                    gone: !now.found && now.error.is_empty(),
                    error: now.error.clone(),
                }
            })
            .collect();
        Self { documents }
    }

    pub fn document(&self, id: &str) -> Option<&ConflictDocument> {
        self.documents.iter().find(|d| d.id == id)
    }
}

fn note(text: &str, warning: bool) -> gtk::Label {
    let label = gtk::Label::new(Some(&format!("{} {text}", if warning { "⚠" } else { "ⓘ" })));
    label.set_wrap(true);
    label.set_xalign(0.0);
    if warning {
        label.add_css_class("warning");
    }
    label
}

fn cell(text: &str, tinted: bool) -> gtk::Label {
    let label = gtk::Label::new(Some(text));
    label.set_xalign(0.0);
    label.set_ellipsize(gtk::pango::EllipsizeMode::End);
    label.set_tooltip_text(Some(text));
    label.add_css_class("monospace");
    if tinted {
        label.add_css_class("warning");
    }
    label
}

fn summary(document: &ConflictDocument) -> String {
    match document.contested() {
        0 => "The fields you edited are unchanged — somebody changed this document elsewhere. \
              Re-applying writes your edits onto their version and overwrites nothing of theirs."
            .to_owned(),
        1 => "1 of the fields you edited was changed by somebody else. Re-applying overwrites \
              their value with yours."
            .to_owned(),
        n => format!(
            "{n} of the fields you edited were changed by somebody else. Re-applying overwrites \
             their values with yours."
        ),
    }
}

fn comparison(document: &ConflictDocument) -> gtk::Grid {
    let grid = gtk::Grid::new();
    grid.set_column_spacing(16);
    grid.set_row_spacing(3);
    for (column, title) in ["field", "you loaded", "on the server now", "you typed"]
        .into_iter()
        .enumerate()
    {
        let head = gtk::Label::new(Some(title));
        head.add_css_class("caption-heading");
        head.set_xalign(0.0);
        grid.attach(&head, column as i32, 0, 1, 1);
    }
    for (index, field) in document.fields.iter().enumerate() {
        let row = index as i32 + 1;
        grid.attach(&cell(&field.name, false), 0, row, 1, 1);
        let loaded = field
            .loaded
            .as_ref()
            .map(MutationValue::display)
            .unwrap_or_else(|| "—".to_owned());
        grid.attach(&cell(&loaded, false), 1, row, 1, 1);
        grid.attach(
            &cell(&field.server.display(), field.moved_underneath()),
            2,
            row,
            1,
            1,
        );
        grid.attach(&cell(&field.typed.display(), false), 3, row, 1, 1);
    }
    grid
}

fn document_block(document: &ConflictDocument, choose: &Choose) -> gtk::Widget {
    let block = gtk::Box::new(gtk::Orientation::Vertical, 6);

    let head = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    let title = gtk::Label::new(Some(&format!("⑂ {}", document.title)));
    title.add_css_class("monospace");
    title.set_xalign(0.0);
    head.append(&title);
    if document.is_delete {
        let tag = gtk::Label::new(Some("staged for deletion"));
        tag.add_css_class("error");
        tag.add_css_class("caption");
        head.append(&tag);
    }
    block.append(&head);

    if !document.error.is_empty() {
        block.append(&note(&document.error, true));
    } else if document.gone {
        block.append(&note(
            "This document is no longer on the server — somebody deleted it. There is no version \
             to re-apply your edits onto.",
            true,
        ));
    }

    if !document.fields.is_empty() {
        block.append(&comparison(document));
    } else if document.is_delete {
        block.append(&note(
            "A delete has no fields of its own. Re-applying it means deleting whatever the \
             document is now, including the change somebody else just made.",
            false,
        ));
    }

    if !document.gone && document.error.is_empty() {
        // The one sentence that decides which button is right.
        block.append(&note(&summary(document), document.contested() > 0));
    }

    let choices = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    choices.set_halign(gtk::Align::End);
    let discard = gtk::Button::with_label("Discard Mine");
    let rebase = gtk::Button::with_label(match document.is_delete {
        true => "Delete It Anyway",
        false => "Re-apply Onto This Version",
    });
    rebase.add_css_class("suggested-action");
    rebase.set_sensitive(document.can_rebase);
    for (button, is_rebase) in [(&discard, false), (&rebase, true)] {
        let (choose, id) = (Rc::clone(choose), document.id.clone());
        button.connect_clicked(move |_| choose(&id, is_rebase));
    }
    choices.append(&discard);
    choices.append(&rebase);
    block.append(&choices);
    block.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
    block.upcast()
}

/// Deliberately never offers "retry as written" — that is the clobber the guard exists to prevent.
pub struct ConflictDialog {
    dialog: adw::Dialog,
    list: gtk::Box,
    blocks: Rc<RefCell<HashMap<String, gtk::Widget>>>,
}

impl ConflictDialog {
    pub fn new(review: &ConflictReview, on_choice: impl Fn(&str, bool) + 'static) -> Self {
        let list = gtk::Box::new(gtk::Orientation::Vertical, 14);
        let blocks: Rc<RefCell<HashMap<String, gtk::Widget>>> = Rc::default();
        let choose: Choose = Rc::new(on_choice);
        for document in &review.documents {
            let block = document_block(document, &choose);
            blocks
                .borrow_mut()
                .insert(document.id.clone(), block.clone());
            list.append(&block);
        }
        let title = match review.documents.len() {
            1 => "1 document changed after you loaded it".to_owned(),
            n => format!("{n} documents changed after you loaded them"),
        };
        let dialog = shell(
            Shell {
                dialog_title: "Resolve conflicts",
                title: &title,
                subtitle: "Nothing was written for these. Each one is shown as you loaded it, as \
                           the server holds it now, and as you typed it — so you can re-apply your \
                           edits onto the current version, or drop them.",
                footer: "Anything left unresolved stays staged and unwritten.",
                size: (720, 520),
            },
            &list,
            None,
        );
        Self {
            dialog,
            list,
            blocks,
        }
    }

    pub fn present(&self, parent: &impl IsA<gtk::Widget>) {
        self.dialog.present(Some(parent));
    }

    /// A resolved document leaves the view; an empty conflict view is nothing to read.
    pub fn resolved(&self, id: &str) {
        if let Some(block) = self.blocks.borrow_mut().remove(id) {
            self.list.remove(&block);
        }
        if self.blocks.borrow().is_empty() {
            self.dialog.close();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::StagedField;

    fn field(name: &str, loaded: &str, typed: &str) -> StagedField {
        StagedField {
            field: name.to_owned(),
            value: MutationValue::Str(typed.to_owned()),
            loaded: Some(MutationValue::Str(loaded.to_owned())),
        }
    }

    fn staged(id: &str, sets: Vec<StagedField>) -> StagedDocument {
        StagedDocument {
            id: id.to_owned(),
            key: vec![FieldValue {
                field: "_id".to_owned(),
                value: MutationValue::Str(id.to_owned()),
            }],
            sets,
            ..StagedDocument::default()
        }
    }

    fn editable() -> EditableResult {
        EditableResult {
            identity: vec!["_index".to_owned(), "_id".to_owned()],
            guard: vec!["_seq_no".to_owned(), "_primary_term".to_owned()],
            root: "_source".to_owned(),
            atomic_batch: false,
        }
    }

    fn server(json: &str) -> Vec<ServerDocument> {
        ServerDocument::decode_all(json).expect("a re-read")
    }

    #[test]
    fn a_rebase_guard_is_the_version_the_user_is_being_shown() {
        let documents = server(
            r#"{"documents":[{"found":true,"envelope":{"_seq_no":44,"_primary_term":3},
                 "fields":{"status":"claimed"}}]}"#,
        );
        let review = ConflictReview::build(
            &[staged("a", vec![field("status", "open", "done")])],
            &documents,
            &editable(),
        );
        let document = &review.documents[0];
        assert!(document.can_rebase);
        assert_eq!(
            document.rebase_guard,
            vec![
                FieldValue {
                    field: "_seq_no".to_owned(),
                    value: MutationValue::I64(44)
                },
                FieldValue {
                    field: "_primary_term".to_owned(),
                    value: MutationValue::I64(3)
                },
            ],
            "re-guarded against 44/3 — never against the 41 that was just refused"
        );
        assert_eq!(document.contested(), 1);
    }

    #[test]
    fn a_half_returned_guard_cannot_be_rebased_onto() {
        let documents = server(
            r#"{"documents":[{"found":true,"envelope":{"_seq_no":44},"fields":{"status":"x"}}]}"#,
        );
        let review = ConflictReview::build(
            &[staged("a", vec![field("status", "open", "done")])],
            &documents,
            &editable(),
        );
        assert!(
            !review.documents[0].can_rebase,
            "a missing _primary_term could only be re-sent unguarded"
        );
        assert!(review.documents[0].rebase_guard.is_empty());
    }

    #[test]
    fn a_document_somebody_deleted_offers_no_version_to_re_apply_onto() {
        let review = ConflictReview::build(
            &[staged("a", vec![field("status", "open", "done")])],
            &server(r#"{"documents":[{"found":false}]}"#),
            &editable(),
        );
        assert!(review.documents[0].gone);
        assert!(!review.documents[0].can_rebase);
    }

    #[test]
    fn an_untouched_field_is_not_reported_as_contested() {
        let review = ConflictReview::build(
            &[staged("a", vec![field("status", "open", "done")])],
            &server(
                r#"{"documents":[{"found":true,"envelope":{"_seq_no":44,"_primary_term":3},
                     "fields":{"status":"open"}}]}"#,
            ),
            &editable(),
        );
        assert_eq!(review.documents[0].contested(), 0);
        assert!(summary(&review.documents[0]).contains("overwrites nothing of theirs"));
    }

    #[test]
    fn a_field_that_became_a_nested_shape_counts_as_moved() {
        let review = ConflictReview::build(
            &[staged("a", vec![field("tags", "one", "two")])],
            &server(
                r#"{"documents":[{"found":true,"envelope":{"_seq_no":44,"_primary_term":3},
                     "fields":{"tags":["one","two"]}}]}"#,
            ),
            &editable(),
        );
        assert_eq!(review.documents[0].contested(), 1);
        assert_eq!(review.documents[0].fields[0].server.display(), "an array");
    }

    #[test]
    fn a_short_re_read_pairs_only_what_lines_up() {
        let review = ConflictReview::build(
            &[staged("a", Vec::new()), staged("b", Vec::new())],
            &server(r#"{"documents":[{"found":true}]}"#),
            &editable(),
        );
        assert_eq!(review.documents.len(), 1);
    }
}
