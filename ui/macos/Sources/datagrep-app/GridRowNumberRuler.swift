import AppKit

/// The header cell of the row-number gutter — the "#" corner at the top-left,
/// level with the column headers, which the scroll view otherwise leaves
/// blank. Clicking it selects every row.
final class GridGutterHeader: NSView {
    var onSelectAll: (() -> Void)?

    override var isFlipped: Bool { true }
    override var isOpaque: Bool { true }

    override func draw(_ dirtyRect: NSRect) {
        // Same faint tint as the gutter body, so the column is one surface.
        (NSColor.textBackgroundColor.blended(withFraction: 0.5, of: .windowBackgroundColor)
            ?? .textBackgroundColor).setFill()
        bounds.fill()
        // Hairlines on the right and bottom, matching the column-header row.
        NSColor.separatorColor.setFill()
        NSRect(x: bounds.maxX - 1, y: 0, width: 1, height: bounds.height).fill()
        NSRect(x: 0, y: bounds.maxY - 1, width: bounds.width, height: 1).fill()

        let s = "#" as NSString
        let attrs: [NSAttributedString.Key: Any] = [
            .font: NSFont.systemFont(ofSize: 11, weight: .semibold),
            .foregroundColor: NSColor.tertiaryLabelColor,
        ]
        let size = s.size(withAttributes: attrs)
        s.draw(
            at: NSPoint(x: (bounds.width - size.width) / 2, y: (bounds.height - size.height) / 2),
            withAttributes: attrs)
    }

    override func mouseDown(with event: NSEvent) { onSelectAll?() }
}

/// The pinned row-number gutter.
///
/// An `NSRulerView`, NOT an `NSTableColumn` — load-bearing three times over:
///
///  1. **Pinned by construction** — the ruler tiles outside the clip view, so
///     horizontal scrolling can never move it.
///  2. **Excluded from copy by construction** — every copy path enumerates
///     `tableView.tableColumns`; the gutter is not a column, so the row number
///     can never leak onto the pasteboard.
///  3. **Cannot break virtualisation** — the number is derived purely from the
///     row index; drawing never touches the pager.
///
/// `clipsToBounds` must be true: macOS 14 defaults it to false, a vertical
/// ruler overlays the clip view, and an unclipped ruler gets dirty rects
/// larger than its bounds — `drawHashMarksAndLabels(in:)` then paints over the
/// clip view, which inside a layer-backed `NSHostingView` leaves the whole
/// table blank until a live resize.
final class GridRowNumberRuler: NSRulerView {
    /// Weak: the scroll view owns both of us.
    weak var grid: GridTableView?
    /// Fired on click/drag: (row, extend). Routed into the grid's existing
    /// block-selection model — the gutter never invents its own selection path.
    var onSelectRow: ((Int, Bool) -> Void)?

    // Cached once — never allocated per draw.
    private static let font = NSFont.monospacedSystemFont(ofSize: 10, weight: .regular)
    private static let para: NSParagraphStyle = {
        let p = NSMutableParagraphStyle()
        p.alignment = .left
        p.lineBreakMode = .byClipping
        return p
    }()
    private static let normal: [NSAttributedString.Key: Any] = [
        .font: font, .foregroundColor: NSColor.secondaryLabelColor, .paragraphStyle: para,
    ]
    /// Selected rows get the primary label colour, so the gutter answers
    /// "which rows are selected" even when the block is scrolled tall.
    private static let selected: [NSAttributedString.Key: Any] = [
        .font: font, .foregroundColor: NSColor.labelColor, .paragraphStyle: para,
    ]
    private static let digitWidth: CGFloat = ("0" as NSString).size(withAttributes: normal).width
    private static let lineHeight: CGFloat = ("0" as NSString).size(withAttributes: normal).height

    private static let padLeft: CGFloat = 10
    private static let padRight: CGFloat = 8

    private var digits = 2

    override init(scrollView: NSScrollView?, orientation: NSRulerView.Orientation) {
        super.init(scrollView: scrollView, orientation: orientation)
        // See the type doc: without this the table never composites inside
        // NSHostingView.
        clipsToBounds = true
    }

    required init(coder: NSCoder) {
        super.init(coder: coder)
        clipsToBounds = true
    }

    /// Width adapts to the magnitude of the row count: 1,000,000 rows widen
    /// the gutter to 7 digits instead of clipping.
    func update(rowCount: Int) {
        // Min two digits, so single-digit results still read as a column.
        let d = max(2, String(max(rowCount, 1)).count)
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
        // Fill the whole bounds (clipsToBounds confines it to the gutter), so
        // a dirty rect narrower than the ruler never leaves an unpainted band.
        (NSColor.textBackgroundColor.blended(withFraction: 0.5, of: .windowBackgroundColor)
            ?? .textBackgroundColor).setFill()
        bounds.fill()
        NSColor.separatorColor.setFill()
        NSRect(x: bounds.maxX - 1, y: bounds.minY, width: 1, height: bounds.height).fill()

        guard let grid, grid.numberOfRows > 0 else { return }
        // Derive the row set from the table's own visibleRect, NOT the passed
        // dirty rect — its converted Y-band comes up empty once the ruler
        // clips to its bounds. Still O(viewport), never O(rows).
        let visible = grid.rows(in: grid.visibleRect)
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

    /// Click selects the row, shift-click extends, drag sweeps a range — all
    /// routed through the grid's existing block-selection model.
    override func mouseDown(with event: NSEvent) {
        guard let grid, grid.numberOfRows > 0 else { return }
        // Row from the y-coordinate alone: the gutter is horizontally OUTSIDE
        // the table, so `row(at:)` would answer -1 for every gutter click.
        let p = grid.convert(event.locationInWindow, from: nil)
        let step = grid.rowHeight + grid.intercellSpacing.height
        guard step > 0, p.y >= 0 else { return }
        let hit = Int(floor(p.y / step))
        guard hit >= 0, hit < grid.numberOfRows else { return }
        onSelectRow?(hit, event.modifierFlags.contains(.shift))
        trackDrag(from: event, lastRow: hit)
    }

    /// Same loop shape as the grid's own drag tracking: a 50 ms `nextEvent`
    /// timeout keeps autoscroll alive while the pointer rests outside the
    /// viewport, and costs nothing while it rests inside.
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
                // Vertical-only autoscroll. `autoscroll(with:)` would chase
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
