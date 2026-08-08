import AppKit

/// The pinned row-number gutter (the P0 "row-number column" from the UX study).
///
/// This is an `NSRulerView`, NOT an `NSTableColumn`, and that choice is
/// load-bearing three times over:
///
///  1. **Pinned by construction** — the scroll view tiles a vertical ruler
///     outside the clip view, so horizontal scrolling can never move it.
///  2. **Excluded from copy by construction** — every copy path in the grid
///     (⌘C TSV, row JSON, row TSV, column, selection) enumerates
///     `tableView.tableColumns`; the gutter is not a column, so the row number
///     can never leak onto the pasteboard. The number is chrome, not data.
///  3. **Cannot break virtualisation** — the number is derived purely from the
///     row INDEX. Drawing never touches the pager, so scrolling the gutter
///     costs zero row fetches and zero page-cache churn.
///
/// Idle cost is zero: the ruler redraws only when AppKit invalidates it (the
/// clip view scrolled) or when the grid explicitly pokes it (row count grew,
/// selection changed). No timers, no animations.
final class GridRowNumberRuler: NSRulerView {
    /// The grid this gutter numbers. Weak: the scroll view owns both of us.
    weak var grid: GridTableView?
    /// Fired on click/drag: (row, extend). Routed into the grid's existing
    /// block-selection model — the gutter never invents its own selection path.
    var onSelectRow: ((Int, Bool) -> Void)?

    // Cached once — never allocated per draw (design §5.1).
    private static let font = NSFont.monospacedSystemFont(ofSize: 10, weight: .regular)
    private static let para: NSParagraphStyle = {
        let p = NSMutableParagraphStyle()
        p.alignment = .right
        p.lineBreakMode = .byClipping
        return p
    }()
    /// Row numbers are `.secondary` — chrome, not data.
    private static let normal: [NSAttributedString.Key: Any] = [
        .font: font, .foregroundColor: NSColor.secondaryLabelColor, .paragraphStyle: para,
    ]
    /// Rows inside the selection get the primary label colour, so the gutter
    /// answers "which rows are selected" even when the block is scrolled tall.
    private static let selected: [NSAttributedString.Key: Any] = [
        .font: font, .foregroundColor: NSColor.labelColor, .paragraphStyle: para,
    ]
    private static let digitWidth: CGFloat = ("0" as NSString).size(withAttributes: normal).width
    private static let lineHeight: CGFloat = ("0" as NSString).size(withAttributes: normal).height

    private static let padLeft: CGFloat = 6
    private static let padRight: CGFloat = 7

    private var digits = 3

    /// Width adapts to the magnitude of the row count: 1,000,000 rows widen the
    /// gutter to 7 digits instead of clipping. Still narrower than the
    /// narrowest data column (64 pt): 7 digits at 10 pt monospaced is ~56 pt.
    func update(rowCount: Int) {
        let d = max(3, String(max(rowCount, 1)).count)
        if d != digits {
            digits = d
            ruleThickness = requiredThickness
        }
        // New rows may already be inside the viewport (a short result growing);
        // their numbers must appear without waiting for a scroll.
        needsDisplay = true
    }

    override var requiredThickness: CGFloat {
        ceil(CGFloat(digits) * Self.digitWidth) + Self.padLeft + Self.padRight
    }

    override func drawHashMarksAndLabels(in rect: NSRect) {
        // Flat chrome background (no alternating stripes — the flatness is what
        // separates the gutter from the data), plus a hairline on the right.
        // Semantic colors only, so dark mode needs no extra code.
        NSColor.textBackgroundColor.setFill()
        rect.fill()
        NSColor.separatorColor.setFill()
        NSRect(x: bounds.maxX - 1, y: rect.minY, width: 1, height: rect.height).fill()

        guard let grid, grid.numberOfRows > 0 else { return }
        // Only the rows whose rects intersect the dirty rect are drawn — the
        // gutter is exactly as virtual as the table it annotates. The converted
        // rect's x-range lies OUTSIDE the table (the gutter is left of it), and
        // `rows(in:)` intersects rects, not y-bands — so re-anchor x inside the
        // table's visible area and keep only the y-band.
        var dirtyInGrid = grid.convert(rect, from: self)
        dirtyInGrid.origin.x = grid.visibleRect.minX
        dirtyInGrid.size.width = 1
        let visible = grid.rows(in: dirtyInGrid)
        guard visible.length > 0 else { return }
        let sel = grid.selectedRowIndexes
        let textWidth = bounds.width - Self.padLeft - Self.padRight
        for row in visible.lowerBound..<visible.upperBound {
            let rowRect = convert(grid.rect(ofRow: row), from: grid)
            // Centered via midY, which is flippedness-agnostic.
            let line = NSRect(
                x: Self.padLeft, y: rowRect.midY - Self.lineHeight / 2,
                width: textWidth, height: Self.lineHeight)
            // 1-based, ungrouped: a gutter shows "4821", not "4,821".
            (String(row + 1) as NSString).draw(
                in: line, withAttributes: sel.contains(row) ? Self.selected : Self.normal)
        }
    }

    // MARK: - click / drag -> whole-row selection

    /// Clicking a number selects the whole row (DataGrip's convention).
    /// Shift-click extends, and a drag sweeps a row range. All of it routes
    /// through `GridTableView.selectWholeRow`, i.e. the existing block model.
    override func mouseDown(with event: NSEvent) {
        guard let grid, grid.numberOfRows > 0 else { return }
        // Row from the y-coordinate alone: the gutter is horizontally OUTSIDE
        // the table, so `row(at:)` (which intersects the full point) would
        // answer -1 for every gutter click.
        let p = grid.convert(event.locationInWindow, from: nil)
        let step = grid.rowHeight + grid.intercellSpacing.height
        guard step > 0, p.y >= 0 else { return }
        let hit = Int(floor(p.y / step))
        guard hit >= 0, hit < grid.numberOfRows else { return }
        onSelectRow?(hit, event.modifierFlags.contains(.shift))
        trackDrag(from: event, lastRow: hit)
    }

    /// Same idle-friendly loop shape as the grid's own drag tracking: a 50 ms
    /// `nextEvent` timeout keeps autoscroll alive while the pointer rests
    /// outside the viewport, and costs nothing while it rests inside.
    private func trackDrag(from initial: NSEvent, lastRow: Int) {
        guard let window, let grid else { return }
        var last = lastRow
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
                continue  // button held, pointer never moved
            }
            let p = grid.convert(latest.locationInWindow, from: nil)
            let vr = grid.visibleRect
            let outside = p.y < vr.minY || p.y > vr.maxY
            if outside {
                // Vertical-only autoscroll. `autoscroll(with:)` would also chase
                // the pointer's x — which sits in the gutter, left of every
                // column — and drag the grid horizontally back to column 0.
                let y = min(max(p.y, 0), max(grid.bounds.maxY - 1, 0))
                grid.scrollToVisible(
                    NSRect(x: vr.midX, y: y, width: 1, height: grid.rowHeight))
            }
            if e == nil && !outside { continue }
            let row = clampedRow(at: p, in: grid)
            guard row != last else { continue }
            last = row
            onSelectRow?(row, true)
        }
    }

    /// Clamped, not nil'd: during an autoscroll drag the pointer is outside the
    /// rows, and the sweep must still follow it to the first/last row.
    private func clampedRow(at p: NSPoint, in grid: GridTableView) -> Int {
        let step = grid.rowHeight + grid.intercellSpacing.height
        let r = step > 0 ? Int(floor(p.y / step)) : 0
        return max(0, min(max(grid.numberOfRows - 1, 0), r))
    }
}
