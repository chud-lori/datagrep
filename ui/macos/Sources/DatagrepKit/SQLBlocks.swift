import Foundation

/// The four block directives — the entire meta-language datagrep adds to SQL.
public struct BlockDirectives: Sendable, Equatable {
    public var limit: Int?
    public var timeout: String?
    public var connection: String?
    public var readOnly: Bool = false

    public init() {}

    public var summary: String {
        var parts: [String] = []
        if let limit { parts.append("@limit \(limit)") }
        if let timeout { parts.append("@timeout \(timeout)") }
        if let connection { parts.append("@connection \(connection)") }
        if readOnly { parts.append("@readonly") }
        return parts.joined(separator: "  ")
    }
}

public struct SQLBlock: Sendable {
    public let text: String
    public let range: Range<Int>  // UTF-16 offsets into the source, for NSTextView
    public let directives: BlockDirectives

    public init(text: String, range: Range<Int>, directives: BlockDirectives) {
        self.text = text
        self.range = range
        self.directives = directives
    }
}

public enum SQLBlocks {
    /// Splits on top-level `;`, honouring '…', "…", $tag$…$tag$, `--` and `/*…*/`.
    public static func split(_ source: String) -> [SQLBlock] {
        let chars = Array(source.utf16)
        var blocks: [SQLBlock] = []
        var start = 0
        var i = 0

        func isAt(_ idx: Int, _ a: UInt16, _ b: UInt16) -> Bool {
            idx + 1 < chars.count && chars[idx] == a && chars[idx + 1] == b
        }
        let quote: UInt16 = 39, dquote: UInt16 = 34, dash: UInt16 = 45
        let slash: UInt16 = 47, star: UInt16 = 42, semi: UInt16 = 59
        let newline: UInt16 = 10, dollar: UInt16 = 36

        while i < chars.count {
            let c = chars[i]
            if c == quote || c == dquote {
                let closer = c
                i += 1
                while i < chars.count {
                    if chars[i] == closer {
                        if i + 1 < chars.count && chars[i + 1] == closer { i += 2; continue }
                        i += 1
                        break
                    }
                    i += 1
                }
                continue
            }
            if isAt(i, dash, dash) {
                while i < chars.count && chars[i] != newline { i += 1 }
                continue
            }
            if isAt(i, slash, star) {
                i += 2
                while i < chars.count && !isAt(i, star, slash) { i += 1 }
                i = min(i + 2, chars.count)
                continue
            }
            if c == dollar {
                // $tag$ … $tag$
                var j = i + 1
                while j < chars.count && chars[j] != dollar && chars[j] != newline { j += 1 }
                if j < chars.count && chars[j] == dollar {
                    let tag = Array(chars[i...j])
                    var k = j + 1
                    while k + tag.count <= chars.count {
                        if Array(chars[k..<(k + tag.count)]) == tag { k += tag.count; break }
                        k += 1
                    }
                    i = min(k, chars.count)
                    continue
                }
            }
            if c == semi {
                blocks.append(makeBlock(chars, start, i + 1))
                i += 1
                start = i
                continue
            }
            i += 1
        }
        if start < chars.count { blocks.append(makeBlock(chars, start, chars.count)) }
        return blocks.filter { !$0.text.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty }
    }

    private static func makeBlock(_ chars: [UInt16], _ lo: Int, _ hi: Int) -> SQLBlock {
        let text = String(decoding: chars[lo..<hi], as: UTF16.self)
        return SQLBlock(text: text, range: lo..<hi, directives: directives(in: text))
    }

    public static func block(at caret: Int, in source: String) -> SQLBlock? {
        let blocks = split(source)
        if blocks.isEmpty { return nil }
        for b in blocks where b.range.contains(caret) { return b }
        for b in blocks.reversed() where b.range.lowerBound <= caret { return b }
        return blocks.first
    }

    public static func directives(in text: String) -> BlockDirectives {
        var d = BlockDirectives()
        for rawLine in text.split(separator: "\n", omittingEmptySubsequences: false) {
            let line = rawLine.trimmingCharacters(in: .whitespaces)
            guard line.hasPrefix("--") else { continue }
            let body = line.dropFirst(2).trimmingCharacters(in: .whitespaces)
            guard body.hasPrefix("@") else { continue }
            let parts = body.dropFirst().split(separator: " ", maxSplits: 1).map {
                $0.trimmingCharacters(in: .whitespaces)
            }
            guard let key = parts.first?.lowercased() else { continue }
            let value = parts.count > 1 ? parts[1] : ""
            switch key {
            case "limit": d.limit = Int(value)
            case "timeout": d.timeout = value.isEmpty ? nil : value
            case "connection": d.connection = value.isEmpty ? nil : value
            case "readonly": d.readOnly = true
            default: break
            }
        }
        return d
    }

    /// A fat-finger guardrail, not an adversary defence.
    public static func isWriteStatement(_ sql: String) -> Bool {
        var s = sql
        while true {
            s = s.trimmingCharacters(in: .whitespacesAndNewlines)
            if s.hasPrefix("--") {
                if let nl = s.firstIndex(of: "\n") { s = String(s[s.index(after: nl)...]) } else { s = "" }
                continue
            }
            break
        }
        let head = s.prefix(while: { $0.isLetter }).lowercased()
        return [
            "insert", "update", "delete", "drop", "truncate", "alter", "create", "grant",
            "revoke", "replace", "merge", "vacuum", "call", "copy",
        ].contains(head)
    }
}
