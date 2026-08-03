import CDbxFFI
import Foundation

public struct Profile: Sendable, Hashable {
    public let name: String
    public let driver: String
    public let env: String
    public let hasSecret: Bool
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

/// Owns the `DbxCore*`. Freed exactly once, in `deinit`.
public final class DbxCoreHandle: @unchecked Sendable {
    let raw: OpaquePointer

    public init(profilesDBPath: String) throws {
        self.raw = try profilesDBPath.withCString { path in
            try dbxTry { errOut in dbx_core_new(path, errOut) }
        }
    }

    deinit { dbx_core_free(raw) }

    // MARK: profiles

    public func profiles() throws -> [Profile] {
        let json = try dbxTry { errOut in takeOwnedString(dbx_profiles_list_json(raw, errOut)) }
        guard let arr = jsonObject(json) as? [[String: Any]] else { return [] }
        return arr.compactMap { d in
            guard let n = d["name"] as? String else { return nil }
            return Profile(
                name: n,
                driver: d["driver"] as? String ?? "?",
                env: d["env"] as? String ?? "dev",
                hasSecret: d["has_secret"] as? Bool ?? false)
        }
    }

    public func addProfile(name: String, url: String) throws {
        try name.withCString { n in
            try url.withCString { u in
                try dbxTryBool { errOut in dbx_profiles_add(raw, n, u, errOut) }
            }
        }
    }

    public func removeProfile(name: String) throws {
        try name.withCString { n in
            try dbxTryBool { errOut in dbx_profiles_remove(raw, n, errOut) }
        }
    }

    // MARK: catalog — ONE LEVEL PER CALL, never a crawl

    public func children(profile: String, path: [String]) throws -> [CatalogEntry] {
        let pathJSON = Self.encodePath(path)
        let json = try dbxTry { errOut in
            profile.withCString { p in
                pathJSON.withCString { pj in
                    takeOwnedString(dbx_catalog_children_json(raw, p, pj, errOut))
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
        return try dbxTry { errOut in
            profile.withCString { p in
                pathJSON.withCString { pj in
                    takeOwnedString(dbx_catalog_describe_json(raw, p, pj, errOut))
                }
            }
        }
    }

    // MARK: query

    public func run(profile: String, sql: String) throws -> DbxQueryHandle {
        let ptr = try dbxTry { errOut in
            profile.withCString { p in
                sql.withCString { s in dbx_query_run(raw, p, s, errOut) }
            }
        }
        return DbxQueryHandle(raw: ptr)
    }

    static func encodePath(_ path: [String]) -> String {
        let data = try? JSONSerialization.data(withJSONObject: path, options: [])
        return data.flatMap { String(data: $0, encoding: .utf8) } ?? "[]"
    }
}
