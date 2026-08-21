import AppKit
import DatagrepKit
import QuartzCore

enum GridStyle {
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
    static let editedFont = NSFont.monospacedSystemFont(ofSize: 11.5, weight: .semibold)
    static let edited = Set2(editedFont, .controlAccentColor)

    static func struckThrough(_ attrs: [NSAttributedString.Key: Any]) -> [NSAttributedString.Key:
        Any]
    {
        var out = attrs
        out[.strikethroughStyle] = NSUnderlineStyle.single.rawValue
        out[.foregroundColor] = NSColor.secondaryLabelColor
        return out
    }

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

    private static var chipWidths: [String: CGFloat] = [:]
    static func chipWidth(_ s: String) -> CGFloat {
        if let w = chipWidths[s] { return w }
        let w = (s as NSString).size(withAttributes: chip.left).width
        chipWidths[s] = w
        return w
    }
}

final class GridCellView: NSView {
    static let reuseID = NSUserInterfaceItemIdentifier("datagrep.grid.cell")

    private(set) var text: String = ""
    private(set) var kind: CellKind = .value
    private var isPending = false
    private var rightAligned = false
    private(set) var isEditable = false
    private var staged: MutationValue?
    private var stagedState: StagedState?
    /// This row is staged for deletion.
    private var isDeleted = false
    private var isEditing = false
    private var chipRect: NSRect = .zero
    private var skeletonSeed: CGFloat = 0.7

    /// The value text goes through a real `NSTextField`, not `draw()`.
    private let label: NSTextField = {
        let f = NSTextField(labelWithString: "")
        f.translatesAutoresizingMaskIntoConstraints = false
        f.lineBreakMode = .byTruncatingTail
        f.cell?.usesSingleLineMode = true
        f.drawsBackground = false
        f.isBordered = false
        f.isEditable = false
        f.isSelectable = false
        return f
    }()

    var onNestedClick: ((GridCellView) -> Void)?
    var onEditCommitted: ((GridCellView, String) -> Void)?
    var row: Int = -1
    /// The ENGINE column index this cell reads from — stable across reordering.
    var column: UInt32 = 0
    var columnPosition: Int = 0

    override var isFlipped: Bool { true }
    override var isOpaque: Bool { false }

    override init(frame: NSRect) {
        super.init(frame: frame)
        addSubview(label)
        NSLayoutConstraint.activate([
            label.leadingAnchor.constraint(equalTo: leadingAnchor, constant: GridStyle.cellPadX),
            label.trailingAnchor.constraint(equalTo: trailingAnchor, constant: -GridStyle.cellPadX),
            label.centerYAnchor.constraint(equalTo: centerYAnchor),
        ])
    }

    required init?(coder: NSCoder) { fatalError() }

    func configure(
        kind: CellKind, text: String, row: Int, column: UInt32, position: Int, pending: Bool,
        rightAligned: Bool, editable: Bool = false, staged: MutationValue? = nil,
        deleted: Bool = false, stagedState: StagedState? = nil
    ) {
        if isEditing { endEditing(commit: false) }
        self.columnPosition = position
        self.isEditable = editable
        self.staged = staged
        self.isDeleted = deleted
        self.stagedState = stagedState
        let wasPending = isPending
        self.kind = kind
        self.text = text
        self.row = row
        self.column = column
        self.isPending = pending
        self.rightAligned = rightAligned
        self.skeletonSeed = 0.45 + CGFloat((row &* 2_654_435_761 &+ Int(column) &* 40_503) % 46) / 100
        updateLabel()
        needsDisplay = true
        if wasPending && !pending { fadeIn() }
    }

