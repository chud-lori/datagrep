import AppKit

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

    func clamped(rowCount: Int, columnCount: Int) -> GridCellRange? {
        guard rowCount > 0, columnCount > 0 else { return nil }
        var r = self
        r.anchorRow = max(0, min(rowCount - 1, r.anchorRow))
        r.focusRow = max(0, min(rowCount - 1, r.focusRow))
        r.anchorColumn = max(0, min(columnCount - 1, r.anchorColumn))
        r.focusColumn = max(0, min(columnCount - 1, r.focusColumn))
        return r
    }

    func spansAllColumns(of columnCount: Int) -> Bool {
        columns.lowerBound <= 0 && columns.upperBound >= columnCount - 1
    }
}

enum GridCopy {
    static let maxRows = 20_000

    static func summary(rows: Int, columns: Int, truncated: Bool) -> String {
        let r = rows == 1 ? "1 row" : "\(rows) rows"
        let c = columns == 1 ? "1 column" : "\(columns) columns"
        return truncated ? "copied first \(r) × \(c) (selection capped)" : "copied \(r) × \(c)"
    }
}
