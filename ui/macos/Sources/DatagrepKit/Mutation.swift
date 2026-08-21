import CDatagrepFFI
import Foundation

/// What a result says about editing it — the `editable` block of
/// `datagrep_query_status_json`.
///
/// Absent (`nil`) means no edit may be offered at all, and the UI must take
/// that literally: it is also what a connection that has not answered yet
/// reports, because "we have not asked" and "yes" are different facts.
public struct EditableResult: Sendable, Equatable {
    /// Fields that name exactly one row (`_index`, `_id`, `_routing`). These
    /// become the mutation's `key`; the engine never guesses which row a write
    /// is for, so an edit is impossible without every one of them.
    public let identity: [String]
    /// Fields a write compares against before applying (`_seq_no`,
    /// `_primary_term`). These become the mutation's `expect`, carrying the
    /// values that were *loaded* — that is the whole compare-and-swap.
    public let guardFields: [String]
    /// The field the grid's columns are projected from (`_source`). Everything
    /// outside it is the row envelope, which is where identity and guard live.
    public let root: String?
    /// False means a failing batch can leave the mutations before it applied.
    /// The confirmation has to say so *before* the click, which is the only
    /// moment it is still the user's decision.
    public let atomicBatch: Bool

    public init(identity: [String], guardFields: [String], root: String?, atomicBatch: Bool) {
        self.identity = identity
        self.guardFields = guardFields
        self.root = root
        self.atomicBatch = atomicBatch
    }

    /// Decodes the block, or `nil` for anything that is not a usable identity.
    /// A malformed or half-present block reads as "not editable" rather than
    /// as a partial yes.
    public static func decode(_ any: Any?) -> EditableResult? {
        guard let d = any as? [String: Any] else { return nil }
        let identity = (d["identity"] as? [Any] ?? []).compactMap { $0 as? String }
        guard !identity.isEmpty else { return nil }
        return EditableResult(
            identity: identity,
            guardFields: (d["guard"] as? [Any] ?? []).compactMap { $0 as? String },
            root: (d["root"] as? String).flatMap { $0.isEmpty ? nil : $0 },
            atomicBatch: d["atomic_batch"] as? Bool ?? false)
    }
}

extension EditableResult {
    /// The address one row's write needs, read out of that row's envelope.
    ///
    /// Two different failures, both refused here rather than sent:
    ///
    /// - no identity at all — nothing names the document, and guessing which
    ///   one to write is the mistake this whole path exists to avoid;
    /// - a missing guard field — the write could then only go unguarded, which
    ///   the engine refuses anyway, so the sentence is worth more here where it
    ///   can still say which field was missing and why it matters.
    ///
    /// An identity field that is simply not on this row (an unrouted document
    /// carries no `_routing`) is left out of the key rather than sent as null:
    /// absent and null are different facts, and the engine reads them so.
    public func address(envelope: [String: Any]) -> Result<
        (key: [(field: String, value: MutationValue)],
        expect: [(field: String, value: MutationValue)]), DatagrepError
    > {
        var key: [(field: String, value: MutationValue)] = []
        for field in identity {
            guard let value = MutationValue.decode(envelope[field]), value != .null else {
                continue
            }
            key.append((field, value))
        }
        guard !key.isEmpty else {
            return .failure(
                DatagrepError(
                    "this row carries none of the fields that identify a document (\(identity.joined(separator: ", "))), so there is nothing to address a write to"
                ))
        }
        var expect: [(field: String, value: MutationValue)] = []
        for field in guardFields {
            guard let value = MutationValue.decode(envelope[field]), value != .null else {
                return .failure(
                    DatagrepError(
                        "this document was loaded without `\(field)`, so an edit to it could only be sent unguarded — and an unguarded write would overwrite whatever the server holds now"
                    ))
            }
            expect.append((field, value))
        }
        return .success((key, expect))
    }
}

// MARK: - values

/// One value crossing the mutation ABI, in the engine's own `Value` spelling.
///
/// Deliberately a small set: these are the types a grid cell can be typed
/// into. Anything richer — an object, an array — is edited as a document
/// rather than as a cell, and is not offered here at all.
public enum MutationValue: Sendable, Equatable {
    case string(String)
    case int(Int64)
    case double(Double)
    case bool(Bool)
    case null

    /// serde's externally-tagged form, as `JSONSerialization` fragments:
    /// `{"Str":"x"}`, `{"I64":42}`, and a bare `"Null"` for the unit variant.
    var abiJSON: Any {
        switch self {
        case .string(let s): return ["Str": s]
        case .int(let i): return ["I64": NSNumber(value: i)]
        case .double(let d): return ["F64": NSNumber(value: d)]
        case .bool(let b): return ["Bool": NSNumber(value: b)]
        case .null: return "Null"
        }
    }

