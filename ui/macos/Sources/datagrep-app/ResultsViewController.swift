import AppKit
import DatagrepKit
import SwiftUI

// MARK: - the table view itself

/// `NSTableView` with a pointer and a keyboard.
///
/// Everything here is presentation state — hover row, focused column, copy,
/// context menu. None of it touches the pager, so none of it can break the
/// virtualisation contract: the table still only ever asks for rows that are
/// on screen.
final class GridTableView: NSTableView {
    var onCopyRequest: (() -> String?)?
    var onOpenFocusedCell: (() -> Void)?
    var onFocusChanged: (() -> Void)?

    private(set) var hoverRow = -1
    private(set) var focusedColumn = 0
    private var tracking: NSTrackingArea?

    override func updateTrackingAreas() {
        super.updateTrackingAreas()
        if let tracking { removeTrackingArea(tracking) }
        let t = NSTrackingArea(
            rect: .zero,
            options: [
                .mouseEnteredAndExited, .mouseMoved, .activeInKeyWindow, .inVisibleRect,
            ],
            owner: self)
        addTrackingArea(t)
        tracking = t
    }

    override func mouseMoved(with event: NSEvent) {
        let p = convert(event.locationInWindow, from: nil)
        setHover(row(at: p))
        super.mouseMoved(with: event)
    }

    override func mouseExited(with event: NSEvent) {
        setHover(-1)
        super.mouseExited(with: event)
    }

    /// Repaints exactly two rows: the one the pointer left and the one it
    /// entered. Hover costs 2 row rects per pointer move, never a reload.
    private func setHover(_ newRow: Int) {
        guard newRow != hoverRow else { return }
        let old = hoverRow
        hoverRow = newRow
        for r in [old, newRow] where r >= 0 && r < numberOfRows {
            (rowView(atRow: r, makeIfNecessary: false) as? GridRowView)?.isHovered = (r == hoverRow)
        }
    }

    // MARK: keyboard

    override func keyDown(with event: NSEvent) {
        let cmd = event.modifierFlags.contains(.command)
        switch event.keyCode {
        case 123:  // left
            moveFocus(by: -1)
            return
        case 124:  // right
            moveFocus(by: +1)
            return
        case 126 where cmd:  // ⌘↑ -> first row
            jump(to: 0)
            return
        case 125 where cmd:  // ⌘↓ -> last row
            jump(to: numberOfRows - 1)
            return
        case 49:  // space -> inspect the focused cell
            onOpenFocusedCell?()
            return
        default:
            super.keyDown(with: event)
        }
    }

    private func moveFocus(by delta: Int) {
        guard numberOfColumns > 0 else { return }
        let next = max(0, min(numberOfColumns - 1, focusedColumn + delta))
        guard next != focusedColumn else { return }
        focusedColumn = next
        scrollColumnToVisible(next)
        refreshFocusRing()
        onFocusChanged?()
    }

    private func jump(to row: Int) {
        guard row >= 0, row < numberOfRows else { return }
        selectRowIndexes(IndexSet(integer: row), byExtendingSelection: false)
        scrollRowToVisible(row)
        refreshFocusRing()
    }

    func refreshFocusRing() {
        let rows = self.rows(in: visibleRect)
        guard rows.length > 0 else { return }
        for r in rows.lowerBound..<rows.upperBound {
            (rowView(atRow: r, makeIfNecessary: false) as? GridRowView)?.focusedColumn =
                (r == selectedRow) ? focusedColumn : -1
        }
    }

    var focusedCell: (row: Int, column: Int)? {
        guard selectedRow >= 0, focusedColumn < numberOfColumns else { return nil }
        return (selectedRow, focusedColumn)
    }

    // MARK: copy

    /// ⌘C. Routed here by the Edit menu's `copy:` because the table is first
    /// responder — no extra key handling needed.
    @objc func copy(_ sender: Any?) {
        guard let text = onCopyRequest?(), !text.isEmpty else { return }
        NSPasteboard.general.clearContents()
        NSPasteboard.general.setString(text, forType: .string)
    }

    override func validateUserInterfaceItem(_ item: NSValidatedUserInterfaceItem) -> Bool {
        if item.action == #selector(copy(_:)) { return selectedRowIndexes.count > 0 }
        return super.validateUserInterfaceItem(item)
    }

