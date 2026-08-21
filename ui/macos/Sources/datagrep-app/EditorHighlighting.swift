import AppKit

// MARK: - tokens

enum SQLTokenKind {
    case keyword, ident, string, comment, number, op, punct, directive
}

struct SQLToken {
    var range: NSRange
    var kind: SQLTokenKind
}

struct SQLLexState: Equatable {
    var commentDepth: Int = 0
    var inSingleQuote: Bool = false
    var dollarTag: String? = nil

    var isDefault: Bool { commentDepth == 0 && !inSingleQuote && dollarTag == nil }
}

// MARK: - keyword table

enum SQLKeywords {
    static let set: Set<String> = [
        "SELECT", "INSERT", "UPDATE", "DELETE", "MERGE", "UPSERT", "REPLACE", "COPY",
        "CREATE", "ALTER", "DROP", "TRUNCATE", "COMMENT", "RENAME", "BEGIN", "COMMIT",
        "ROLLBACK", "SAVEPOINT", "START", "TRANSACTION", "GRANT", "REVOKE", "VACUUM",
        "ANALYZE", "SET", "KILL", "SHOW", "EXPLAIN", "VALUES", "WITH", "RECURSIVE",
        "MATERIALIZED", "FROM", "WHERE", "JOIN", "INNER", "OUTER", "LEFT", "RIGHT",
        "FULL", "CROSS", "ON", "GROUP", "BY", "ORDER", "HAVING", "LIMIT", "OFFSET",
        "AND", "OR", "NOT", "NULL", "IS", "IN", "EXISTS", "BETWEEN", "LIKE", "ILIKE",
        "AS", "DISTINCT", "UNION", "INTERSECT", "EXCEPT", "ALL", "ANY", "SOME", "INTO",
        "DEFAULT", "PRIMARY", "KEY", "FOREIGN", "REFERENCES", "UNIQUE", "CHECK",
        "INDEX", "VIEW", "FUNCTION", "PROCEDURE", "TRIGGER", "CASE", "WHEN", "THEN",
        "ELSE", "END", "CAST", "RETURNING", "USING", "CONFLICT", "DO", "NOTHING",
        "TABLE", "COLUMN", "CONSTRAINT", "CASCADE", "RESTRICT", "IF", "TEMP",
        "TEMPORARY", "SCHEMA", "DATABASE", "SEQUENCE", "TRUE", "FALSE", "ASC", "DESC",
        "NULLS", "FIRST", "LAST", "OVER", "PARTITION", "WINDOW", "FILTER", "LATERAL",
        "FOR", "OF", "NOWAIT", "SKIP", "LOCKED", "RETURN", "DECLARE", "LOOP", "WHILE",
        "DELIMITER", "GO",
    ]

    /// The whole meta-language, per `SQLBlocks.directives(in:)`.
    static let directives: Set<String> = ["limit", "timeout", "connection", "readonly"]
}

// MARK: - the line lexer

/// One line in, one line's tokens plus the state the next line inherits out.
enum SQLLineLexer {
    private static let cTab: unichar = 9
    private static let cCR: unichar = 13
    private static let cSpace: unichar = 32
    private static let cDQuote: unichar = 34
    private static let cDollar: unichar = 36
    private static let cQuote: unichar = 39
    private static let cStar: unichar = 42
    private static let cPlus: unichar = 43
    private static let cDash: unichar = 45
    private static let cDot: unichar = 46
    private static let cSlash: unichar = 47
    private static let cSemi: unichar = 59
    private static let cAt: unichar = 64
    private static let cUnderscore: unichar = 95
    private static let cBacktick: unichar = 96

    @inline(__always) static func isDigit(_ c: unichar) -> Bool { c >= 48 && c <= 57 }

    @inline(__always) static func isAlpha(_ c: unichar) -> Bool {
        (c >= 65 && c <= 90) || (c >= 97 && c <= 122)
    }

    @inline(__always) static func isWordStart(_ c: unichar) -> Bool {
        isAlpha(c) || c == cUnderscore || c >= 0x80
    }

    @inline(__always) static func isWordContinue(_ c: unichar) -> Bool {
        isWordStart(c) || isDigit(c) || c == cDollar
    }

    @inline(__always) static func isHexDigit(_ c: unichar) -> Bool {
        isDigit(c) || (c >= 65 && c <= 70) || (c >= 97 && c <= 102)
    }

