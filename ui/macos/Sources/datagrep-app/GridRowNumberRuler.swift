import AppKit

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
// Row numbers are ruler chrome, never table data — structurally excluded from copy.
final class GridRowNumberRuler: NSRulerView {
    /// Weak: the scroll view owns both of us.
    weak var grid: GridTableView?
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
    private static let selected: [NSAttributedString.Key: Any] = [
        .font: font, .foregroundColor: NSColor.labelColor, .paragraphStyle: para,
    ]
    private static let digitWidth: CGFloat = ("0" as NSString).size(withAttributes: normal).width
    private static let lineHeight: CGFloat = ("0" as NSString).size(withAttributes: normal).height

    private static let padLeft: CGFloat = 10
    private static let padRight: CGFloat = 8

    private var digits = 2

    // macOS 14 flipped the clipsToBounds default; without it the gutter paints outside itself.
    override init(scrollView: NSScrollView?, orientation: NSRulerView.Orientation) {
        super.init(scrollView: scrollView, orientation: orientation)
        clipsToBounds = true
    }

    required init(coder: NSCoder) {
        super.init(coder: coder)
        clipsToBounds = true
    }

    func update(rowCount: Int) {
        // Min two digits, so single-digit results still read as a column.
        let d = max(2, String(max(rowCount, 1)).count)
        if d != digits {
            digits = d
            ruleThickness = requiredThickness
        }
        needsDisplay = true
    }

    override var requiredThickness: CGFloat {
        ceil(CGFloat(digits) * Self.digitWidth) + Self.padLeft + Self.padRight
    }

    override func drawHashMarksAndLabels(in rect: NSRect) {
        (NSColor.textBackgroundColor.blended(withFraction: 0.5, of: .windowBackgroundColor)
            ?? .textBackgroundColor).setFill()
        bounds.fill()
        NSColor.separatorColor.setFill()
        NSRect(x: bounds.maxX - 1, y: bounds.minY, width: 1, height: bounds.height).fill()

        guard let grid, grid.numberOfRows > 0 else { return }
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

    override func mouseDown(with event: NSEvent) {
        guard let grid, grid.numberOfRows > 0 else { return }
        let p = grid.convert(event.locationInWindow, from: nil)
        let step = grid.rowHeight + grid.intercellSpacing.height
        guard step > 0, p.y >= 0 else { return }
        let hit = Int(floor(p.y / step))
        guard hit >= 0, hit < grid.numberOfRows else { return }
        onSelectRow?(hit, event.modifierFlags.contains(.shift))
        trackDrag(from: event, lastRow: hit)
    }

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

    private func clampedRow(at p: NSPoint, in grid: GridTableView) -> Int {
        let step = grid.rowHeight + grid.intercellSpacing.height
        let r = step > 0 ? Int(floor(p.y / step)) : 0
        return max(0, min(max(grid.numberOfRows - 1, 0), r))
    }
}