    override var acceptsFirstResponder: Bool { true }
}

// MARK: - controller

/// The results grid. Virtual by construction:
///   - `numberOfRows` returns the full row count the core reports
///   - `viewFor row:` pulls ONLY that row, out of a 512-row page window
///   - at most 4 pages (2,048 rows) are resident; eviction frees the DatagrepRows
final class ResultsViewController: NSViewController, NSTableViewDataSource, NSTableViewDelegate,
    NSMenuDelegate
{
    let tableView = GridTableView()
    private let scrollView = NSScrollView()

    private(set) var pager: RowPager?
    private var rowCount: Int = 0
    private var columnIndexByID: [NSUserInterfaceItemIdentifier: UInt32] = [:]
    private var columnByName: [String: NSTableColumn] = [:]
    /// Numeric columns render right-aligned; text left-aligned.
    private var rightAlignedByID: [NSUserInterfaceItemIdentifier: Bool] = [:]
    private var columnNames: [String] = []
    /// Design risk #7: a 400-path Mongo collection must not become a 400-column
    /// grid. Columns beyond this are created but hidden.
    let maxVisibleColumns = 30
    private(set) var hiddenColumnCount = 0
    private var didSizeColumns = false
    private var isStreaming = false
    private var resultIsCapped = false

    var onNestedCell: ((Int, UInt32, RowWindow) -> Void)?
    var onHiddenColumnsChanged: ((Int) -> Void)?
    var onSortRequested: ((String) -> Void)?
    var onFilterRequested: ((String, String) -> Void)?
    var onCopied: ((String) -> Void)?
    /// Which column the engine is currently sorted by, and which way. Owned by
    /// the model (the sort is a re-issued query, not a client-side shuffle).
    var sortColumn: String?
    var sortAscending = true

    override func loadView() {
        let root = NSView()

        tableView.dataSource = self
        tableView.delegate = self
        tableView.rowHeight = GridStyle.rowHeight
        tableView.usesAlternatingRowBackgroundColors = true
        tableView.columnAutoresizingStyle = .noColumnAutoresizing
        // Reordering is now allowed: `viewFor` resolves a column by IDENTITY,
        // not by position, so moving a column cannot mis-address a cell.
        tableView.allowsColumnReordering = true
        tableView.allowsColumnResizing = true
        tableView.allowsMultipleSelection = true
        tableView.allowsEmptySelection = true
        tableView.style = .plain
        tableView.selectionHighlightStyle = .regular
        tableView.gridStyleMask = [.solidVerticalGridLineMask]
        tableView.gridColor = .separatorColor
        tableView.intercellSpacing = NSSize(width: 1, height: 0)
        tableView.backgroundColor = .textBackgroundColor

        let header = GridHeaderView()
        header.frame = NSRect(x: 0, y: 0, width: 0, height: GridStyle.headerHeight)
        tableView.headerView = header

        let menu = NSMenu()
        menu.delegate = self
        tableView.menu = menu

        tableView.onCopyRequest = { [weak self] in self?.selectionAsTSV() }
        tableView.onOpenFocusedCell = { [weak self] in self?.openFocusedCell() }
        tableView.onFocusChanged = {}
        tableView.doubleAction = #selector(tableDoubleClicked(_:))
        tableView.target = self

        scrollView.documentView = tableView
        scrollView.hasVerticalScroller = true
        scrollView.hasHorizontalScroller = true
        scrollView.autohidesScrollers = true
        scrollView.translatesAutoresizingMaskIntoConstraints = false
        scrollView.borderType = .noBorder
        scrollView.drawsBackground = true
        scrollView.backgroundColor = .textBackgroundColor
        scrollView.scrollerStyle = .overlay

        root.addSubview(scrollView)
        NSLayoutConstraint.activate([
            scrollView.topAnchor.constraint(equalTo: root.topAnchor),
            scrollView.bottomAnchor.constraint(equalTo: root.bottomAnchor),
            scrollView.leadingAnchor.constraint(equalTo: root.leadingAnchor),
            scrollView.trailingAnchor.constraint(equalTo: root.trailingAnchor),
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
        columnNames.removeAll()
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
        columnNames.removeAll()
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
        isStreaming = !status.state.isTerminal
        resultIsCapped = status.state == .capped

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
        applySortIndicator()
    }

    private func applySchema(_ columns: [ColumnSpec]) {
        var newlyHidden = hiddenColumnCount
        for (i, spec) in columns.enumerated() {
            if columnByName[spec.name] != nil { continue }  // already present: leave it alone
            let id = NSUserInterfaceItemIdentifier("datagrep.col.\(i).\(spec.name)")
            let col = NSTableColumn(identifier: id)
            let numeric = Self.isNumeric(spec.type)
            col.title = spec.name
            col.headerCell.attributedStringValue = GridStyle.headerString(
                spec.name, rightAligned: numeric)
            col.headerToolTip = headerTooltip(name: spec.name, type: spec.type)
            col.width = 130
            col.minWidth = 48
            col.maxWidth = 900
            col.resizingMask = [.userResizingMask]
            columnIndexByID[id] = UInt32(i)
            rightAlignedByID[id] = numeric
            columnByName[spec.name] = col
            while columnNames.count <= i { columnNames.append("") }
            columnNames[i] = spec.name
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

    private func headerTooltip(name: String, type: String) -> String {
        var t = "\(name) — \(type)\nclick to sort (re-runs the query with ORDER BY)"
        if resultIsCapped {
            t +=
                "\nthis result hit the engine's row cap, so a sort re-runs the query rather than reordering what is loaded"
        }
        return t
    }

    /// The chevron in the header. `NSAscendingSortIndicator` is AppKit's own
    /// image, so it matches Finder's exactly.
    private func applySortIndicator() {
        for col in tableView.tableColumns {
            let isSorted = col.title == sortColumn
            tableView.setIndicatorImage(
                isSorted
                    ? NSImage(
                        named: sortAscending
                            ? "NSAscendingSortIndicator" : "NSDescendingSortIndicator")
                    : nil,
                in: col)
        }
        tableView.highlightedTableColumn = sortColumn.flatMap { columnByName[$0] }
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
            let header =
                (col.title as NSString).size(withAttributes: [.font: GridStyle.headerFont]).width
            col.width = min(max(max(widths[ci], header) + 34, 64), 340)
        }
    }

    // MARK: - NSTableViewDataSource / Delegate

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

    func tableView(_ tableView: NSTableView, rowViewForRow row: Int) -> NSTableRowView? {
        let id = NSUserInterfaceItemIdentifier("datagrep.grid.row")
        if let reused = tableView.makeView(withIdentifier: id, owner: nil) as? GridRowView {
            reused.isHovered = (row == self.tableView.hoverRow)
            reused.focusedColumn =
                (row == tableView.selectedRow) ? self.tableView.focusedColumn : -1
            return reused
        }
        let v = GridRowView()
        v.identifier = id
        v.isHovered = (row == self.tableView.hoverRow)
        return v
    }

    func tableViewSelectionDidChange(_ notification: Notification) {
        tableView.refreshFocusRing()
    }

    /// Double-click a column divider. AppKit asks the delegate how wide the
    /// column would like to be; we answer from the VISIBLE rows only, so
    /// size-to-fit is O(viewport) and not O(500 000).
    func tableView(_ tableView: NSTableView, sizeToFitWidthOfColumn column: Int) -> CGFloat {
        let col = tableView.tableColumns[column]
        guard let pager, let idx = columnIndexByID[col.identifier] else { return col.width }
        var maxWidth =
            (col.title as NSString).size(withAttributes: [.font: GridStyle.headerFont]).width
        let visible = tableView.rows(in: tableView.visibleRect)
        guard visible.length > 0 else { return col.width }
        for r in visible.lowerBound..<visible.upperBound {
            guard let win = pager.window(for: UInt64(r)), idx < win.columns else { continue }
            let s = displayText(win, row: r, col: idx)
            let w = (s as NSString).size(withAttributes: GridStyle.value.left).width
            if w > maxWidth { maxWidth = w }
        }
        return min(max(maxWidth + 2 * GridStyle.cellPadX + 14, 64), 900)
    }

    /// Header click -> sort. Handled by re-issuing the query through the
    /// engine, never by sorting the page cache: with 500 000 rows behind a
    /// 2 048-row window, a client-side sort would be a visible lie.
    func tableView(_ tableView: NSTableView, didClick tableColumn: NSTableColumn) {
        onSortRequested?(tableColumn.title)
    }

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
            // Shimmer only while rows are genuinely still arriving; a terminal
            // query leaves a completely static window (design §5 idle budget).
            cell.setShimmer(isStreaming)
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
        cell.setShimmer(false)
        cell.onNestedClick = { [weak self] c in
            guard let self, let p = self.pager, let w = p.window(for: UInt64(c.row)) else { return }
            self.onNestedCell?(c.row, c.column, w)
        }
        return cell
    }

    @objc private func tableDoubleClicked(_ sender: Any?) {
        openFocusedCell()
    }

    // MARK: - cell text helpers

    private func displayText(_ win: RowWindow, row: Int, col: UInt32) -> String {
        switch win.kind(absoluteRow: UInt64(row), col: col) {
        case .null: return "NULL"
        case .absent: return "—"
        default: return win.text(absoluteRow: UInt64(row), col: col)
        }
    }

    /// The raw value, for the clipboard and for `WHERE x = …`: a NULL copies as
    /// an empty string here rather than the word NULL, because pasting the word
    /// NULL into a spreadsheet would be wrong.
    private func rawText(_ win: RowWindow, row: Int, col: UInt32) -> String {
        switch win.kind(absoluteRow: UInt64(row), col: col) {
        case .null, .absent: return ""
        default: return win.text(absoluteRow: UInt64(row), col: col)
        }
    }

    private func visibleColumnPairs() -> [(name: String, index: UInt32)] {
        tableView.tableColumns.compactMap { col in
            guard !col.isHidden, let idx = columnIndexByID[col.identifier] else { return nil }
            return (col.title, idx)
        }
    }

    func selectionAsTSV() -> String? {
        let rows = tableView.selectedRowIndexes
        guard !rows.isEmpty, let pager else { return nil }
        let cols = visibleColumnPairs()
        var out = cols.map(\.name).joined(separator: "\t")
        for r in rows {
            guard let win = pager.window(for: UInt64(r)) else { continue }
            out += "\n" + cols.map { rawText(win, row: r, col: $0.index) }.joined(separator: "\t")
        }
        return out
    }

    private func rowAsJSON(_ row: Int) -> String? {
        guard let pager, let win = pager.window(for: UInt64(row)) else { return nil }
        var obj: [String: Any] = [:]
        for (name, idx) in visibleColumnPairs() where idx < win.columns {
            switch win.kind(absoluteRow: UInt64(row), col: idx) {
            case .null: obj[name] = NSNull()
            case .absent: continue  // ABSENT means the key is NOT in the object
            case .nested:
                let raw = win.detailJSON(absoluteRow: UInt64(row), col: idx) ?? "null"
                obj[name] =
                    (try? JSONSerialization.jsonObject(
                        with: Data(raw.utf8), options: [.fragmentsAllowed])) ?? raw
            case .value: obj[name] = win.text(absoluteRow: UInt64(row), col: idx)
            }
        }
        guard
            let data = try? JSONSerialization.data(
                withJSONObject: obj, options: [.prettyPrinted, .sortedKeys])
        else { return nil }
        return String(data: data, encoding: .utf8)
    }

    private func openFocusedCell() {
        guard let (row, colPos) = tableView.focusedCell, colPos < tableView.tableColumns.count,
            let idx = columnIndexByID[tableView.tableColumns[colPos].identifier],
            let pager, let win = pager.window(for: UInt64(row))
        else { return }
        onNestedCell?(row, idx, win)
    }

    private func copyToPasteboard(_ text: String?, label: String) {
        guard let text, !text.isEmpty else { return }
        NSPasteboard.general.clearContents()
        NSPasteboard.general.setString(text, forType: .string)
        onCopied?(label)
    }

    // MARK: - context menu

    /// Built fresh on every right-click from `clickedRow`/`clickedColumn`, so
    /// the items always name the cell actually under the pointer.
    func menuNeedsUpdate(_ menu: NSMenu) {
        menu.removeAllItems()
        let row = tableView.clickedRow
        let colPos = tableView.clickedColumn
        guard row >= 0, colPos >= 0, colPos < tableView.tableColumns.count else { return }
        let column = tableView.tableColumns[colPos]
        guard let idx = columnIndexByID[column.identifier], let pager,
            let win = pager.window(for: UInt64(row))
        else { return }

        let kind = win.kind(absoluteRow: UInt64(row), col: idx)
        let preview = String(displayText(win, row: row, col: idx).prefix(28))

        func item(_ title: String, _ action: Selector) -> NSMenuItem {
            let i = NSMenuItem(title: title, action: action, keyEquivalent: "")
            i.target = self
            i.representedObject = ["row": row, "col": colPos]
            return i
        }

        menu.addItem(item("Copy Cell", #selector(ctxCopyCell(_:))))
        menu.addItem(item("Copy Row as JSON", #selector(ctxCopyRowJSON(_:))))
        menu.addItem(item("Copy Row as TSV", #selector(ctxCopyRowTSV(_:))))
        menu.addItem(item("Copy Column “\(column.title)” (visible rows)", #selector(ctxCopyColumn(_:))))
        menu.addItem(.separator())
        if kind == .value || kind == .null {
            let f = item("Filter by “\(preview)”", #selector(ctxFilter(_:)))
            menu.addItem(f)
        }
        menu.addItem(item("Sort by “\(column.title)”", #selector(ctxSort(_:))))
        menu.addItem(.separator())
        menu.addItem(item("Open in Inspector", #selector(ctxInspect(_:))))
    }

    private func ctx(_ sender: Any?) -> (row: Int, colPos: Int, idx: UInt32, win: RowWindow)? {
        guard let item = sender as? NSMenuItem,
            let d = item.representedObject as? [String: Int],
            let row = d["row"], let colPos = d["col"],
            colPos < tableView.tableColumns.count,
            let idx = columnIndexByID[tableView.tableColumns[colPos].identifier],
            let pager, let win = pager.window(for: UInt64(row))
        else { return nil }
        return (row, colPos, idx, win)
    }

    @objc private func ctxCopyCell(_ sender: Any?) {
        guard let c = ctx(sender) else { return }
        copyToPasteboard(rawText(c.win, row: c.row, col: c.idx), label: "cell copied")
    }

    @objc private func ctxCopyRowJSON(_ sender: Any?) {
        guard let c = ctx(sender) else { return }
        copyToPasteboard(rowAsJSON(c.row), label: "row copied as JSON")
    }

    @objc private func ctxCopyRowTSV(_ sender: Any?) {
        guard let c = ctx(sender) else { return }
        let cols = visibleColumnPairs()
        let line = cols.map { rawText(c.win, row: c.row, col: $0.index) }.joined(separator: "\t")
        copyToPasteboard(
            cols.map(\.name).joined(separator: "\t") + "\n" + line, label: "row copied as TSV")
    }

    /// Visible rows only, and the menu item says so. Copying a column of a
    /// 500 000-row result would materialise every page in the store.
    @objc private func ctxCopyColumn(_ sender: Any?) {
        guard let c = ctx(sender), let pager else { return }
        let visible = tableView.rows(in: tableView.visibleRect)
        guard visible.length > 0 else { return }
        var lines: [String] = [tableView.tableColumns[c.colPos].title]
        for r in visible.lowerBound..<visible.upperBound {
            guard let win = pager.window(for: UInt64(r)) else { continue }
            lines.append(rawText(win, row: r, col: c.idx))
        }
        copyToPasteboard(lines.joined(separator: "\n"), label: "\(lines.count - 1) cells copied")
    }

    @objc private func ctxFilter(_ sender: Any?) {
        guard let c = ctx(sender) else { return }
        onFilterRequested?(
            tableView.tableColumns[c.colPos].title, rawText(c.win, row: c.row, col: c.idx))
    }

    @objc private func ctxSort(_ sender: Any?) {
        guard let c = ctx(sender) else { return }
        onSortRequested?(tableView.tableColumns[c.colPos].title)
    }

    @objc private func ctxInspect(_ sender: Any?) {
        guard let c = ctx(sender) else { return }
        onNestedCell?(c.row, c.idx, c.win)
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
