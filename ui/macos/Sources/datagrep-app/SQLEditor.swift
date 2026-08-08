import AppKit
import DatagrepKit
import SwiftUI

/// NSTextView blinks its caret forever while focused. Design §5.0: that alone
/// is 2 full-window repaints/sec and fails P12/P19/P20/P21/P22 simultaneously.
/// Fix (b) from the design: stop blinking after 10 s of no input, resume on the
/// next keystroke. The 10 s arm is a ONE-SHOT `asyncAfter`, not a repeating
/// timer, and it fully disarms — so a settled window is genuinely quiescent.
final class IdleCaretTextView: NSTextView {
    private var caretParked = false
    private var parkWorkItem: DispatchWorkItem?
    static let idleParkSeconds: TimeInterval = 10

    override func updateInsertionPointStateAndRestartTimer(_ restartFlag: Bool) {
        super.updateInsertionPointStateAndRestartTimer(caretParked ? false : restartFlag)
    }

    /// Split out because Swift will not allow `super` inside a closure that
    /// explicitly captures self.
    private func parkCaretNow() {
        caretParked = true
        super.updateInsertionPointStateAndRestartTimer(false)
    }

    private func armPark() {
        parkWorkItem?.cancel()
        let item = DispatchWorkItem { [weak self] in self?.parkCaretNow() }
        parkWorkItem = item
        DispatchQueue.main.asyncAfter(deadline: .now() + Self.idleParkSeconds, execute: item)
    }

    private func wake() {
        if caretParked {
            caretParked = false
            super.updateInsertionPointStateAndRestartTimer(true)
        }
        armPark()
    }

    override func keyDown(with event: NSEvent) {
        wake()
        super.keyDown(with: event)
    }

    override func mouseDown(with event: NSEvent) {
        wake()
        super.mouseDown(with: event)
    }

    override func becomeFirstResponder() -> Bool {
        let ok = super.becomeFirstResponder()
        if ok { wake() }
        return ok
    }

    override func resignFirstResponder() -> Bool {
        parkWorkItem?.cancel()
        parkWorkItem = nil
        caretParked = true
        return super.resignFirstResponder()
    }
}

/// Owns the real NSTextView. SwiftUI's `TextEditor` is deliberately NOT used:
/// v2 needs tree-sitter highlighting, completion popovers anchored to the
/// caret, and find/replace, all of which need the AppKit text system.
final class SQLEditorController: NSViewController, NSTextViewDelegate {
    private(set) var textView: IdleCaretTextView!
    private let scroll = NSScrollView()
    var onSelectionChanged: (() -> Void)?

    override func loadView() {
        let tv = IdleCaretTextView(frame: .zero)
        tv.isRichText = false
        tv.isAutomaticQuoteSubstitutionEnabled = false
        tv.isAutomaticDashSubstitutionEnabled = false
        tv.isAutomaticTextReplacementEnabled = false
        tv.isAutomaticSpellingCorrectionEnabled = false
        tv.isContinuousSpellCheckingEnabled = false
        tv.isGrammarCheckingEnabled = false
        tv.allowsUndo = true
        tv.font = NSFont.monospacedSystemFont(ofSize: 12, weight: .regular)
        tv.textContainerInset = NSSize(width: 10, height: 10)
        tv.autoresizingMask = [.width]
        tv.isVerticallyResizable = true
        tv.isHorizontallyResizable = false
        tv.textContainer?.widthTracksTextView = true
        tv.minSize = NSSize(width: 0, height: 0)
        tv.maxSize = NSSize(
            width: CGFloat.greatestFiniteMagnitude, height: CGFloat.greatestFiniteMagnitude)
        tv.drawsBackground = true
        tv.backgroundColor = .textBackgroundColor
        tv.delegate = self
        textView = tv

        scroll.documentView = tv
        scroll.hasVerticalScroller = true
        scroll.borderType = .noBorder
        scroll.drawsBackground = false
        view = scroll
    }

    func setText(_ text: String) {
        loadViewIfNeeded()
        textView.string = text
        // `string =` leaves the selection wherever AppKit likes; put the caret at
        // the top so "run the statement under the caret" is deterministic.
        textView.setSelectedRange(NSRange(location: 0, length: 0))
    }

    var text: String {
        loadViewIfNeeded()
        return textView.string
    }

    func currentBlock() -> SQLBlock? {
        loadViewIfNeeded()
        return SQLBlocks.block(at: textView.selectedRange().location, in: textView.string)
    }

    func focus() { view.window?.makeFirstResponder(textView) }

    // Directive readout updates on user input only — nothing polls.
    func textDidChange(_ notification: Notification) { onSelectionChanged?() }
    func textViewDidChangeSelection(_ notification: Notification) { onSelectionChanged?() }
}

/// SwiftUI bridge. `makeNSViewController` hands back the model-owned instance so
/// SwiftUI re-evaluation never rebuilds the text system.
struct SQLEditorView: NSViewControllerRepresentable {
    let controller: SQLEditorController
    func makeNSViewController(context: Context) -> SQLEditorController { controller }
    func updateNSViewController(_ nsViewController: SQLEditorController, context: Context) {}
}