    private func updateLabel() {
        if isEditing { return }  // the field editor owns the text right now
        let selected = isSelectedRow
        var attrs: [NSAttributedString.Key: Any]
        let string: String
        if let staged, !isPending {
            label.isHidden = false
            label.alignment = rightAligned ? .right : .left
            var typed = (selected ? GridStyle.selected : GridStyle.edited).of(rightAligned)
            if isDeleted { typed = GridStyle.struckThrough(typed) }
            label.attributedStringValue = NSAttributedString(
                string: staged.display, attributes: typed)
            return
        }
        switch (isPending, kind) {
        case (true, _):
            label.isHidden = true
            return
        case (_, .value):
            if text.isEmpty {
                label.isHidden = true
                return
            }
            string = text
            attrs = (selected ? GridStyle.selected : GridStyle.value).of(rightAligned)
        case (_, .null):
            string = "NULL"
            attrs = (selected ? GridStyle.selected : GridStyle.null).of(rightAligned)
        case (_, .absent):
            string = "—"
            attrs = (selected ? GridStyle.selected : GridStyle.absent).of(rightAligned)
        case (_, .nested):
            string = text
            attrs = (selected ? GridStyle.selected : GridStyle.chip).left
        }
        label.isHidden = false
        label.alignment = rightAligned ? .right : .left
        if isDeleted { attrs = GridStyle.struckThrough(attrs) }
        label.attributedStringValue = NSAttributedString(string: string, attributes: attrs)
    }

