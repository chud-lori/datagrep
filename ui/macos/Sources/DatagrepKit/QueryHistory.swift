import Foundation

/// Query history: the automatic log of every statement datagrep actually ran.
///
/// This is deliberately **not** `SavedQueries.swift`. Saved queries are things a
/// person chose to keep and gave a name to; history is a record of what happened,
/// which nobody curates and which must never need curating to stay useful. The
/// UX study is explicit that conflating the two (as several competitors do) is
/// the mistake, and that TablePlus's clean split is the right shape — so these
/// are two stores, two directories, two panels, and they never write to each
/// other's files.
///
/// ## Why this lives in Swift and not in the engine
///
/// `crates/datagrep-profiles` already has a `query_history` table with an FTS5
/// index, a dedupe window and retention trimming — but the C ABI
/// (`crates/datagrep-ffi/include/datagrep.h`) exposes profiles, catalog, query
/// and rows and nothing else. There is no `datagrep_history_*` entry point, so
/// that table is unreachable from Swift today. Rather than block the feature on
/// an ABI change, this store mirrors the engine's schema field-for-field
/// (`text`/`started_at`/`duration_ms`/`row_count`/`status`/`error`) so that when
/// the ABI does arrive the move is a copy, not a redesign — and the CLI and the
/// GUI then share one history instead of keeping two.
///
/// ## On-disk format
///
/// `~/Library/Application Support/datagrep/history/YYYY-MM-DD.jsonl` — one JSON
/// object per line, one file per day, plus `retention.json`.
///
/// One file per day and not one big blob, for the same reason `SavedQueries`
/// keeps one file pair per tab: a truncated blob loses every query you ever ran,
/// a truncated day file loses one day. It also makes retention a `rm` of whole
/// files rather than a rewrite, makes "group by day" free, and leaves the whole
/// thing readable with `cat` — history you cannot inspect is history you cannot
/// trust.

// MARK: - values

/// How a recorded statement finished. Same three cases, same spellings, as
/// `datagrep-profiles`' `HistoryStatus` `CHECK` constraint.
public enum QueryOutcome: String, Codable, Sendable, CaseIterable {
    case ok, error, cancelled

    public var symbol: String {
        switch self {
        case .ok: return "checkmark.circle.fill"
        case .error: return "exclamationmark.octagon.fill"
        case .cancelled: return "stop.circle.fill"
        }
    }

    public var label: String {
        switch self {
        case .ok: return "ok"
        case .error: return "failed"
        case .cancelled: return "cancelled"
        }
    }
}

/// One executed statement.
///
/// `startedAtMs` is epoch milliseconds rather than a `Date` so the JSONL is
/// stable, greppable, and identical in shape to `query_history.started_at`.
public struct QueryHistoryEntry: Codable, Sendable, Identifiable, Equatable {
    public var id: String
    /// The statement as the user ran it, verbatim — never reformatted. Sequel
    /// Ace's history mangles multi-line queries and that is a filed bug.
    public var sql: String
    /// Profile name. Empty only if the app somehow ran without one.
    public var connection: String
    /// Driver id (`postgres`, `mysql`, …) for the engine glyph. Kept on the
    /// entry rather than looked up later: the profile may be gone by the time
    /// anyone reads this back, and HeidiSQL's [#1142] is precisely the complaint
    /// that history is useless once it depends on a connection still existing.
    public var engine: String
    public var startedAtMs: Int64
    public var durationMs: Int
    /// Rows returned. `nil` when the statement returned no result set.
    public var rowCount: Int?
    /// Rows affected by a write. Always `nil` today — this ABI carries no
    /// `Shape::Ack`, so an affected-row count is not available (the same gap the
    /// status bar reports). The field exists so the number has somewhere to land
    /// the day it does.
    public var affectedRows: Int?
    public var outcome: QueryOutcome
    /// The server's message when `outcome == .error`. A failed query is often
    /// the one you most want back, and it is worthless without its error.
    public var error: String?
    /// How many times this exact statement collapsed into this entry via the
    /// dedupe window. 1 for a normal entry.
    public var runCount: Int
    /// FNV-1a over the whitespace-normalised SQL. Persisted (like the engine's
    /// `text_hash`) so dedupe never re-normalises the whole file.
    public var textHash: String

