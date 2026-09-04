use std::cell::RefCell;
use std::collections::HashMap;

use glib::prelude::*;
use glib::subclass::prelude::*;

use crate::model::mutation::{
    Address, DocumentAddress, DocumentMutation, FieldValue, MutationOutcome, MutationReport,
    MutationValue,
};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum StagedState {
    #[default]
    Pending,
    Applied,
    /// The document changed on the server, so the guard refused the write.
    Conflicted,
    Failed,
    /// The batch halted before this one: still pending, nothing written.
    NotAttempted,
}

#[derive(Debug, Clone, PartialEq)]
pub struct StagedField {
    pub field: String,
    pub value: MutationValue,
    pub loaded: Option<MutationValue>,
}

#[derive(Debug, Clone, Default)]
pub struct StagedDocument {
    pub id: String,
    pub key: Vec<FieldValue>,
    pub expect: Vec<FieldValue>,
    pub row: u64,
    pub sets: Vec<StagedField>,
    pub is_delete: bool,
    pub state: StagedState,
    pub message: String,
}

impl StagedDocument {
    pub fn is_pending(&self) -> bool {
        self.state != StagedState::Applied
    }

    pub fn is_conflicted(&self) -> bool {
        self.state == StagedState::Conflicted
    }

    pub fn value_of(&self, field: &str) -> Option<&MutationValue> {
        self.sets
            .iter()
            .find(|set| set.field == field)
            .map(|set| &set.value)
    }

    pub fn mutation(&self) -> DocumentMutation {
        DocumentMutation {
            path: Vec::new(),
            key: self.key.clone(),
            expect: self.expect.clone(),
            sets: if self.is_delete {
                Vec::new()
            } else {
                self.sets
                    .iter()
                    .map(|set| FieldValue {
                        field: set.field.clone(),
                        value: set.value.clone(),
                    })
                    .collect()
            },
            is_delete: self.is_delete,
        }
    }

    /// A re-read addresses a document with the very same key its write did.
    pub fn address(&self) -> DocumentAddress {
        DocumentAddress {
            key: self.key.clone(),
        }
    }

    /// The identity, spelled the way the engine spells it.
    pub fn title(&self) -> String {
        self.key
            .iter()
            .map(|fv| format!("{}={}", fv.field, fv.value.display()))
            .collect::<Vec<_>>()
            .join("  ")
    }
}

mod imp {
    use super::*;
    use glib::subclass::Signal;
    use std::sync::OnceLock;

    #[derive(Default)]
    pub struct PendingEdits {
        pub documents: RefCell<Vec<StagedDocument>>,
        // Grid row -> document id, so per-cell "is this row staged?" is one hash lookup.
        pub rows: RefCell<HashMap<u64, String>>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for PendingEdits {
        const NAME: &'static str = "DgPendingEdits";
        type Type = super::PendingEdits;
    }

    impl ObjectImpl for PendingEdits {
        fn signals() -> &'static [Signal] {
            static SIGNALS: OnceLock<Vec<Signal>> = OnceLock::new();
            SIGNALS.get_or_init(|| vec![Signal::builder("staging-changed").build()])
        }
    }
}

glib::wrapper! {
    /// Every edit typed into the grid and not yet committed.
    pub struct PendingEdits(ObjectSubclass<imp::PendingEdits>);
}

impl Default for PendingEdits {
    fn default() -> Self {
        Self::new()
    }
}

impl PendingEdits {
    pub fn new() -> Self {
        glib::Object::new()
    }

    pub fn is_empty(&self) -> bool {
        self.imp().documents.borrow().is_empty()
    }

    pub fn len(&self) -> usize {
        self.imp().documents.borrow().len()
    }

    pub fn pending(&self) -> Vec<StagedDocument> {
        self.filtered(StagedDocument::is_pending)
    }

    /// Documents whose last commit the guard refused — still staged.
    pub fn conflicted(&self) -> Vec<StagedDocument> {
        self.filtered(StagedDocument::is_conflicted)
    }

    pub fn counts(&self) -> StagedCounts {
        let documents = self.imp().documents.borrow();
        let mut counts = StagedCounts::default();
        for document in documents.iter() {
            if document.is_pending() {
                counts.pending += 1;
                if document.is_delete {
                    counts.deletes += 1;
                } else {
                    counts.updates += 1;
                }
            } else {
                counts.written += 1;
            }
            if document.is_conflicted() {
                counts.conflicts += 1;
            }
        }
        counts
    }

