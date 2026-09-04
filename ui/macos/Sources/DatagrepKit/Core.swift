import CDatagrepFFI
import Foundation

/// One saved connection, as `datagrep_profiles_list_json` describes it.
public struct Profile: Sendable, Hashable {
    public let name: String
    public let driver: String
    public let hasSecret: Bool
    public let readOnly: Bool
    public let safety: SafetyLevel
    public let enforcement: ReadOnlyEnforcement
    public let color: String?

    public init(
        name: String, driver: String, hasSecret: Bool, readOnly: Bool = false,
        safety: SafetyLevel = .silent, enforcement: ReadOnlyEnforcement = .unknown,
        color: String? = nil
    ) {
        self.name = name
        self.driver = driver
        self.hasSecret = hasSecret
        self.readOnly = readOnly
        self.safety = safety
        self.enforcement = enforcement
        self.color = color
    }

}

public enum Enumeration: String, Sendable {
    case cheap, scanOnly = "scan_only", paged, onDemand = "on_demand"

    /// The single rule that stops the app firing `KEYS *` at a 40 GB Redis.
    public var autoExpandable: Bool { self == .cheap }
}

public struct CatalogEntry: Sendable {
    public let name: String
    public let kind: String
    public let hasChildren: Bool
    public let enumeration: Enumeration
}

/// Owns the `DatagrepCore*`. Freed exactly once, in `deinit`.
public final class DatagrepCoreHandle: @unchecked Sendable {
    let raw: OpaquePointer

    public init(profilesDBPath: String) throws {
        self.raw = try profilesDBPath.withCString { path in
            try datagrepTry { errOut in datagrep_core_new(path, errOut) }
        }
    }

    deinit { datagrep_core_free(raw) }

    // MARK: profiles

    /// True once a `_list_json` payload has been seen carrying `read_only`.
    public private(set) var listReportsReadOnly = false

    public func profiles() throws -> [Profile] {
        let json = try datagrepTry { errOut in takeOwnedString(datagrep_profiles_list_json(raw, errOut)) }
        guard let arr = jsonObject(json) as? [[String: Any]] else { return [] }
        listReportsReadOnly = arr.first?["read_only"] != nil
        return arr.compactMap { d in
            guard let n = d["name"] as? String else { return nil }
            return Profile(
                name: n,
                driver: d["driver"] as? String ?? "?",
                hasSecret: d["has_secret"] as? Bool ?? false,
                readOnly: d["read_only"] as? Bool ?? false,
                safety: SafetyLevel(abi: d["safety"] as? String) ?? .silent,
                enforcement: ReadOnlyEnforcement(
                    abi: d["enforcement"] as? String ?? d["read_only_enforcement"] as? String),
                color: (d["color"] as? String).flatMap { $0.isEmpty ? nil : $0 })
        }
    }

    public struct ConnectionInfo: Sendable, Equatable {
        public let profile: String
        public let driver: String
        public let database: String?
        public let product: String?
        public let version: String?

        public init(
            profile: String, driver: String, database: String?, product: String?, version: String?
        ) {
            self.profile = profile
            self.driver = driver
            self.database = database
            self.product = product
            self.version = version
        }
    }

    public func connectionInfo(profile: String) throws -> ConnectionInfo {
        let json = try profile.withCString { n in
            try datagrepTry { errOut in
                takeOwnedString(datagrep_connection_info_json(raw, n, errOut))
            }
        }
        guard let d = jsonObject(json) as? [String: Any] else {
            throw DatagrepError("the connection info was not an object")
        }
        let server = d["server"] as? [String: Any]
        return ConnectionInfo(
            profile: d["profile"] as? String ?? profile,
            driver: d["driver"] as? String ?? "?",
            database: (d["database"] as? String).flatMap { $0.isEmpty ? nil : $0 },
            product: (server?["product"] as? String).flatMap { $0.isEmpty ? nil : $0 },
            version: (server?["version"] as? String).flatMap { $0.isEmpty ? nil : $0 })
    }

    public func addProfile(name: String, url: String, safety: SafetyLevel = .silent) throws {
        let options = #"{"safety":"\#(safety.rawValue)"}"#
        if let add = ProfileABI.addJSON {
            return try name.withCString { n in
                try url.withCString { u in
                    try options.withCString { o in
                        try datagrepTryBool { errOut in add(raw, n, u, o, errOut) }
                    }
                }
            }
        }
        try name.withCString { n in
            try url.withCString { u in
                try datagrepTryBool { errOut in datagrep_profiles_add(raw, n, u, errOut) }
            }
        }
        if safety != .silent { try updateProfile(name: name, patchJSON: options) }
    }

    public func removeProfile(name: String) throws {
        try name.withCString { n in
            try datagrepTryBool { errOut in datagrep_profiles_remove(raw, n, errOut) }
        }
    }

    // MARK: catalog — ONE LEVEL PER CALL, never a crawl

    public func children(profile: String, path: [String]) throws -> [CatalogEntry] {
        let pathJSON = Self.encodePath(path)
        let json = try datagrepTry { errOut in
            profile.withCString { p in
                pathJSON.withCString { pj in
                    takeOwnedString(datagrep_catalog_children_json(raw, p, pj, errOut))
                }
            }
        }
        guard let arr = jsonObject(json) as? [[String: Any]] else { return [] }
        return arr.compactMap { d in
            guard let n = d["name"] as? String else { return nil }
            return CatalogEntry(
                name: n,
                kind: d["kind"] as? String ?? "node",
                hasChildren: d["has_children"] as? Bool ?? false,
                enumeration: Enumeration(rawValue: d["enumeration"] as? String ?? "cheap")
                    ?? .cheap)
        }
    }

    public func describe(profile: String, path: [String]) throws -> String {
        let pathJSON = Self.encodePath(path)
        return try datagrepTry { errOut in
            profile.withCString { p in
                pathJSON.withCString { pj in
                    takeOwnedString(datagrep_catalog_describe_json(raw, p, pj, errOut))
                }
            }
        }
    }

    // MARK: query

    public func run(profile: String, sql: String) throws -> DatagrepQueryHandle {
        let ptr = try datagrepTry { errOut in
            profile.withCString { p in
                sql.withCString { s in datagrep_query_run(raw, p, s, errOut) }
            }
        }
        return DatagrepQueryHandle(raw: ptr)
    }

    static func encodePath(_ path: [String]) -> String {
        let data = try? JSONSerialization.data(withJSONObject: path, options: [])
        return data.flatMap { String(data: $0, encoding: .utf8) } ?? "[]"
    }
}

/// The statement that reads one catalog object. The engine's own language and
/// quoting live in the core; nothing here assembles a dialect string.
public enum BrowseStatement {
    public static func forObject(driver: String, path: [String], database: String?) throws -> String
    {
        let pathJSON = DatagrepCoreHandle.encodePath(path)
        return try driver.withCString { d in
            try pathJSON.withCString { pj in
                try withOptionalCString(database) { db in
                    try datagrepTry { errOut in
                        takeOwnedString(datagrep_browse_statement(d, pj, db, errOut))
                    }
                }
            }
        }
    }
}

private func withOptionalCString<T>(
    _ text: String?, _ body: (UnsafePointer<CChar>?) throws -> T
) rethrows -> T {
    guard let text else { return try body(nil) }
    return try text.withCString { try body($0) }
}
