import CDatagrepFFI
import Foundation

public struct CompletionItem: Equatable, Sendable {
    public let label: String
    public let insert: String
    public let kind: String
    public let detail: String?
}

/// Accepting an item replaces `prefix`, the text just before the caret, with its `insert`.
public struct CompletionAnswer: Equatable, Sendable {
    public let prefix: String
    public let items: [CompletionItem]
    public let error: String?
}

extension DatagrepCoreHandle {
    /// Blocks on the first call per connection while its tables are listed: call off the main thread.
    public func complete(profile: String, text: String, caretUTF8: Int) throws -> CompletionAnswer {
        let json = try datagrepTry { errOut in
            profile.withCString { p in
                text.withCString { t in
                    takeOwnedString(datagrep_complete_json(raw, p, t, caretUTF8, errOut))
                }
            }
        }
        guard let d = jsonObject(json) as? [String: Any] else {
            throw DatagrepError("the completion answer was not an object")
        }
        let items = (d["items"] as? [[String: Any]] ?? []).compactMap { i -> CompletionItem? in
            guard let label = i["label"] as? String, let insert = i["insert"] as? String else {
                return nil
            }
            return CompletionItem(
                label: label, insert: insert, kind: i["kind"] as? String ?? "",
                detail: i["detail"] as? String)
        }
        return CompletionAnswer(
            prefix: d["prefix"] as? String ?? "", items: items, error: d["error"] as? String)
    }

    public func forgetCompletions(profile: String) {
        profile.withCString { datagrep_complete_forget(raw, $0) }
    }
}

public enum SQLFormatter {
    /// Throws for an engine whose language is not SQL.
    public static func format(driver: String, sql: String) throws -> String {
        try driver.withCString { d in
            try sql.withCString { s in
                try datagrepTry { errOut in takeOwnedString(datagrep_sql_format(d, s, errOut)) }
            }
        }
    }
}
