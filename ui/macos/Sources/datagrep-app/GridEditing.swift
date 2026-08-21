import DatagrepKit
import Foundation
import SwiftUI

/// Where one staged document stands. Everything except `.applied` is still
/// owed to the server — that is what makes a halted batch resumable.
enum StagedState: Equatable {
    case pending
    case applied
    /// The document changed on the server after it was loaded, so the guard
    /// refused the write. Nothing was written.
    case conflicted(String)
    case failed(String)
    /// The batch halted before this one. Nothing was written and nothing was
    /// lost — still pending, and says so rather than silently reverting.
    case notAttempted

    var isDone: Bool { self == .applied }
}

/// One field's staged write, with the value it was typed over. The loaded
/// value is kept rather than re-read from the grid later: it is half of what
/// a version conflict has to show, and the grid can be re-queried out from
/// under it.
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
/// itself. The one exception is an explicit rebase.
struct StagedDocument: Identifiable {
    /// Identity values joined — stable across a re-render, unlike a row index.
    let id: String
    let key: [(field: String, value: MutationValue)]
    private(set) var expect: [(field: String, value: MutationValue)]
    /// The grid row this was staged from. Display only: rows are re-numbered
    /// by the next query, and the address above is what a write uses.
    let row: Int

    /// Field name → what was typed, ordered by first edit.
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

    /// Re-guard this document against a version the user has been shown. Only
    /// ever called from the conflict view with guard values from a re-read the
    /// user has just looked at — which is what makes it a rebase rather than
    /// the silent retry the guard exists to prevent.
    mutating func rebase(onto expect: [(field: String, value: MutationValue)]) {
        self.expect = expect
        state = .pending
    }

    var mutation: DocumentMutation {
        DocumentMutation(
            // `path` is where a *new* document would go; nothing here inserts
            // one, so it is sent empty rather than guessed.
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
/// Keyed by document identity, not by row — a row number is a position in one
/// result and the write is addressed to a document. Cleared wholesale when a
/// new result arrives: the values it was diffed against are gone with it.
@MainActor
final class PendingEdits: ObservableObject {
    /// Staging order, which is also commit order.
    @Published private(set) var documents: [StagedDocument] = []
    /// Grid row → document id, so per-cell "is this row staged?" is O(1).
    private var rowIndex: [Int: String] = [:]

    /// Documents still owed to the server. The applied ones stay in the list
    /// so their new values keep showing in the grid, but they are not work.
    var pending: [StagedDocument] { documents.filter(\.isPending) }
    var pendingCount: Int { pending.count }
    var isEmpty: Bool { documents.isEmpty }

    var deleteCount: Int { pending.filter(\.isDelete).count }
    var updateCount: Int { pending.filter { !$0.isDelete }.count }

    /// Documents whose last commit the guard refused — still staged, and what
    /// the conflict view resolves.
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

    /// Stage one typed cell. The last thing typed for a field is what gets
    /// written.
    func stage(
        id: String, row: Int,
        key: [(field: String, value: MutationValue)],
        expect: [(field: String, value: MutationValue)],
        field: String, value: MutationValue, loaded: MutationValue?
    ) {
        var doc = existing(id: id, row: row, key: key, expect: expect)
        if let at = doc.sets.firstIndex(where: { $0.field == field }) {
            // Retyping a field keeps the value it was *first* typed over: that
            // is the version the guard was taken against.
            doc.sets[at].value = value
        } else {
            doc.sets.append(StagedField(field: field, value: value, loaded: loaded))
        }
        // A row edited again after a failed commit is pending again; leaving
        // it marked failed would report a stale verdict.
        doc.state = .pending
        put(doc, row: row)
    }

    /// Stage a whole document for deletion. Its field edits are kept, not
    /// dropped: undoing the delete has to give them back.
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

    /// Drop one field's staged value. A document left with nothing staged
    /// stops being staged at all, rather than lingering as a no-op write.
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

    /// Re-guard one document with values from a re-read the user has just been
    /// shown. The typed values are untouched. Returns the grid row to repaint,
    /// or nil when the document is no longer staged.
    @discardableResult
    func rebase(id: String, onto expect: [(field: String, value: MutationValue)]) -> Int? {
        guard let at = documents.firstIndex(where: { $0.id == id }) else { return nil }
        documents[at].rebase(onto: expect)
        return documents[at].row
    }

    /// Drop one whole document's staged edits. Returns the grid row to
    /// repaint, or nil when it was not staged.
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
    /// Matched by position, not by document id: the engine returns one row per
    /// submitted mutation, in submission order. Matching by id would need this
    /// layer to know which identity field *is* the id.
    ///
    /// A report that does not line up one-for-one is not guessed at: nothing
    /// is folded in and every document stays pending — the safe reading, since
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

/// `--dump-mutation` / `--dump-reread`: print the JSON this app's encoders
/// build for one representative edit and its conflict re-read, then exit.
///
/// The batch blob is hand-encoded against serde's externally-tagged spelling,
/// and there is no Swift test target to pin it from this side — so this prints
/// exactly what would be sent, and the engine's own test
/// (`the_json_the_macos_grid_sends_parses_and_compiles_to_a_guarded_write` in
/// `crates/datagrep-ffi/src/mutate.rs`) parses that string back and compiles
/// it.
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

    /// The address list the conflict view sends. Pinned by
    /// `the_json_the_macos_grid_sends_parses_into_addresses` in
    /// `crates/datagrep-ffi/src/reread.rs` — a re-read addresses a document
    /// exactly as its write did.
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
