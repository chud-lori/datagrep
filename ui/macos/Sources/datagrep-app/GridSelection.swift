import AppKit

/// A rectangular block of cells, expressed in DISPLAY coordinates: a view row
/// index and a column *position* in the table (not the engine's column index,
/// which survives reordering and is resolved separately).
///
/// The block is stored as anchor + focus rather than as two ranges, because
/// which corner is which is exactly what shift-click and shift-arrow need to
/// know: extending always moves the focus and leaves the anchor pinned.
struct GridCellRange: Equatable {
    var anchorRow: Int
    var anchorColumn: Int
    var focusRow: Int
    var focusColumn: Int

    init(row: Int, column: Int) {
        anchorRow = row
        anchorColumn = column
        focusRow = row
        focusColumn = column
    }

    var rows: ClosedRange<Int> { min(anchorRow, focusRow)...max(anchorRow, focusRow) }
    var columns: ClosedRange<Int> { min(anchorColumn, focusColumn)...max(anchorColumn, focusColumn) }

    var isSingleCell: Bool { rows.count == 1 && columns.count == 1 }
    var rowCount: Int { rows.count }
    var columnCount: Int { columns.count }

    mutating func extend(toRow row: Int, column: Int) {
        focusRow = row
        focusColumn = column
    }

    mutating func moveTo(row: Int, column: Int) {
        anchorRow = row
        anchorColumn = column
        focusRow = row
        focusColumn = column
    }

    /// Keeps the block inside a table that may have shrunk (a new result, or a
    /// streaming one that was reset) without ever silently pointing at rows the
    /// engine no longer reports.
    func clamped(rowCount: Int, columnCount: Int) -> GridCellRange? {
        guard rowCount > 0, columnCount > 0 else { return nil }
        var r = self
        r.anchorRow = max(0, min(rowCount - 1, r.anchorRow))
        r.focusRow = max(0, min(rowCount - 1, r.focusRow))
        r.anchorColumn = max(0, min(columnCount - 1, r.anchorColumn))
        r.focusColumn = max(0, min(columnCount - 1, r.focusColumn))
        return r
    }

    /// True when the block covers every column the table has, i.e. the user is
    /// really selecting whole rows and should see a whole-row highlight.
    func spansAllColumns(of columnCount: Int) -> Bool {
        columns.lowerBound <= 0 && columns.upperBound >= columnCount - 1
    }
}

enum GridCopy {
    /// A drag with autoscroll can cover an unbounded number of rows. Copying is
    /// the one place where a big selection would pull page after page through
    /// the FFI, so it is capped and the status message says so.
    static let maxRows = 20_000

    static func summary(rows: Int, columns: Int, truncated: Bool) -> String {
        let r = rows == 1 ? "1 row" : "\(rows) rows"
        let c = columns == 1 ? "1 column" : "\(columns) columns"
        return truncated ? "copied first \(r) × \(c) (selection capped)" : "copied \(r) × \(c)"
    }
}