    /// Borrowed, not cloned: this runs once per bound cell.
    pub fn with_row<R>(&self, row: u64, f: impl FnOnce(Option<&StagedDocument>) -> R) -> R {
        let documents = self.imp().documents.borrow();
        let found = self
            .imp()
            .rows
            .borrow()
            .get(&row)
            .and_then(|id| documents.iter().find(|d| &d.id == id));
        f(found)
    }

    pub fn is_staged(&self, row: u64) -> bool {
        self.with_row(row, |doc| doc.is_some())
    }

    pub fn is_deleted(&self, row: u64) -> bool {
        self.with_row(row, |doc| doc.is_some_and(|d| d.is_delete))
    }

    pub fn stage(
        &self,
        address: Address,
        row: u64,
        field: &str,
        value: MutationValue,
        loaded: Option<MutationValue>,
    ) {
        let mut document = self.existing(address, row);
        match document.sets.iter_mut().find(|set| set.field == field) {
            // Retyping keeps the loaded value it was FIRST typed over.
            Some(set) => set.value = value,
            None => document.sets.push(StagedField {
                field: field.to_owned(),
                value,
                loaded,
            }),
        }
        document.state = StagedState::Pending;
        document.message.clear();
        self.put(document, row);
    }

    /// Field edits are kept, not dropped: undoing the delete gives them back.
    pub fn stage_delete(&self, address: Address, row: u64) {
        let mut document = self.existing(address, row);
        document.is_delete = true;
        document.state = StagedState::Pending;
        document.message.clear();
        self.put(document, row);
    }

    pub fn unstage(&self, row: u64, field: &str) {
        let imp = self.imp();
        let Some(at) = self.index_of_row(row) else {
            return;
        };
        let mut documents = imp.documents.borrow_mut();
        documents[at].sets.retain(|set| set.field != field);
        if documents[at].sets.is_empty() && !documents[at].is_delete {
            documents.remove(at);
            imp.rows.borrow_mut().remove(&row);
        }
        drop(documents);
        self.changed();
    }

    pub fn discard_row(&self, row: u64) {
        let imp = self.imp();
        if let Some(at) = self.index_of_row(row) {
            imp.documents.borrow_mut().remove(at);
        }
        imp.rows.borrow_mut().remove(&row);
        self.changed();
    }

    pub fn discard_all(&self) {
        let imp = self.imp();
        if imp.documents.borrow().is_empty() {
            return;
        }
        imp.documents.borrow_mut().clear();
        imp.rows.borrow_mut().clear();
        self.changed();
    }

    /// Lifted out whole, so another tab's result can take the screen without
    /// this one's edits landing on it.
    pub fn take_all(&self) -> Vec<StagedDocument> {
        let imp = self.imp();
        let documents = std::mem::take(&mut *imp.documents.borrow_mut());
        imp.rows.borrow_mut().clear();
        if !documents.is_empty() {
            self.changed();
        }
        documents
    }

    pub fn restore_all(&self, documents: Vec<StagedDocument>) {
        let imp = self.imp();
        *imp.rows.borrow_mut() = documents.iter().map(|d| (d.row, d.id.clone())).collect();
        let empty = documents.is_empty();
        *imp.documents.borrow_mut() = documents;
        if !empty {
            self.changed();
        }
    }

    /// The rows every staged document sits on, for the one redraw a discard needs.
    pub fn rows(&self) -> Vec<u64> {
        self.imp()
            .documents
            .borrow()
            .iter()
            .map(|d| d.row)
            .collect()
    }

    /// Re-guards an edit against a version the user has just been shown; still unwritten.
    pub fn rebase(&self, id: &str, expect: Vec<FieldValue>) -> Option<u64> {
        let at = self.index_of(id)?;
        let row = {
            let mut documents = self.imp().documents.borrow_mut();
            documents[at].expect = expect;
            documents[at].state = StagedState::Pending;
            documents[at].message.clear();
            documents[at].row
        };
        self.changed();
        Some(row)
    }

