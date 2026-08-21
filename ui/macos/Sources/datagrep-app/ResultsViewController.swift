import AppKit
import DatagrepKit
import SwiftUI

// MARK: - the table view itself

/// `NSTableView` with a pointer and a keyboard.
final class GridTableView: NSTableView {
    var onCopy: (() -> Void)?
    var onOpenFocusedCell: (() -> Void)?
    var onFocusChanged: (() -> Void)?
    var onBeginEdit: (() -> Void)?

    private(set) var hoverRow = -1
    private(set) var cellRange: GridCellRange?
    private var isSyncingSelection = false
    private var tracking: NSTrackingArea?

    var selectionMatchesRange: Bool {
        guard let r = cellRange else { return false }
        let sel = selectedRowIndexes
        return sel.count == r.rowCount && sel.contains(integersIn: r.rows)
    }

    var focusedColumn: Int { cellRange?.focusColumn ?? 0 }

    var focusedCell: (row: Int, column: Int)? {
        guard let r = cellRange, r.focusRow < numberOfRows, r.focusColumn < numberOfColumns else {
            return nil
        }
        return (r.focusRow, r.focusColumn)
    }

    func resetSelection() {
        cellRange = nil
        isSyncingSelection = true
        deselectAll(nil)
        isSyncingSelection = false
    }

    // MARK: - hover

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

    override func viewDidMoveToWindow() {
        super.viewDidMoveToWindow()
        NotificationCenter.default.removeObserver(
            self, name: NSScrollView.didLiveScrollNotification, object: nil)
        guard let clip = enclosingScrollView else { return }
        NotificationCenter.default.addObserver(
            self, selector: #selector(liveScrolled), name: NSScrollView.didLiveScrollNotification,
            object: clip)
    }

    @objc private func liveScrolled() {
        guard let window, window.isKeyWindow else { return }
        let p = convert(window.mouseLocationOutsideOfEventStream, from: nil)
        setHover(visibleRect.contains(p) ? row(at: p) : -1)
    }

    private func setHover(_ newRow: Int) {
        guard newRow != hoverRow else { return }
        let old = hoverRow
        hoverRow = newRow
        for r in [old, newRow] where r >= 0 && r < numberOfRows {
            (rowView(atRow: r, makeIfNecessary: false) as? GridRowView)?.isHovered = (r == hoverRow)
        }
    }

    // MARK: - selection plumbing

    private func apply(_ range: GridCellRange, scroll: Bool = true) {
        guard let r = range.clamped(rowCount: numberOfRows, columnCount: numberOfColumns) else {
            return
        }
        cellRange = r
        isSyncingSelection = true
        selectRowIndexes(
            IndexSet(integersIn: r.rows.lowerBound...r.rows.upperBound), byExtendingSelection: false)
        isSyncingSelection = false
        if scroll {
            scrollRowToVisible(r.focusRow)
            if r.focusColumn < numberOfColumns { scrollColumnToVisible(r.focusColumn) }
        }
        refreshSelectionDecorations()
        onFocusChanged?()
    }

    func selectionChangedExternally() {
        guard !isSyncingSelection else {
            refreshSelectionDecorations()
            return
        }
        let sel = selectedRowIndexes
        guard let first = sel.first, let last = sel.last else {
            cellRange = nil
            refreshSelectionDecorations()
            return
        }
        if var r = cellRange {
            if sel.count == 1 {
                r.anchorRow = first
                r.focusRow = first
            } else if r.anchorRow == first {
                r.focusRow = last
            } else if r.anchorRow == last {
                r.focusRow = first
            } else {
                r.anchorRow = first
                r.focusRow = last
            }
            cellRange = r
        } else {
            var r = GridCellRange(row: first, column: 0)
            r.extend(toRow: last, column: max(0, numberOfColumns - 1))
            cellRange = r
        }
        refreshSelectionDecorations()
        onFocusChanged?()
    }

