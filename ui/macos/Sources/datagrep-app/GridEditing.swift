import DatagrepKit
import Foundation
import SwiftUI

enum StagedState: Equatable {
    case pending
    case applied
    case conflicted(String)
    case failed(String)
    case notAttempted

    var isDone: Bool { self == .applied }
}

struct StagedField {
    let field: String
    var value: MutationValue
    var loaded: MutationValue?
}

/// One document's staged changes, addressed the way the engine addresses it.
struct StagedDocument: Identifiable {
    /// Identity values joined — stable across a re-render, unlike a row index.
    let id: String
    let key: [(field: String, value: MutationValue)]
    private(set) var expect: [(field: String, value: MutationValue)]
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

    mutating func rebase(onto expect: [(field: String, value: MutationValue)]) {
        self.expect = expect
        state = .pending
    }

    var mutation: DocumentMutation {
        DocumentMutation(
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
@MainActor
// Addresses are captured at staging; refreshing expect at commit would compare the server against itself.
final class PendingEdits: ObservableObject {
    /// Staging order, which is also commit order.
    @Published private(set) var documents: [StagedDocument] = []
    /// Grid row → document id, so per-cell "is this row staged?" is O(1).
    private var rowIndex: [Int: String] = [:]

    var pending: [StagedDocument] { documents.filter(\.isPending) }
    var pendingCount: Int { pending.count }
    var isEmpty: Bool { documents.isEmpty }

    var deleteCount: Int { pending.filter(\.isDelete).count }
    var updateCount: Int { pending.filter { !$0.isDelete }.count }

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

    func stage(
        id: String, row: Int,
        key: [(field: String, value: MutationValue)],
        expect: [(field: String, value: MutationValue)],
        field: String, value: MutationValue, loaded: MutationValue?
    ) {
        var doc = existing(id: id, row: row, key: key, expect: expect)
        if let at = doc.sets.firstIndex(where: { $0.field == field }) {
            doc.sets[at].value = value
        } else {
            doc.sets.append(StagedField(field: field, value: value, loaded: loaded))
        }
        doc.state = .pending
        put(doc, row: row)
    }

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

    // MARK: - parking staging while another tab's result is on screen

    struct Snapshot {
        fileprivate let documents: [StagedDocument]
        fileprivate let rowIndex: [Int: String]
        static let empty = Snapshot(documents: [], rowIndex: [:])
        var isEmpty: Bool { documents.isEmpty }
    }

    func snapshot() -> Snapshot { Snapshot(documents: documents, rowIndex: rowIndex) }

    func restore(_ snapshot: Snapshot) {
        documents = snapshot.documents
        rowIndex = snapshot.rowIndex
    }

    // MARK: - resolving a version conflict

    @discardableResult
    func rebase(id: String, onto expect: [(field: String, value: MutationValue)]) -> Int? {
        guard let at = documents.firstIndex(where: { $0.id == id }) else { return nil }
        documents[at].rebase(onto: expect)
        return documents[at].row
    }

    @discardableResult
    func discard(id: String) -> Int? {
        guard let at = documents.firstIndex(where: { $0.id == id }) else { return nil }
        let row = documents[at].row
        documents.remove(at: at)
        rowIndex[row] = nil
        return row
    }

    /// Fold a commit report back into the staging list.
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
