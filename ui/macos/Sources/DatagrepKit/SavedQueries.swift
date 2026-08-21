import Foundation

public struct SavedQueryRecord: Codable, Sendable, Equatable {
    /// Stable identity, and the sidecar/SQL basename for scratch tabs.
    public var id: String
    public var name: String?
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

    public var basename: String {
        guard let name, !name.isEmpty else { return "scratch-" + id }
        let slug = SavedQueryStore.slug(name)
        return slug.isEmpty ? "scratch-" + id : slug
    }
}

public struct EditorSession: Codable, Sendable, Equatable {
    public var order: [String]
    public var activeID: String?
    public var activeConnection: String?

    public static let unbound = ""

    public init(
        order: [String] = [], activeID: String? = nil, activeConnection: String? = nil
    ) {
        self.order = order
        self.activeID = activeID
        self.activeConnection = activeConnection
    }

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

    public func save(_ record: SavedQueryRecord, text: String) {
        let sql = sqlURL(for: record)
        try? text.write(to: sql, atomically: true, encoding: .utf8)
        if let data = try? Self.encoder.encode(record) {
            try? data.write(to: sidecarURL(for: record), options: .atomic)
        }
    }

    public func delete(_ record: SavedQueryRecord) {
        try? FileManager.default.removeItem(at: sqlURL(for: record))
        try? FileManager.default.removeItem(at: sidecarURL(for: record))
    }

    public func saveSession(_ session: EditorSession) {
        guard let data = try? Self.encoder.encode(session) else { return }
        try? data.write(to: sessionURL, options: .atomic)
    }

    // MARK: - read

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
        for id in discovered.sorted() {
            guard let entry = byID[id], entry.0.isScratch, seen.insert(id).inserted else { continue }
            ordered.append(entry)
        }

        // Never restore pointing at a tab that cannot be loaded.
        var cleaned = session
        if let a = cleaned.activeID, !seen.contains(a) { cleaned.activeID = nil }
        return (ordered, cleaned)
    }

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