    public init(
        id: String = UUID().uuidString,
        sql: String,
        connection: String,
        engine: String = "",
        startedAt: Date = Date(),
        durationMs: Int = 0,
        rowCount: Int? = nil,
        affectedRows: Int? = nil,
        outcome: QueryOutcome = .ok,
        error: String? = nil,
        runCount: Int = 1
    ) {
        self.id = id
        self.sql = sql
        self.connection = connection
        self.engine = engine
        self.startedAtMs = Int64((startedAt.timeIntervalSince1970 * 1000).rounded())
        self.durationMs = durationMs
        self.rowCount = rowCount
        self.affectedRows = affectedRows
        self.outcome = outcome
        self.error = error
        self.runCount = runCount
        self.textHash = QueryHistoryEntry.hash(sql)
    }

    public var startedAt: Date { Date(timeIntervalSince1970: Double(startedAtMs) / 1000) }

    /// Day bucket, in the user's own time zone — "Today" has to mean the day
    /// they had, not UTC's.
    public var dayKey: String { HistoryFormat.dayKey(for: startedAt) }

    /// One line, whitespace collapsed, for the list. The full text is kept
    /// intact in `sql` and shown on selection.
    public var oneLine: String {
        let flat = sql.split(whereSeparator: { $0.isNewline || $0 == "\t" })
            .map { $0.trimmingCharacters(in: .whitespaces) }
            .filter { !$0.isEmpty }
            .joined(separator: " ")
        return flat.isEmpty ? sql.trimmingCharacters(in: .whitespacesAndNewlines) : flat
    }

    public var isMultiline: Bool {
        sql.trimmingCharacters(in: .whitespacesAndNewlines).contains(where: { $0.isNewline })
    }

    /// Whitespace-normalised, trailing semicolons dropped. Case is *kept*:
    /// identifiers are case-sensitive on several engines, and two statements
    /// that differ only in case are still two different things a person typed.
    public static func normalise(_ sql: String) -> String {
        var out = ""
        var lastWasSpace = false
        for ch in sql {
            if ch.isWhitespace {
                if !lastWasSpace, !out.isEmpty { out.append(" ") }
                lastWasSpace = true
            } else {
                out.append(ch)
                lastWasSpace = false
            }
        }
        while out.hasSuffix(" ") || out.hasSuffix(";") { out.removeLast() }
        return out
    }

    /// FNV-1a, not `Hasher`: `Hasher` is seeded per process, so its output would
    /// change between launches and dedupe would stop working across restarts.
    public static func hash(_ sql: String) -> String {
        var h: UInt64 = 0xcbf2_9ce4_8422_2325
        for byte in Data(normalise(sql).utf8) {
            h ^= UInt64(byte)
            h = h &* 0x0000_0100_0000_01b3
        }
        return String(h, radix: 16)
    }
}

/// How much history to keep. **User-configurable, and stated in the UI** — the
/// study names DBeaver (no retention control at all, [#22238]) and Sequel Ace
/// (a silent hard cap of 100 entries, [#1551]) as the two documented ways to get
/// this wrong. The defaults are generous on purpose: 10 000 entries or 180 days,
/// whichever bites first.
public struct HistoryRetention: Codable, Sendable, Equatable {
    public var maxEntries: Int
    public var maxDays: Int

    public static let `default` = HistoryRetention(maxEntries: 10_000, maxDays: 180)

    public init(maxEntries: Int = 10_000, maxDays: Int = 180) {
        // 0 or negative would silently mean "keep nothing"; clamp instead of
        // quietly deleting the user's history because a field was left empty.
        self.maxEntries = max(100, maxEntries)
        self.maxDays = max(1, maxDays)
    }

    /// Decoded values are clamped too — a `retention.json` someone hand-edited
    /// to `0` must not be read as "delete everything".
    public init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        self.init(
            maxEntries: try c.decodeIfPresent(Int.self, forKey: .maxEntries) ?? 10_000,
            maxDays: try c.decodeIfPresent(Int.self, forKey: .maxDays) ?? 180)
    }

    /// The sentence the panel shows. Retention the user cannot read is the same
    /// as retention the user cannot set.
    public var summary: String {
        "keeping the last \(maxEntries.formatted()) queries, up to \(maxDays) days"
    }
}

/// Date window for the filter bar. The four coarse buckets people actually
/// think in — DBeaver's filed complaint is that it has no date filtering at all.
public enum HistoryDateRange: String, Codable, Sendable, CaseIterable, Identifiable {
    case day, week, month, all
    public var id: String { rawValue }

    public var title: String {
        switch self {
        case .day: return "Today"
        case .week: return "Week"
        case .month: return "Month"
        case .all: return "All"
        }
    }

