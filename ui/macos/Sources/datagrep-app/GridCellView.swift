import AppKit
import DatagrepKit
import QuartzCore

/// Cached text attributes. Built once at startup, never per cell and never per
/// frame — design §5.1 ("never allocate per cell per frame").
enum GridStyle {
    /// 24, not 20: 20 pt rows with 6 pt padding read as a spreadsheet from
    /// 1998. This is one line of monospaced 11.5 with real air around it.
    static let rowHeight: CGFloat = 24
    static let headerHeight: CGFloat = 26
    static let cellPadX: CGFloat = 10

    static let font = NSFont.monospacedSystemFont(ofSize: 11.5, weight: .regular)
    static let italic = NSFontManager.shared.convert(font, toHaveTrait: .italicFontMask)
    static let headerFont = NSFont.systemFont(ofSize: 10.5, weight: .semibold)

    private static func para(_ alignment: NSTextAlignment) -> NSParagraphStyle {
        let p = NSMutableParagraphStyle()
        p.lineBreakMode = alignment == .right ? .byTruncatingHead : .byTruncatingTail
        p.alignment = alignment
        return p
    }
    static let leftPara = para(.left)
    static let rightPara = para(.right)

    /// Semantic colors only, so light/dark mode both work with no extra code.
    private static func make(_ f: NSFont, _ c: NSColor, _ right: Bool) -> [NSAttributedString
        .Key: Any]
    {
        [.font: f, .foregroundColor: c, .paragraphStyle: right ? rightPara : leftPara]
    }

    struct Set2 {
        let left: [NSAttributedString.Key: Any]
        let right: [NSAttributedString.Key: Any]
        init(_ f: NSFont, _ c: NSColor) {
            left = make(f, c, false)
            right = make(f, c, true)
        }
        func of(_ rightAligned: Bool) -> [NSAttributedString.Key: Any] {
            rightAligned ? right : left
        }
    }

    static let value = Set2(font, .labelColor)
    static let selected = Set2(font, .alternateSelectedControlTextColor)
    /// NULL: the value is present and is null.
    static let null = Set2(italic, .secondaryLabelColor)
    /// ABSENT: the field is not in the document at all. A different fact.
    static let absent = Set2(font, .tertiaryLabelColor)
    static let chip = Set2(font, .controlAccentColor)
    static let pending = Set2(italic, .quaternaryLabelColor)

    /// Header text: caption-sized, semibold, secondary — a label, not a button.
    static func headerString(_ title: String, rightAligned: Bool) -> NSAttributedString {
        NSAttributedString(
            string: title,
            attributes: [
                .font: headerFont,
                .foregroundColor: NSColor.secondaryLabelColor,
                .paragraphStyle: rightAligned ? rightPara : leftPara,
            ])
    }

    /// Chip labels come from a tiny fixed vocabulary ("{4 fields}", "[2 items]"),
    /// so their measured widths are memoised instead of re-measured per draw.
    private static var chipWidths: [String: CGFloat] = [:]
    static func chipWidth(_ s: String) -> CGFloat {
        if let w = chipWidths[s] { return w }
        let w = (s as NSString).size(withAttributes: chip.left).width
        chipWidths[s] = w
        return w
    }
}

/// One grid cell. Deliberately NOT an NSTextField: this draws a single string
/// with cached attributes, which keeps a 24-column viewport at ~1k trivial
/// views instead of ~1k full text-layout engines.
final class GridCellView: NSView {
    static let reuseID = NSUserInterfaceItemIdentifier("datagrep.grid.cell")

    private(set) var text: String = ""
    private(set) var kind: CellKind = .value
    private var isPending = false
    private var rightAligned = false
    private var chipRect: NSRect = .zero
    /// Deterministic per-row width for the skeleton bar, so placeholders look
    /// like data instead of a row of identical gray sticks.
    private var skeletonSeed: CGFloat = 0.7

    var onNestedClick: ((GridCellView) -> Void)?
    var row: Int = -1
    var column: UInt32 = 0

    override var isFlipped: Bool { true }
    override var isOpaque: Bool { false }

    func configure(
        kind: CellKind, text: String, row: Int, column: UInt32, pending: Bool, rightAligned: Bool
    ) {
        // A cell that was a skeleton and is now real fades in once. This is the
        // difference between rows "arriving" and rows "popping".
        let wasPending = isPending
        self.kind = kind
        self.text = text
        self.row = row
        self.column = column
        self.isPending = pending
        self.rightAligned = rightAligned
        self.skeletonSeed = 0.45 + CGFloat((row &* 2_654_435_761 &+ Int(column) &* 40_503) % 46) / 100
        needsDisplay = true
        if wasPending && !pending { fadeIn() }
    }

