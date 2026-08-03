import AppKit
import DbxKit

/// Cached text attributes. Built once at startup, never per cell and never per
/// frame — design §5.1 ("never allocate per cell per frame").
enum GridStyle {
    static let rowHeight: CGFloat = 20
    static let font = NSFont.monospacedSystemFont(ofSize: 11, weight: .regular)
    static let italic = NSFontManager.shared.convert(font, toHaveTrait: .italicFontMask)

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
    static let reuseID = NSUserInterfaceItemIdentifier("dbx.grid.cell")

    private(set) var text: String = ""
    private(set) var kind: CellKind = .value
    private var isPending = false
    private var rightAligned = false
    private var chipRect: NSRect = .zero

    var onNestedClick: ((GridCellView) -> Void)?
    var row: Int = -1
    var column: UInt32 = 0

    override var isFlipped: Bool { true }
    override var isOpaque: Bool { false }

    func configure(
        kind: CellKind, text: String, row: Int, column: UInt32, pending: Bool, rightAligned: Bool
    ) {
        self.kind = kind
        self.text = text
        self.row = row
        self.column = column
        self.isPending = pending
        self.rightAligned = rightAligned
        needsDisplay = true
    }

    private var isSelectedRow: Bool {
        guard let rv = superview as? NSTableRowView else { return false }
        return rv.isSelected && rv.isEmphasized
    }

    override func draw(_ dirtyRect: NSRect) {
        let inset = bounds.insetBy(dx: 6, dy: 2)
        if isPending {
            ("…" as NSString).draw(in: inset, withAttributes: GridStyle.pending.of(rightAligned))
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
            let w = min(GridStyle.chipWidth(text) + 12, inset.width)
            chipRect = NSRect(x: inset.minX, y: inset.minY, width: w, height: inset.height)
            NSColor.controlAccentColor.withAlphaComponent(selected ? 0.35 : 0.14).setFill()
            NSBezierPath(roundedRect: chipRect, xRadius: 4, yRadius: 4).fill()
            (text as NSString).draw(
                in: chipRect.insetBy(dx: 6, dy: 0),
                withAttributes: (selected ? GridStyle.selected : GridStyle.chip).left)
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
