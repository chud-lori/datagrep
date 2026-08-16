import Foundation

/// On-disk shape of one editor tab.
///
/// **This is the one editor-tab store.** The C ABI
/// (`crates/datagrep-ffi/include/datagrep.h`) exposes profiles, catalog, query
/// and rows and nothing else, so `datagrep-profiles`' `editor_tab` table has
/// never been reachable from Swift and nothing has ever written a row to it;
/// its Rust side has been removed rather than left as a second, dead store the
/// next reader has to work out the status of. (The empty table itself stays in
/// the schema: dropping it would mean a destructive migration of a database
/// that holds the user's real connections, to reclaim nothing.)
///
/// Plain files are the deliberate choice, not a fallback: a saved query is a
/// `.sql` file you can open in any editor and commit to git, plus a small JSON
/// sidecar holding the things SQL cannot carry — which connection the tab
/// belongs to, and where the caret was.
///
/// One file pair per tab, never one big blob: a half-written blob loses every
/// tab, a half-written sidecar loses one tab's caret position.
public struct SavedQueryRecord: Codable, Sendable, Equatable {
    /// Stable identity, and the sidecar/SQL basename for scratch tabs.
    public var id: String
    /// `nil` for an untitled scratch tab. Scratch tabs are persisted too —
    /// losing unsaved SQL because the app crashed is not acceptable.
    public var name: String?
    /// The profile this editor **belongs to**. Tabs are scoped by it: selecting
    /// a connection shows that connection's editors and no others. `nil` means
    /// the editor belongs to no connection — where tabs written before editors
    /// were scoped land, and where a statement runs against whatever the window
    /// has selected.
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

/// Tab order and which single tab was frontmost. Separate from the per-tab
/// sidecars so that rewriting the order (a cheap, frequent event) never
/// rewrites SQL.
///
/// The tab bar shows every open editor at once, whatever connection it targets,
/// so there is ONE active tab across the whole window — not one per connection.
/// `activeConnection` records the sidebar selection only, which seeds the
/// connection a new (⌘T) editor is created for; it does not hide the others.
///
/// `order` is one global list. A tab's position is a property of the tab, not of
/// the connection it is filed under, so rebinding one does not shuffle the rest.
public struct EditorSession: Codable, Sendable, Equatable {
    public var order: [String]
    /// The single globally-active editor tab. The tab bar shows every editor at
    /// once (across all connections), so there is ONE active tab — not one per
    /// connection as an earlier, scoped design kept.
    public var activeID: String?
    /// The connection a NEW editor is created for — the sidebar selection. It no
    /// longer hides the other connections' editors; it only seeds ⌘T.
    public var activeConnection: String?

    /// The key for tabs with no connection — kept for decoding an older,
    /// per-connection `session.json`.
    public static let unbound = ""

    public init(
        order: [String] = [], activeID: String? = nil, activeConnection: String? = nil
    ) {
        self.order = order
        self.activeID = activeID
        self.activeConnection = activeConnection
    }

    /// Hand-rolled for backward compatibility: a `session.json` written by the
    /// oldest (global) build carried `activeID` directly, and the intermediate
    /// (scoped) build carried `activeByConnection`. Both still restore every tab
    /// and a sensible active one — losing someone's open SQL to a format change
    /// is not a trade this store is allowed to make.
    private enum CodingKeys: String, CodingKey {
        case order, activeID, activeConnection, activeByConnection
    }

    public init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        order = try c.decodeIfPresent([String].self, forKey: .order) ?? []
        activeConnection = try c.decodeIfPresent(String.self, forKey: .activeConnection)
        if let a = try c.decodeIfPresent(String.self, forKey: .activeID) {
            activeID = a
        } else {
            // Scoped format: the active tab was recorded per connection. Take the
            // one for whichever connection was showing, or any as a fallback.
            let byConn =
                try c.decodeIfPresent([String: String].self, forKey: .activeByConnection) ?? [:]
            activeID = byConn[activeConnection ?? Self.unbound] ?? byConn.values.first
        }
    }

    public func encode(to encoder: Encoder) throws {
        var c = encoder.container(keyedBy: CodingKeys.self)
        try c.encode(order, forKey: .order)
        try c.encodeIfPresent(activeID, forKey: .activeID)
        try c.encodeIfPresent(activeConnection, forKey: .activeConnection)
    }
}

/// Reads and writes the tab directory. Pure file I/O — no engine, no ABI.
public final class SavedQueryStore: @unchecked Sendable {
    public let directory: URL

    /// `~/Library/Application Support/datagrep/tabs/`, alongside the engine's
    /// `profiles.sqlite`. Not the temp directory: a query you were half way
    /// through writing has to still be there tomorrow.
    public static var defaultDirectory: URL {
        SupportDirectory.base.appendingPathComponent("tabs", isDirectory: true)
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
    public func load() -> (tabs: [(record: SavedQueryRecord, text: String)], session: EditorSession)
    {
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
        // Anything the session forgot: reopen it only if it is a *scratch* tab.
        // Unsaved work has nowhere else to live, so a lost or truncated
        // session.json must never be able to strand it. A named query does have
        // somewhere else to live — the saved list — so it stays closed rather
        // than forcing its way back into the tab bar every launch.
        for id in discovered.sorted() {
            guard let entry = byID[id], entry.0.isScratch, seen.insert(id).inserted else { continue }
            ordered.append(entry)
        }

        // Drop a remembered active tab that is no longer on disk, so the session
        // never restores pointing at a tab that cannot be loaded.
        var cleaned = session
        if let a = cleaned.activeID, !seen.contains(a) { cleaned.activeID = nil }
        return (ordered, cleaned)
    }

    /// Every tab on disk, named and scratch alike.
    ///
    /// This is what the connection's "Open Editor" menu lists: an editor the
    /// user made for a connection and then closed still belongs to it, and
    /// offering only the *named* ones would hide most of them.
    public func allRecords() -> [SavedQueryRecord] {
        let files =
            (try? FileManager.default.contentsOfDirectory(
                at: directory, includingPropertiesForKeys: nil)) ?? []
        var out: [SavedQueryRecord] = []
        for url in files where url.pathExtension == "json" {
            guard url.lastPathComponent != "session.json" else { continue }
            guard let data = try? Data(contentsOf: url),
                let record = try? Self.decoder.decode(SavedQueryRecord.self, from: data)
            else { continue }
            out.append(record)
        }
        return out
    }

    /// Every named query on disk, for an "open a saved query" list. Closing a
    /// named tab removes it from the session but never from the folder — the
    /// `.sql` file is the artefact the user asked us to keep.
    public func allSaved() -> [SavedQueryRecord] {
        let files =
            (try? FileManager.default.contentsOfDirectory(
                at: directory, includingPropertiesForKeys: nil)) ?? []
        var out: [SavedQueryRecord] = []
        for url in files where url.pathExtension == "json" {
            guard url.lastPathComponent != "session.json" else { continue }
            guard let data = try? Data(contentsOf: url),
                let record = try? Self.decoder.decode(SavedQueryRecord.self, from: data),
                record.name != nil
            else { continue }
            out.append(record)
        }
        return out.sorted { ($0.name ?? "") < ($1.name ?? "") }
    }

    public func text(for record: SavedQueryRecord) -> String? {
        try? String(contentsOf: sqlURL(for: record), encoding: .utf8)
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
