import CDatagrepFFI
import Foundation

public struct FilterOperator: Hashable, Sendable {
    public let op: String
    public let label: String
    public let needsValue: Bool

    /// What this engine's results can be filtered by; empty where the core cannot wrap the statement.
    public static func available(driver: String) -> [FilterOperator] {
        let json = driver.withCString { takeOwnedString(datagrep_filter_operators_json($0)) } ?? "[]"
        return (jsonObject(json) as? [[String: Any]] ?? []).compactMap { d in
            guard let op = d["op"] as? String, let label = d["label"] as? String else { return nil }
            return FilterOperator(op: op, label: label, needsValue: d["needs_value"] as? Bool ?? true)
        }
    }
}

public struct RowFilter: Equatable, Sendable {
    public var column: String
    public var op: String
    public var value: String

    public init(column: String, op: String, value: String = "") {
        self.column = column
        self.op = op
        self.value = value
    }
}

/// The statement re-asked with a WHERE and ORDER BY; the core does all quoting.
public enum DerivedStatement {
    public static func render(
        driver: String, statement: String, filters: [RowFilter],
        sort: (column: String, ascending: Bool)?
    ) throws -> String {
        let spec: [String: Any] = [
            "filters": filters.map { ["column": $0.column, "op": $0.op, "value": $0.value] },
            "sort": sort.map { ["column": $0.column, "ascending": $0.ascending] as [String: Any] }
                ?? NSNull(),
        ]
        let data = try JSONSerialization.data(withJSONObject: spec, options: [])
        let specJSON = String(decoding: data, as: UTF8.self)
        return try driver.withCString { d in
            try statement.withCString { s in
                try specJSON.withCString { j in
                    try datagrepTry { errOut in
                        takeOwnedString(datagrep_derive_statement(d, s, j, errOut))
                    }
                }
            }
        }
    }
}