    /// Oldest instant still inside the window, or `nil` for "all".
    public func earliest(now: Date = Date(), calendar: Calendar = .current) -> Date? {
        switch self {
        case .all: return nil
        case .day: return calendar.startOfDay(for: now)
        case .week: return calendar.date(byAdding: .day, value: -7, to: now)
        case .month: return calendar.date(byAdding: .month, value: -1, to: now)
        }
    }
}

/// What the panel is currently asking for. A value, so filtering is a pure
/// function of it and the snapshot — no hidden state, nothing to invalidate.
public struct HistoryFilter: Sendable, Equatable {
    public var text: String
    /// `nil` = every connection. History is *not* scoped to whatever you happen
    /// to be connected to right now — that is the HeidiSQL complaint ([#1142]);
    /// the connection is a filter you may apply, never one applied for you.
    public var connection: String?
    public var range: HistoryDateRange
    public var outcome: QueryOutcome?

    public init(
        text: String = "", connection: String? = nil, range: HistoryDateRange = .all,
        outcome: QueryOutcome? = nil
    ) {
        self.text = text
        self.connection = connection
        self.range = range
        self.outcome = outcome
    }

    public var isEmpty: Bool {
        text.trimmingCharacters(in: .whitespaces).isEmpty && connection == nil && range == .all
            && outcome == nil
    }
}

// MARK: - formatting helpers

public enum HistoryFormat {
    private static let dayFormatter: DateFormatter = {
        let f = DateFormatter()
        f.locale = Locale(identifier: "en_US_POSIX")
        f.dateFormat = "yyyy-MM-dd"
        return f
    }()

    private static let sameYearFormatter: DateFormatter = {
        let f = DateFormatter()
        f.setLocalizedDateFormatFromTemplate("EEEE d MMMM")
        return f
    }()

    private static let otherYearFormatter: DateFormatter = {
        let f = DateFormatter()
        f.setLocalizedDateFormatFromTemplate("d MMMM yyyy")
        return f
    }()

    private static let timeFormatter: DateFormatter = {
        let f = DateFormatter()
        f.timeStyle = .medium
        f.dateStyle = .none
        return f
    }()

    public static func dayKey(for date: Date) -> String { dayFormatter.string(from: date) }

    public static func date(fromDayKey key: String) -> Date? { dayFormatter.date(from: key) }

    /// "Today" / "Yesterday" / "Monday 4 August". A date the reader has to
    /// decode is a date they will not read.
    public static func dayTitle(for date: Date, now: Date = Date()) -> String {
        let cal = Calendar.current
        if cal.isDateInToday(date) { return "Today" }
        if cal.isDateInYesterday(date) { return "Yesterday" }
        if cal.component(.year, from: date) == cal.component(.year, from: now) {
            return sameYearFormatter.string(from: date)
        }
        return otherYearFormatter.string(from: date)
    }

    public static func time(_ date: Date) -> String { timeFormatter.string(from: date) }

    public static func duration(_ ms: Int) -> String {
        if ms < 1000 { return "\(ms) ms" }
        if ms < 60_000 { return String(format: "%.2f s", Double(ms) / 1000) }
        return String(format: "%d m %02d s", ms / 60_000, (ms % 60_000) / 1000)
    }

    public static func rows(_ n: Int?) -> String? {
        guard let n else { return nil }
        return n == 1 ? "1 row" : "\(n.formatted()) rows"
    }
}

/// One day's worth of entries, ready to draw as a section.
public struct HistoryDay: Identifiable, Equatable {
    public let id: String
    public let title: String
    public let entries: [QueryHistoryEntry]
    public init(id: String, title: String, entries: [QueryHistoryEntry]) {
        self.id = id
        self.title = title
        self.entries = entries
    }
}

// MARK: - store

/// Reads and writes the history directory. Pure file I/O — no engine, no ABI.
///
/// Every mutation and every byte of I/O happens on one private serial queue, so
/// no caller can put a write on the main thread or on the query path by
/// accident. Writes are debounced with a single `asyncAfter` (the same
/// discipline `SavedQueries` uses) — there is no timer and nothing polls.
public final class QueryHistoryStore: @unchecked Sendable {
    public let directory: URL

