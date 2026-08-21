import CDatagrepFFI
import Foundation

public struct EditableResult: Sendable, Equatable {
    public let identity: [String]
    public let guardFields: [String]
    public let root: String?
    /// False means a failing batch can leave the mutations before it applied.
    public let atomicBatch: Bool

    public init(identity: [String], guardFields: [String], root: String?, atomicBatch: Bool) {
        self.identity = identity
        self.guardFields = guardFields
        self.root = root
        self.atomicBatch = atomicBatch
    }

    /// Decodes the block, or `nil` for anything that is not a usable identity.
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
public enum MutationValue: Sendable, Equatable {
    case string(String)
    case int(Int64)
    case double(Double)
    case bool(Bool)
    case null

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

    public static func decode(_ any: Any?) -> MutationValue? {
        switch any {
        case let s as String: return .string(s)
        case is NSNull: return .null
        case let n as NSNumber:
            if CFGetTypeID(n) == CFBooleanGetTypeID() { return .bool(n.boolValue) }
            let objCType = String(cString: n.objCType)
            if objCType == "d" || objCType == "f" { return .double(n.doubleValue) }
            return .int(n.int64Value)
        default: return nil
        }
    }

    /// The typed text, coerced to the type the loaded value had.
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
public struct DocumentMutation: Sendable {
    /// The object path the write targets — the index the row was read from.
    public var path: [String]
    public var key: [(field: String, value: MutationValue)]
    public var expect: [(field: String, value: MutationValue)]
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

    static func pair(_ field: String, _ value: MutationValue) -> [Any] {
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

// MARK: - asking what the server holds now

/// One document to re-read, addressed exactly as its write was.
public struct DocumentAddress: Sendable {
    public var key: [(field: String, value: MutationValue)]

    public init(key: [(field: String, value: MutationValue)]) {
        self.key = key
    }

    fileprivate var abiJSON: [String: Any] {
        ["key": key.map { DocumentMutation.pair($0.field, $0.value) }]
    }
}

/// The address list `datagrep_reread_documents` parses.
public enum DocumentAddressBatch {
    public static func json(_ addresses: [DocumentAddress]) throws -> String {
        let payload: [String: Any] = ["documents": addresses.map(\.abiJSON)]
        let data = try JSONSerialization.data(withJSONObject: payload, options: [])
        guard let text = String(data: data, encoding: .utf8) else {
            throw DatagrepError("the address list could not be encoded as UTF-8")
        }
        return text
    }
}

/// One field of a document as the server holds it now.
public enum ServerValue: Sendable, Equatable {
    case value(MutationValue)
    case nested(String)
    /// The field is not on the document at all any more.
    case missing

    static func decode(_ any: Any?) -> ServerValue {
        if any == nil { return .missing }
        if let value = MutationValue.decode(any) { return .value(value) }
        if any is [Any] { return .nested("an array") }
        if any is [String: Any] { return .nested("an object") }
        return .nested("a value this view cannot show")
    }

    /// What this reads as in the "on the server now" column.
    public var display: String {
        switch self {
        case .value(let v): return v.display
        case .nested(let what): return what
        case .missing: return "—"
        }
    }

    public var mutationValue: MutationValue? {
        if case .value(let v) = self { return v }
        return nil
    }
}

public struct ServerDocument: Sendable {
    public let found: Bool
    public let error: String?
    public let envelope: [String: ServerValue]
    /// The document itself, at its root.
    public let fields: [String: ServerValue]

    static func decode(_ d: [String: Any]) -> ServerDocument {
        func map(_ any: Any?) -> [String: ServerValue] {
            guard let object = any as? [String: Any] else { return [:] }
            return object.mapValues(ServerValue.decode)
        }
        return ServerDocument(
            found: d["found"] as? Bool ?? false,
            error: d["error"] as? String,
            envelope: map(d["envelope"]),
            fields: map(d["fields"]))
    }

    static func decodeAll(_ text: String) throws -> [ServerDocument] {
        guard let d = jsonObject(text) as? [String: Any],
            let documents = d["documents"] as? [[String: Any]]
        else {
            throw DatagrepError("the re-read was not a document list: \(text)")
        }
        return documents.map(ServerDocument.decode)
    }
}

// MARK: - the report

public struct MutationRow: Sendable, Identifiable {
    public enum Outcome: String, Sendable {
        case applied
        case failed
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
    public func mutate(profile: String, mutations: [DocumentMutation]) throws -> MutationReport {
        let batch = try MutationBatch.json(mutations)
        let json = try profile.withCString { p in
            try batch.withCString { b in
                try datagrepTry { errOut in takeOwnedString(datagrep_mutate(raw, p, b, errOut)) }
            }
        }
        return try MutationReport.decode(json)
    }

    /// Read what the server holds now for each address, in the order given.
    public func reread(profile: String, addresses: [DocumentAddress]) throws -> [ServerDocument] {
        let body = try DocumentAddressBatch.json(addresses)
        let json = try profile.withCString { p in
            try body.withCString { b in
                try datagrepTry { errOut in
                    takeOwnedString(datagrep_reread_documents(raw, p, b, errOut))
                }
            }
        }
        return try ServerDocument.decodeAll(json)
    }
}