    /// 120 ms, once, on the cell that changed. Not a view-wide animation and
    /// not a repeating one — when nothing arrives, nothing animates.
    private func fadeIn() {
        wantsLayer = true
        let a = CABasicAnimation(keyPath: "opacity")
        a.fromValue = 0.0
        a.toValue = 1.0
        a.duration = 0.12
        a.timingFunction = CAMediaTimingFunction(name: .easeOut)
        layer?.add(a, forKey: "datagrep.fade")
    }

    private var isSelectedRow: Bool {
        guard let rv = superview as? NSTableRowView else { return false }
        return rv.isSelected && rv.isEmphasized
    }

    override func draw(_ dirtyRect: NSRect) {
        let inset = bounds.insetBy(dx: GridStyle.cellPadX, dy: 4)
        if isPending {
            drawSkeleton(in: inset)
            return
        }
        let selected = isSelectedRow
        switch kind {
        case .value:
            if text.isEmpty {
                // EMPTY STRING: present, and empty. A thin baseline mark, so it is
                // tellable apart from NULL and from ABSENT at a glance.
                let y = inset.maxY - 2.5
                let x = rightAligned ? inset.maxX - 11 : inset.minX
                (selected ? NSColor.alternateSelectedControlTextColor : NSColor.quaternaryLabelColor)
                    .setFill()
                NSBezierPath(rect: NSRect(x: x, y: y, width: 11, height: 1)).fill()
                return
            }
            (text as NSString).draw(
                in: inset,
                withAttributes: (selected ? GridStyle.selected : GridStyle.value).of(rightAligned))
        case .null:
            ("NULL" as NSString).draw(
                in: inset,
                withAttributes: (selected ? GridStyle.selected : GridStyle.null).of(rightAligned))
        case .absent:
            ("—" as NSString).draw(
                in: inset,
                withAttributes: (selected ? GridStyle.selected : GridStyle.absent).of(rightAligned))
        case .nested:
            let w = min(GridStyle.chipWidth(text) + 14, inset.width)
            chipRect = NSRect(
                x: inset.minX, y: inset.minY + 1, width: w, height: inset.height - 2)
            NSColor.controlAccentColor.withAlphaComponent(selected ? 0.35 : 0.14).setFill()
            NSBezierPath(roundedRect: chipRect, xRadius: 5, yRadius: 5).fill()
            NSColor.controlAccentColor.withAlphaComponent(selected ? 0.5 : 0.28).setStroke()
            NSBezierPath(roundedRect: chipRect.insetBy(dx: 0.5, dy: 0.5), xRadius: 5, yRadius: 5)
                .stroke()
            (text as NSString).draw(
                in: chipRect.insetBy(dx: 7, dy: 0),
                withAttributes: (selected ? GridStyle.selected : GridStyle.chip).left)
        }
    }

    /// A skeleton BAR, not an ellipsis: the eye reads "a value is coming here"
    /// from a shape at the right size, and reads nothing at all from "…".
    private func drawSkeleton(in inset: NSRect) {
        let w = inset.width * skeletonSeed
        let x = rightAligned ? inset.maxX - w : inset.minX
        let bar = NSRect(x: x, y: inset.midY - 4, width: max(10, w), height: 8)
        NSColor.quaternaryLabelColor.withAlphaComponent(0.55).setFill()
        NSBezierPath(roundedRect: bar, xRadius: 4, yRadius: 4).fill()
    }

    /// Added only while a query is actually streaming, and removed the moment
    /// it is terminal, so a settled window has no live animations at all.
    func setShimmer(_ on: Bool) {
        guard isPending else {
            layer?.removeAnimation(forKey: "datagrep.shimmer")
            return
        }
        if on {
            guard layer?.animation(forKey: "datagrep.shimmer") == nil else { return }
            wantsLayer = true
            let a = CABasicAnimation(keyPath: "opacity")
            a.fromValue = 0.45
            a.toValue = 0.95
            a.duration = 0.9
            a.autoreverses = true
            a.repeatCount = .infinity
            layer?.add(a, forKey: "datagrep.shimmer")
        } else {
            layer?.removeAnimation(forKey: "datagrep.shimmer")
        }
    }