    /// `~/Library/Application Support/datagrep/history/`, alongside the engine's
    /// `profiles.sqlite` and the editor's `tabs/`. Not the temp directory: a
    /// statement you ran last month has to still be there next month.
    public static var defaultDirectory: URL {
        FileManager.default
            .urls(for: .applicationSupportDirectory, in: .userDomainMask)[0]
            .appendingPathComponent("datagrep", isDirectory: true)
            .appendingPathComponent("history", isDirectory: true)
    }

    /// Re-running the same statement on the same connection inside this window
    /// updates the entry in place instead of adding a second one. Two minutes is
    /// long enough to absorb "run, tweak nothing, run again" and short enough
    /// that this morning's run and this afternoon's stay two events.
    ///
    /// A *different* outcome always makes a new entry, even one second later:
    /// the query that worked and the query that failed are not the same event,
    /// and collapsing them would delete exactly the row you went looking for.
    public var dedupeWindow: TimeInterval = 120

    private let queue = DispatchQueue(label: "datagrep.history", qos: .utility)
    private let lock = NSLock()

    /// Newest first. Queue-confined: only ever touched inside `queue`.
    private var entries: [QueryHistoryEntry] = []
    private var dirtyDays: Set<String> = []
    private var flushScheduled = false
    private var didLoad = false

    private var retentionValue: HistoryRetention
    private var changeHandler: (([QueryHistoryEntry]) -> Void)?

    public init(directory: URL = QueryHistoryStore.defaultDirectory) {
        self.directory = directory
        try? FileManager.default.createDirectory(at: directory, withIntermediateDirectories: true)
        self.retentionValue = Self.readRetention(in: directory)
    }

    // MARK: observation

    /// Called on the main queue whenever the snapshot changes. Set it before
    /// `load()`; that call is what delivers the first snapshot.
    public func onChange(_ handler: @escaping ([QueryHistoryEntry]) -> Void) {
        lock.lock()
        changeHandler = handler
        lock.unlock()
    }

    private func publish() {
        let snapshot = entries
        lock.lock()
        let handler = changeHandler
        lock.unlock()
        guard let handler else { return }
        DispatchQueue.main.async { handler(snapshot) }
    }

    // MARK: retention

    public var retention: HistoryRetention {
        lock.lock()
        defer { lock.unlock() }
        return retentionValue
    }

    /// Changing retention prunes immediately — a setting that only takes effect
    /// "eventually" is a setting the user cannot verify.
    public func setRetention(_ new: HistoryRetention) {
        lock.lock()
        retentionValue = new
        lock.unlock()
        queue.async { [weak self] in
            guard let self else { return }
            Self.writeRetention(new, in: self.directory)
            self.prune()
            self.scheduleFlush()
            self.publish()
        }
    }

    // MARK: reading

    /// Loads everything inside retention, newest day first, and drops what has
    /// aged out. Cheap enough to be synchronous on the queue at launch: 10 000
    /// short JSON lines is a few hundred KB.
    ///
    /// Idempotent, and deliberately so: a second `load()` would read the last
    /// *flushed* state and so would silently discard anything recorded inside
    /// the debounce window. Reading from disk must never be able to lose a query
    /// that has already been run.
    public func load() {
        queue.async { [weak self] in
            guard let self, !self.didLoad else { return }
            self.didLoad = true
            var loaded: [QueryHistoryEntry] = []
            let retention = self.retention
            let cutoffKey = Self.cutoffDayKey(days: retention.maxDays)
            let decoder = JSONDecoder()

            // Newest day file first — the filenames are ISO dates, so a plain
            // string sort is a chronological one.
            let newestFirst = self.dayFiles().sorted {
                $0.lastPathComponent > $1.lastPathComponent
            }
            for file in newestFirst {
                let key = file.deletingPathExtension().lastPathComponent
                if key < cutoffKey {
                    try? FileManager.default.removeItem(at: file)
                    continue
                }
                guard let text = try? String(contentsOf: file, encoding: .utf8) else { continue }
                for line in text.split(whereSeparator: \.isNewline) {
                    guard let data = line.data(using: .utf8),
                        let entry = try? decoder.decode(QueryHistoryEntry.self, from: data)
                    else { continue }
                    loaded.append(entry)
                }
                // Newest-first order means we can stop reading as soon as the
                // budget is met; older files stay on disk until pruned.
                if loaded.count >= retention.maxEntries { break }
            }

            loaded.sort { $0.startedAtMs > $1.startedAtMs }
            self.entries = loaded
            self.prune()
            self.publish()
        }
    }

