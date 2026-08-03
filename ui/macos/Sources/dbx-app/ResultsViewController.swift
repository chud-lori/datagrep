import AppKit
import DbxKit

/// The results grid. Virtual by construction:
///   - `numberOfRows` returns the full row count the core reports
///   - `viewFor row:` pulls ONLY that row, out of a 512-row page window
///   - at most 4 pages (2,048 rows) are resident; eviction frees the DbxRows
final class ResultsViewController: NSViewController, NSTableViewDataSource, NSTableViewDelegate {
    let tableView = NSTableView()
    private let scrollView = NSScrollView()
    private let placeholder = NSTextField(labelWithString: "")

    private(set) var pager: RowPager?
    private var rowCount: Int = 0
    private var columnIndexByID: [NSUserInterfaceItemIdentifier: UInt32] = [:]
    private var columnByName: [String: NSTableColumn] = [:]
    /// Numeric columns render right-aligned; text left-aligned.
    private var rightAlignedByID: [NSUserInterfaceItemIdentifier: Bool] = [:]
    /// Design risk #7: a 400-path Mongo collection must not become a 400-column
    /// grid. Columns beyond this are created but hidden.
    let maxVisibleColumns = 30
    private(set) var hiddenColumnCount = 0
    private var didSizeColumns = false

    var onNestedCell: ((Int, UInt32, RowWindow) -> Void)?
    var onHiddenColumnsChanged: ((Int) -> Void)?

    override func loadView() {
        let root = NSView()

        tableView.dataSource = self
        tableView.delegate = self
        tableView.rowHeight = GridStyle.rowHeight
        tableView.usesAlternatingRowBackgroundColors = true
        tableView.columnAutoresizingStyle = .noColumnAutoresizing
        tableView.allowsColumnReordering = false  // never reorder existing columns
        tableView.allowsColumnResizing = true
        tableView.allowsMultipleSelection = true
        tableView.style = .plain
        tableView.gridStyleMask = [.solidVerticalGridLineMask]
        tableView.headerView = NSTableHeaderView()
        tableView.intercellSpacing = NSSize(width: 1, height: 0)

        scrollView.documentView = tableView
        scrollView.hasVerticalScroller = true
        scrollView.hasHorizontalScroller = true
        scrollView.autohidesScrollers = false
        scrollView.translatesAutoresizingMaskIntoConstraints = false
        scrollView.borderType = .noBorder

        placeholder.stringValue = "No result yet — press ⌘⏎ to run the statement under the caret."
        placeholder.textColor = .tertiaryLabelColor
        placeholder.font = NSFont.systemFont(ofSize: 12)
        placeholder.translatesAutoresizingMaskIntoConstraints = false

        root.addSubview(scrollView)
        root.addSubview(placeholder)
        NSLayoutConstraint.activate([
            scrollView.topAnchor.constraint(equalTo: root.topAnchor),
            scrollView.bottomAnchor.constraint(equalTo: root.bottomAnchor),
            scrollView.leadingAnchor.constraint(equalTo: root.leadingAnchor),
            scrollView.trailingAnchor.constraint(equalTo: root.trailingAnchor),
            placeholder.centerXAnchor.constraint(equalTo: root.centerXAnchor),
            placeholder.centerYAnchor.constraint(equalTo: root.centerYAnchor),
        ])
        self.view = root
    }

    // MARK: - result lifecycle

    /// A brand new result: everything about the previous one is dropped.
    func beginNewResult(pager: RowPager) {
        self.pager?.invalidateAll()
        self.pager = pager
        rowCount = 0
        didSizeColumns = false
        hiddenColumnCount = 0
        for c in tableView.tableColumns { tableView.removeTableColumn(c) }
        columnIndexByID.removeAll()
        columnByName.removeAll()
        rightAlignedByID.removeAll()
        placeholder.isHidden = false
        tableView.reloadData()
        onHiddenColumnsChanged?(0)
    }

    func clear() {
        pager?.invalidateAll()
        pager = nil
        rowCount = 0
        for c in tableView.tableColumns { tableView.removeTableColumn(c) }
        columnIndexByID.removeAll()
        columnByName.removeAll()
        rightAlignedByID.removeAll()
        placeholder.isHidden = false
        tableView.reloadData()
    }

    /// Applies a status snapshot. Columns only ever APPEND on the right; an
    /// existing column is never moved, renamed or re-sized by a schema delta
    /// (design risk #7 — columns jumping mid-scroll is the failure mode).
    func apply(status: QueryStatus) {
        applySchema(status.columns)
        let newCount = Int(status.rowsLoaded)
        let grew = newCount != rowCount
        rowCount = newCount
        placeholder.isHidden = newCount > 0 || !status.columns.isEmpty

        // Pages fetched while the tail was still streaming may be short; drop
        // only those, keep fully-materialised ones.
        pager?.invalidatePartialPages()

        if grew { tableView.noteNumberOfRowsChanged() }

        if !didSizeColumns, newCount > 0, !status.columns.isEmpty {
            sizeColumnsFromFirstPageSample()
            didSizeColumns = true
        }

        // Redraw the visible window only. Never the whole 1M rows.
        let visible = tableView.rows(in: tableView.visibleRect)
        if visible.length > 0 {
            tableView.reloadData(
                forRowIndexes: IndexSet(integersIn: visible.lowerBound..<visible.upperBound),
                columnIndexes: IndexSet(integersIn: 0..<tableView.tableColumns.count))
        }
    }

