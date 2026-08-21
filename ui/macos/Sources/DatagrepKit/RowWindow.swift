import CDatagrepFFI
import Foundation

public enum CellKind: UInt8, Sendable {
    case value = 0
    case null = 1
    case absent = 2
    case nested = 3

    public init(raw: UInt8) { self = CellKind(rawValue: raw) ?? .value }
}

public final class RowWindow {
    let raw: OpaquePointer
    public let offset: UInt64
    public let count: UInt64
    public let columns: UInt32
    /// True when the store could not supply the whole requested window yet.
    public let pending: Bool

    init(raw: OpaquePointer, offset: UInt64) {
        self.raw = raw
        self.offset = offset
        self.count = datagrep_rows_count(raw)
        self.columns = datagrep_rows_columns(raw)
        self.pending = datagrep_rows_pending(raw)
    }

    deinit { datagrep_rows_free(raw) }

    public func contains(row: UInt64) -> Bool {
        row >= offset && row < offset &+ count
    }

    @inline(__always)
    public func kind(absoluteRow: UInt64, col: UInt32) -> CellKind {
        CellKind(raw: datagrep_rows_cell_kind(raw, absoluteRow &- offset, col))
    }

    /// Builds a Swift String from the borrowed, NOT nul-terminated pointer.
    @inline(__always)
    public func text(absoluteRow: UInt64, col: UInt32) -> String {
        var len = 0
        guard let p = datagrep_rows_cell(raw, absoluteRow &- offset, col, &len), len > 0 else {
            return ""
        }
        return p.withMemoryRebound(to: UInt8.self, capacity: len) { bytes in
            String(decoding: UnsafeBufferPointer(start: bytes, count: len), as: UTF8.self)
        }
    }

    public func detailJSON(absoluteRow: UInt64, col: UInt32) -> String? {
        takeOwnedString(datagrep_rows_cell_detail_json(raw, absoluteRow &- offset, col))
    }

    /// One cell's loaded value, when it is a value an edit can carry.
    public func loadedValue(absoluteRow: UInt64, col: UInt32) -> MutationValue? {
        guard kind(absoluteRow: absoluteRow, col: col) != .nested,
            let text = detailJSON(absoluteRow: absoluteRow, col: col)
        else { return nil }
        return MutationValue.decode(jsonObject(text))
    }

    /// The field names THIS window projected, in column order.
    public func columnNames() -> [String] {
        guard let text = takeOwnedString(datagrep_rows_column_names_json(raw)) else { return [] }
        return jsonObject(text) as? [String] ?? []
    }

    public func envelope(absoluteRow: UInt64) -> [String: Any]? {
        guard let text = takeOwnedString(datagrep_rows_envelope_json(raw, absoluteRow &- offset))
        else { return nil }
        return jsonObject(text) as? [String: Any]
    }
}

/// A bounded, page-keyed LRU over `RowWindow`s.
public final class RowPager {
    public let pageSize: UInt64
    public let maxPages: Int
    private unowned let query: DatagrepQueryHandle
    private var pages: [UInt64: RowWindow] = [:]
    private var order: [UInt64] = []  // least-recently-used first

    // Instrumentation (read by the status bar / bench; never drives a redraw).
    public private(set) var fetches: UInt64 = 0
    public private(set) var evictions: UInt64 = 0
    public private(set) var totalFetchNanos: UInt64 = 0
    public private(set) var maxFetchNanos: UInt64 = 0

    public init(query: DatagrepQueryHandle, pageSize: UInt64 = 512, maxPages: Int = 4) {
        self.query = query
        self.pageSize = pageSize
        self.maxPages = maxPages
    }

    public var residentRows: UInt64 { pages.values.reduce(0) { $0 + $1.count } }
    public var residentPages: Int { pages.count }

    public func invalidateAll() {
        pages.removeAll()  // each RowWindow deinit -> datagrep_rows_free
        order.removeAll()
    }

    public func invalidatePartialPages() {
        for (key, w) in pages where w.pending || w.count < pageSize {
            pages[key] = nil
            order.removeAll { $0 == key }
        }
    }

    public func window(for row: UInt64) -> RowWindow? {
        let page = row / pageSize
        if let w = pages[page] {
            touch(page)
            return w.contains(row: row) ? w : nil
        }
        let start = DispatchTime.now().uptimeNanoseconds
        guard let w = try? query.rows(offset: page * pageSize, len: pageSize) else { return nil }
        let elapsed = DispatchTime.now().uptimeNanoseconds - start
        fetches += 1
        totalFetchNanos += elapsed
        maxFetchNanos = max(maxFetchNanos, elapsed)

        pages[page] = w
        order.append(page)
        while order.count > maxPages {
            let victim = order.removeFirst()
            pages[victim] = nil  // datagrep_rows_free happens here
            evictions += 1
        }
        return w.contains(row: row) ? w : nil
    }

    private func touch(_ page: UInt64) {
        if let i = order.firstIndex(of: page) {
            order.remove(at: i)
            order.append(page)
        }
    }
}