    /// The current snapshot, for a caller that missed the callback.
    public func snapshot(_ completion: @escaping ([QueryHistoryEntry]) -> Void) {
        queue.async { [weak self] in
            let s = self?.entries ?? []
            DispatchQueue.main.async { completion(s) }
        }
    }

    // MARK: writing

    /// Records one executed statement. Returns immediately; the caller is never
    /// blocked, and this is never on the query path — call it after the fact.
    public func record(_ entry: QueryHistoryEntry) {
        queue.async { [weak self] in
            guard let self else { return }
            let text = entry.sql.trimmingCharacters(in: .whitespacesAndNewlines)
            guard !text.isEmpty else { return }

            // Dedupe: the most recent entry with the same statement on the same
            // connection, inside the window, and with the same outcome.
            if let i = self.entries.firstIndex(where: {
                $0.textHash == entry.textHash && $0.connection == entry.connection
                    && $0.outcome == entry.outcome && $0.error == entry.error
                    && entry.startedAt.timeIntervalSince($0.startedAt) <= self.dedupeWindow
                    && entry.startedAt.timeIntervalSince($0.startedAt) >= -self.dedupeWindow
            }) {
                var merged = self.entries[i]
                self.dirtyDays.insert(merged.dayKey)  // it may be leaving this day
                merged.startedAtMs = entry.startedAtMs
                merged.durationMs = entry.durationMs
                merged.rowCount = entry.rowCount
                merged.affectedRows = entry.affectedRows
                merged.runCount += 1
                self.entries.remove(at: i)
                self.entries.insert(merged, at: 0)
                self.dirtyDays.insert(merged.dayKey)
            } else {
                self.entries.insert(entry, at: 0)
                self.dirtyDays.insert(entry.dayKey)
            }

            self.entries.sort { $0.startedAtMs > $1.startedAtMs }
            self.prune()
            self.scheduleFlush()
            self.publish()
        }
    }

    public func delete(ids: Set<String>) {
        guard !ids.isEmpty else { return }
        queue.async { [weak self] in
            guard let self else { return }
            for e in self.entries where ids.contains(e.id) { self.dirtyDays.insert(e.dayKey) }
            self.entries.removeAll { ids.contains($0.id) }
            self.scheduleFlush()
            self.publish()
        }
    }

    /// Clears everything, or everything for one connection. Destructive and
    /// explicit — nothing here ever clears history as a side effect.
    public func clear(connection: String? = nil) {
        queue.async { [weak self] in
            guard let self else { return }
            if let connection {
                for e in self.entries where e.connection == connection {
                    self.dirtyDays.insert(e.dayKey)
                }
                self.entries.removeAll { $0.connection == connection }
            } else {
                self.entries.removeAll()
            }
            self.scheduleFlush()
            self.publish()
        }
    }

    // MARK: - filtering (pure)

    /// Applies a filter to a snapshot. Pure and static so the panel can call it
    /// on whatever thread it likes, and so it is trivially testable.
    public static func filter(
        _ entries: [QueryHistoryEntry], with filter: HistoryFilter, now: Date = Date()
    ) -> [QueryHistoryEntry] {
        let earliest = filter.range.earliest(now: now)
        let terms =
            filter.text
            .split(whereSeparator: { $0 == " " || $0.isNewline })
            .map(String.init)
            .filter { !$0.isEmpty }

        return entries.filter { e in
            if let c = filter.connection, e.connection != c { return false }
            if let o = filter.outcome, e.outcome != o { return false }
            if let earliest, e.startedAt < earliest { return false }
            guard !terms.isEmpty else { return true }
            // AND across terms: typing more words narrows, which is what every
            // search box has trained people to expect. The error text is searched
            // too — "deadlock" should find the query that hit one.
            for t in terms {
                let opts: String.CompareOptions = [.caseInsensitive, .diacriticInsensitive]
                let inSQL = e.sql.range(of: t, options: opts) != nil
                let inError = e.error.map { $0.range(of: t, options: opts) != nil } ?? false
                if !inSQL && !inError { return false }
            }
            return true
        }
    }

    /// Groups a (already newest-first) list into day sections.
    public static func group(_ entries: [QueryHistoryEntry], now: Date = Date()) -> [HistoryDay] {
        var order: [String] = []
        var buckets: [String: [QueryHistoryEntry]] = [:]
        for e in entries {
            let key = e.dayKey
            if buckets[key] == nil {
                buckets[key] = []
                order.append(key)
            }
            buckets[key]?.append(e)
        }
        return order.map { key in
            let date = HistoryFormat.date(fromDayKey: key) ?? Date()
            return HistoryDay(
                id: key, title: HistoryFormat.dayTitle(for: date, now: now),
                entries: buckets[key] ?? [])
        }
    }

