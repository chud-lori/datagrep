import DatagrepKit
import Foundation
import SwiftUI

/// Where one staged document stands.
///
/// Everything except `.applied` is still owed to the server, which is what
/// makes a halted batch resumable: the rows the engine never attempted are not
/// an error to dismiss, they are work still queued.
enum StagedState: Equatable {
    case pending
    case applied
    /// The document changed on the server after it was loaded, so the guard
    /// refused the write. Nothing was written.
    case conflicted(String)
    case failed(String)
    /// The batch halted before this one. Nothing was written and nothing was
    /// lost — it is simply still pending, and says so out loud rather than
    /// silently reverting to `.pending`.
    case notAttempted

    var isDone: Bool { self == .applied }
}

/// One field's staged write, with the value it was typed over.
///
/// The loaded value is kept rather than re-read from the grid later, because it
/// is half of what a version conflict has to show: "the value you loaded" is
/// the value that was on screen at the moment of the edit, and the grid can be
/// re-queried out from under it. It is also what says whether a conflict
/// actually touched this field or only the document around it.
struct StagedField {
    let field: String
    var value: MutationValue
    /// What the cell held when it was typed into, or `nil` when the field was
    /// not on the row at all — absent and null are different facts.
    var loaded: MutationValue?
}

/// One document's staged changes, addressed the way the engine addresses it.
///
/// The address is captured when the edit is staged, never at commit time: the
/// `expect` values are the version that was on screen when the user typed, and
/// refreshing them just before the write would compare the server against
/// itself. The one exception is an explicit rebase, where re-guarding against
/// the version the user has just been shown is the whole point.
struct StagedDocument: Identifiable {
    /// Identity values joined — stable across a re-render, unlike a row index.
    let id: String
    let key: [(field: String, value: MutationValue)]
    private(set) var expect: [(field: String, value: MutationValue)]
    /// The grid row this was staged from. Display only: rows are re-numbered
    /// by the next query, and the address above is what a write uses.
    let row: Int

    /// Field name → what was typed. Ordered by first edit so the review list
    /// reads in the order the user worked.
    var sets: [StagedField] = []
    var isDelete = false
    var state: StagedState = .pending

    var isPending: Bool { !state.isDone }

    var isConflicted: Bool {
        if case .conflicted = state { return true }
        return false
    }

    func value(of field: String) -> MutationValue? {
        sets.first { $0.field == field }?.value
    }

    /// Re-guard this document against a version the user has been shown.
    ///
    /// Only ever called from the conflict view, and only with guard values that
    /// came from a re-read the user has just looked at — which is what makes it
    /// a rebase rather than the silent retry the guard exists to prevent.
    mutating func rebase(onto expect: [(field: String, value: MutationValue)]) {
        self.expect = expect
        state = .pending
    }

    var mutation: DocumentMutation {
        DocumentMutation(
            // An update addresses its row by `key`; `path` is where a *new*
            // document would go, and nothing here inserts one. Sent empty
            // rather than guessed at from an identity field this layer would
            // have to recognise by name.
            path: [],
            key: key,
            expect: expect,
            sets: sets.map { (field: $0.field, value: $0.value) },
            isDelete: isDelete)
    }

    /// The address a re-read uses — the same key the write is addressed by.
    var address: DocumentAddress { DocumentAddress(key: key) }
}

/// Every edit typed into the grid and not yet committed.
///
/// Keyed by document identity rather than by row, because a row number is a
/// position in one result and the write is addressed to a document. Cleared
/// wholesale when a new result arrives: the values it was diffed against are
/// gone with it.
@MainActor
final class PendingEdits: ObservableObject {
    /// Staging order, which is also commit order — the report can then say
    /// "#3 failed, #1 and #2 are written" and mean the list the user saw.
    @Published private(set) var documents: [StagedDocument] = []
    /// Grid row → document id, so the grid can ask "is this row staged?" per
    /// cell without searching.
    private var rowIndex: [Int: String] = [:]

    /// Documents still owed to the server. The applied ones stay in the list
    /// so their new values keep showing in the grid, but they are not work.
    var pending: [StagedDocument] { documents.filter(\.isPending) }
    var pendingCount: Int { pending.count }
    var isEmpty: Bool { documents.isEmpty }

    var deleteCount: Int { pending.filter(\.isDelete).count }
    var updateCount: Int { pending.filter { !$0.isDelete }.count }