    @inline(__always) static func isOperator(_ c: unichar) -> Bool {
        switch c {
        case 61, 60, 62, 33, 43, 45, 42, 47, 37, 124, 38, 94, 126: return true
        default: return false
        }
    }

    @inline(__always) static func isBracket(_ c: unichar) -> Bool {
        c == 40 || c == 41 || c == 91 || c == 93 || c == 123 || c == 125
    }

    @discardableResult
    static func lex(
        _ c: UnsafeBufferPointer<unichar>,
        base: Int,
        entry: SQLLexState,
        nestComments: Bool,
        collectTokens: Bool,
        tokens: inout [SQLToken],
        semicolons: inout [Int]
    ) -> SQLLexState {
        var state = entry
        let n = c.count
        var i = 0

        @inline(__always) func emit(_ lo: Int, _ hi: Int, _ k: SQLTokenKind) {
            guard collectTokens, hi > lo else { return }
            tokens.append(SQLToken(range: NSRange(location: base + lo, length: hi - lo), kind: k))
        }

        /// Consumes a block comment body from `i`, honouring the entry depth.
        @inline(__always) func consumeBlockComment() {
            while i < n {
                if i + 1 < n, c[i] == cStar, c[i + 1] == cSlash {
                    state.commentDepth -= 1
                    i += 2
                    if state.commentDepth == 0 { return }
                    continue
                }
                if nestComments, i + 1 < n, c[i] == cSlash, c[i + 1] == cStar {
                    state.commentDepth += 1
                    i += 2
                    continue
                }
                i += 1
            }
        }

        @inline(__always) func consumeSingleQuote() {
            while i < n {
                if c[i] == cQuote {
                    if i + 1 < n, c[i + 1] == cQuote {
                        i += 2
                        continue
                    }
                    i += 1
                    state.inSingleQuote = false
                    return
                }
                i += 1
            }
        }

        // ---- continuation of a construct opened on an earlier line ---------
        if state.commentDepth > 0 {
            consumeBlockComment()
            emit(0, i, .comment)
        } else if state.inSingleQuote {
            consumeSingleQuote()
            emit(0, i, .string)
        } else if let tag = state.dollarTag {
            let t = Array(tag.utf16)
            var closed = false
            while i + t.count <= n {
                var match = true
                for k in 0..<t.count where c[i + k] != t[k] {
                    match = false
                    break
                }
                if match {
                    i += t.count
                    closed = true
                    break
                }
                i += 1
            }
            if !closed { i = n } else { state.dollarTag = nil }
            emit(0, i, .string)
        }

        var sawNonSpace = i > 0

        while i < n {
            let ch = c[i]

            if ch == cSpace || ch == cTab || ch == cCR {
                i += 1
                continue
            }

            if ch == cDash, i + 1 < n, c[i + 1] == cDash {
                let start = i
                let kind: SQLTokenKind =
                    (!sawNonSpace && directiveFollows(c, from: i + 2)) ? .directive : .comment
                i = n
                emit(start, i, kind)
                break
            }

            if ch == cSlash, i + 1 < n, c[i + 1] == cStar {
                let start = i
                state.commentDepth = 1
                i += 2
                consumeBlockComment()
                emit(start, i, .comment)
                sawNonSpace = true
                continue
            }

            if ch == cQuote {
                let start = i
                i += 1
                state.inSingleQuote = true
                consumeSingleQuote()
                emit(start, i, .string)
                sawNonSpace = true
                continue
            }

            if ch == cDQuote || ch == cBacktick {
                let closer = ch
                let start = i
                i += 1
                while i < n {
                    if c[i] == closer {
                        if i + 1 < n, c[i + 1] == closer {
                            i += 2
                            continue
                        }
                        i += 1
                        break
                    }
                    i += 1
                }
                emit(start, i, .ident)
                sawNonSpace = true
                continue
            }

            if ch == cDollar {
                var j = i + 1
                while j < n, isWordContinue(c[j]), c[j] != cDollar { j += 1 }
                if j < n, c[j] == cDollar {
                    var t = [unichar]()
                    t.reserveCapacity(j - i + 1)
                    for k in i...j { t.append(c[k]) }
                    let start = i
                    i = j + 1
                    var closed = false
                    while i + t.count <= n {
                        var match = true
                        for k in 0..<t.count where c[i + k] != t[k] {
                            match = false
                            break
                        }
                        if match {
                            i += t.count
                            closed = true
                            break
                        }
                        i += 1
                    }
                    if !closed {
                        i = n
                        state.dollarTag = String(decoding: t, as: UTF16.self)
                    }
                    emit(start, i, .string)
                    sawNonSpace = true
                    continue
                }
            }

            if isWordStart(ch) {
                let start = i
                i += 1
                while i < n, isWordContinue(c[i]) { i += 1 }
                if collectTokens {
                    var w = [unichar]()
                    w.reserveCapacity(i - start)
                    for k in start..<i { w.append(c[k]) }
                    let word = String(decoding: w, as: UTF16.self).uppercased()
                    emit(start, i, SQLKeywords.set.contains(word) ? .keyword : .ident)
                }
                sawNonSpace = true
                continue
            }

            if isDigit(ch) || (ch == cDot && i + 1 < n && isDigit(c[i + 1])) {
                let start = i
                i += 1
                while i < n {
                    let d = c[i]
                    if isDigit(d) || d == cDot || isHexDigit(d) || d == 120 || d == 88 {
                        i += 1
                    } else if d == 101 || d == 69 {
                        // exponent, only when an exponent actually follows
                        if i + 1 < n, c[i + 1] == cPlus || c[i + 1] == cDash || isDigit(c[i + 1]) {
                            i += 2
                        } else {
                            break
                        }
                    } else {
                        break
                    }
                }
                emit(start, i, .number)
                sawNonSpace = true
                continue
            }

            if isOperator(ch) {
                let start = i
                i += 1
                while i < n, isOperator(c[i]) {
                    if c[i] == cDash, i + 1 < n, c[i + 1] == cDash { break }
                    if c[i] == cSlash, i + 1 < n, c[i + 1] == cStar { break }
                    i += 1
                }
                emit(start, i, .op)
                sawNonSpace = true
                continue
            }

            if ch == cSemi { semicolons.append(base + i) }
            emit(i, i + 1, .punct)
            i += 1
            sawNonSpace = true
        }

        return state
    }

