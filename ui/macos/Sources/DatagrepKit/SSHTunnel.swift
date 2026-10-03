import CDatagrepFFI
import Foundation

public enum SSHAuth: String, Sendable, CaseIterable, Identifiable {
    case agent, key, password

    public var id: String { rawValue }

    public var title: String {
        switch self {
        case .agent: return "SSH Agent"
        case .key: return "Key File"
        case .password: return "Password"
        }
    }
}

/// A profile's SSH tunnel as the profile ABI reports it; the secret itself never crosses.
public struct SSHSettings: Sendable, Hashable {
    public var host: String
    public var port: Int
    public var user: String
    public var auth: SSHAuth
    public var keyPath: String
    public var hasSecret: Bool

    public init(
        host: String, port: Int = 22, user: String, auth: SSHAuth = .agent, keyPath: String = "",
        hasSecret: Bool = false
    ) {
        self.host = host
        self.port = port
        self.user = user
        self.auth = auth
        self.keyPath = keyPath
        self.hasSecret = hasSecret
    }

    public static func decode(_ value: Any?) -> SSHSettings? {
        guard let d = value as? [String: Any], let host = d["host"] as? String else { return nil }
        return SSHSettings(
            host: host,
            port: (d["port"] as? NSNumber)?.intValue ?? 22,
            user: d["user"] as? String ?? "",
            auth: SSHAuth(rawValue: d["auth"] as? String ?? "") ?? .agent,
            keyPath: d["key_path"] as? String ?? "",
            hasSecret: d["has_secret"] as? Bool ?? false)
    }

    /// Same route and credentials kind, ignoring whether a secret is saved.
    public func sameSettings(as other: SSHSettings?) -> Bool {
        guard let other else { return false }
        return host == other.host && port == other.port && user == other.user
            && auth == other.auth && (auth != .key || keyPath == other.keyPath)
    }

    /// The `ssh` object for the profile ABI; an empty `secret` keeps the saved one.
    public func json(secret: String) -> [String: Any] {
        var d: [String: Any] = ["host": host, "port": port, "user": user, "auth": auth.rawValue]
        if auth == .key { d["key_path"] = keyPath }
        if auth != .agent, !secret.isEmpty { d["secret"] = secret }
        return d
    }
}

public struct HostKeyReview: Sendable, Hashable {
    public enum Status: String, Sendable { case trusted, unknown, changed }

    public let host: String
    public let port: Int
    public let algorithm: String
    public let fingerprint: String
    public let status: Status
    public let expected: String?
    public let knownHosts: String
}

func jsonText(_ object: [String: Any]) -> String {
    guard let data = try? JSONSerialization.data(withJSONObject: object, options: [.sortedKeys]),
        let text = String(data: data, encoding: .utf8)
    else { return "{}" }
    return text
}

extension DatagrepCoreHandle {
    /// Key exchange only: shows what the host offers before anything is trusted or sent.
    public func reviewHostKey(host: String, port: Int) throws -> HostKeyReview {
        let json = try datagrepTry { errOut in
            host.withCString { h in
                takeOwnedString(datagrep_ssh_host_key_json(raw, h, UInt16(clamping: port), errOut))
            }
        }
        guard let d = jsonObject(json) as? [String: Any] else {
            throw DatagrepError("datagrep_ssh_host_key_json did not return an object")
        }
        return HostKeyReview(
            host: d["host"] as? String ?? host,
            port: (d["port"] as? NSNumber)?.intValue ?? port,
            algorithm: d["algorithm"] as? String ?? "",
            fingerprint: d["fingerprint"] as? String ?? "",
            status: HostKeyReview.Status(rawValue: d["status"] as? String ?? "") ?? .unknown,
            expected: d["expected"] as? String,
            knownHosts: d["known_hosts"] as? String ?? "")
    }

    public func trustHostKey(host: String, port: Int, fingerprint: String) throws {
        try host.withCString { h in
            try fingerprint.withCString { f in
                try datagrepTryBool { errOut in
                    datagrep_ssh_trust_host_key(raw, h, UInt16(clamping: port), f, errOut)
                }
            }
        }
    }

    /// `ssh`: nil = the saved tunnel, `NSNull()` = none, or a `SSHSettings.json` object.
    public func testConnection(name: String?, url: String?, ssh: Any?) throws
        -> ConnectionTestResult
    {
        let options = ssh.map { jsonText(["ssh": $0]) } ?? ""
        let json = try datagrepTry { errOut in
            (name ?? "").withCString { n in
                (url ?? "").withCString { u in
                    options.withCString { o in
                        takeOwnedString(datagrep_connection_test_with_json(raw, n, u, o, errOut))
                    }
                }
            }
        }
        return try ConnectionTestResult.decode(json)
    }
}