    /// Connection names that actually appear in history, for the filter menu.
    /// Taken from the entries and not from the live profile list on purpose: a
    /// connection you deleted still has a past, and hiding it would be exactly
    /// the coupling HeidiSQL's users complained about.
    public static func connections(in entries: [QueryHistoryEntry]) -> [String] {
        Array(Set(entries.map(\.connection))).filter { !$0.isEmpty }.sorted()
    }

    // MARK: - private

    /// Retention, applied. Entry count first, then age.
    private func prune() {
        let r = retention
        if entries.count > r.maxEntries {
            for e in entries[r.maxEntries...] { dirtyDays.insert(e.dayKey) }
            entries.removeSubrange(r.maxEntries...)
        }
        let cutoffKey = Self.cutoffDayKey(days: r.maxDays)
        if let firstStale = entries.firstIndex(where: { $0.dayKey < cutoffKey }) {
            for e in entries[firstStale...] { dirtyDays.insert(e.dayKey) }
            entries.removeSubrange(firstStale...)
        }
    }

    /// One pending write, coalesced. Not a timer: a single `asyncAfter` that
    /// re-arms only when something else changes, exactly as `SavedQueries` does.
    private func scheduleFlush() {
        guard !flushScheduled else { return }
        flushScheduled = true
        queue.asyncAfter(deadline: .now() + 0.6) { [weak self] in
            guard let self else { return }
            self.flushScheduled = false
            self.flush()
        }
    }

    private func flush() {
        let encoder = JSONEncoder()
        encoder.outputFormatting = [.sortedKeys]

        var byDay: [String: [QueryHistoryEntry]] = [:]
        for e in entries { byDay[e.dayKey, default: []].append(e) }

        // Rewrite only the days that changed…
        for day in dirtyDays {
            let url = directory.appendingPathComponent(day + ".jsonl")
            guard let rows = byDay[day], !rows.isEmpty else {
                try? FileManager.default.removeItem(at: url)
                continue
            }
            var text = ""
            // Oldest first inside a file, so appending stays natural for anyone
            // reading it with `tail`.
            for e in rows.reversed() {
                guard let data = try? encoder.encode(e),
                    let line = String(data: data, encoding: .utf8)
                else { continue }
                text += line + "\n"
            }
            try? text.write(to: url, atomically: true, encoding: .utf8)
        }
        dirtyDays.removeAll()

        // …and drop any day file retention has since outlived. Deriving this
        // from what is actually in memory means a stale or hand-edited directory
        // heals itself instead of accumulating forever.
        let live = Set(byDay.keys)
        let cutoffKey = Self.cutoffDayKey(days: retention.maxDays)
        for file in dayFiles() {
            let key = file.deletingPathExtension().lastPathComponent
            if key < cutoffKey && !live.contains(key) {
                try? FileManager.default.removeItem(at: file)
            }
        }
    }

    private func dayFiles() -> [URL] {
        let files =
            (try? FileManager.default.contentsOfDirectory(
                at: directory, includingPropertiesForKeys: nil)) ?? []
        return files.filter { $0.pathExtension == "jsonl" }
    }

    /// Oldest day still inside retention. `days` counts *inclusive* of today, so
    /// a retention of 1 keeps today and nothing else — "keep 1 day" that quietly
    /// kept two would be the same class of surprise as an undocumented cap.
    private static func cutoffDayKey(days: Int) -> String {
        let cutoff =
            Calendar.current.date(byAdding: .day, value: -(max(1, days) - 1), to: Date())
            ?? Date.distantPast
        return HistoryFormat.dayKey(for: cutoff)
    }

    private static func retentionURL(in dir: URL) -> URL {
        dir.appendingPathComponent("retention.json")
    }

    private static func readRetention(in dir: URL) -> HistoryRetention {
        guard let data = try? Data(contentsOf: retentionURL(in: dir)),
            let r = try? JSONDecoder().decode(HistoryRetention.self, from: data)
        else { return .default }
        return r
    }

    private static func writeRetention(_ r: HistoryRetention, in dir: URL) {
        let e = JSONEncoder()
        e.outputFormatting = [.prettyPrinted, .sortedKeys]
        guard let data = try? e.encode(r) else { return }
        try? data.write(to: retentionURL(in: dir), options: .atomic)
    }
}
