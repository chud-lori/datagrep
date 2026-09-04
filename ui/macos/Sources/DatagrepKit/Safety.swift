import CDatagrepFFI
import Foundation

/// One connection's rung on the query-safety ladder.
public enum SafetyLevel: String, CaseIterable, Sendable, Hashable {
    case silent
    case warnAll = "warn_all"
    case warnWrites = "warn_writes"
    case authAll = "auth_all"
    case authWrites = "auth_writes"

    public init?(abi: String?) {
        guard let abi, let level = SafetyLevel(rawValue: abi) else { return nil }
        self = level
    }

    public var title: String {
        switch self {
        case .silent: return "Silent"
        case .warnAll: return "Warn on every query"
        case .warnWrites: return "Warn on writes"
        case .authAll: return "Authenticate on every query"
        case .authWrites: return "Authenticate on writes"
        }
    }

    public var detail: String {
        switch self {
        case .silent:
            return "Statements are sent as soon as you run them."
        case .warnAll:
            return "Every statement, reads included, is shown for confirmation before it is sent."
        case .warnWrites:
            return "Reads go straight through. Anything else is shown for confirmation first."
        case .authAll:
            return
                "Every statement, reads included, needs Touch ID or the connection name typed out before it is sent."
        case .authWrites:
            return
                "Reads go straight through. Anything else needs Touch ID or the connection name typed out."
        }
    }

    public var symbol: String {
        switch self {
        case .silent: return "lock.open"
        case .warnAll: return "lock.fill"
        case .warnWrites: return "lock"
        case .authAll: return "lock.shield.fill"
        case .authWrites: return "lock.shield"
        }
    }

    public var asksForAnything: Bool { self != .silent }
}

public enum SafetyRequirement: String, Sendable, Hashable {
    case none, warn, authenticate

    public init(abi: String?) { self = SafetyRequirement(rawValue: abi ?? "") ?? .none }
}

/// What datagrep-lang called one statement. Anything but `read` is gated as a write.
public enum StatementClass: String, Sendable, Hashable {
    case read, write, ddl, tcl, admin, unknown

    public init(abi: String?) { self = StatementClass(rawValue: abi ?? "") ?? .unknown }

    public var label: String {
        switch self {
        case .read: return "READ"
        case .write: return "WRITE"
        case .ddl: return "SCHEMA"
        case .tcl: return "TRANSACTION"
        case .admin: return "ADMIN"
        case .unknown: return "UNCLASSIFIED"
        }
    }

    public var note: String {
        switch self {
        case .read: return "reads data"
        case .write: return "changes data"
        case .ddl: return "changes the schema"
        case .tcl: return "changes the transaction"
        case .admin: return "an administrative command"
        case .unknown: return "datagrep could not classify it, so it counts as a write"
        }
    }
}

public struct SafetyStatement: Sendable, Hashable {
    public let text: String
    public let kind: StatementClass
    public let requires: SafetyRequirement
}

/// The engine's verdict on one statement, plus the challenge that clears it.
public struct SafetyDecision: Sendable, Hashable, Identifiable {
    public let profile: String
    public let level: SafetyLevel
    public let requires: SafetyRequirement
    public let challenge: String?
    public let statements: [SafetyStatement]

    public var id: String { challenge ?? profile }

    public static func decode(_ any: Any?) -> SafetyDecision? {
        guard let d = any as? [String: Any], let profile = d["profile"] as? String else {
            return nil
        }
        let statements = (d["statements"] as? [[String: Any]] ?? []).map { s in
            SafetyStatement(
                text: s["text"] as? String ?? "",
                kind: StatementClass(abi: s["class"] as? String),
                requires: SafetyRequirement(abi: s["requires"] as? String))
        }
        return SafetyDecision(
            profile: profile,
            level: SafetyLevel(abi: d["level"] as? String) ?? .silent,
            requires: SafetyRequirement(abi: d["requires"] as? String),
            challenge: d["challenge"] as? String,
            statements: statements)
    }

    /// The challenge id the engine names in a synchronous refusal message.
    public static func challengeID(inRefusal message: String) -> String? {
        guard let open = message.range(of: "(challenge "),
            let close = message.range(of: ")", range: open.upperBound..<message.endIndex)
        else { return nil }
        let id = message[open.upperBound..<close.lowerBound].trimmingCharacters(in: .whitespaces)
        return id.isEmpty ? nil : id
    }
}

/// What the user actually did. The engine judges it; this reports it.
public enum Attestation: Sendable, Hashable {
    case acknowledged
    case typedPhrase(String)
    case systemAuth(method: String)

    var json: String {
        let fields: [String: String]
        switch self {
        case .acknowledged: fields = ["kind": "acknowledged"]
        case .typedPhrase(let typed): fields = ["kind": "typed_phrase", "typed": typed]
        case .systemAuth(let method): fields = ["kind": "system_auth", "method": method]
        }
        guard let data = try? JSONSerialization.data(withJSONObject: fields, options: []),
            let text = String(data: data, encoding: .utf8)
        else { return #"{"kind":"acknowledged"}"# }
        return text
    }
}

extension DatagrepCoreHandle {
    public func evaluateSafety(profile: String, sql: String) throws -> SafetyDecision {
        let json = try datagrepTry { errOut in
            profile.withCString { p in
                sql.withCString { s in
                    takeOwnedString(datagrep_safety_evaluate_json(raw, p, s, errOut))
                }
            }
        }
        guard let decision = SafetyDecision.decode(jsonObject(json)) else {
            throw DatagrepError("datagrep_safety_evaluate_json did not return a decision")
        }
        return decision
    }

    public func pendingSafety(profile: String) throws -> [SafetyDecision] {
        let json = try profile.withCString { p in
            try datagrepTry { errOut in
                takeOwnedString(datagrep_safety_pending_json(raw, p, errOut))
            }
        }
        return (jsonObject(json) as? [Any] ?? []).compactMap(SafetyDecision.decode)
    }

    public func satisfySafety(profile: String, challenge: String, with attestation: Attestation)
        throws
    {
        try profile.withCString { p in
            try challenge.withCString { c in
                try attestation.json.withCString { a in
                    try datagrepTryBool { errOut in
                        datagrep_safety_satisfy(raw, p, c, a, errOut)
                    }
                }
            }
        }
    }
}