    func refreshSelectionColour() {
        updateLabel()
    }

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
        guard let rv = superview as? GridRowView, rv.isSelected, rv.isEmphasized else {
            return false
        }
        guard let cols = rv.rangeColumns else { return true }
        return cols.contains(columnPosition)
    }

    override func draw(_ dirtyRect: NSRect) {
        let inset = bounds.insetBy(dx: GridStyle.cellPadX, dy: 4)
        if isPending {
            drawSkeleton(in: inset)
            return
        }
        let selected = isSelectedRow
        if let wash = washColour, !isEditing {
            wash.withAlphaComponent(selected ? 0.30 : 0.12).setFill()
            bounds.fill()
        }
        switch kind {
        case .value where text.isEmpty:
            let y = inset.maxY - 2.5
            let x = rightAligned ? inset.maxX - 11 : inset.minX
            (selected ? NSColor.alternateSelectedControlTextColor : NSColor.quaternaryLabelColor)
                .setFill()
            NSBezierPath(rect: NSRect(x: x, y: y, width: 11, height: 1)).fill()
        case .nested:
            // The chip background; its text rides in the label above it.
            let w = min(GridStyle.chipWidth(text) + 14, inset.width)
            chipRect = NSRect(
                x: inset.minX, y: inset.minY + 1, width: w, height: inset.height - 2)
            NSColor.controlAccentColor.withAlphaComponent(selected ? 0.35 : 0.14).setFill()
            NSBezierPath(roundedRect: chipRect, xRadius: 5, yRadius: 5).fill()
            NSColor.controlAccentColor.withAlphaComponent(selected ? 0.5 : 0.28).setStroke()
            NSBezierPath(roundedRect: chipRect.insetBy(dx: 0.5, dy: 0.5), xRadius: 5, yRadius: 5)
                .stroke()
        default:
            break
        }
    }

    /// The colour behind a cell that is not plain server truth.
    private var washColour: NSColor? {
        let touched = isDeleted || staged != nil
        guard touched else { return nil }
        switch stagedState {
        case .applied: return .systemGreen
        case .conflicted: return .systemOrange
        case .failed: return .systemRed
        case .pending, .notAttempted, .none:
            return isDeleted ? .systemRed : .controlAccentColor
        }
    }

    private func drawSkeleton(in inset: NSRect) {
        let w = inset.width * skeletonSeed
        let x = rightAligned ? inset.maxX - w : inset.minX
        let bar = NSRect(x: x, y: inset.midY - 4, width: max(10, w), height: 8)
        NSColor.quaternaryLabelColor.withAlphaComponent(0.55).setFill()
        NSBezierPath(roundedRect: bar, xRadius: 4, yRadius: 4).fill()
    }

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

    // MARK: - inline editing

    private var table: NSTableView? { superview?.superview as? NSTableView }

    var canBeginEditing: Bool { isEditable && !isPending && kind != .nested }

    /// Turn this cell into a field editor over the value it is showing.
    func beginEditing() {
        guard canBeginEditing, !isEditing else { return }
        isEditing = true
        let seed = staged?.display ?? (kind == .value ? text : "")
        label.isHidden = false
        label.isEditable = true
        label.isSelectable = true
        label.isBordered = true
        label.drawsBackground = true
        label.backgroundColor = .textBackgroundColor
        label.font = GridStyle.font
        label.textColor = .labelColor
        label.alignment = rightAligned ? .right : .left
        label.stringValue = seed
        label.delegate = self
        window?.makeFirstResponder(label)
        label.currentEditor()?.selectAll(nil)
    }

    private func endEditing(commit: Bool) {
        guard isEditing else { return }
        isEditing = false
        let typed = label.stringValue
        label.isEditable = false
        label.isSelectable = false
        label.isBordered = false
        label.drawsBackground = false
        label.delegate = nil
        updateLabel()
        needsDisplay = true
        if commit { onEditCommitted?(self, typed) }
    }

    private var stagingToolTip: String? {
        guard isDeleted || staged != nil else { return nil }
        let what = isDeleted ? "this document is staged for deletion" : "you typed this value"
        switch stagedState {
        case .applied:
            return
                "\(what) — written to the server. The grid still shows the rows as they were loaded; reload to see what is stored now."
        case .conflicted(let why):
            return
                "\(what), and the server refused it: this document changed after you loaded it, so nothing was written. \(why)"
        case .failed(let why):
            return "\(what), and the write failed: \(why)"
        case .notAttempted:
            return
                "\(what) — the batch stopped before reaching this document, so nothing was written for it and it is still staged."
        case .pending, .none:
            return "\(what) — not written yet. Commit it from the bar below the grid."
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
            if isPending { return "loading — this row has not been fetched yet" }
            if let staging = stagingToolTip { return staging }
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

// MARK: - the field editor's delegate

extension GridCellView: NSTextFieldDelegate {
    func controlTextDidEndEditing(_ obj: Notification) {
        endEditing(commit: true)
    }

    func control(_ control: NSControl, textView: NSTextView, doCommandBy selector: Selector) -> Bool
    {
        guard selector == #selector(NSResponder.cancelOperation(_:)) else { return false }
        endEditing(commit: false)
        // Hand focus back to the grid, or Escape leaves the keyboard nowhere.
        if let table { window?.makeFirstResponder(table) }
        return true
    }
}

// MARK: - row view

/// Draws the hover highlight and the focused-cell ring.
final class GridRowView: NSTableRowView {
    var isHovered = false {
        didSet { if isHovered != oldValue { needsDisplay = true } }
    }
    var focusedColumn: Int = -1 {
        didSet { if focusedColumn != oldValue { needsDisplay = true } }
    }
    var rangeColumns: ClosedRange<Int>? {
        didSet {
            guard rangeColumns != oldValue else { return }
            needsDisplay = true
            // The text colour inside vs outside the block changes with it.
            for sub in subviews {
                sub.needsDisplay = true
                (sub as? GridCellView)?.refreshSelectionColour()
            }
        }
    }
    var isRangeTop = false {
        didSet { if isRangeTop != oldValue { needsDisplay = true } }
    }
    var isRangeBottom = false {
        didSet { if isRangeBottom != oldValue { needsDisplay = true } }
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
        guard let cols = rangeColumns, let block = blockRect(cols) else {
            color.setFill()
            dirtyRect.fill()
            return
        }
        color.withAlphaComponent(0.14).setFill()
        dirtyRect.fill()
        let solid = block.intersection(dirtyRect)
        guard !solid.isNull, !solid.isEmpty else { return }
        color.setFill()
        solid.fill()
    }

    override func draw(_ dirtyRect: NSRect) {
        super.draw(dirtyRect)
        if let cols = rangeColumns, let block = blockRect(cols) {
            NSColor.controlAccentColor.setStroke()
            let path = NSBezierPath()
            path.lineWidth = 1.5
            path.move(to: NSPoint(x: block.minX + 0.75, y: block.minY))
            path.line(to: NSPoint(x: block.minX + 0.75, y: block.maxY))
            path.move(to: NSPoint(x: block.maxX - 0.75, y: block.minY))
            path.line(to: NSPoint(x: block.maxX - 0.75, y: block.maxY))
            let topY = isFlipped ? block.minY + 0.75 : block.maxY - 0.75
            let bottomY = isFlipped ? block.maxY - 0.75 : block.minY + 0.75
            if isRangeTop {
                path.move(to: NSPoint(x: block.minX, y: topY))
                path.line(to: NSPoint(x: block.maxX, y: topY))
            }
            if isRangeBottom {
                path.move(to: NSPoint(x: block.minX, y: bottomY))
                path.line(to: NSPoint(x: block.maxX, y: bottomY))
            }
            path.stroke()
        }
        guard focusedColumn >= 0, let table = superview as? NSTableView,
            focusedColumn < table.numberOfColumns
        else { return }
        let r = table.frameOfCell(atColumn: focusedColumn, row: table.row(for: self))
        guard !r.isEmpty else { return }
        let local = convert(r, from: table).insetBy(dx: 1, dy: 1)
        NSColor.controlAccentColor.setStroke()
        let path = NSBezierPath(roundedRect: local, xRadius: 3, yRadius: 3)
        path.lineWidth = 1.5
        path.stroke()
    }

    private func blockRect(_ cols: ClosedRange<Int>) -> NSRect? {
        guard let table = superview as? NSTableView else { return nil }
        let row = table.row(for: self)
        guard row >= 0 else { return nil }
        let lo = max(0, cols.lowerBound)
        let hi = min(table.numberOfColumns - 1, cols.upperBound)
        guard lo <= hi else { return nil }
        var union: NSRect?
        for c in lo...hi {
            let f = table.frameOfCell(atColumn: c, row: row)
            if f.isEmpty { continue }
            union = union.map { $0.union(f) } ?? f
        }
        guard let union else { return nil }
        return convert(union, from: table)
    }
}

// MARK: - header view

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
        guard let table = tableView, hoverColumn >= 0, hoverColumn < table.numberOfColumns else {
            return
        }
        let r = headerRect(ofColumn: hoverColumn)
        NSColor.secondaryLabelColor.withAlphaComponent(0.10).setFill()
        r.insetBy(dx: 0, dy: 1).fill()
        NSColor.controlAccentColor.withAlphaComponent(0.65).setFill()
        NSRect(x: r.minX, y: r.maxY - 2, width: r.width, height: 2).fill()
        let sorted = table.tableColumns.indices.contains(hoverColumn)
            && table.indicatorImage(in: table.tableColumns[hoverColumn]) != nil
        guard !sorted, r.width > 46 else { return }
        drawGhostChevron(in: r)
    }

    private func drawGhostChevron(in r: NSRect) {
        let w: CGFloat = 7
        let h: CGFloat = 4
        let x = r.maxX - w - 7
        let y = r.midY - h / 2
        let p = NSBezierPath()
        p.move(to: NSPoint(x: x, y: y + h))
        p.line(to: NSPoint(x: x + w / 2, y: y))
        p.line(to: NSPoint(x: x + w, y: y + h))
        p.lineWidth = 1.3
        p.lineCapStyle = .round
        p.lineJoinStyle = .round
        NSColor.secondaryLabelColor.withAlphaComponent(0.45).setStroke()
        p.stroke()
    }

    override func resetCursorRects() {
        super.resetCursorRects()
        guard let table = tableView else { return }
        let gutter: CGFloat = 4
        for c in 0..<table.numberOfColumns {
            let r = headerRect(ofColumn: c)
            guard r.width > 2 * gutter + 4 else { continue }
            addCursorRect(r.insetBy(dx: gutter, dy: 0), cursor: .pointingHand)
        }
    }
}