    private static func directiveFollows(_ c: UnsafeBufferPointer<unichar>, from: Int) -> Bool {
        var i = from
        let n = c.count
        while i < n, c[i] == cSpace || c[i] == cTab { i += 1 }
        guard i < n, c[i] == cAt else { return false }
        i += 1
        var w = [unichar]()
        while i < n, isAlpha(c[i]) {
            w.append(c[i])
            i += 1
        }
        guard !w.isEmpty else { return false }
        return SQLKeywords.directives.contains(String(decoding: w, as: UTF16.self).lowercased())
    }
}

// MARK: - theme

enum SQLTheme {
    static func dynamic(light: NSColor, dark: NSColor) -> NSColor {
        NSColor(name: nil) { appearance in
            appearance.bestMatch(from: [.aqua, .darkAqua]) == .darkAqua ? dark : light
        }
    }

    static let keyword = dynamic(
        light: NSColor(srgbRed: 0.122, green: 0.373, blue: 0.749, alpha: 1),
        dark: NSColor(srgbRed: 0.510, green: 0.667, blue: 1.000, alpha: 1))

    static let string = dynamic(
        light: NSColor(srgbRed: 0.612, green: 0.231, blue: 0.137, alpha: 1),
        dark: NSColor(srgbRed: 0.898, green: 0.580, blue: 0.478, alpha: 1))

    static let comment = dynamic(
        light: NSColor(srgbRed: 0.431, green: 0.463, blue: 0.506, alpha: 1),
        dark: NSColor(srgbRed: 0.486, green: 0.533, blue: 0.580, alpha: 1))

    static let directive = dynamic(
        light: NSColor(srgbRed: 0.043, green: 0.447, blue: 0.522, alpha: 1),
        dark: NSColor(srgbRed: 0.302, green: 0.816, blue: 0.780, alpha: 1))

    /// The line the caret is on.
    static let currentLine = dynamic(
        light: NSColor(white: 0, alpha: 0.045),
        dark: NSColor(white: 1, alpha: 0.055))

