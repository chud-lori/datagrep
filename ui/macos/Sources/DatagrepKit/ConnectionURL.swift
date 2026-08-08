import Foundation

/// One engine, as the connection dialogs need to describe it.
///
/// The `id` is the same string the engine's driver registry uses, so a saved
/// profile's `driver` can be matched against this list without a second
/// translation table — `EngineStyle.canonicalID` folds the spelling variants
/// (`mongodb` → `mongo`, `mariadb` → `mysql`) that arrive from URLs and from
/// the store.
public struct ConnectionEngine: Sendable, Hashable, Identifiable {
    public let id: String
    /// The scheme written back into a generated URL. Matched case-insensitively
    /// against a pasted one; `aliases` carries the other spellings each
    /// driver's own `parse_url` accepts.
    public let scheme: String
    public let aliases: [String]
    /// The scheme used when the TLS toggle is on, or nil where this engine
    /// carries TLS somewhere other than the scheme (or not at all yet).
    public let tlsScheme: String?
    public let defaultPort: Int?
    public let example: String
    /// SQLite is a file on disk, not a server. Host/port/user fields would be
    /// meaningless for it, so the sheet asks for a path instead.
    public let isFileBased: Bool
    /// What the field after the host is called for this engine — "Database" is
    /// wrong for Redis (a numbered slot) and for Elasticsearch (an index).
    public let databaseLabel: String
    public let databasePlaceholder: String

    public init(
        id: String, scheme: String, aliases: [String] = [], tlsScheme: String? = nil,
        defaultPort: Int?, example: String, isFileBased: Bool = false,
        databaseLabel: String = "Database", databasePlaceholder: String = "mydb"
    ) {
        self.id = id
        self.scheme = scheme
        self.aliases = aliases
        self.tlsScheme = tlsScheme
        self.defaultPort = defaultPort
        self.example = example
        self.isFileBased = isFileBased
        self.databaseLabel = databaseLabel
        self.databasePlaceholder = databasePlaceholder
    }

    /// Every scheme a URL for this engine may legally start with.
    public var allSchemes: [String] { [scheme] + aliases + (tlsScheme.map { [$0] } ?? []) }
}

/// The engines the connection dialogs offer.
///
/// Kept in step with `crates/datagrep-ffi/src/drivers.rs`: an engine listed
/// here that the engine build cannot route a URL for produces a picker entry
/// that fails on Add, which is worse than not offering it at all.
public enum ConnectionEngines {
    public static let all: [ConnectionEngine] = [
        ConnectionEngine(
            id: "postgres", scheme: "postgres://", aliases: ["postgresql://"],
            defaultPort: 5432, example: "postgres://user@localhost:5432/mydb"),
        ConnectionEngine(
            id: "mysql", scheme: "mysql://", aliases: ["mariadb://"],
            defaultPort: 3306, example: "mysql://user@localhost:3306/mydb"),
        ConnectionEngine(
            id: "sqlite", scheme: "sqlite://", defaultPort: nil,
            example: "sqlite:///Users/me/data.db", isFileBased: true,
            databaseLabel: "File", databasePlaceholder: "/Users/me/data.db"),
        ConnectionEngine(
            id: "redis", scheme: "redis://", aliases: ["rediss://"],
            defaultPort: 6379, example: "redis://localhost:6379/0",
            databaseLabel: "Database index", databasePlaceholder: "0"),
        ConnectionEngine(
            id: "mongo", scheme: "mongodb://", aliases: ["mongodb+srv://"],
            defaultPort: 27017, example: "mongodb://localhost:27017/mydb"),
        // http(s), not `elasticsearch://`: that is the form Kibana, curl and
        // every Elastic doc print, and `ElasticsearchDriver::parse_url` takes
        // all three. `https://` is the TLS spelling — this is the one engine
        // in this build whose TLS actually works end to end, which is why it
        // is the only one offered a TLS toggle.
        ConnectionEngine(
            id: "elasticsearch", scheme: "http://",
            aliases: ["elasticsearch://"], tlsScheme: "https://",
            defaultPort: 9200, example: "http://localhost:9200",
            databaseLabel: "Default index", databasePlaceholder: "optional"),
    ]

    public static func engine(id: String) -> ConnectionEngine? {
        let key = EngineStyle.canonicalID(id)
        return all.first { EngineStyle.canonicalID($0.id) == key }
    }