    /// What this value looks like in a grid cell / an error message.
    public var display: String {
        switch self {
        case .string(let s): return s
        case .int(let i): return String(i)
        case .double(let d): return String(d)
        case .bool(let b): return b ? "true" : "false"
        case .null: return "NULL"
        }
    }

    /// Reads one value out of parsed JSON — an envelope field, or a cell's
    /// detail payload. Objects and arrays return `nil`: they are values this
    /// type deliberately cannot carry, not values it should flatten.
    public static func decode(_ any: Any?) -> MutationValue? {
        switch any {
        case let s as String: return .string(s)
        case is NSNull: return .null
        case let n as NSNumber:
            // JSONSerialization hands back NSNumber for true/false as well, and
            // sending a bool on as an integer would rewrite the field's type on
            // a server that types its fields.
            if CFGetTypeID(n) == CFBooleanGetTypeID() { return .bool(n.boolValue) }
            let objCType = String(cString: n.objCType)
            if objCType == "d" || objCType == "f" { return .double(n.doubleValue) }
            return .int(n.int64Value)
        default: return nil
        }
    }

    /// The typed text, coerced to the type the loaded value had.
    ///
    /// A field that came back as a number goes back as a number. Retyping it
    /// as a string would be accepted by this side and then either rejected by
    /// the server or — worse — silently stored under a different type, so the
    /// coercion is refused here, where the sentence can still name the field
    /// and the value the user typed.
    ///
    /// A field loaded as NULL has no type to preserve, so its text is read the
    /// way JSON would read it: `42` is a number, `true` is a bool, everything
    /// else is a string.
    public static func typed(_ text: String, like loaded: MutationValue?) -> Result<
        MutationValue, DatagrepError
    > {
        switch loaded {
        case .string:
            return .success(.string(text))
        case .int:
            guard let i = Int64(text.trimmingCharacters(in: .whitespaces)) else {
                return .failure(DatagrepError("this field holds a whole number; “\(text)” is not one"))
            }
            return .success(.int(i))
        case .double:
            guard let d = Double(text.trimmingCharacters(in: .whitespaces)) else {
                return .failure(DatagrepError("this field holds a number; “\(text)” is not one"))
            }
            return .success(.double(d))
        case .bool:
            switch text.trimmingCharacters(in: .whitespaces).lowercased() {
            case "true", "yes", "1": return .success(.bool(true))
            case "false", "no", "0": return .success(.bool(false))
            default:
                return .failure(
                    DatagrepError("this field holds true or false; “\(text)” is neither"))
            }
        case .null, .none:
            let trimmed = text.trimmingCharacters(in: .whitespaces)
            if let i = Int64(trimmed) { return .success(.int(i)) }
            if let d = Double(trimmed) { return .success(.double(d)) }
            if trimmed == "true" { return .success(.bool(true)) }
            if trimmed == "false" { return .success(.bool(false)) }
            return .success(.string(text))
        }
    }
}

// MARK: - the batch

/// One document's write, addressed the way the engine addresses it.
///
/// `key` and `expect` are separate on purpose and mean different things: the
/// key says *which* document, the expectation says *which version of it*. A
/// driver that cannot honour an expectation must refuse the write rather than
/// drop it, so nothing here ever leaves `expect` off to "make it work".
public struct DocumentMutation: Sendable {
    /// The object path the write targets — the index the row was read from.
    public var path: [String]
    public var key: [(field: String, value: MutationValue)]
    public var expect: [(field: String, value: MutationValue)]
    /// Field name → new value, relative to the document root. Empty for a
    /// delete.
    public var sets: [(field: String, value: MutationValue)]
    public var isDelete: Bool

    public init(
        path: [String], key: [(field: String, value: MutationValue)],
        expect: [(field: String, value: MutationValue)],
        sets: [(field: String, value: MutationValue)], isDelete: Bool
    ) {
        self.path = path
        self.key = key
        self.expect = expect
        self.sets = sets
        self.isDelete = isDelete
    }

    /// serde's externally-tagged `Mutation`.
    fileprivate var abiJSON: [String: Any] {
        var body: [String: Any] = [
            "path": path,
            "key": key.map { Self.pair($0.field, $0.value) },
            "expect": expect.map { Self.pair($0.field, $0.value) },
        ]
        if isDelete { return ["Delete": body] }
        body["sets"] = sets.map { Self.pair($0.field, $0.value) }
        return ["Update": body]
    }

    /// `(FieldPath, Value)` — a one-segment path paired with its value, which
    /// on the wire is `[[{"Field":"status"}], {"Str":"done"}]`.
    private static func pair(_ field: String, _ value: MutationValue) -> [Any] {
        [[["Field": field]], value.abiJSON]
    }
}

