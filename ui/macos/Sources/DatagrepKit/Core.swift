import CDatagrepFFI
import Foundation

/// One saved connection, as `datagrep_profiles_list_json` describes it.
///
/// Everything past `hasSecret` is decoded defensively: the safety fields
/// (`env`, `read_only`, `enforcement`, …) are being added to the list payload
/// by a separate change, and a build without them must degrade to "we do not
/// know" rather than to "not read-only" — see `ReadOnlyEnforcement`.
public struct Profile: Sendable, Hashable {
    public let name: String
    public let driver: String
    public let env: String
    public let hasSecret: Bool
    public let readOnly: Bool
    public let confirmWrites: Bool
    public let enforcement: ReadOnlyEnforcement
    public let color: String?

    public init(
        name: String, driver: String, env: String, hasSecret: Bool, readOnly: Bool = false,
        confirmWrites: Bool = false, enforcement: ReadOnlyEnforcement = .unknown,
        color: String? = nil
    ) {
        self.name = name
        self.driver = driver
        self.env = env
        self.hasSecret = hasSecret
        self.readOnly = readOnly
        self.confirmWrites = confirmWrites
        self.enforcement = enforcement
        self.color = color
    }

    public var isProd: Bool { env == "prod" }
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
    /// A build without it cannot be asked which connections are protected, and
    /// the UI must not let the absence of a badge read as "checked, and safe".
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
                env: d["env"] as? String ?? "dev",
                hasSecret: d["has_secret"] as? Bool ?? false,
                readOnly: d["read_only"] as? Bool ?? false,
                confirmWrites: d["confirm_writes"] as? Bool ?? false,
                enforcement: ReadOnlyEnforcement(
                    abi: d["enforcement"] as? String ?? d["read_only_enforcement"] as? String),
                color: (d["color"] as? String).flatMap { $0.isEmpty ? nil : $0 })
        }
    }


    public func addProfile(name: String, url: String) throws {
        try name.withCString { n in
            try url.withCString { u in
                try datagrepTryBool { errOut in datagrep_profiles_add(raw, n, u, errOut) }
            }
        }
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
