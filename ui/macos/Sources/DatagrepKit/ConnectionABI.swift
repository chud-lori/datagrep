import CDatagrepFFI
import Darwin
import Foundation

// MARK: - how real is the read-only promise

/// How strongly a profile's read-only flag is actually enforced.
///
/// This mirrors `datagrep_api::Enforcement`, plus a fourth case the Rust enum
/// does not need: `.unknown`, for a build whose ABI does not report the level
/// at all. A badge implying the *server* refuses writes when only our own
/// classifier does is worse than showing no badge, so `.unknown` is
/// deliberately worded down to the same promise as `.client` — never up.
public enum ReadOnlyEnforcement: String, Sendable, Hashable, CaseIterable {
    /// The engine itself refuses writes on this session.
    case server
    /// Only datagrep's client-side classifier stands in the way.
    case client
    /// Nothing enforces it.
    case none
    /// This build does not say. Treated as *at most* `.client`.
    case unknown

    public init(abi: String?) {
        switch abi?.lowercased() {
        case "server": self = .server
        case "client": self = .client
        case "none": self = .none
        default: self = .unknown
        }
    }

    /// The one line that goes on the badge. Plain words, no jargon, and it
    /// never claims more protection than exists.
    public var headline: String {
        switch self {
        case .server: return "read-only — enforced by the server"
        case .client: return "read-only — blocked by datagrep only"
        case .none: return "read-only — blocked by datagrep only"
        case .unknown: return "read-only — blocked by datagrep only"
        }
    }

    /// The sentence under it, for a tooltip or the editor sheet.
    public var detail: String {
        switch self {
        case .server:
            return
                "The engine opened this session read-only and will refuse a write itself, even if something gets past datagrep."
        case .client:
            return
                "datagrep classifies each statement and refuses to send writes. The server would still accept one — anything that bypasses datagrep is not protected."
        case .none:
            return
                "datagrep refuses to send writes on this connection. The engine accepted no read-only session at all, so anything that is not datagrep — another client, a script, a shell on the box — can still write freely."
        case .unknown:
            return
                "datagrep classifies each statement and refuses to send writes. This build does not report whether the engine also refuses them, so assume it does not."
        }
    }

    /// The clause a refusal message uses. Short enough for the status bar, and
    /// it still separates "the server would refuse this too" from "we are the
    /// only thing in the way".
    public var refusalClause: String {
        switch self {
        case .server: return "the server enforces it too"
        case .client, .unknown, .none: return "datagrep is the only thing enforcing it"
        }
    }

    /// Short word for the compact badge next to the lock.
    public var shortLabel: String {
        switch self {
        case .server: return "SERVER"
        case .client, .unknown: return "APP ONLY"
        case .none: return "APP ONLY"
        }
    }

    /// A lock, always — the difference between the levels is carried by the
    /// glyph *and* by the word, never by tint alone (a tint is the part of a
    /// safety signal most likely to be invisible in one theme or the other).
    public var symbol: String {
        switch self {
        case .server: return "lock.shield.fill"
        case .client, .unknown, .none: return "lock.fill"
        }
    }

    /// True only when the engine is the thing saying no.
    ///
    /// datagrep refuses a write on a read-only profile in *every* case — that
    /// part is a real guarantee it can make on its own. This is the only thing
    /// that decides whether the UI is allowed to say the server is also
    /// protecting you.
    public var isServerEnforced: Bool { self == .server }
}

// MARK: - the optional half of the profile ABI

/// The connection-editing calls, bound at run time rather than at link time.
///
/// `datagrep_profiles_update` / `datagrep_profiles_get_json` are being added to
/// the engine by a separate change. Declaring them in the frozen header would
/// make this UI fail to *link* against a core that does not have them yet;
/// looking them up with `dlsym` instead means an older core produces a UI that
/// says so out loud (see `ProfileABI.unavailableReason`) rather than a dialog
/// with a Save button that quietly does nothing.
///
/// Nothing here caches a `char*`: every call goes through the same
/// `datagrepTry` ownership rules as the linked half of the ABI.
public enum ProfileABI {
    public typealias UpdateFn = @convention(c) (
        OpaquePointer?, UnsafePointer<CChar>?, UnsafePointer<CChar>?,
        UnsafeMutablePointer<UnsafeMutablePointer<CChar>?>?
    ) -> Bool