/// The `MutationBatch` blob `datagrep_mutate` parses.
public enum MutationBatch {
    public static func json(_ mutations: [DocumentMutation]) throws -> String {
        let payload: [String: Any] = ["mutations": mutations.map(\.abiJSON)]
        let data = try JSONSerialization.data(withJSONObject: payload, options: [])
        guard let text = String(data: data, encoding: .utf8) else {
            throw DatagrepError("the mutation batch could not be encoded as UTF-8")
        }
        return text
    }
}

// MARK: - the report

/// What happened to one document. A conflict is a row here, not an error:
/// the batch still returns a report, and the conflict is a state the UI shows.
public struct MutationRow: Sendable, Identifiable {
    public enum Outcome: String, Sendable {
        case applied
        case failed
        /// The batch halted before reaching this one. Nothing was written and
        /// nothing was rolled back — it is simply still pending.
        case notAttempted = "not attempted"
    }

    public let op: String
    public let index: String
    public let documentID: String
    public let routing: String?
    public let outcome: Outcome
    public let seqNo: Int64?
    public let primaryTerm: Int64?
    public let conflict: Bool
    public let errorCode: String?
    public let error: String?
    /// The server escalated a `wait_for` refresh to an immediate one — a load
    /// the cluster paid for this save. The driver reports it as a notice too.
    public let forcedRefresh: Bool

    public var id: String { "\(index)\u{1}\(documentID)\u{1}\(op)" }

    static func decode(_ d: [String: Any]) -> MutationRow {
        MutationRow(
            op: d["op"] as? String ?? "?",
            index: d["_index"] as? String ?? "",
            documentID: d["_id"] as? String ?? "",
            routing: d["_routing"] as? String,
            outcome: Outcome(rawValue: d["outcome"] as? String ?? "") ?? .failed,
            seqNo: (d["_seq_no"] as? NSNumber)?.int64Value,
            primaryTerm: (d["_primary_term"] as? NSNumber)?.int64Value,
            conflict: d["conflict"] as? Bool ?? false,
            errorCode: d["error_code"] as? String,
            error: d["error"] as? String,
            forcedRefresh: d["forced_refresh"] as? Bool ?? false)
    }
}

/// A non-fatal message the engine sent along with the batch. Shown, never
/// swallowed: `forced_refresh` and a partial-batch summary both arrive here.
public struct MutationNotice: Sendable, Identifiable {
    public let severity: String
    public let code: String?
    public let message: String
    public var id: String { "\(code ?? "")\u{1}\(message)" }
    public var isWarning: Bool { severity == "warning" }
}

/// The whole report `datagrep_mutate` returns.
public struct MutationReport: Sendable {
    public let rows: [MutationRow]
    public let notices: [MutationNotice]
    public let applied: Int
    public let failed: Int
    public let notAttempted: Int
    public let conflicts: Int

    public var isClean: Bool { failed == 0 && notAttempted == 0 }

    static func decode(_ text: String) throws -> MutationReport {
        guard let d = jsonObject(text) as? [String: Any] else {
            throw DatagrepError("the mutation report was not an object: \(text)")
        }
        let summary = d["summary"] as? [String: Any] ?? [:]
        func count(_ key: String) -> Int { (summary[key] as? NSNumber)?.intValue ?? 0 }
        return MutationReport(
            rows: (d["rows"] as? [[String: Any]] ?? []).map(MutationRow.decode),
            notices: (d["notices"] as? [[String: Any]] ?? []).compactMap { n in
                guard let message = n["message"] as? String else { return nil }
                return MutationNotice(
                    severity: n["severity"] as? String ?? "info",
                    code: n["code"] as? String, message: message)
            },
            applied: count("applied"),
            failed: count("failed"),
            notAttempted: count("not_attempted"),
            conflicts: count("conflicts"))
    }
}

extension DatagrepCoreHandle {
    /// Commit one batch of document edits and wait for the report.
    ///
    /// **This blocks.** Unlike running a query, a save is a discrete commit
    /// with an answer, so the ABI is synchronous and this must be called off
    /// the main queue — the window stays live and says "committing…" instead
    /// of beachballing on a cluster that is thinking.
    ///
    /// A per-row version conflict comes back *in the report*, not as a throw:
    /// nothing about it is exceptional, and the UI has a state for it. A throw
    /// means the batch as a whole never ran — a read-only refusal, an
    /// unparseable batch, a driver that refused every write up front.
    public func mutate(profile: String, mutations: [DocumentMutation]) throws -> MutationReport {
        let batch = try MutationBatch.json(mutations)
        let json = try profile.withCString { p in
            try batch.withCString { b in
                try datagrepTry { errOut in takeOwnedString(datagrep_mutate(raw, p, b, errOut)) }
            }
        }
        return try MutationReport.decode(json)
    }
}