    /// The engine a pasted URL belongs to, by scheme alone. Nil for a URL that
    /// is still half-typed — the caller keeps the last known engine rather than
    /// flickering the whole form while someone types.
    public static func engine(forURL url: String) -> ConnectionEngine? {
        let u = url.lowercased().trimmingCharacters(in: .whitespacesAndNewlines)
        if u == ":memory:" { return engine(id: "sqlite") }
        return all.first { e in e.allSchemes.contains { u.hasPrefix($0) } }
    }
}

/// A connection expressed the way a person thinks about it — host, port,
/// database, user — plus the URL that is the profile's actual storage format.
///
/// The two are the *same value*: `url(...)` renders these fields, `parse(_:)`
/// reads them back, and the dialogs keep no third copy. Anything a round trip
/// through here cannot express (a Mongo `replicaSet`, an Elasticsearch
/// `path_prefix`) is carried verbatim in `extras` rather than being dropped —
/// silently deleting half of a pasted URL is how a connection dialog earns
/// distrust.
public struct ConnectionFields: Hashable, Sendable {
    public var engineID: String
    public var host: String
    public var port: String
    public var database: String
    public var username: String
    /// Only ever set by parsing a URL that had one inline. The dialogs lift it
    /// straight into their own secure field and never render it back into the
    /// visible URL — a live password does not belong in a text field anyone can
    /// screenshot.
    public var password: String
    public var filePath: String
    public var useTLS: Bool
    /// The query string, untouched.
    public var extras: String

    public init(
        engineID: String = "", host: String = "", port: String = "", database: String = "",
        username: String = "", password: String = "", filePath: String = "", useTLS: Bool = false,
        extras: String = ""
    ) {
        self.engineID = engineID
        self.host = host
        self.port = port
        self.database = database
        self.username = username
        self.password = password
        self.filePath = filePath
        self.useTLS = useTLS
        self.extras = extras
    }

    public var engine: ConnectionEngine? { ConnectionEngines.engine(id: engineID) }

    /// The port actually used when the field is left blank.
    public var effectivePort: Int? {
        Int(port.trimmingCharacters(in: .whitespaces)) ?? engine?.defaultPort
    }

    // MARK: - render

    /// The connection URL for these fields.
    ///
    /// `includingPassword` is false everywhere the URL is *shown* and true only
    /// on the one path that hands it to `datagrep_profiles_add`, which lifts the
    /// password into the keychain and drops it from the stored config.
    public func url(includingPassword: Bool = false) -> String {
        guard let engine else { return "" }
        if engine.isFileBased {
            let path = filePath.trimmingCharacters(in: .whitespaces)
            guard !path.isEmpty else { return "" }
            if path == ":memory:" { return path }
            // `sqlite://` + an absolute path is `sqlite:///Users/…` — three
            // slashes, and the driver's parser takes everything after the
            // second as the path.
            return engine.scheme + (path.hasPrefix("/") ? path : "/" + path)
        }

        let host = self.host.trimmingCharacters(in: .whitespaces)
        guard !host.isEmpty else { return "" }
        var out = (useTLS ? (engine.tlsScheme ?? engine.scheme) : engine.scheme)

        let user = username.trimmingCharacters(in: .whitespaces)
        if !user.isEmpty {
            out += Self.encode(user)
            if includingPassword, !password.isEmpty {
                out += ":" + Self.encode(password)
            }
            out += "@"
        }
        // An IPv6 literal has to keep its brackets or the `:` before the port
        // is read as part of the address.
        out += host.contains(":") && !host.hasPrefix("[") ? "[\(host)]" : host
        if let p = effectivePort { out += ":\(p)" }

        let db = database.trimmingCharacters(in: .whitespaces)
        if !db.isEmpty { out += "/" + db }
        let extras = self.extras.trimmingCharacters(in: .whitespaces)
        if !extras.isEmpty { out += "?" + extras }
        return out
    }

    private static func encode(_ s: String) -> String {
        var allowed = CharacterSet.alphanumerics
        allowed.insert(charactersIn: "-._~")
        return s.addingPercentEncoding(withAllowedCharacters: allowed) ?? s
    }

    private static func decode(_ s: String) -> String { s.removingPercentEncoding ?? s }

    // MARK: - parse

