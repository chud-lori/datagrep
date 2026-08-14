import AppKit

/// The header cell of the row-number gutter — the small square at the top-left,
/// level with the column headers, that turns the gutter from a bare strip of
/// numbers into a proper labelled column (the "⊙"/"#" corner every data grid
/// has). The scroll view leaves this corner blank; this fills it so the header
/// row reads as continuous across the gutter and the data columns.
///
/// Clicking it selects every row — the gutter's header is the natural
/// "select all" affordance, matching the numbers below it that select one row.
final class GridGutterHeader: NSView {
    var onSelectAll: (() -> Void)?

    override var isFlipped: Bool { true }
    override var isOpaque: Bool { true }

    override func draw(_ dirtyRect: NSRect) {
        // Same faint tint as the gutter body, so the column is one surface.
        (NSColor.textBackgroundColor.blended(withFraction: 0.5, of: .windowBackgroundColor)
            ?? .textBackgroundColor).setFill()
        bounds.fill()
        // Hairlines: right (against the first data column) and bottom (against
        // the rows), the same borders the column-header row draws.
        NSColor.separatorColor.setFill()
        NSRect(x: bounds.maxX - 1, y: 0, width: 1, height: bounds.height).fill()
        NSRect(x: 0, y: bounds.maxY - 1, width: bounds.width, height: 1).fill()

        // A small hash mark, centred — the column's "label". Secondary weight:
        // it is chrome, like the numbers it heads.
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
/// ### Why `clipsToBounds` is set true on install
///
/// macOS 14 changed `NSView.clipsToBounds` to default **false**, and since
/// Mojave a vertical ruler *overlays* the clip view rather than tiling beside
/// it. An unclipped ruler is handed a dirty rect that can be LARGER than its own
/// bounds, and its `drawHashMarksAndLabels(in:)` then paints its chrome
/// background across the neighbouring clip-view region — inside SwiftUI's
/// layer-backed `NSHostingView` that leaves the whole table blank until a live
/// resize. Clipping the ruler to its bounds confines the paint to the gutter and
/// the table composites normally. (Apple Dev Forums 767825; Scintilla bug 2402.)
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

    // Cached once — never allocated per draw.
    private static let font = NSFont.monospacedSystemFont(ofSize: 10, weight: .regular)
    private static let para: NSParagraphStyle = {
        let p = NSMutableParagraphStyle()
        // Left-aligned and tight against the leading padding — a compact gutter
        // where the number sits at a fixed x, not floating at the right edge of
        // an over-wide strip (which read as dead space beside the data).
        p.alignment = .left
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

    private static let padLeft: CGFloat = 10
    private static let padRight: CGFloat = 8

    private var digits = 2

    override init(scrollView: NSScrollView?, orientation: NSRulerView.Orientation) {
        super.init(scrollView: scrollView, orientation: orientation)
        // See the type doc: without this the ruler's over-sized draw pass paints
        // over the clip view and the table never composites inside NSHostingView.
        clipsToBounds = true
    }

    required init(coder: NSCoder) {
        super.init(coder: coder)
        clipsToBounds = true
    }

    /// Width adapts to the magnitude of the row count: 1,000,000 rows widen the
    /// gutter to 7 digits instead of clipping. Still narrower than the
    /// narrowest data column (64 pt): 7 digits at 10 pt monospaced is ~56 pt.
    func update(rowCount: Int) {
        // Fit the actual magnitude (min two digits, so single-digit results
        // still read as a column, not a sliver). No fixed 3-digit floor — that
        // is what padded small results out into a wide empty strip.
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
        // Flat chrome background (no alternating stripes — the flatness is what
        // separates the gutter from the data), plus a hairline on the right.
        // Fill the whole bounds (clipsToBounds confines it to the gutter), so a
        // dirty rect narrower than the ruler never leaves an unpainted band.
        // A hair of contrast against the data area so the gutter reads as chrome,
        // not a first data column — the same faint tint a code editor's line-number
        // margin uses. Falls back cleanly in both light and dark.
        (NSColor.textBackgroundColor.blended(withFraction: 0.5, of: .windowBackgroundColor)
            ?? .textBackgroundColor).setFill()
        bounds.fill()
        NSColor.separatorColor.setFill()
        NSRect(x: bounds.maxX - 1, y: bounds.minY, width: 1, height: bounds.height).fill()

        guard let grid, grid.numberOfRows > 0 else { return }
        // Number every row currently on screen, positioned by converting each
        // row's rect from the table into the ruler. Deriving the set from the
        // table's own visibleRect (not the passed-in dirty rect, whose converted
        // Y-band went empty once the ruler clipped to its bounds) is what keeps
        // the numbers actually drawing. It is still O(viewport), never O(rows).
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

    /// Clicking a number selects the whole row (the row-header convention).
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