    public typealias GetFn = @convention(c) (
        OpaquePointer?, UnsafePointer<CChar>?,
        UnsafeMutablePointer<UnsafeMutablePointer<CChar>?>?
    ) -> UnsafeMutablePointer<CChar>?

    private static func lookup<T>(_ symbol: String, as: T.Type) -> T? {
        // RTLD_DEFAULT: search this image and everything already loaded. The
        // engine is linked statically into the executable, so this is the
        // executable's own symbol table.
        guard let p = dlsym(UnsafeMutableRawPointer(bitPattern: -2), symbol) else { return nil }
        return unsafeBitCast(p, to: T.self)
    }

    /// `datagrep_connection_test_json(core, name, url, err_out)`. Either
    /// selector may be NULL; see the Rust doc comment.
    public typealias TestFn = @convention(c) (
        OpaquePointer?, UnsafePointer<CChar>?, UnsafePointer<CChar>?,
        UnsafeMutablePointer<UnsafeMutablePointer<CChar>?>?
    ) -> UnsafeMutablePointer<CChar>?

    static let update: UpdateFn? = lookup("datagrep_profiles_update", as: UpdateFn.self)
    static let fetch: GetFn? = lookup("datagrep_profiles_get_json", as: GetFn.self)
    static let test: TestFn? = lookup("datagrep_connection_test_json", as: TestFn.self)

    /// Can this build save an edited connection at all?
    public static var canEdit: Bool { update != nil }
    /// Can it read one back field-by-field to pre-populate the sheet?
    public static var canPrefill: Bool { fetch != nil }
    /// Can it dial a connection and report what answered? Same `dlsym` reason
    /// as the two above: the stub engine has no such symbol, and a Test button
    /// that links against nothing is worse than one that is not offered.
    public static var canTest: Bool { test != nil }

    /// Exactly why the editor is disabled, in words a user can act on.
    public static var unavailableReason: String? {
        canEdit
            ? nil
            : "This build of the datagrep engine has no `datagrep_profiles_update` call, so an edited connection could not be saved. Remove and re-add the connection, or update the engine."
    }
}

// MARK: - values

/// Everything the editor sheet needs about one saved connection.
///
/// Every field is optional in the wire format and defaulted here: the sheet has
/// to open against an engine that reports four keys and against one that
/// reports ten, without either version guessing.
public struct ProfileDetail: Sendable, Hashable {
    public var name: String
    public var url: String
    public var driver: String
    public var env: String
    public var readOnly: Bool
    public var confirmWrites: Bool
    public var autoLimit: Int?
    public var idleTimeoutS: Int?
    public var color: String?
    public var hasSecret: Bool
    public var enforcement: ReadOnlyEnforcement
    /// Keys the engine actually sent. The sheet greys out what it cannot round
    /// trip instead of showing an empty box that looks like a cleared value.
    public var reported: Set<String>
    /// Host / port / database / user, rebuilt from the engine's parsed
    /// `config`. `datagrep_profiles_get_json` reports no `url` key at all, so
    /// this is the only way the editor learns what the connection points at —
    /// without it the sheet can only say it cannot read the connection back.
    public var fields: ConnectionFields?

    public init(
        name: String, url: String = "", driver: String = "", env: String = "dev",
        readOnly: Bool = false, confirmWrites: Bool = false, autoLimit: Int? = nil,
        idleTimeoutS: Int? = nil, color: String? = nil, hasSecret: Bool = false,
        enforcement: ReadOnlyEnforcement = .unknown, reported: Set<String> = [],
        fields: ConnectionFields? = nil
    ) {
        self.name = name
        self.url = url
        self.driver = driver
        self.env = env
        self.readOnly = readOnly
        self.confirmWrites = confirmWrites
        self.autoLimit = autoLimit
        self.idleTimeoutS = idleTimeoutS
        self.color = color
        self.hasSecret = hasSecret
        self.enforcement = enforcement
        self.reported = reported
        self.fields = fields
    }