    /// Split a URL into fields. Returns nil only when the scheme names no
    /// engine this build knows; everything after that is best effort, because
    /// this runs on every keystroke in the URL box.
    public static func parse(_ url: String) -> ConnectionFields? {
        let trimmed = url.trimmingCharacters(in: .whitespacesAndNewlines)
        guard let engine = ConnectionEngines.engine(forURL: trimmed) else { return nil }
        var fields = ConnectionFields(engineID: engine.id)

        if trimmed.lowercased() == ":memory:" {
            fields.filePath = ":memory:"
            return fields
        }
        guard let schemeEnd = trimmed.range(of: "://") else { return fields }
        let scheme = trimmed[..<schemeEnd.lowerBound].lowercased() + "://"
        fields.useTLS = engine.tlsScheme.map { $0 == scheme } ?? false
        var rest = String(trimmed[schemeEnd.upperBound...])

        if engine.isFileBased {
            fields.filePath = rest
            return fields
        }

        if let q = rest.firstIndex(of: "?") {
            fields.extras = String(rest[rest.index(after: q)...])
            rest = String(rest[..<q])
        }
        var path = ""
        // `firstIndex`, so a path that itself contains `/` (an Elasticsearch
        // proxy prefix) is kept whole rather than truncated at the second one.
        if let slash = rest.firstIndex(of: "/") {
            path = String(rest[rest.index(after: slash)...])
            rest = String(rest[..<slash])
        }
        fields.database = decode(path)

        // `lastIndex`: a password may legally contain an `@`.
        if let at = rest.lastIndex(of: "@") {
            let userinfo = String(rest[..<at])
            rest = String(rest[rest.index(after: at)...])
            if let colon = userinfo.firstIndex(of: ":") {
                fields.username = decode(String(userinfo[..<colon]))
                fields.password = decode(String(userinfo[userinfo.index(after: colon)...]))
            } else {
                fields.username = decode(userinfo)
            }
        }

        if rest.hasPrefix("["), let close = rest.firstIndex(of: "]") {
            fields.host = String(rest[rest.index(after: rest.startIndex)..<close])
            let tail = rest[rest.index(after: close)...]
            if tail.hasPrefix(":") { fields.port = String(tail.dropFirst()) }
        } else if let colon = rest.lastIndex(of: ":") {
            fields.host = String(rest[..<colon])
            fields.port = String(rest[rest.index(after: colon)...])
        } else {
            fields.host = rest
        }
        return fields
    }

    /// Rebuild the fields from the `config` map `datagrep_profiles_get_json`
    /// reports.
    ///
    /// That call returns the *parsed* config and no `url` key at all, so
    /// without this the Edit sheet has nothing to show and has to tell the user
    /// it cannot read their connection back. The key names are each driver's
    /// own `config_schema` fields; a key this does not know about is ignored
    /// rather than guessed at, and the secret field is skipped entirely (the
    /// ABI masks it to `••••`, which must never be pasted into a URL).
    public static func fromConfig(driver: String, config: [String: Any]) -> ConnectionFields? {
        guard let engine = ConnectionEngines.engine(id: driver) else { return nil }
        var fields = ConnectionFields(engineID: engine.id)

        func str(_ key: String) -> String? {
            guard let s = config[key] as? String, !s.isEmpty, s != "••••" else { return nil }
            return s
        }
        func num(_ key: String) -> String? {
            if let n = config[key] as? NSNumber { return String(n.intValue) }
            if let s = str(key) { return s }
            return nil
        }
        func flag(_ key: String) -> Bool? {
            if let b = config[key] as? Bool { return b }
            if let s = str(key) { return s == "true" || s == "require" }
            return nil
        }

        if engine.isFileBased {
            fields.filePath = str("path") ?? ""
            return fields
        }
        // Mongo stores a host list; the first entry is the one the structured
        // form can show, and a real multi-host URL keeps its shape because the
        // sheet leaves such a profile on the raw URL.
        fields.host = str("host") ?? str("hosts")?.split(separator: ",").first.map(String.init) ?? ""
        fields.port = num("port") ?? ""
        fields.username = str("user") ?? str("username") ?? ""
        fields.database = str("database") ?? str("db") ?? str("index") ?? ""
        if engine.tlsScheme != nil { fields.useTLS = flag("tls") ?? false }
        return fields
    }
}
