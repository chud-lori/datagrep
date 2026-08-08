import Foundation

/// On-disk shape of one editor tab.
///
/// The C ABI (`crates/datagrep-ffi/include/datagrep.h`) exposes profiles,
/// catalog, query and rows — and nothing else. There is no
/// `datagrep_saved_query_*` / `datagrep_editor_tab_*` entry point, so the
/// `saved_query` and `editor_tab` tables in `datagrep-profiles` are simply not
/// reachable from Swift today. That is fine: the design doc's stated preference
/// for saved queries is plain files ("git-friendly, not a proprietary store"),
/// so this store writes exactly that — a `.sql` file you can open in any editor,
/// plus a small JSON sidecar holding the things SQL cannot carry (which
/// connection the tab is bound to, and where the caret was).
///
/// One file pair per tab, never one big blob: a half-written blob loses every
/// tab, a half-written sidecar loses one tab's caret position.
public struct SavedQueryRecord: Codable, Sendable, Equatable {
    /// Stable identity, and the sidecar/SQL basename for scratch tabs.
    public var id: String
    /// `nil` for an untitled scratch tab. Scratch tabs are persisted too —
    /// losing unsaved SQL because the app crashed is not acceptable.
    public var name: String?
    /// Profile name this tab runs against. `nil` = inherit the window's
    /// current connection.
    public var connection: String?
    public var cursorLocation: Int
    public var cursorLength: Int
    /// Whether the buffer differs from the last explicit ⌘S.
    public var isDirty: Bool

    public init(
        id: String = UUID().uuidString,
        name: String? = nil,
        connection: String? = nil,
        cursorLocation: Int = 0,
        cursorLength: Int = 0,
        isDirty: Bool = false
    ) {
        self.id = id
        self.name = name
        self.connection = connection
        self.cursorLocation = cursorLocation
        self.cursorLength = cursorLength
        self.isDirty = isDirty
    }

    public var isScratch: Bool { name == nil }

    /// Basename shared by the `.sql` and the `.json`. Named tabs get a
    /// human-readable, git-friendly filename; scratch tabs get their uuid,
    /// because a scratch tab has no name to slug.
    public var basename: String {
        guard let name, !name.isEmpty else { return "scratch-" + id }
        let slug = SavedQueryStore.slug(name)
        return slug.isEmpty ? "scratch-" + id : slug
    }
}

/// Tab order and which tab was frontmost. Separate from the per-tab sidecars so
/// that rewriting the order (a cheap, frequent event) never rewrites SQL.
public struct EditorSession: Codable, Sendable, Equatable {
    public var order: [String]
    public var activeID: String?

    public init(order: [String] = [], activeID: String? = nil) {
        self.order = order
        self.activeID = activeID
    }
}

/// Reads and writes the tab directory. Pure file I/O — no engine, no ABI.
public final class SavedQueryStore: @unchecked Sendable {
    public let directory: URL

    /// `~/Library/Application Support/datagrep/tabs/`, alongside the engine's
    /// `profiles.sqlite`. Not the temp directory: a query you were half way
    /// through writing has to still be there tomorrow.
    public static var defaultDirectory: URL {
        FileManager.default
            .urls(for: .applicationSupportDirectory, in: .userDomainMask)[0]
            .appendingPathComponent("datagrep", isDirectory: true)
            .appendingPathComponent("tabs", isDirectory: true)
    }

    public init(directory: URL = SavedQueryStore.defaultDirectory) {
        self.directory = directory
        try? FileManager.default.createDirectory(
            at: directory, withIntermediateDirectories: true)
    }

    private var sessionURL: URL { directory.appendingPathComponent("session.json") }

    public func sqlURL(for record: SavedQueryRecord) -> URL {
        directory.appendingPathComponent(record.basename + ".sql")
    }

    public func sidecarURL(for record: SavedQueryRecord) -> URL {
        directory.appendingPathComponent(record.basename + ".json")
    }

    // MARK: - write

    /// Writes the SQL and its sidecar. Atomic per file, so a crash mid-write
    /// leaves the previous version intact rather than a truncated one.
    public func save(_ record: SavedQueryRecord, text: String) {
        let sql = sqlURL(for: record)
        try? text.write(to: sql, atomically: true, encoding: .utf8)
        if let data = try? Self.encoder.encode(record) {
            try? data.write(to: sidecarURL(for: record), options: .atomic)
        }
    }