    override func mouseDown(with event: NSEvent) {
        if kind == .nested {
            let p = convert(event.locationInWindow, from: nil)
            if chipRect.contains(p) {
                onNestedClick?(self)
                return
            }
        }
        super.mouseDown(with: event)
    }

    override func resetCursorRects() {
        if kind == .nested { addCursorRect(chipRect, cursor: .pointingHand) }
    }

    override var toolTip: String? {
        get {
            switch kind {
            case .absent: return "ABSENT — this field is not present in the document"
            case .null: return "NULL"
            case .nested: return "click to open the detail panel"
            case .value: return text.isEmpty ? "empty string" : nil
            }
        }
        set { _ = newValue }
    }
}

// MARK: - row view

/// Draws the hover highlight and the focused-cell ring.
///
/// Both live on the ROW, not the cell: a hover repaint then dirties one row
/// rectangle instead of twenty-four cell rectangles, and the focus ring can
/// straddle the intercell gap.
final class GridRowView: NSTableRowView {
    var isHovered = false {
        didSet { if isHovered != oldValue { needsDisplay = true } }
    }
    var focusedColumn: Int = -1 {
        didSet { if focusedColumn != oldValue { needsDisplay = true } }
    }

    override func drawBackground(in dirtyRect: NSRect) {
        super.drawBackground(in: dirtyRect)
        guard isHovered, !isSelected else { return }
        NSColor.secondaryLabelColor.withAlphaComponent(0.09).setFill()
        dirtyRect.fill()
    }

    override func drawSelection(in dirtyRect: NSRect) {
        guard selectionHighlightStyle != .none else { return }
        let color =
            isEmphasized
            ? NSColor.selectedContentBackgroundColor
            : NSColor.unemphasizedSelectedContentBackgroundColor
        color.setFill()
        dirtyRect.fill()
    }

    override func draw(_ dirtyRect: NSRect) {
        super.draw(dirtyRect)
        guard focusedColumn >= 0, let table = superview as? NSTableView,
            focusedColumn < table.numberOfColumns
        else { return }
        let r = table.frameOfCell(atColumn: focusedColumn, row: table.row(for: self))
        let local = convert(r, from: table).insetBy(dx: 1, dy: 1)
        NSColor.controlAccentColor.setStroke()
        let path = NSBezierPath(roundedRect: local, xRadius: 3, yRadius: 3)
        path.lineWidth = 1.5
        path.stroke()
    }
}

// MARK: - header view

/// `NSTableHeaderView` with a pointer. Stock AppKit headers do not react to
/// hover at all, which is most of why a stock table feels like a printout.
final class GridHeaderView: NSTableHeaderView {
    private var hoverColumn = -1
    private var tracking: NSTrackingArea?

    override func updateTrackingAreas() {
        super.updateTrackingAreas()
        if let tracking { removeTrackingArea(tracking) }
        let t = NSTrackingArea(
            rect: bounds,
            options: [.mouseEnteredAndExited, .mouseMoved, .activeInKeyWindow, .inVisibleRect],
            owner: self)
        addTrackingArea(t)
        tracking = t
    }

    override func mouseMoved(with event: NSEvent) {
        let p = convert(event.locationInWindow, from: nil)
        let c = column(at: p)
        if c != hoverColumn {
            hoverColumn = c
            needsDisplay = true
        }
        super.mouseMoved(with: event)
    }

    override func mouseExited(with event: NSEvent) {
        if hoverColumn != -1 {
            hoverColumn = -1
            needsDisplay = true
        }
        super.mouseExited(with: event)
    }

    override func draw(_ dirtyRect: NSRect) {
        super.draw(dirtyRect)
        guard hoverColumn >= 0, hoverColumn < (tableView?.numberOfColumns ?? 0) else { return }
        let r = headerRect(ofColumn: hoverColumn)
        NSColor.secondaryLabelColor.withAlphaComponent(0.10).setFill()
        r.insetBy(dx: 0, dy: 1).fill()
        // A one-pixel accent underline says "this is clickable" without
        // repainting the whole header in a different colour.
        NSColor.controlAccentColor.withAlphaComponent(0.65).setFill()
        NSRect(x: r.minX, y: r.maxY - 2, width: r.width, height: 2).fill()
    }

    override func resetCursorRects() {
        addCursorRect(bounds, cursor: .pointingHand)
    }
}