    /// The statement ⌘↵ will actually run.
    static let currentBlock = dynamic(
        light: NSColor(white: 0, alpha: 0.024),
        dark: NSColor(white: 1, alpha: 0.030))

    static let bracketMatch = dynamic(
        light: NSColor(srgbRed: 0.122, green: 0.373, blue: 0.749, alpha: 0.22),
        dark: NSColor(srgbRed: 0.510, green: 0.667, blue: 1.000, alpha: 0.28))

    static func color(for kind: SQLTokenKind) -> NSColor? {
        switch kind {
        case .keyword: return keyword
        case .string: return string
        case .comment: return comment
        case .directive: return directive
        case .ident, .number, .op, .punct: return nil
        }
    }
}

// MARK: - the incremental highlighter

final class SQLHighlighter: NSObject, NSTextStorageDelegate {
    private weak var storage: NSTextStorage?

    var visibleRangeProvider: (() -> NSRange)?

    var font: NSFont = .monospacedSystemFont(ofSize: 12, weight: .regular) {
        didSet {
            boldFont = NSFontManager.shared.convert(font, toHaveTrait: .boldFontMask)
            invalidateAllAttributes()
        }
    }
    private var boldFont: NSFont = .monospacedSystemFont(ofSize: 12, weight: .semibold)

    var nestComments = true

    // Parallel arrays, one entry per line. Spliced together, always.
    private var lineStarts: [Int] = [0]
    private var lineEndState: [SQLLexState?] = [nil]
    private var lineClean: [Bool] = [false]
    private var lineSemis: [[Int]] = [[]]

    private var isApplying = false
    private var charBuffer = [unichar](repeating: 0, count: 512)

    var isSuspended = false

    private static let maxEditApply = 40_000

    // MARK: attach

    func attach(to storage: NSTextStorage) {
        self.storage = storage
        storage.delegate = self
        rebuildFromScratch()
    }

    /// Call after replacing the whole document (a tab switch, a file load).
    func documentDidChangeWholesale() {
        rebuildFromScratch()
    }

    private var length: Int { storage?.length ?? 0 }

    private var nsString: NSString { (storage?.string ?? "") as NSString }

    // MARK: - line index

    private func lineIndex(for offset: Int) -> Int {
        var lo = 0
        var hi = lineStarts.count - 1
        while lo < hi {
            let mid = (lo + hi + 1) / 2
            if lineStarts[mid] <= offset { lo = mid } else { hi = mid - 1 }
        }
        return lo
    }

    private func lineRange(_ i: Int) -> NSRange {
        let len = length
        let start = min(lineStarts[i], len)
        let raw = i + 1 < lineStarts.count ? lineStarts[i + 1] : len
        let end = min(max(raw, start), len)
        return NSRange(location: start, length: end - start)
    }

    private func ensureBuffer(_ n: Int) {
        if charBuffer.count < n { charBuffer = [unichar](repeating: 0, count: max(n, charBuffer.count * 2)) }
    }

    private func lexLine(
        _ i: Int, collectTokens: Bool, tokens: inout [SQLToken], semis: inout [Int]
    ) -> SQLLexState {
        let range = lineRange(i)
        let entry = i == 0 ? SQLLexState() : (lineEndState[i - 1] ?? SQLLexState())
        guard range.length > 0 else { return entry }
        ensureBuffer(range.length)
        nsString.getCharacters(&charBuffer, range: range)
        return charBuffer.withUnsafeBufferPointer { full in
            let slice = UnsafeBufferPointer(start: full.baseAddress, count: range.length)
            return SQLLineLexer.lex(
                slice, base: range.location, entry: entry, nestComments: nestComments,
                collectTokens: collectTokens, tokens: &tokens, semicolons: &semis)
        }
    }

    // MARK: - full seed

    private func rebuildFromScratch() {
        let ns = nsString
        let n = ns.length
        var starts: [Int] = [0]
        var offset = 0
        let chunk = 1 << 16
        var buf = [unichar](repeating: 0, count: min(chunk, max(n, 1)))
        while offset < n {
            let len = min(chunk, n - offset)
            ns.getCharacters(&buf, range: NSRange(location: offset, length: len))
            for k in 0..<len where buf[k] == 10 { starts.append(offset + k + 1) }
            offset += len
        }
        lineStarts = starts
        lineEndState = Array(repeating: nil, count: starts.count)
        lineClean = Array(repeating: false, count: starts.count)
        lineSemis = Array(repeating: [], count: starts.count)

        var tokens: [SQLToken] = []
        for i in 0..<lineStarts.count {
            var semis: [Int] = []
            lineEndState[i] = lexLine(i, collectTokens: false, tokens: &tokens, semis: &semis)
            lineSemis[i] = semis
        }
        refreshVisible()
    }

