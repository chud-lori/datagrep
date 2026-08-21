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

/// One document's staged changes, addressed the way the engine addresses it.
///
/// The address is captured when the edit is staged, never at commit time: the
/// `expect` values are the version that was on screen when the user typed, and
/// refreshing them just before the write would compare the server against
/// itself.
struct StagedDocument: Identifiable {
    /// Identity values joined — stable across a re-render, unlike a row index.
    let id: String
    let key: [(field: String, value: MutationValue)]
    let expect: [(field: String, value: MutationValue)]
    /// The grid row this was staged from. Display only: rows are re-numbered
    /// by the next query, and the address above is what a write uses.
    let row: Int

    /// Field name → what was typed. Ordered by first edit so the review list
    /// reads in the order the user worked.
    var sets: [(field: String, value: MutationValue)] = []
    var isDelete = false
    var state: StagedState = .pending

    var isPending: Bool { !state.isDone }

    func value(of field: String) -> MutationValue? {
        sets.first { $0.field == field }?.value
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
            sets: sets,
            isDelete: isDelete)
    }
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
        field: String, value: MutationValue
    ) {
        var doc = existing(id: id, row: row, key: key, expect: expect)
        if let at = doc.sets.firstIndex(where: { $0.field == field }) {
            doc.sets[at] = (field, value)
        } else {
            doc.sets.append((field, value))
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

/// `--dump-mutation`: print the `MutationBatch` JSON this app's encoder builds
/// for one representative edit, then exit.
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
        guard ProcessInfo.processInfo.arguments.contains("--dump-mutation") else { return false }
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
        return true
    }
}