    func selectWholeRow(_ row: Int, extend: Bool) {
        guard numberOfRows > 0, numberOfColumns > 0 else { return }
        let r = max(0, min(numberOfRows - 1, row))
        window?.makeFirstResponder(self)
        var range: GridCellRange
        if extend, let existing = cellRange {
            range = existing
            range.anchorColumn = 0
            range.extend(toRow: r, column: numberOfColumns - 1)
        } else {
            range = GridCellRange(row: r, column: 0)
            range.extend(toRow: r, column: numberOfColumns - 1)
        }
        apply(range, scroll: false)
    }

    func refreshSelectionDecorations() {
        enclosingScrollView?.verticalRulerView?.needsDisplay = true
        let vis = rows(in: visibleRect)
        guard vis.length > 0 else { return }
        for r in vis.lowerBound..<vis.upperBound {
            guard let rv = rowView(atRow: r, makeIfNecessary: false) as? GridRowView else {
                continue
            }
            decorate(rv, row: r)
        }
    }

    func decorate(_ rowView: GridRowView, row: Int) {
        rowView.isHovered = (row == hoverRow)
        guard let r = cellRange, selectionMatchesRange, r.rows.contains(row) else {
            rowView.rangeColumns = nil
            rowView.isRangeTop = false
            rowView.isRangeBottom = false
            rowView.focusedColumn = -1
            return
        }
        rowView.rangeColumns = r.spansAllColumns(of: numberOfColumns) ? nil : r.columns
        rowView.isRangeTop = (row == r.rows.lowerBound)
        rowView.isRangeBottom = (row == r.rows.upperBound)
        rowView.focusedColumn = (r.isSingleCell && row == r.focusRow) ? r.focusColumn : -1
    }

    private func visibleColumnPositions() -> [Int] {
        (0..<numberOfColumns).filter { !tableColumns[$0].isHidden }
    }

    // MARK: - mouse: click, shift-click, drag-select with autoscroll

    override func mouseDown(with event: NSEvent) {
        let p = convert(event.locationInWindow, from: nil)
        let r = row(at: p)
        let c = column(at: p)
        let flags = event.modifierFlags.intersection(.deviceIndependentFlagsMask)
        guard r >= 0, c >= 0, event.clickCount == 1,
            !flags.contains(.command), !flags.contains(.control)
        else {
            super.mouseDown(with: event)
            return
        }
        window?.makeFirstResponder(self)
        var range = cellRange ?? GridCellRange(row: r, column: c)
        if flags.contains(.shift), cellRange != nil {
            range.extend(toRow: r, column: c)
        } else {
            range.moveTo(row: r, column: c)
        }
        apply(range, scroll: false)
        trackDrag(startingFrom: event, range: range)
    }

    private func trackDrag(startingFrom initial: NSEvent, range initialRange: GridCellRange) {
        guard let window else { return }
        var range = initialRange
        var latest = initial
        var didDrag = false
        while true {
            let e = window.nextEvent(
                matching: [.leftMouseDragged, .leftMouseUp],
                until: Date(timeIntervalSinceNow: 0.05), inMode: .eventTracking, dequeue: true)
            if let e {
                if e.type == .leftMouseUp { break }
                latest = e
                didDrag = true
            } else if !didDrag {
                continue  // button held, pointer never moved: nothing to do
            }
            let p = convert(latest.locationInWindow, from: nil)
            let outside = !visibleRect.contains(p)
            if outside { autoscroll(with: latest) }
            if e == nil && !outside { continue }
            let target = cell(at: convert(latest.locationInWindow, from: nil))
            var next = range
            next.extend(toRow: target.row, column: target.column)
            guard next != range else { continue }
            range = next
            setHover(target.row)
            apply(range, scroll: false)
        }
    }

    private func cell(at p: NSPoint) -> (row: Int, column: Int) {
        let step = rowHeight + intercellSpacing.height
        var r = step > 0 ? Int(floor(p.y / step)) : 0
        r = max(0, min(max(numberOfRows - 1, 0), r))
        var c = column(at: p)
        if c < 0 {
            let positions = visibleColumnPositions()
            c = p.x <= 0 ? (positions.first ?? 0) : (positions.last ?? 0)
        }
        return (r, c)
    }