    private func applySchema(_ columns: [ColumnSpec]) {
        var newlyHidden = hiddenColumnCount
        for (i, spec) in columns.enumerated() {
            if columnByName[spec.name] != nil { continue }  // already present: leave it alone
            let id = NSUserInterfaceItemIdentifier("dbx.col.\(i).\(spec.name)")
            let col = NSTableColumn(identifier: id)
            col.title = spec.name
            col.headerToolTip = "\(spec.name) — \(spec.type)"
            col.width = 120
            col.minWidth = 44
            col.maxWidth = 900
            columnIndexByID[id] = UInt32(i)
            rightAlignedByID[id] = Self.isNumeric(spec.type)
            col.headerCell.alignment = Self.isNumeric(spec.type) ? .right : .left
            columnByName[spec.name] = col
            if tableView.tableColumns.count >= maxVisibleColumns {
                col.isHidden = true
                newlyHidden += 1
            }
            tableView.addTableColumn(col)  // appends on the RIGHT
        }
        if newlyHidden != hiddenColumnCount {
            hiddenColumnCount = newlyHidden
            onHiddenColumnsChanged?(hiddenColumnCount)
        }
    }

    /// Widths come from a sample of the first page only — never from a scan.
    private func sizeColumnsFromFirstPageSample() {
        guard let pager else { return }
        let sampleRows = min(64, rowCount)
        guard sampleRows > 0 else { return }
        var widths = [CGFloat](repeating: 0, count: tableView.tableColumns.count)
        for r in 0..<sampleRows {
            guard let win = pager.window(for: UInt64(r)) else { break }
            for (ci, col) in tableView.tableColumns.enumerated() {
                guard let idx = columnIndexByID[col.identifier], idx < win.columns else { continue }
                let k = win.kind(absoluteRow: UInt64(r), col: idx)
                let s: String
                switch k {
                case .null: s = "NULL"
                case .absent: s = "—"
                default: s = win.text(absoluteRow: UInt64(r), col: idx)
                }
                let w = (s as NSString).size(withAttributes: GridStyle.value.left).width
                if w > widths[ci] { widths[ci] = w }
            }
        }
        for (ci, col) in tableView.tableColumns.enumerated() {
            let header = (col.title as NSString).size(withAttributes: GridStyle.value.left).width
            col.width = min(max(max(widths[ci], header) + 20, 56), 340)
        }
    }

    // MARK: - NSTableViewDataSource

    static func isNumeric(_ type: String) -> Bool {
        let t = type.lowercased()
        for token in [
            "int", "double", "float", "numeric", "decimal", "real", "serial", "money", "bigint",
        ] where t.contains(token) {
            return true
        }
        return false
    }

    func numberOfRows(in tableView: NSTableView) -> Int { rowCount }

    func tableView(_ tableView: NSTableView, viewFor tableColumn: NSTableColumn?, row: Int)
        -> NSView?
    {
        guard let tableColumn, let colIndex = columnIndexByID[tableColumn.identifier] else {
            return nil
        }
        let cell: GridCellView
        if let reused = tableView.makeView(withIdentifier: GridCellView.reuseID, owner: nil)
            as? GridCellView
        {
            cell = reused
        } else {
            cell = GridCellView()
            cell.identifier = GridCellView.reuseID
        }

        guard let pager, let win = pager.window(for: UInt64(row)), colIndex < win.columns else {
            cell.configure(
                kind: .value, text: "", row: row, column: colIndex, pending: true,
                rightAligned: rightAlignedByID[tableColumn.identifier] ?? false)
            cell.onNestedClick = nil
            return cell
        }

        let kind = win.kind(absoluteRow: UInt64(row), col: colIndex)
        // Strings are built ONLY for cells that will actually be drawn, and only
        // for the kinds that have text.
        let text: String
        switch kind {
        case .null, .absent: text = ""
        default: text = win.text(absoluteRow: UInt64(row), col: colIndex)
        }
        cell.configure(
            kind: kind, text: text, row: row, column: colIndex, pending: false,
            rightAligned: rightAlignedByID[tableColumn.identifier] ?? false)
        cell.onNestedClick = { [weak self] c in
            guard let self, let p = self.pager, let w = p.window(for: UInt64(c.row)) else { return }
            self.onNestedCell?(c.row, c.column, w)
        }
        return cell
    }
}


/// SwiftUI bridge for the grid. SwiftUI's own `Table`/`List` are NOT used: they
/// do not virtualise predictably at a million rows, which is the entire product
/// claim. `makeNSViewController` returns the model-owned controller so SwiftUI
/// re-evaluation can never rebuild the table or drop the page cache.
struct ResultsGridView: NSViewControllerRepresentable {
    let controller: ResultsViewController
    func makeNSViewController(context: Context) -> ResultsViewController { controller }
    func updateNSViewController(_ nsViewController: ResultsViewController, context: Context) {}
}
