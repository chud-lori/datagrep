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
    var onCopy: (() -> Void)?
    var onOpenFocusedCell: (() -> Void)?
    var onFocusChanged: (() -> Void)?

    private(set) var hoverRow = -1
    /// The rectangular block of cells the user has selected. `nil` until the
    /// pointer or the keyboard has touched a cell.
    private(set) var cellRange: GridCellRange?
    /// Set while WE are the ones changing the row selection, so the
    /// selection-did-change hook does not fight the block it just applied.
    private var isSyncingSelection = false
    private var tracking: NSTrackingArea?

    /// The block is only a *block* while the table's row selection is exactly
    /// its rows. A ⌘-click that punches a hole in the selection makes it
    /// discontiguous, and then the honest rendering is plain whole-row.
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

    /// The pointer can also change which row it is over without moving at all —
    /// the wheel moves the rows under a stationary pointer. This is driven by
    /// the scroll notification, not by a timer, so a still grid posts nothing.
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

    // MARK: - selection plumbing

    /// The single funnel every selection change goes through: clamp, mirror the
    /// block onto the table's own row selection, repaint the visible rows.
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

    /// Called when AppKit changed the row selection behind our back — arrow
    /// up/down, shift-arrow, ⌘-click. The column span of the block is kept and
    /// the rows are re-derived from whatever AppKit decided.
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

    /// Gutter click: select the whole row. Routed through the SAME block model
    /// as every other selection gesture — anchor on the first column, focus on
    /// the last, which `spansAllColumns` then renders as a plain row highlight.
    /// No parallel selection path exists for the gutter to diverge from.
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

    /// Only the rows on screen are ever decorated: the block may be 20 000 rows
    /// tall, but at most a viewport of row views exists to paint it.
    func refreshSelectionDecorations() {
        // The gutter emphasises selected row numbers, so it repaints with the
        // rows. One small strip, redrawn only on selection changes — never idle.
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

    /// Pushes the block's shape onto one row view. Also called from
    /// `rowViewForRow` so a row scrolled into view arrives already correct.
    func decorate(_ rowView: GridRowView, row: Int) {
        rowView.isHovered = (row == hoverRow)
        guard let r = cellRange, selectionMatchesRange, r.rows.contains(row) else {
            rowView.rangeColumns = nil
            rowView.isRangeTop = false
            rowView.isRangeBottom = false
            rowView.focusedColumn = -1
            return
        }
        // A block that covers every column IS a row selection; drawing a border
        // around it would only add noise.
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
        // Double-click (inspector), ⌘-click (discontiguous row selection) and
        // control-click (context menu) stay with AppKit; everything else is a
        // block gesture.
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

    /// A hand-rolled tracking loop rather than `super.mouseDown`, because
    /// AppKit's own loop only ever reports rows and we need the column too.
    /// It idles on a 50 ms `nextEvent` timeout so that holding the pointer
    /// still outside the viewport keeps autoscrolling, and holding it still
    /// inside costs nothing at all.
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

    /// Point -> cell, clamped rather than nil'd: during an autoscroll drag the
    /// pointer is by definition outside the rows, and the block must still
    /// follow it to the first/last row and column.
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

    /// Right-click outside the current block re-targets it, so "Copy Selection"
    /// can never mean something other than what is highlighted.
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

    /// ⌘A means "the rows I can see", never "all 500 000 rows": a select-all
    /// that spanned the whole result would turn the next ⌘C into a full-table
    /// scan through the FFI, which is precisely the thing this grid does not do.
    override func selectAll(_ sender: Any?) {
        let vis = rows(in: visibleRect)
        guard vis.length > 0, numberOfColumns > 0 else { return }
        var range = GridCellRange(row: vis.lowerBound, column: 0)
        range.extend(toRow: vis.lowerBound + vis.length - 1, column: numberOfColumns - 1)
        apply(range, scroll: false)
    }

    // MARK: copy

    /// ⌘C. Routed here by the Edit menu's `copy:` because the table is first
    /// responder — no extra key handling needed.
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

/// The results grid. Virtual by construction:
///   - `numberOfRows` returns the full row count the core reports
///   - `viewFor row:` pulls ONLY that row, out of a 512-row page window
///   - at most 4 pages (2,048 rows) are resident; eviction frees the DatagrepRows
final class ResultsViewController: NSViewController, NSTableViewDataSource, NSTableViewDelegate,
    NSMenuDelegate
{
    let tableView = GridTableView()
    private let scrollView = NSScrollView()
    private let rowNumberRuler = GridRowNumberRuler(scrollView: nil, orientation: .verticalRuler)

    private(set) var pager: RowPager?
    private var rowCount: Int = 0
    private var columnIndexByID: [NSUserInterfaceItemIdentifier: UInt32] = [:]
    private var columnByName: [String: NSTableColumn] = [:]
    /// Numeric columns render right-aligned; text left-aligned.
    private var rightAlignedByID: [NSUserInterfaceItemIdentifier: Bool] = [:]
    private var columnNames: [String] = []
    /// A 400-path Mongo collection must not become a 400-column grid. Columns
    /// beyond this are created but hidden.
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
    /// Fires whenever the selected block changes: "3 rows × 2 columns", or nil
    /// when nothing is selected. Purely informational — a status-bar hook.
    var onSelectionChanged: ((String?) -> Void)?
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
        // Own a layer, and fill it from draw(). Inside SwiftUI's NSHostingView
        // the table had `wantsLayer == false` while its ancestors were
        // layer-backed, so its draw() output — which a PDF capture proved was
        // correct — never reached any on-screen layer: the pane showed its own
        // background straight through the unpopulated table. Making the table
        // and its scroll view layer-backed, with the redraw policy that repaints
        // the layer on `needsDisplay`, is what puts draw() on the screen.
        for v in [tableView, scrollView, rowNumberRuler] as [NSView] {
            v.wantsLayer = true
            v.layerContentsRedrawPolicy = .onSetNeedsDisplay
        }
        tableView.canDrawSubviewsIntoLayer = true
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

        // The row-number gutter. A vertical RULER, not a table column, which is
        // what pins it against horizontal scrolling and keeps it out of every
        // copy path (those enumerate `tableColumns` only) — see
        // GridRowNumberRuler for the full rationale.
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
        self.view = root
    }

    // MARK: - result lifecycle

    /// A brand new result: everything about the previous one is dropped.
    func beginNewResult(pager: RowPager) {
        self.pager?.invalidateAll()
        self.pager = pager
        rowCount = 0
        rowNumberRuler.update(rowCount: 0)
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
        tableView.resetSelection()
        onSelectionChanged?(nil)
        for c in tableView.tableColumns { tableView.removeTableColumn(c) }
        columnIndexByID.removeAll()
        columnByName.removeAll()
        rightAlignedByID.removeAll()
        columnNames.removeAll()
        tableView.reloadData()
    }

    /// Applies a status snapshot. Columns only ever APPEND on the right; an
    /// existing column is never moved, renamed or re-sized by a schema delta —
    /// columns jumping mid-scroll is the failure mode.
    func apply(status: QueryStatus) {
        applySchema(status.columns)
        let newCount = Int(status.rowsLoaded)
        let grew = newCount != rowCount
        rowCount = newCount
        // Gutter width follows the magnitude of the row count (1,000,000 must
        // not clip). Derived from the count alone — no row fetch involved.
        if grew { rowNumberRuler.update(rowCount: newCount) }
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
        // Put the pixels on screen, on the NEXT runloop turn.
        //
        // This runs during the SwiftUI update that delivered the result, and a
        // display forced inside that pass is thrown away — which is the whole
        // bug: the cells drew correctly (a capture showed the full grid, the
        // cell layers held the right content) but the pane stayed blank until
        // something later — a window resize — triggered a real redraw. Doing it
        // one hop later is exactly what the resize did, and it sticks.
        DispatchQueue.main.async { [weak self] in
            guard let self else { return }
            self.tableView.reloadData()
            // Each row draws its cells into its OWN layer, and that layer was
            // rendered before the data arrived — so it held a blank frame while
            // the cell sublayers underneath it had the real text. Re-render
            // every visible row so its layer picks up the cells it now has.
            self.tableView.enumerateAvailableRowViews { rowView, _ in
                rowView.needsDisplay = true
                for cell in rowView.subviews { cell.needsDisplay = true }
            }
            self.tableView.displayIfNeeded()
            self.tableView.headerView?.displayIfNeeded()
            self.scrollView.displayIfNeeded()
            self.rowNumberRuler.needsDisplay = true
        }
        applySortIndicator()
        tableView.refreshSelectionDecorations()
    }

    /// Repaint the whole grid now. Called from the representable's SwiftUI
    /// update so the hosting layer's snapshot is refreshed when a result lands.
    func forceRedraw() {
        tableView.reloadData()
        tableView.needsDisplay = true
        tableView.headerView?.needsDisplay = true
        scrollView.needsDisplay = true
        rowNumberRuler.needsDisplay = true
        view.displayIfNeeded()
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
        // Re-tile after changing widths. NSTableView sizes its own frame from
        // the sum of its column widths, and setting `col.width` directly does
        // not always trigger that — the row views and the header were laid out
        // across the true 3930 pt while the table's frame still claimed 1950,
        // so rows were drawn into a coordinate space the view did not admit to
        // having and the horizontal scroller could not reach them.
        tableView.tile()
        // Existing row views were built at the old width and do not re-tile with
        // the table, so they stay wider than the table admits to being. Rebuild
        // them, then put the viewport back at the first column — otherwise the
        // first thing shown is somewhere in the middle of a wide result, which
        // reads as "no results" when the visible slice happens to be past the
        // data.
        tableView.reloadData()
        tableView.scrollRowToVisible(0)
        // Vertically only. The clip view's resting x is -ruleThickness when a
        // vertical ruler is installed — that negative origin is the gutter's
        // space, not a scroll offset. Forcing x to 0 slid the first column left
        // underneath the ruler, which is why its text came out clipped ("lo"
        // instead of "hello") and the first value looked missing entirely.
        //
        // The clip view still has to be re-reflected, though: that is what keeps
        // the ruler in step, and without it the row numbers stop being drawn.
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
        // A row arriving from the reuse pool is decorated before it is ever
        // drawn, so scrolling through a selection never flashes an unstyled row.
        self.tableView.decorate(v, row: row)
        return v
    }

    func tableViewSelectionDidChange(_ notification: Notification) {
        tableView.selectionChangedExternally()
    }

    /// Reordering changes every cell's column POSITION, which is what the block
    /// highlight is expressed in. The engine column index each cell reads from
    /// is unaffected — it is resolved by identifier — so this is repaint-only.
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

        // The block highlight is expressed in column POSITIONS, so each cell
        // carries the position it is currently drawn at as well as the engine
        // column index it reads from.
        let position = tableView.column(withIdentifier: tableColumn.identifier)

        guard let pager, let win = pager.window(for: UInt64(row)), colIndex < win.columns else {
            cell.configure(
                kind: .value, text: "", row: row, column: colIndex, position: position,
                pending: true,
                rightAligned: rightAlignedByID[tableColumn.identifier] ?? false)
            cell.onNestedClick = nil
            // Shimmer only while rows are genuinely still arriving; a terminal
            // query leaves a completely static window, within the idle budget.
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
            kind: kind, text: text, row: row, column: colIndex, position: position, pending: false,
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

    /// The columns inside a block, in display order, skipping hidden ones — a
    /// block drawn across a hidden column must not paste that column's data.
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

    /// What ⌘C and "Copy Selection as TSV" put on the pasteboard.
    ///
    /// A single cell copies bare (a header line above one value is noise);
    /// anything larger copies as TSV with a header row. Row count is capped:
    /// a drag with autoscroll can cover more rows than it is sane to pull back
    /// through the FFI one page at a time.
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

    /// Built fresh on every right-click from `clickedRow`/`clickedColumn`, so
    /// the items always name the cell actually under the pointer.
    func menuNeedsUpdate(_ menu: NSMenu) {
        menu.removeAllItems()
        // `clickedRow` is the cell actually under the pointer; the focused cell
        // is the fallback for a menu opened from the keyboard.
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
}

/// SwiftUI bridge for the grid. SwiftUI's own `Table`/`List` are NOT used: they
/// do not virtualise predictably at a million rows, which is the entire product
/// claim. `makeNSViewController` returns the model-owned controller so SwiftUI
/// re-evaluation can never rebuild the table or drop the page cache.
struct ResultsGridView: NSViewControllerRepresentable {
    let controller: ResultsViewController
    /// Changes every time a result is applied. SwiftUI's `PlatformViewHost`
    /// snapshots the hosted AppKit view's layer and only re-snapshots when this
    /// representable's `body`/`update` runs — and the result data flows through
    /// the controller, not through SwiftUI state, so without a value SwiftUI can
    /// see change, the stale (empty) snapshot stayed on screen until a resize
    /// forced a re-layout. Reading `generation` here is what makes the update
    /// fire, and re-displaying the table in it is what refreshes the snapshot.
    let generation: Int
    func makeNSViewController(context: Context) -> ResultsViewController { controller }
    func updateNSViewController(_ nsViewController: ResultsViewController, context: Context) {
        _ = generation
        nsViewController.forceRedraw()
    }

    /// Take the size offered, never the table's own.
    ///
    /// Same trap the editor pane fell into: without this SwiftUI sizes the
    /// representable from the controller view's Auto Layout fitting size, which
    /// for an `NSTableView` grows with the columns and rows in it. A result
    /// with two dozen columns laid the grid out far wider and taller than the
    /// pane, so the rows sat outside the visible area and the pane looked empty
    /// even though the query had returned. The split and the window decide this
    /// pane's size; the result set does not.
    func sizeThatFits(
        _ proposal: ProposedViewSize,
        nsViewController: ResultsViewController,
        context: Context
    ) -> CGSize? {
        // Never `return nil` on an unspecified proposal. SwiftUI proposes nil on
        // some passes to ask "what size do you want?", and nil hands the answer
        // back to AppKit's fitting size — the very thing this exists to ignore.
        // One such pass is enough to blow the pane up again, which looked like
        // the rows drawing and then vanishing a frame later.
        CGSize(width: proposal.width ?? 10, height: proposal.height ?? 10)
    }
}