    private func invalidateAllAttributes() {
        for i in 0..<lineClean.count { lineClean[i] = false }
        refreshVisible()
    }

    // MARK: - NSTextStorageDelegate

    func textStorage(
        _ textStorage: NSTextStorage,
        didProcessEditing editedMask: NSTextStorageEditActions,
        range editedRange: NSRange,
        changeInLength delta: Int
    ) {
        guard editedMask.contains(.editedCharacters), !isApplying, !isSuspended else { return }
        updateLineIndex(editedRange: editedRange, delta: delta)
        let first = lineIndex(for: editedRange.location)
        relex(from: first, mustCover: editedRange.upperBound)

        // Colour what was just typed, immediately, without touching layout.
        let capped = NSRange(
            location: editedRange.location,
            length: min(editedRange.length, Self.maxEditApply))
        applyAttributes(in: capped, batched: false)
    }

    private func updateLineIndex(editedRange: NSRange, delta: Int) {
        let oldEnd = editedRange.location + editedRange.length - delta
        let L = lineIndex(for: editedRange.location)
        let lo = lineStarts[L]

        var k = L + 1
        while k < lineStarts.count, lineStarts[k] <= oldEnd { k += 1 }
        if delta != 0 {
            for j in k..<lineStarts.count {
                lineStarts[j] += delta
                for m in lineSemis[j].indices { lineSemis[j][m] += delta }
            }
        }

        var inserted: [Int] = []
        let scanLen = max(0, editedRange.upperBound - lo)
        if scanLen > 0 {
            ensureBuffer(scanLen)
            nsString.getCharacters(&charBuffer, range: NSRange(location: lo, length: scanLen))
            for t in 0..<scanLen where charBuffer[t] == 10 { inserted.append(lo + t + 1) }
        }

        let replaced = (L + 1)..<k
        lineStarts.replaceSubrange(replaced, with: inserted)
        lineEndState.replaceSubrange(
            replaced, with: Array(repeating: nil, count: inserted.count))
        lineClean.replaceSubrange(replaced, with: Array(repeating: false, count: inserted.count))
        lineSemis.replaceSubrange(replaced, with: Array(repeating: [], count: inserted.count))
        lineClean[L] = false
    }

    private func relex(from first: Int, mustCover: Int) {
        var tokens: [SQLToken] = []
        var i = max(0, first)
        while i < lineStarts.count {
            var semis: [Int] = []
            let end = lexLine(i, collectTokens: false, tokens: &tokens, semis: &semis)
            let settled = (lineEndState[i] == end) && (lineStarts[i] >= mustCover)
            lineEndState[i] = end
            lineSemis[i] = semis
            lineClean[i] = false
            if settled { break }
            i += 1
        }
    }

    // MARK: - attribute application

    func refreshVisible() {
        guard !isSuspended, let range = visibleRangeProvider?() else { return }
        applyAttributes(in: range, batched: true)
    }

    private func applyAttributes(in range: NSRange, batched: Bool) {
        guard let storage, storage.length > 0, range.length >= 0 else { return }
        let clamped = NSRange(
            location: min(range.location, storage.length),
            length: min(range.length, max(0, storage.length - min(range.location, storage.length))))
        let first = lineIndex(for: clamped.location)
        let last = lineIndex(for: min(clamped.upperBound, max(0, storage.length - 1)))
        guard first <= last else { return }

        var tokens: [SQLToken] = []
        var dirty = false
        for i in first...last where !lineClean[i] {
            dirty = true
            break
        }
        guard dirty else { return }

        isApplying = true
        if batched { storage.beginEditing() }
        let base: [NSAttributedString.Key: Any] = [
            .font: font, .foregroundColor: NSColor.textColor,
        ]
        for i in first...last where !lineClean[i] {
            let lr = lineRange(i)
            guard lr.length > 0 else {
                lineClean[i] = true
                continue
            }
            tokens.removeAll(keepingCapacity: true)
            var semis: [Int] = []
            _ = lexLine(i, collectTokens: true, tokens: &tokens, semis: &semis)
            storage.setAttributes(base, range: lr)
            for tok in tokens {
                guard NSMaxRange(tok.range) <= storage.length else { continue }
                if let c = SQLTheme.color(for: tok.kind) {
                    storage.addAttribute(.foregroundColor, value: c, range: tok.range)
                }
                if tok.kind == .directive {
                    storage.addAttribute(.font, value: boldFont, range: tok.range)
                }
            }
            lineClean[i] = true
        }
        if batched { storage.endEditing() }
        isApplying = false
    }