    /// Removes both files for a record. Used when a tab is closed, and when a
    /// tab is renamed (the old basename's pair is dropped after the new one is
    /// written, never before).
    public func delete(_ record: SavedQueryRecord) {
        try? FileManager.default.removeItem(at: sqlURL(for: record))
        try? FileManager.default.removeItem(at: sidecarURL(for: record))
    }

    public func saveSession(_ session: EditorSession) {
        guard let data = try? Self.encoder.encode(session) else { return }
        try? data.write(to: sessionURL, options: .atomic)
    }

    // MARK: - read

    /// Everything on disk, in the order `session.json` remembers. A record whose
    /// `.sql` has gone missing is dropped, and a record whose sidecar exists but
    /// which `session.json` has never heard of is appended — so a session file
    /// that is stale or corrupt costs tab *order*, never tab *content*.
    ///
    /// A bare `.sql` with no sidecar is ignored: without one there is no id, no
    /// connection and no caret, and inventing them would be guessing.
    public func load() -> (tabs: [(record: SavedQueryRecord, text: String)], activeID: String?) {
        let session = loadSession()
        var byID: [String: (SavedQueryRecord, String)] = [:]
        var discovered: [String] = []

        let files =
            (try? FileManager.default.contentsOfDirectory(
                at: directory, includingPropertiesForKeys: nil)) ?? []
        for url in files where url.pathExtension == "json" {
            guard url.lastPathComponent != "session.json" else { continue }
            guard let data = try? Data(contentsOf: url),
                let record = try? Self.decoder.decode(SavedQueryRecord.self, from: data)
            else { continue }
            guard let text = try? String(contentsOf: sqlURL(for: record), encoding: .utf8) else {
                continue
            }
            byID[record.id] = (record, text)
            discovered.append(record.id)
        }

        var ordered: [(SavedQueryRecord, String)] = []
        var seen = Set<String>()
        for id in session.order {
            if let entry = byID[id], seen.insert(id).inserted { ordered.append(entry) }
        }
        for id in discovered.sorted() {
            if let entry = byID[id], seen.insert(id).inserted { ordered.append(entry) }
        }

        let active = session.activeID.flatMap { seen.contains($0) ? $0 : nil }
        return (ordered, active ?? ordered.first?.0.id)
    }

    public func loadSession() -> EditorSession {
        guard let data = try? Data(contentsOf: sessionURL),
            let s = try? Self.decoder.decode(EditorSession.self, from: data)
        else { return EditorSession() }
        return s
    }

    /// Names already taken, so ⌘S can warn instead of silently overwriting.
    public func existingNames() -> Set<String> {
        let files =
            (try? FileManager.default.contentsOfDirectory(
                at: directory, includingPropertiesForKeys: nil)) ?? []
        var names = Set<String>()
        for url in files where url.pathExtension == "json" {
            guard url.lastPathComponent != "session.json" else { continue }
            guard let data = try? Data(contentsOf: url),
                let record = try? Self.decoder.decode(SavedQueryRecord.self, from: data),
                let name = record.name
            else { continue }
            names.insert(name)
        }
        return names
    }

    // MARK: - helpers

    /// Filesystem-safe, lower-kebab. Keeps the saved file recognisable from a
    /// shell (`ls ~/Library/Application\ Support/datagrep/tabs/`) which is the
    /// whole point of not using a proprietary store.
    public static func slug(_ name: String) -> String {
        var out = ""
        var lastWasDash = false
        for ch in name.lowercased() {
            if ch.isLetter || ch.isNumber {
                out.append(ch)
                lastWasDash = false
            } else if !lastWasDash, !out.isEmpty {
                out.append("-")
                lastWasDash = true
            }
        }
        while out.hasSuffix("-") { out.removeLast() }
        return String(out.prefix(64))
    }

    private static let encoder: JSONEncoder = {
        let e = JSONEncoder()
        e.outputFormatting = [.prettyPrinted, .sortedKeys]
        return e
    }()

    private static let decoder = JSONDecoder()
}