    /// Decodes whatever subset of the documented shape is present. An engine
    /// that reports nothing but a name still produces a usable value.
    public static func decode(_ dict: [String: Any], fallbackName: String) -> ProfileDetail {
        var d = ProfileDetail(name: dict["name"] as? String ?? fallbackName)
        d.reported = Set(dict.keys)
        d.url = ProfileDetail.string(dict, "url") ?? ProfileDetail.string(dict, "dsn") ?? ""
        d.driver =
            ProfileDetail.string(dict, "driver") ?? ProfileDetail.string(dict, "driver_id") ?? ""
        d.env = ProfileDetail.string(dict, "env") ?? "dev"
        d.readOnly = dict["read_only"] as? Bool ?? false
        d.confirmWrites = dict["confirm_writes"] as? Bool ?? false
        d.autoLimit = ProfileDetail.int(dict, "auto_limit")
        d.idleTimeoutS = ProfileDetail.int(dict, "idle_timeout_s")
        d.color = ProfileDetail.string(dict, "color")
        d.hasSecret = dict["has_secret"] as? Bool ?? (dict["secret_ref"] is String)
        d.enforcement = ReadOnlyEnforcement(
            abi: ProfileDetail.string(dict, "enforcement")
                ?? ProfileDetail.string(dict, "read_only_enforcement"))
        if let config = dict["config"] as? [String: Any] {
            d.fields = ConnectionFields.fromConfig(driver: d.driver, config: config)
            // The URL is derived, never stored, so an engine that stops
            // reporting `config` degrades to the old "we do not know" wording
            // instead of showing a URL assembled out of nothing.
            if d.url.isEmpty, let rendered = d.fields?.url(), !rendered.isEmpty {
                d.url = rendered
                d.reported.insert("url")
            }
        }
        return d
    }

    static func string(_ d: [String: Any], _ key: String) -> String? {
        guard let s = d[key] as? String, !s.isEmpty else { return nil }
        return s
    }

    static func int(_ d: [String: Any], _ key: String) -> Int? {
        if let n = d[key] as? Int { return n }
        if let n = d[key] as? NSNumber { return n.intValue }
        if let s = d[key] as? String { return Int(s) }
        return nil
    }
}

/// A JSON patch for `datagrep_profiles_update`. Only the keys the user actually
/// changed are ever put in it — a full-object PUT would round-trip fields this
/// build does not understand and silently reset them.
public struct ProfilePatch {
    private var fields: [String: Any] = [:]
    public init() {}

    public mutating func set(_ key: String, _ value: String?) {
        fields[key] = value.map { $0 as Any } ?? NSNull()
    }
    public mutating func set(_ key: String, _ value: Bool) { fields[key] = value }
    public mutating func set(_ key: String, _ value: Int?) {
        fields[key] = value.map { $0 as Any } ?? NSNull()
    }

    public var isEmpty: Bool { fields.isEmpty }
    public var changedKeys: [String] { fields.keys.sorted() }

    /// Serialised on the caller's thread, so what crosses to the worker queue is
    /// an immutable `String` and not a box of `Any`.
    public var json: String {
        guard let data = try? JSONSerialization.data(withJSONObject: fields, options: [.sortedKeys]),
            let text = String(data: data, encoding: .utf8)
        else { return "{}" }
        return text
    }
}

/// What a Test Connection actually found. Built only from a handshake that
/// succeeded — a failure is thrown, with the driver's own message, because
/// "could not connect" with the reason removed is the thing the button exists
/// to replace.
public struct ConnectionTestResult: Sendable, Hashable {
    public let driver: String
    public let product: String
    public let version: String
    public let details: [(String, String)]
    public let elapsedMs: UInt64

    /// One line for the callout: what answered, and how long it took.
    public var headline: String {
        var s = product.isEmpty ? EngineStyle.displayName(for: driver) : product
        if !version.isEmpty, version.lowercased() != "unknown" { s += " \(version)" }
        return "Connected to \(s) in \(elapsedMs) ms"
    }