    override func menu(for event: NSEvent) -> NSMenu? {
        let p = convert(event.locationInWindow, from: nil)
        let r = row(at: p)
        let c = column(at: p)
        if r >= 0, c >= 0 {
            let inside =
                selectedRowIndexes.contains(r) && (cellRange?.columns.contains(c) ?? false)
                && selectionMatchesRange
            if !inside {
                var range = cellRange ?? GridCellRange(row: r, column: c)
                range.moveTo(row: r, column: c)
                apply(range, scroll: false)
            }
        }
        return super.menu(for: event)
    }

    // MARK: - keyboard

    override func keyDown(with event: NSEvent) {
        let flags = event.modifierFlags
        let cmd = flags.contains(.command)
        let shift = flags.contains(.shift)
        switch event.keyCode {
        case 123:  // left
            moveHorizontally(by: -1, extend: shift, toEdge: cmd, wrap: false)
        case 124:  // right
            moveHorizontally(by: +1, extend: shift, toEdge: cmd, wrap: false)
        case 48:  // tab / shift-tab
            moveHorizontally(by: shift ? -1 : +1, extend: false, toEdge: false, wrap: true)
        case 126 where cmd, 115:  // ⌘↑ / home -> first loaded row
            jump(to: 0, extend: shift)
        case 125 where cmd, 119:  // ⌘↓ / end -> last loaded row
            jump(to: numberOfRows - 1, extend: shift)
        case 36:  // return -> edit the focused cell
            onBeginEdit?()
        case 49:  // space -> inspect the focused cell
            onOpenFocusedCell?()
        case 53:  // escape -> collapse the block back to its focused cell
            if let r = cellRange, !r.isSingleCell {
                var c = r
                c.moveTo(row: r.focusRow, column: r.focusColumn)
                apply(c)
            } else {
                super.keyDown(with: event)
            }
        default:
            super.keyDown(with: event)
        }
    }

    private func moveHorizontally(by delta: Int, extend: Bool, toEdge: Bool, wrap: Bool) {
        let positions = visibleColumnPositions()
        guard !positions.isEmpty, numberOfRows > 0 else { return }
        var range = cellRange ?? GridCellRange(row: max(0, selectedRow), column: positions[0])
        let current = positions.firstIndex(of: range.focusColumn) ?? 0
        var index = toEdge ? (delta < 0 ? 0 : positions.count - 1) : current + delta
        var row = range.focusRow
        if wrap {
            if index < 0 {
                index = positions.count - 1
                row = max(0, row - 1)
            } else if index >= positions.count {
                index = 0
                row = min(numberOfRows - 1, row + 1)
            }
        } else {
            index = max(0, min(positions.count - 1, index))
        }
        if extend {
            range.extend(toRow: row, column: positions[index])
        } else {
            range.moveTo(row: row, column: positions[index])
        }
        apply(range)
    }

    private func jump(to row: Int, extend: Bool) {
        guard row >= 0, row < numberOfRows else { return }
        let positions = visibleColumnPositions()
        var range = cellRange ?? GridCellRange(row: row, column: positions.first ?? 0)
        if extend {
            range.extend(toRow: row, column: range.focusColumn)
        } else {
            range.moveTo(row: row, column: range.focusColumn)
        }
        apply(range)
    }

    override func selectAll(_ sender: Any?) {
        let vis = rows(in: visibleRect)
        guard vis.length > 0, numberOfColumns > 0 else { return }
        var range = GridCellRange(row: vis.lowerBound, column: 0)
        range.extend(toRow: vis.lowerBound + vis.length - 1, column: numberOfColumns - 1)
        apply(range, scroll: false)
    }

    // MARK: copy

    @objc func copy(_ sender: Any?) {
        onCopy?()
    }

    override func validateUserInterfaceItem(_ item: NSValidatedUserInterfaceItem) -> Bool {
        if item.action == #selector(copy(_:)) { return selectedRowIndexes.count > 0 }
        if item.action == #selector(selectAll(_:)) { return numberOfRows > 0 }
        return super.validateUserInterfaceItem(item)
    }

    override var acceptsFirstResponder: Bool { true }
}

// MARK: - controller