    // MARK: - block, line and bracket geometry

    func blockRange(containing offset: Int) -> NSRange {
        guard length > 0 else { return NSRange(location: 0, length: 0) }
        let caret = min(max(0, offset), length)
        let L = lineIndex(for: caret)

        var start = 0
        search: for i in stride(from: L, through: 0, by: -1) {
            for s in lineSemis[i].reversed() where s < caret {
                start = s + 1
                break search
            }
        }
        var end = length
        search2: for i in L..<lineSemis.count {
            for s in lineSemis[i] where s >= caret {
                end = s + 1
                break search2
            }
        }
        if end < start { end = start }
        return NSRange(location: start, length: end - start)
    }

    func lineRange(containing offset: Int) -> NSRange {
        guard length > 0 else { return NSRange(location: 0, length: 0) }
        return lineRange(lineIndex(for: min(max(0, offset), length)))
    }

    private static let bracketScanLines = 400

    func bracketPair(at caret: Int) -> (NSRange, NSRange)? {
        guard length > 0 else { return nil }
        let openers: [unichar] = [40, 91, 123]
        let closers: [unichar] = [41, 93, 125]

        // Prefer the character *before* the caret (closing) then the one after.
        for probe in [caret - 1, caret] {
            guard probe >= 0, probe < length else { continue }
            guard let ch = bracketChar(at: probe) else { continue }
            if let idx = openers.firstIndex(of: ch) {
                if let partner = scanForward(from: probe, open: ch, close: closers[idx]) {
                    return (NSRange(location: probe, length: 1), NSRange(location: partner, length: 1))
                }
            } else if let idx = closers.firstIndex(of: ch) {
                if let partner = scanBackward(from: probe, open: openers[idx], close: ch) {
                    return (NSRange(location: partner, length: 1), NSRange(location: probe, length: 1))
                }
            }
        }
        return nil
    }

    private func bracketChar(at offset: Int) -> unichar? {
        let i = lineIndex(for: offset)
        for t in brackets(inLine: i) where t.offset == offset { return t.ch }
        return nil
    }

    private func brackets(inLine i: Int) -> [(offset: Int, ch: unichar)] {
        var tokens: [SQLToken] = []
        var semis: [Int] = []
        _ = lexLine(i, collectTokens: true, tokens: &tokens, semis: &semis)
        var out: [(Int, unichar)] = []
        let ns = nsString
        for tok in tokens where tok.kind == .punct && tok.range.length == 1 {
            let ch = ns.character(at: tok.range.location)
            if SQLLineLexer.isBracket(ch) { out.append((tok.range.location, ch)) }
        }
        return out
    }

    private func scanForward(from offset: Int, open: unichar, close: unichar) -> Int? {
        var depth = 0
        let startLine = lineIndex(for: offset)
        let lastLine = min(lineStarts.count - 1, startLine + Self.bracketScanLines)
        for i in startLine...lastLine {
            for b in brackets(inLine: i) where b.offset >= offset {
                if b.ch == open { depth += 1 }
                if b.ch == close {
                    depth -= 1
                    if depth == 0 { return b.offset }
                }
            }
        }
        return nil
    }

    private func scanBackward(from offset: Int, open: unichar, close: unichar) -> Int? {
        var depth = 0
        let startLine = lineIndex(for: offset)
        let firstLine = max(0, startLine - Self.bracketScanLines)
        for i in stride(from: startLine, through: firstLine, by: -1) {
            for b in brackets(inLine: i).reversed() where b.offset <= offset {
                if b.ch == close { depth += 1 }
                if b.ch == open {
                    depth -= 1
                    if depth == 0 { return b.offset }
                }
            }
        }
        return nil
    }
}