    public static func == (a: ConnectionTestResult, b: ConnectionTestResult) -> Bool {
        a.driver == b.driver && a.product == b.product && a.version == b.version
            && a.elapsedMs == b.elapsedMs && a.details.map(\.0) == b.details.map(\.0)
            && a.details.map(\.1) == b.details.map(\.1)
    }

    public func hash(into hasher: inout Hasher) {
        hasher.combine(driver)
        hasher.combine(product)
        hasher.combine(version)
    }
}

// MARK: - calls

extension DatagrepCoreHandle {
    /// Dial once and report what answered. Blocking, and it opens a socket —
    /// call it from a background queue, never the main one.
    ///
    /// Pass a saved profile's `name` to test what a query would really run
    /// against (keychain secret included), or a `url` to test something that
    /// has not been saved yet, which is what the New Connection sheet has.
    public func testConnection(name: String? = nil, url: String? = nil) throws
        -> ConnectionTestResult
    {
        guard let test = ProfileABI.test else {
            throw DatagrepError(
                "This build of the datagrep engine has no `datagrep_connection_test_json` call, so a connection cannot be tested from here. Update the engine."
            )
        }
        let json = try datagrepTry { errOut in
            // Both arguments are passed every time; the Rust side treats an
            // empty string exactly as it treats NULL, so there is no nested
            // optional-withCString dance to get wrong.
            (name ?? "").withCString { n in
                (url ?? "").withCString { u in takeOwnedString(test(raw, n, u, errOut)) }
            }
        }
        guard let dict = jsonObject(json) as? [String: Any] else {
            throw DatagrepError("datagrep_connection_test_json did not return an object")
        }
        let details = (dict["details"] as? [[Any]] ?? []).compactMap { pair -> (String, String)? in
            guard pair.count == 2, let k = pair[0] as? String, let v = pair[1] as? String else {
                return nil
            }
            return (k, v)
        }
        return ConnectionTestResult(
            driver: dict["driver"] as? String ?? "",
            product: dict["product"] as? String ?? "",
            version: dict["version"] as? String ?? "",
            details: details,
            elapsedMs: (dict["elapsed_ms"] as? NSNumber)?.uint64Value ?? 0)
    }

    /// Full detail for one profile, for pre-populating the editor.
    ///
    /// Falls back to the list call when `datagrep_profiles_get_json` is absent,
    /// so the sheet still opens with a name, a driver and an env rather than
    /// refusing to appear. The URL is simply blank in that case, and the sheet
    /// says why instead of showing an empty field that looks like a cleared one.
    public func profileDetail(name: String) throws -> ProfileDetail {
        if let fetch = ProfileABI.fetch {
            let json = try datagrepTry { errOut in
                name.withCString { n in takeOwnedString(fetch(raw, n, errOut)) }
            }
            guard let dict = jsonObject(json) as? [String: Any] else {
                throw DatagrepError("datagrep_profiles_get_json did not return an object")
            }
            return ProfileDetail.decode(dict, fallbackName: name)
        }
        guard let p = try profiles().first(where: { $0.name == name }) else {
            throw DatagrepError("no connection named `\(name)`")
        }
        return ProfileDetail(
            name: p.name, url: "", driver: p.driver, env: p.env, readOnly: p.readOnly,
            confirmWrites: p.confirmWrites, color: p.color, hasSecret: p.hasSecret,
            enforcement: p.enforcement, reported: ["name", "driver", "env"])
    }

    /// Applies a JSON patch. Throws — loudly — on a build that cannot save.
    public func updateProfile(name: String, patchJSON: String) throws {
        guard let update = ProfileABI.update else {
            throw DatagrepError(
                ProfileABI.unavailableReason ?? "this engine build cannot edit connections")
        }
        try name.withCString { n in
            try patchJSON.withCString { p in
                try datagrepTryBool { errOut in update(raw, n, p, errOut) }
            }
        }
    }
}
