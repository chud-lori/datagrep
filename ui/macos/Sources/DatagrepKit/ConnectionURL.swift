import Foundation

/// One engine, as the connection dialogs need to describe it.
public struct ConnectionEngine: Sendable, Hashable, Identifiable {
    public let id: String
    public let scheme: String
    public let aliases: [String]
    public let tlsScheme: String?
    public let defaultPort: Int?
    public let example: String
    public let isFileBased: Bool
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

    public static func engine(forURL url: String) -> ConnectionEngine? {
        let u = url.lowercased().trimmingCharacters(in: .whitespacesAndNewlines)
        if u == ":memory:" { return engine(id: "sqlite") }
        return all.first { e in e.allSchemes.contains { u.hasPrefix($0) } }
    }
}

public struct ConnectionFields: Hashable, Sendable {
    public var engineID: String
    public var host: String
    public var port: String
    public var database: String
    public var username: String
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
    public func url(includingPassword: Bool = false) -> String {
        guard let engine else { return "" }
        if engine.isFileBased {
            let path = filePath.trimmingCharacters(in: .whitespaces)
            guard !path.isEmpty else { return "" }
            if path == ":memory:" { return path }
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
        fields.host = str("host") ?? str("hosts")?.split(separator: ",").first.map(String.init) ?? ""
        fields.port = num("port") ?? ""
        fields.username = str("user") ?? str("username") ?? ""
        fields.database = str("database") ?? str("db") ?? str("index") ?? ""
        if engine.tlsScheme != nil { fields.useTLS = flag("tls") ?? false }
        return fields
    }
}