final class ResultsViewController: NSViewController, NSTableViewDataSource, NSTableViewDelegate,
    NSMenuDelegate
{
    let tableView = GridTableView()
    private let scrollView = NSScrollView()
    private let rowNumberRuler = GridRowNumberRuler(scrollView: nil, orientation: .verticalRuler)
    private let gutterHeader = GridGutterHeader()
    private var gutterHeaderWidth: NSLayoutConstraint?

    private(set) var pager: RowPager?
    private var rowCount: Int = 0
    private var columnIndexByID: [NSUserInterfaceItemIdentifier: UInt32] = [:]
    private var columnByName: [String: NSTableColumn] = [:]
    /// Numeric columns render right-aligned; text left-aligned.
    private var rightAlignedByID: [NSUserInterfaceItemIdentifier: Bool] = [:]
    private var columnNames: [String] = []
    let maxVisibleColumns = 30
    private(set) var hiddenColumnCount = 0
    private var didSizeColumns = false
    private var isStreaming = false
    private var resultIsCapped = false

    private(set) var editable: EditableResult?
    var edits: PendingEdits?

    var onNestedCell: ((Int, UInt32, RowWindow) -> Void)?
    var onEditRefused: ((String) -> Void)?
    /// Something was staged or discarded — the commit bar redraws from this.
    var onStagingChanged: (() -> Void)?
    var onHiddenColumnsChanged: ((Int) -> Void)?
    var onSortRequested: ((String) -> Void)?
    var onFilterRequested: ((String, String) -> Void)?
    var onCopied: ((String) -> Void)?
    var onSelectionChanged: ((String?) -> Void)?
    var sortColumn: String?
    var sortAscending = true
    var allowsEditing = true

    override func loadView() {
        let root = NSView()

        tableView.dataSource = self
        tableView.delegate = self
        tableView.rowHeight = GridStyle.rowHeight
        tableView.usesAlternatingRowBackgroundColors = true
        tableView.columnAutoresizingStyle = .noColumnAutoresizing
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

        tableView.onCopy = { [weak self] in self?.copySelection() }
        tableView.onOpenFocusedCell = { [weak self] in self?.openFocusedCell() }
        tableView.onBeginEdit = { [weak self] in self?.beginEditingFocusedCell() }
        tableView.onFocusChanged = { [weak self] in
            guard let self else { return }
            self.onSelectionChanged?(self.selectionSummary())
        }
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

        rowNumberRuler.grid = tableView
        rowNumberRuler.clientView = tableView
        rowNumberRuler.onSelectRow = { [weak self] row, extend in
            self?.tableView.selectWholeRow(row, extend: extend)
        }
        scrollView.hasVerticalRuler = true
        scrollView.verticalRulerView = rowNumberRuler
        scrollView.rulersVisible = true

        root.addSubview(scrollView)
        NSLayoutConstraint.activate([
            scrollView.topAnchor.constraint(equalTo: root.topAnchor),
            scrollView.bottomAnchor.constraint(equalTo: root.bottomAnchor),
            scrollView.leadingAnchor.constraint(equalTo: root.leadingAnchor),
            scrollView.trailingAnchor.constraint(equalTo: root.trailingAnchor),
        ])

        gutterHeader.translatesAutoresizingMaskIntoConstraints = false
        gutterHeader.onSelectAll = { [weak self] in self?.tableView.selectAll(nil) }
        root.addSubview(gutterHeader)
        let gw = gutterHeader.widthAnchor.constraint(equalToConstant: rowNumberRuler.requiredThickness)
        gutterHeaderWidth = gw
        NSLayoutConstraint.activate([
            gutterHeader.topAnchor.constraint(equalTo: root.topAnchor),
            gutterHeader.leadingAnchor.constraint(equalTo: root.leadingAnchor),
            gutterHeader.heightAnchor.constraint(equalToConstant: GridStyle.headerHeight),
            gw,
        ])
        self.view = root
    }

    /// Keep the gutter header the same width as the ruler below it.
    private func syncGutterHeaderWidth() {
        gutterHeaderWidth?.constant = rowNumberRuler.requiredThickness
    }

    // MARK: - result lifecycle

    /// A brand new result: everything about the previous one is dropped.
    func beginNewResult(pager: RowPager) {
        self.pager?.invalidateAll()
        self.pager = pager
        editable = nil
        rowCount = 0
        rowNumberRuler.update(rowCount: 0)
        syncGutterHeaderWidth()
        tableView.resetSelection()
        onSelectionChanged?(nil)
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
        rowNumberRuler.update(rowCount: 0)
        syncGutterHeaderWidth()
        tableView.resetSelection()
        onSelectionChanged?(nil)
        for c in tableView.tableColumns { tableView.removeTableColumn(c) }
        columnIndexByID.removeAll()
        columnByName.removeAll()
        rightAlignedByID.removeAll()
        columnNames.removeAll()
        tableView.reloadData()
    }

    func apply(status: QueryStatus) {
        editable = allowsEditing ? status.editable : nil
        applySchema(status.columns)
        let newCount = Int(status.rowsLoaded)
        let grew = newCount != rowCount
        rowCount = newCount
        if grew { rowNumberRuler.update(rowCount: newCount); syncGutterHeaderWidth() }
        isStreaming = !status.state.isTerminal
        resultIsCapped = status.state == .capped

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
        // Put the pixels on screen, on the NEXT runloop turn.
        DispatchQueue.main.async { [weak self] in self?.flushToScreen() }
        applySortIndicator()
        tableView.refreshSelectionDecorations()
    }

    func forceRedraw() {
        tableView.reloadData()
        invalidateGrid()
        DispatchQueue.main.async { [weak self] in self?.flushToScreen() }
    }

    private func invalidateGrid() {
        tableView.needsDisplay = true
        tableView.headerView?.needsDisplay = true
        scrollView.needsDisplay = true
        rowNumberRuler.needsDisplay = true
    }

    func flushToScreen() {
        invalidateGrid()
        view.displayIfNeeded()
        CATransaction.flush()
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
        tableView.tile()
        tableView.reloadData()
        tableView.scrollRowToVisible(0)
        scrollView.reflectScrolledClipView(scrollView.contentView)
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
        let v: GridRowView
        if let reused = tableView.makeView(withIdentifier: id, owner: nil) as? GridRowView {
            v = reused
        } else {
            v = GridRowView()
            v.identifier = id
        }
        self.tableView.decorate(v, row: row)
        return v
    }

    func tableViewSelectionDidChange(_ notification: Notification) {
        tableView.selectionChangedExternally()
    }

    func tableViewColumnDidMove(_ notification: Notification) {
        reloadVisibleRows()
    }

    func tableViewColumnDidResize(_ notification: Notification) {
        tableView.refreshSelectionDecorations()
        let vis = tableView.rows(in: tableView.visibleRect)
        guard vis.length > 0 else { return }
        for r in vis.lowerBound..<vis.upperBound {
            tableView.rowView(atRow: r, makeIfNecessary: false)?.needsDisplay = true
        }
    }

    private func reloadVisibleRows() {
        let vis = tableView.rows(in: tableView.visibleRect)
        guard vis.length > 0, tableView.tableColumns.count > 0 else { return }
        tableView.reloadData(
            forRowIndexes: IndexSet(integersIn: vis.lowerBound..<vis.upperBound),
            columnIndexes: IndexSet(integersIn: 0..<tableView.tableColumns.count))
        tableView.refreshSelectionDecorations()
    }

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

        let position = tableView.column(withIdentifier: tableColumn.identifier)

        guard let pager, let win = pager.window(for: UInt64(row)), colIndex < win.columns else {
            cell.configure(
                kind: .value, text: "", row: row, column: colIndex, position: position,
                pending: true,
                rightAligned: rightAlignedByID[tableColumn.identifier] ?? false)
            cell.onNestedClick = nil
            cell.setShimmer(isStreaming)
            return cell
        }

        let kind = win.kind(absoluteRow: UInt64(row), col: colIndex)
        let text: String
        switch kind {
        case .null, .absent: text = ""
        default: text = win.text(absoluteRow: UInt64(row), col: colIndex)
        }
        let field = fieldName(of: colIndex, in: win)
        cell.configure(
            kind: kind, text: text, row: row, column: colIndex, position: position, pending: false,
            rightAligned: rightAlignedByID[tableColumn.identifier] ?? false,
            editable: editable != nil && kind != .nested,
            staged: field.flatMap { edits?.value(row: row, field: $0) },
            deleted: edits?.isDeleted(row: row) ?? false,
            stagedState: edits?.document(atRow: row)?.state)
        cell.setShimmer(false)
        cell.onEditCommitted = { [weak self] c, typed in
            self?.stageEdit(row: c.row, column: c.column, typed: typed)
        }
        cell.onNestedClick = { [weak self] c in
            guard let self, let p = self.pager, let w = p.window(for: UInt64(c.row)) else { return }
            self.onNestedCell?(c.row, c.column, w)
        }
        return cell
    }

    @objc private func tableDoubleClicked(_ sender: Any?) {
        guard editable != nil else {
            openFocusedCell()
            return
        }
        let row = tableView.clickedRow
        let column = tableView.clickedColumn
        guard row >= 0, column >= 0 else {
            beginEditingFocusedCell()
            return
        }
        beginEditing(row: row, columnPosition: column)
    }

    // MARK: - editing

    /// The field name column `col` was read under, from the window itself.
    private func fieldName(of col: UInt32, in window: RowWindow) -> String? {
        let names = window.columnNames()
        guard col < names.count else { return nil }
        return names[Int(col)]
    }

    /// Begin editing one cell, if it is one that can be edited.
    func beginEditing(row: Int, columnPosition: Int) {
        guard editable != nil, row >= 0, row < rowCount,
            columnPosition >= 0, columnPosition < tableView.tableColumns.count
        else { return }
        let column = tableView.tableColumns[columnPosition]
        guard !column.isHidden,
            let cell = tableView.view(atColumn: columnPosition, row: row, makeIfNecessary: false)
                as? GridCellView
        else { return }
        guard cell.canBeginEditing else {
            if cell.kind == .nested {
                onEditRefused?(
                    "a document or an array is edited in the inspector, not in a grid cell")
            }
            return
        }
        cell.beginEditing()
    }

    func beginEditingFocusedCell() {
        guard let (row, column) = tableView.focusedCell else { return }
        beginEditing(row: row, columnPosition: column)
    }

    /// Stage one typed cell against the document it belongs to.
    private func stageEdit(row: Int, column: UInt32, typed: String) {
        guard let edits, let editable, let pager, let window = pager.window(for: UInt64(row))
        else { return }
        guard let field = fieldName(of: column, in: window) else {
            onEditRefused?("this column is not one of the fields the row was read under")
            return
        }
        let loaded = window.loadedValue(absoluteRow: UInt64(row), col: column)
        if let loaded, loaded.display == typed {
            edits.unstage(row: row, field: field)
            repaint(row)
            onStagingChanged?()
            return
        }
        switch MutationValue.typed(typed, like: loaded) {
        case .failure(let why):
            onEditRefused?("`\(field)`: \(why.message)")
        case .success(let value):
            guard let address = address(row: row, window: window, editable: editable) else { return }
            edits.stage(
                id: address.id, row: row, key: address.key, expect: address.expect,
                field: field, value: value, loaded: loaded)
            repaint(row)
            onStagingChanged?()
        }
    }

    /// Stage the document under `row` for deletion.
    func stageDelete(row: Int) {
        guard let edits, let editable, let pager, let window = pager.window(for: UInt64(row)),
            let address = address(row: row, window: window, editable: editable)
        else { return }
        edits.stageDelete(id: address.id, row: row, key: address.key, expect: address.expect)
        repaint(row)
        onStagingChanged?()
    }

    /// Drop whatever is staged for one row.
    func discardStaged(row: Int) {
        guard let edits, edits.document(atRow: row) != nil else { return }
        edits.discard(row: row)
        repaint(row)
        onStagingChanged?()
    }

    private func address(row: Int, window: RowWindow, editable: EditableResult)
        -> (id: String, key: [(field: String, value: MutationValue)],
            expect: [(field: String, value: MutationValue)])?
    {
        guard let envelope = window.envelope(absoluteRow: UInt64(row)) else {
            onEditRefused?(
                "this row carries no document envelope, so datagrep cannot tell which document it is"
            )
            return nil
        }
        switch editable.address(envelope: envelope) {
        case .failure(let why):
            onEditRefused?(why.message)
            return nil
        case .success(let parts):
            let id = parts.key.map { "\($0.field)=\($0.value.display)" }.joined(separator: "\u{1}")
            return (id, parts.key, parts.expect)
        }
    }

    /// Repaint one row after its staging changed, on the next runloop turn.
    private func repaint(_ row: Int) {
        DispatchQueue.main.async { [weak self] in self?.refreshRow(row) }
    }

    func refreshRow(_ row: Int) {
        guard row >= 0, row < rowCount, tableView.tableColumns.count > 0 else { return }
        tableView.reloadData(
            forRowIndexes: IndexSet(integer: row),
            columnIndexes: IndexSet(integersIn: 0..<tableView.tableColumns.count))
    }

    func refreshStagedRows(_ rows: [Int]) {
        for row in rows { refreshRow(row) }
    }

    // MARK: - cell text helpers

    private func displayText(_ win: RowWindow, row: Int, col: UInt32) -> String {
        switch win.kind(absoluteRow: UInt64(row), col: col) {
        case .null: return "NULL"
        case .absent: return "—"
        default: return win.text(absoluteRow: UInt64(row), col: col)
        }
    }

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

    private func columnPairs(inPositions positions: ClosedRange<Int>)
        -> [(name: String, index: UInt32)]
    {
        var out: [(name: String, index: UInt32)] = []
        for pos in positions where pos >= 0 && pos < tableView.tableColumns.count {
            let col = tableView.tableColumns[pos]
            guard !col.isHidden, let idx = columnIndexByID[col.identifier] else { continue }
            out.append((name: col.title, index: idx))
        }
        return out
    }

    func resultAsAlignedText(maxRows: Int = 5000) -> String {
        guard let pager, rowCount > 0 else { return "" }
        let cols = visibleColumnPairs()
        guard !cols.isEmpty else { return "" }
        let n = min(rowCount, maxRows)

        var widths = cols.map { $0.name.count }
        var body: [[String]] = []
        body.reserveCapacity(n)
        for r in 0..<n {
            guard let win = pager.window(for: UInt64(r)) else { break }
            var cells: [String] = []
            cells.reserveCapacity(cols.count)
            for (i, c) in cols.enumerated() {
                let s = displayText(win, row: r, col: c.index)
                cells.append(s)
                if s.count > widths[i] { widths[i] = s.count }
            }
            body.append(cells)
        }
        func pad(_ s: String, _ w: Int) -> String {
            s + String(repeating: " ", count: max(0, w - s.count))
        }
        func row(_ cells: [String]) -> String {
            cells.enumerated().map { pad($0.element, widths[$0.offset]) }.joined(separator: " | ")
        }
        var lines = [row(cols.map(\.name))]
        lines.append(widths.map { String(repeating: "-", count: $0) }.joined(separator: "-+-"))
        for cells in body { lines.append(row(cells)) }
        if rowCount > n {
            lines.append("")
            lines.append("… \(rowCount - n) more row(s) not shown")
        }
        return lines.joined(separator: "\n")
    }

    /// What ⌘C and "Copy Selection as TSV" put on the pasteboard.
    func selectionAsTSV() -> (text: String, label: String)? {
        guard let pager else { return nil }
        let selected = tableView.selectedRowIndexes
        guard !selected.isEmpty else { return nil }

        let cols: [(name: String, index: UInt32)]
        let rowList: [Int]
        if let range = tableView.cellRange, tableView.selectionMatchesRange {
            cols = columnPairs(inPositions: range.columns)
            if range.isSingleCell, let first = cols.first,
                let win = pager.window(for: UInt64(range.focusRow))
            {
                return (rawText(win, row: range.focusRow, col: first.index), "cell copied")
            }
            rowList = Array(range.rows)
        } else {
            // Discontiguous (⌘-clicked) selection: whole rows, all visible columns.
            cols = visibleColumnPairs()
            rowList = Array(selected)
        }
        guard !cols.isEmpty else { return nil }

        let truncated = rowList.count > GridCopy.maxRows
        let wanted = truncated ? Array(rowList.prefix(GridCopy.maxRows)) : rowList
        var lines: [String] = [cols.map(\.name).joined(separator: "\t")]
        lines.reserveCapacity(wanted.count + 1)
        for r in wanted {
            guard let win = pager.window(for: UInt64(r)) else { continue }
            lines.append(cols.map { rawText(win, row: r, col: $0.index) }.joined(separator: "\t"))
        }
        return (
            lines.joined(separator: "\n"),
            GridCopy.summary(rows: lines.count - 1, columns: cols.count, truncated: truncated)
        )
    }

    private func copySelection() {
        guard let (text, label) = selectionAsTSV() else { return }
        copyToPasteboard(text, label: label)
    }

    /// "3 rows × 2 columns" for the status bar. Nil when nothing is selected.
    func selectionSummary() -> String? {
        guard let range = tableView.cellRange, !tableView.selectedRowIndexes.isEmpty else {
            return nil
        }
        if !tableView.selectionMatchesRange {
            return "\(tableView.selectedRowIndexes.count) rows"
        }
        if range.isSingleCell { return nil }
        let r = range.rowCount == 1 ? "1 row" : "\(range.rowCount) rows"
        let c = range.columnCount == 1 ? "1 column" : "\(range.columnCount) columns"
        return "\(r) × \(c)"
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

    func menuNeedsUpdate(_ menu: NSMenu) {
        menu.removeAllItems()
        let row = tableView.clickedRow >= 0 ? tableView.clickedRow : (tableView.focusedCell?.row ?? -1)
        let colPos =
            tableView.clickedColumn >= 0
            ? tableView.clickedColumn : (tableView.focusedCell?.column ?? -1)
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
        menu.addItem(
            item("Copy Column “\(column.title)” (visible rows)", #selector(ctxCopyColumn(_:))))
        if let summary = selectionSummary() {
            let sel = item("Copy Selection as TSV (\(summary))", #selector(ctxCopySelection(_:)))
            sel.keyEquivalent = "c"
            sel.keyEquivalentModifierMask = [.command]
            menu.addItem(sel)
        }
        menu.addItem(.separator())
        if kind == .value || kind == .null {
            let f = item("Filter by “\(preview)”", #selector(ctxFilter(_:)))
            menu.addItem(f)
        }
        menu.addItem(item("Sort by “\(column.title)”", #selector(ctxSort(_:))))
        menu.addItem(.separator())
        menu.addItem(item("Open in Inspector", #selector(ctxInspect(_:))))

        guard editable != nil else { return }
        menu.addItem(.separator())
        if kind != .nested {
            menu.addItem(item("Edit Cell", #selector(ctxEdit(_:))))
        }
        let staged = edits?.document(atRow: row)
        if staged?.isDelete == true {
            menu.addItem(item("Keep This Document", #selector(ctxDiscardStaged(_:))))
        } else {
            menu.addItem(item("Delete Document", #selector(ctxDelete(_:))))
        }
        if staged != nil {
            menu.addItem(item("Discard Staged Changes", #selector(ctxDiscardStaged(_:))))
        }
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

    @objc private func ctxCopySelection(_ sender: Any?) {
        copySelection()
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

    @objc private func ctxEdit(_ sender: Any?) {
        guard let c = ctx(sender) else { return }
        beginEditing(row: c.row, columnPosition: c.colPos)
    }

    @objc private func ctxDelete(_ sender: Any?) {
        guard let c = ctx(sender) else { return }
        stageDelete(row: c.row)
    }

    @objc private func ctxDiscardStaged(_ sender: Any?) {
        guard let c = ctx(sender) else { return }
        discardStaged(row: c.row)
    }
}

struct ResultsGridView: NSViewControllerRepresentable {
    let controller: ResultsViewController
    let generation: Int
    func makeNSViewController(context: Context) -> ResultsViewController { controller }
    func updateNSViewController(_ nsViewController: ResultsViewController, context: Context) {
        _ = generation
        nsViewController.forceRedraw()
    }

    /// Take the size offered, never the table's own.
    func sizeThatFits(
        _ proposal: ProposedViewSize,
        nsViewController: ResultsViewController,
        context: Context
    ) -> CGSize? {
        CGSize(width: proposal.width ?? 10, height: proposal.height ?? 10)
    }
}