    pub fn discard_by_id(&self, id: &str) -> Option<u64> {
        let at = self.index_of(id)?;
        let imp = self.imp();
        let row = imp.documents.borrow_mut().remove(at).row;
        imp.rows.borrow_mut().remove(&row);
        self.changed();
        Some(row)
    }

    /// Matched by position; a report that does not line up one-for-one folds nothing in.
    pub fn apply(&self, report: &MutationReport, committed: &[String]) -> bool {
        if report.rows.len() != committed.len() {
            return false;
        }
        for (id, row) in committed.iter().zip(&report.rows) {
            let Some(at) = self.index_of(id) else {
                continue;
            };
            let mut documents = self.imp().documents.borrow_mut();
            let document = &mut documents[at];
            match row.outcome {
                MutationOutcome::Applied => {
                    document.state = StagedState::Applied;
                    document.message.clear();
                }
                MutationOutcome::NotAttempted => {
                    document.state = StagedState::NotAttempted;
                    document.message.clear();
                }
                MutationOutcome::Failed => {
                    document.state = if row.conflict {
                        StagedState::Conflicted
                    } else {
                        StagedState::Failed
                    };
                    document.message = if row.error.is_empty() {
                        "the write failed".to_owned()
                    } else {
                        row.error.clone()
                    };
                }
            }
        }
        self.changed();
        true
    }