    /// Documents whose last commit was refused by the guard. They are still
    /// staged, and they are the ones the conflict view resolves.
    var conflicted: [StagedDocument] { documents.filter(\.isConflicted) }
    var conflictCount: Int { conflicted.count }

    func document(atRow row: Int) -> StagedDocument? {
        guard let id = rowIndex[row] else { return nil }
        return documents.first { $0.id == id }
    }

    /// The staged value for one cell, or nil when nothing was typed there.
    func value(row: Int, field: String) -> MutationValue? {
        document(atRow: row)?.value(of: field)
    }

    func isDeleted(row: Int) -> Bool { document(atRow: row)?.isDelete ?? false }

    /// Stage one typed cell. Replaces an earlier value for the same field —
    /// the last thing typed is what gets written, not both.
    func stage(
        id: String, row: Int,
        key: [(field: String, value: MutationValue)],
        expect: [(field: String, value: MutationValue)],
        field: String, value: MutationValue, loaded: MutationValue?
    ) {
        var doc = existing(id: id, row: row, key: key, expect: expect)
        if let at = doc.sets.firstIndex(where: { $0.field == field }) {
            // Retyping a field keeps the value it was *first* typed over: that
            // is the version the guard was taken against, and the one a
            // conflict has to be explained against.
            doc.sets[at].value = value
        } else {
            doc.sets.append(StagedField(field: field, value: value, loaded: loaded))
        }
        // A row edited again after a failed commit is pending again; leaving it
        // marked failed would report a stale verdict on a value nobody has
        // tried to write yet.
        doc.state = .pending
        put(doc, row: row)
    }

    /// Stage a whole document for deletion. Its field edits are kept, not
    /// dropped: undoing the delete has to give them back, and a delete that
    /// silently threw away typed values would be the worse surprise.
    func stageDelete(
        id: String, row: Int,
        key: [(field: String, value: MutationValue)],
        expect: [(field: String, value: MutationValue)]
    ) {
        var doc = existing(id: id, row: row, key: key, expect: expect)
        doc.isDelete = true
        doc.state = .pending
        put(doc, row: row)
    }

    /// Drop one field's staged value — what typing the loaded value back in
    /// means. A document left with nothing staged stops being staged at all,
    /// rather than lingering in the batch as a write that changes nothing.
    func unstage(row: Int, field: String) {
        guard let id = rowIndex[row], let at = documents.firstIndex(where: { $0.id == id })
        else { return }
        documents[at].sets.removeAll { $0.field == field }
        if documents[at].sets.isEmpty && !documents[at].isDelete {
            documents.remove(at: at)
            rowIndex[row] = nil
        }
    }

    /// Drop everything staged for one row.
    func discard(row: Int) {
        guard let id = rowIndex[row] else { return }
        documents.removeAll { $0.id == id }
        rowIndex[row] = nil
    }

    func discardAll() {
        documents.removeAll()
        rowIndex.removeAll()
    }

    // MARK: - resolving a version conflict

    /// Re-apply one document's staged edits onto the version the server holds
    /// now, given the guard values from a re-read the user has just been shown.
    ///
    /// The typed values are untouched — a rebase is "write what I typed, onto
    /// what is there now", which is only a defensible offer *because* the three
    /// readings were on screen when it was chosen. Returns the grid row to
    /// repaint, or nil when the document is no longer staged.
    @discardableResult
    func rebase(id: String, onto expect: [(field: String, value: MutationValue)]) -> Int? {
        guard let at = documents.firstIndex(where: { $0.id == id }) else { return nil }
        documents[at].rebase(onto: expect)
        return documents[at].row
    }

    /// Drop one whole document's staged edits — "discard mine". Returns the
    /// grid row to repaint, or nil when it was not staged.
    @discardableResult
    func discard(id: String) -> Int? {
        guard let at = documents.firstIndex(where: { $0.id == id }) else { return nil }
        let row = documents[at].row
        documents.remove(at: at)
        rowIndex[row] = nil
        return row
    }

