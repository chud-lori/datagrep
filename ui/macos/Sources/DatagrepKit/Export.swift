import CDatagrepFFI
import Foundation

public enum ExportFormat: String, CaseIterable, Sendable {
    case csv, json, markdown, sql

    public var title: String {
        switch self {
        case .csv: return "CSV"
        case .json: return "JSON"
        case .markdown: return "Markdown table"
        case .sql: return "SQL INSERT statements"
        }
    }

    public var fileExtension: String {
        switch self {
        case .csv: return "csv"
        case .json: return "json"
        case .markdown: return "md"
        case .sql: return "sql"
        }
    }
}

public enum ExportState: String, Sendable {
    case running, done, cancelled, failed
}

public struct ExportStatus: Sendable {
    public let state: ExportState
    public let rowsWritten: UInt64
    public let error: String?
    /// Non-null only when the ladder refused the statement, which means nothing was sent.
    public let safety: SafetyDecision?
}

public final class DatagrepExportHandle: @unchecked Sendable {
    let raw: OpaquePointer

    init(raw: OpaquePointer) { self.raw = raw }

    deinit { datagrep_export_free(raw) }

    public func status() throws -> ExportStatus {
        let json = try datagrepTry { errOut in takeOwnedString(datagrep_export_status_json(raw, errOut)) }
        guard let d = jsonObject(json) as? [String: Any] else {
            throw DatagrepError("unparseable export status JSON: \(json)")
        }
        return ExportStatus(
            state: ExportState(rawValue: d["state"] as? String ?? "failed") ?? .failed,
            rowsWritten: (d["rows_written"] as? NSNumber)?.uint64Value ?? 0,
            error: d["error"] as? String,
            safety: SafetyDecision.decode(d["safety"]))
    }

    public func cancel() { datagrep_export_cancel(raw) }
}

extension DatagrepCoreHandle {
    public static func exportFormats(driver: String) -> [ExportFormat] {
        let json = driver.withCString { takeOwnedString(datagrep_export_formats_json($0)) } ?? "[]"
        return (jsonObject(json) as? [String] ?? []).compactMap(ExportFormat.init(rawValue:))
    }

    public func export(
        profile: String, sql: String, format: ExportFormat, table: String?, to path: String
    ) throws -> DatagrepExportHandle {
        let ptr = try datagrepTry { errOut in
            profile.withCString { p in
                sql.withCString { s in
                    format.rawValue.withCString { f in
                        path.withCString { out in
                            guard let table else {
                                return datagrep_export_start(raw, p, s, f, nil, out, errOut)
                            }
                            return table.withCString { t in
                                datagrep_export_start(raw, p, s, f, t, out, errOut)
                            }
                        }
                    }
                }
            }
        }
        return DatagrepExportHandle(raw: ptr)
    }
}