    pub fn connect_staging_changed<F: Fn(&Self) + 'static>(&self, f: F) -> glib::SignalHandlerId {
        self.connect_local("staging-changed", false, move |values| {
            let edits = values[0]
                .get::<Self>()
                .expect("the signal carries the staging set");
            f(&edits);
            None
        })
    }

    fn filtered(&self, keep: impl Fn(&StagedDocument) -> bool) -> Vec<StagedDocument> {
        self.imp()
            .documents
            .borrow()
            .iter()
            .filter(|d| keep(d))
            .cloned()
            .collect()
    }

    fn existing(&self, address: Address, row: u64) -> StagedDocument {
        match self.index_of(&address.id) {
            Some(at) => self.imp().documents.borrow()[at].clone(),
            None => StagedDocument {
                id: address.id,
                key: address.key,
                expect: address.expect,
                row,
                ..StagedDocument::default()
            },
        }
    }

    fn put(&self, document: StagedDocument, row: u64) {
        let imp = self.imp();
        match self.index_of(&document.id) {
            Some(at) => imp.documents.borrow_mut()[at] = document.clone(),
            None => imp.documents.borrow_mut().push(document.clone()),
        }
        imp.rows.borrow_mut().insert(row, document.id);
        self.changed();
    }

    fn index_of(&self, id: &str) -> Option<usize> {
        self.imp()
            .documents
            .borrow()
            .iter()
            .position(|d| d.id == id)
    }

    fn index_of_row(&self, row: u64) -> Option<usize> {
        let id = self.imp().rows.borrow().get(&row).cloned()?;
        self.index_of(&id)
    }

    fn changed(&self) {
        self.emit_by_name::<()>("staging-changed", &[]);
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct StagedCounts {
    pub pending: u32,
    pub written: u32,
    pub updates: u32,
    pub deletes: u32,
    pub conflicts: u32,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::mutation::MutationRow;

    fn guard(seq: i64) -> Vec<FieldValue> {
        vec![FieldValue {
            field: "_seq_no".to_owned(),
            value: MutationValue::I64(seq),
        }]
    }

    fn address(id: &str, seq: i64) -> Address {
        Address {
            id: id.to_owned(),
            key: vec![FieldValue {
                field: "_id".to_owned(),
                value: MutationValue::Str(id.to_owned()),
            }],
            expect: guard(seq),
        }
    }

    fn edits() -> PendingEdits {
        let edits = PendingEdits::new();
        edits.stage(
            address("a", 41),
            0,
            "status",
            MutationValue::Str("done".to_owned()),
            Some(MutationValue::Str("open".to_owned())),
        );
        edits.stage(
            address("b", 7),
            1,
            "status",
            MutationValue::Str("done".to_owned()),
            Some(MutationValue::Str("open".to_owned())),
        );
        edits
    }

    fn report(outcomes: &[(MutationOutcome, bool)]) -> MutationReport {
        MutationReport {
            rows: outcomes
                .iter()
                .map(|&(outcome, conflict)| MutationRow {
                    outcome,
                    conflict,
                    ..MutationRow::default()
                })
                .collect(),
            ..MutationReport::default()
        }
    }

    #[test]
    fn retyping_a_field_keeps_the_value_it_was_first_typed_over() {
        let edits = edits();
        edits.stage(
            address("a", 41),
            0,
            "status",
            MutationValue::Str("claimed".to_owned()),
            Some(MutationValue::Str("WRONG".to_owned())),
        );
        edits.with_row(0, |doc| {
            let sets = &doc.expect("row 0 is staged").sets;
            assert_eq!(sets.len(), 1);
            assert_eq!(sets[0].value, MutationValue::Str("claimed".to_owned()));
            assert_eq!(sets[0].loaded, Some(MutationValue::Str("open".to_owned())));
        });
    }

    #[test]
    fn unstaging_the_last_field_of_a_row_drops_the_document() {
        let edits = edits();
        edits.unstage(0, "status");
        assert!(!edits.is_staged(0));
        assert_eq!(edits.len(), 1);
    }

    #[test]
    fn a_staged_delete_survives_unstaging_its_fields() {
        let edits = edits();
        edits.stage_delete(address("a", 41), 0);
        edits.unstage(0, "status");
        assert!(edits.is_deleted(0), "the delete outlives the field edits");
    }

    #[test]
    fn a_delete_sends_no_sets_however_many_fields_were_typed() {
        let edits = edits();
        edits.stage_delete(address("a", 41), 0);
        let mutation = edits.pending()[0].mutation();
        assert!(mutation.is_delete);
        assert!(mutation.sets.is_empty());
        assert_eq!(mutation.expect.len(), 1, "a delete is guarded too");
    }

    #[test]
    fn not_attempted_rows_stay_staged_for_another_try() {
        let edits = edits();
        let ids = vec!["a".to_owned(), "b".to_owned()];
        assert!(edits.apply(
            &report(&[
                (MutationOutcome::Applied, false),
                (MutationOutcome::NotAttempted, false)
            ]),
            &ids
        ));
        let counts = edits.counts();
        assert_eq!(counts.written, 1);
        assert_eq!(counts.pending, 1, "the one never attempted is still staged");
        assert_eq!(edits.pending()[0].id, "b");
    }

    #[test]
    fn a_report_that_does_not_line_up_one_for_one_folds_nothing_in() {
        let edits = edits();
        assert!(!edits.apply(
            &report(&[(MutationOutcome::Applied, false)]),
            &["a".to_owned(), "b".to_owned()]
        ));
        assert_eq!(edits.counts().pending, 2, "nothing was guessed at");
    }

    #[test]
    fn a_conflict_stays_staged_and_a_rebase_re_guards_it_without_writing() {
        let edits = edits();
        let ids = vec!["a".to_owned(), "b".to_owned()];
        edits.apply(
            &report(&[
                (MutationOutcome::Failed, true),
                (MutationOutcome::Applied, false),
            ]),
            &ids,
        );
        assert_eq!(edits.conflicted().len(), 1);
        assert_eq!(edits.counts().conflicts, 1);

        assert_eq!(edits.rebase("a", guard(44)), Some(0));
        let rebased = &edits.pending()[0];
        assert_eq!(rebased.expect, guard(44), "guarded against what was shown");
        assert_eq!(rebased.state, StagedState::Pending, "still unwritten");
        assert_eq!(edits.counts().conflicts, 0);
    }

    #[test]
    fn discarding_mine_leaves_the_servers_version_untouched_and_the_row_unstaged() {
        let edits = edits();
        assert_eq!(edits.discard_by_id("a"), Some(0));
        assert!(!edits.is_staged(0));
        assert_eq!(edits.len(), 1);
        assert_eq!(edits.discard_by_id("a"), None);
    }

    #[test]
    fn the_counts_the_bar_reads_split_updates_from_deletes() {
        let edits = edits();
        edits.stage_delete(address("b", 7), 1);
        let counts = edits.counts();
        assert_eq!(counts.pending, 2);
        assert_eq!(counts.updates, 1);
        assert_eq!(counts.deletes, 1);
        assert_eq!(counts.written, 0);
    }
}