    /// Fold a commit report back into the staging list.
    ///
    /// Matched by position, not by document id: the engine returns exactly one
    /// row per submitted mutation, in submission order, and `committed` is the
    /// list this batch was built from. Matching by id would need this layer to
    /// know which identity field *is* the id, and would tie two documents
    /// together whenever two indices happen to use the same one.
    ///
    /// Applied documents keep their typed values on screen — the grid still
    /// holds the rows as they were loaded, so dropping the mark would show the
    /// pre-edit value as though nothing had happened. Everything else stays
    /// pending, carrying its reason.
    ///
    /// A report that does not line up one-for-one is not guessed at: nothing is
    /// folded in, and every document stays pending, which is the safe reading —
    /// a document wrongly marked applied would drop out of the next commit.
    func apply(_ report: MutationReport, committed: [String]) -> Bool {
        guard report.rows.count == committed.count else { return false }
        for (id, row) in zip(committed, report.rows) {
            guard let at = documents.firstIndex(where: { $0.id == id }) else { continue }
            switch row.outcome {
            case .applied:
                documents[at].state = .applied
            case .notAttempted:
                documents[at].state = .notAttempted
            case .failed:
                let message = row.error ?? "the write failed"
                documents[at].state = row.conflict ? .conflicted(message) : .failed(message)
            }
        }
        return true
    }

    // MARK: - internals

    private func existing(
        id: String, row: Int,
        key: [(field: String, value: MutationValue)],
        expect: [(field: String, value: MutationValue)]
    ) -> StagedDocument {
        if let found = documents.first(where: { $0.id == id }) { return found }
        return StagedDocument(id: id, key: key, expect: expect, row: row)
    }

    private func put(_ doc: StagedDocument, row: Int) {
        if let at = documents.firstIndex(where: { $0.id == doc.id }) {
            documents[at] = doc
        } else {
            documents.append(doc)
        }
        rowIndex[row] = doc.id
    }
}

// MARK: - the wire format, checkable without a cluster

/// `--dump-mutation` / `--dump-reread`: print the JSON this app's encoders build
/// for one representative edit and for the re-read that resolves its conflict,
/// then exit.
///
/// The batch blob is hand-encoded against serde's externally-tagged spelling
/// (`FieldPath` is `[{"Field":"_id"}]`, a `Value` is `{"Str":"x"}`), and a
/// single wrong bracket there fails at the engine's parser with the whole edit
/// already typed. There is no Swift test target to pin it from this side, so
/// this prints exactly what would be sent, and the engine's own test parses that
/// string back and compiles it —
/// `the_json_the_macos_grid_sends_parses_and_compiles_to_a_guarded_write` in
/// `crates/datagrep-ffi/src/mutate.rs`. Same family as `--diag` and `--window-size`: an affordance
/// for looking at something that is otherwise only observable against a live
/// server.
enum MutationProbe {
    static func runIfRequested() -> Bool {
        if ProcessInfo.processInfo.arguments.contains("--dump-mutation") {
            dumpMutation()
            return true
        }
        if ProcessInfo.processInfo.arguments.contains("--dump-reread") {
            dumpReread()
            return true
        }
        return false
    }

    private static func dumpMutation() {
        let update = DocumentMutation(
            path: [],
            key: [
                ("_index", .string("events")),
                ("_id", .string("abc")),
                ("_routing", .string("tenant-7")),
            ],
            expect: [("_seq_no", .int(41)), ("_primary_term", .int(3))],
            sets: [
                ("status", .string("done")),
                ("retries", .int(2)),
                ("score", .double(1.5)),
                ("archived", .bool(true)),
            ],
            isDelete: false)
        let delete = DocumentMutation(
            path: [],
            key: [("_index", .string("events")), ("_id", .string("gone"))],
            expect: [("_seq_no", .int(7)), ("_primary_term", .int(1))],
            sets: [],
            isDelete: true)
        let text = (try? MutationBatch.json([update, delete])) ?? "<encoding failed>"
        FileHandle.standardOutput.write(Data((text + "\n").utf8))
    }

    /// `--dump-reread`: the address list the conflict view sends to ask what the
    /// server holds now. Pinned by `the_json_the_macos_grid_sends_parses_into_addresses`
    /// in `crates/datagrep-ffi/src/reread.rs` for the same reason as the batch
    /// above — and the keys are the same two keys, because a re-read addresses a
    /// document exactly as its write did.
    private static func dumpReread() {
        let addresses = [
            DocumentAddress(key: [
                ("_index", .string("events")),
                ("_id", .string("abc")),
                ("_routing", .string("tenant-7")),
            ]),
            DocumentAddress(key: [("_index", .string("events")), ("_id", .string("gone"))]),
        ]
        let text = (try? DocumentAddressBatch.json(addresses)) ?? "<encoding failed>"
        FileHandle.standardOutput.write(Data((text + "\n").utf8))
    }
}
