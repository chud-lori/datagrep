import CDatagrepFFI
import Foundation

/// The four facts a cell can carry. `null` and `absent` are DIFFERENT facts:
/// absent means the field is not present in the document at all.
public enum CellKind: UInt8, Sendable {
    case value = 0
    case null = 1
    case absent = 2
    case nested = 3

    public init(raw: UInt8) { self = CellKind(rawValue: raw) ?? .value }
}

/// Owns one `DatagrepRows*`. Freed exactly once, in `deinit` — which is what makes
/// window eviction from the LRU leak-proof by construction.
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
    /// Called only for cells that are actually on screen — never for a window.
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
}

/// A bounded, page-keyed LRU over `RowWindow`s.
///
/// This is the whole memory story of the grid: `numberOfRows` may say
/// 1,000,000, but at most `maxPages * pageSize` rows are ever materialised, and
/// evicting a page drops its `DatagrepRows` immediately.
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

    /// Drops cached pages that were only partially filled while streaming, so a
    /// half-page fetched at 40% progress is re-fetched once more rows land.
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
