import AppKit
import DatagrepKit
import Foundation
import SwiftUI

/// The view model behind `HistoryPanel`.
@MainActor
final class HistoryModel: ObservableObject {
    let store: QueryHistoryStore

    @Published private(set) var entries: [QueryHistoryEntry] = []

    @Published var search: String = ""
    @Published var connectionFilter: String? = nil
    @Published var range: HistoryDateRange = .all
    @Published var outcomeFilter: QueryOutcome? = nil
    @Published var selectedID: String? = nil

    @Published var isPresented = false

    @Published private(set) var retention: HistoryRetention

    var onOpenInEditor: ((String, String?) -> Void)?
    var onRerun: ((String, String?) -> Void)?
    /// Short confirmations ("copied", "3 entries removed") for the status bar.
    var onStatus: ((String) -> Void)?

    private var pending: PendingRun?

    /// Bumped whenever the snapshot is replaced; part of the memoisation key.
    private var revision = 0
    private var derivedKey = "\u{0}"
    private var cachedFiltered: [QueryHistoryEntry] = []
    private var cachedDays: [HistoryDay] = []
    private var cachedConnections: [String] = []

    private struct PendingRun {
        let sql: String
        let connection: String
        let engine: String
        let startedAt: Date
        var recorded = false
    }

    init(store: QueryHistoryStore = QueryHistoryStore()) {
        self.store = store
        self.retention = store.retention
        store.onChange { [weak self] snapshot in
            // Already hopped to the main queue by the store.
            MainActor.assumeIsolated {
                guard let self else { return }
                self.entries = snapshot
                self.revision &+= 1
            }
        }
        store.load()
    }

    // MARK: - recording (called from the query path, after the fact)

    func executionStarted(sql: String, connection: String, engine: String) {
        let text = sql.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !text.isEmpty else { return }
        pending = PendingRun(sql: sql, connection: connection, engine: engine, startedAt: Date())
    }

    func executionProgressed(
        state: QueryState?, rowsLoaded: UInt64, elapsedMs: UInt64, error: String?
    ) {
        guard let state, state.isTerminal, var run = pending, !run.recorded else { return }
        run.recorded = true
        pending = run

        let outcome: QueryOutcome
        switch state {
        case .failed: outcome = .error
        case .cancelled: outcome = .cancelled
        default: outcome = .ok
        }
        commit(
            run,
            durationMs: Int(elapsedMs),
            rowCount: Int(rowsLoaded),
            outcome: outcome,
            error: outcome == .error ? error : nil)
    }

    func executionFailedToStart(_ message: String) {
        guard var run = pending, !run.recorded else { return }
        run.recorded = true
        pending = run
        commit(
            run,
            durationMs: Int(Date().timeIntervalSince(run.startedAt) * 1000),
            rowCount: nil,
            outcome: .error,
            error: message)
    }

    func executionBlocked(sql: String, connection: String, engine: String, reason: String) {
        pending = nil
        store.record(
            QueryHistoryEntry(
                sql: sql, connection: connection, engine: engine, startedAt: Date(),
                durationMs: 0, rowCount: nil, outcome: .error, error: reason))
    }

    private func commit(
        _ run: PendingRun, durationMs: Int, rowCount: Int?, outcome: QueryOutcome, error: String?
    ) {
        store.record(
            QueryHistoryEntry(
                sql: run.sql,
                connection: run.connection,
                engine: run.engine,
                startedAt: run.startedAt,
                durationMs: durationMs,
                rowCount: rowCount,
                outcome: outcome,
                error: error))
    }

    // MARK: - derived view state

    var filter: HistoryFilter {
        HistoryFilter(
            text: search, connection: connectionFilter, range: range, outcome: outcomeFilter)
    }

    var filtered: [QueryHistoryEntry] {
        refreshDerivedIfNeeded()
        return cachedFiltered
    }

    var days: [HistoryDay] {
        refreshDerivedIfNeeded()
        return cachedDays
    }

    var knownConnections: [String] {
        refreshDerivedIfNeeded()
        return cachedConnections
    }

    private func refreshDerivedIfNeeded() {
        let key =
            "\(revision)|\(search)|\(connectionFilter ?? "")|\(range.rawValue)|\(outcomeFilter?.rawValue ?? "")"
        guard key != derivedKey else { return }
        derivedKey = key
        cachedFiltered = QueryHistoryStore.filter(entries, with: filter)
        cachedDays = QueryHistoryStore.group(cachedFiltered)
        cachedConnections = QueryHistoryStore.connections(in: entries)
    }

    var selected: QueryHistoryEntry? {
        guard let selectedID else { return nil }
        return entries.first { $0.id == selectedID }
    }

    var hasFilter: Bool { !filter.isEmpty }

    func clearFilters() {
        search = ""
        connectionFilter = nil
        range = .all
        outcomeFilter = nil
    }

    // MARK: - actions

    func copy(_ entry: QueryHistoryEntry) {
        let pb = NSPasteboard.general
        pb.clearContents()
        pb.setString(entry.sql, forType: .string)
        onStatus?("copied \(entry.sql.count) characters of SQL")
    }

    func openInEditor(_ entry: QueryHistoryEntry) {
        guard let onOpenInEditor else {
            onStatus?("nothing is wired up to open an editor tab yet")
            return
        }
        onOpenInEditor(entry.sql, entry.connection.isEmpty ? nil : entry.connection)
        isPresented = false
    }

    func rerun(_ entry: QueryHistoryEntry) {
        guard let onRerun else {
            onStatus?("nothing is wired up to run a statement yet")
            return
        }
        onRerun(entry.sql, entry.connection.isEmpty ? nil : entry.connection)
        isPresented = false
    }

    func delete(_ entry: QueryHistoryEntry) {
        if selectedID == entry.id { selectedID = nil }
        store.delete(ids: [entry.id])
    }

    func clearAll() {
        selectedID = nil
        store.clear()
        onStatus?("query history cleared")
    }

    func clearCurrentConnection() {
        guard let c = connectionFilter else { return }
        selectedID = nil
        store.clear(connection: c)
        onStatus?("history cleared for \(c)")
    }

    func setRetention(_ new: HistoryRetention) {
        retention = new
        store.setRetention(new)
    }
}
