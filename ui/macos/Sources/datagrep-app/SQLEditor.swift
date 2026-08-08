import AppKit
import DatagrepKit
import SwiftUI

/// What gets painted *behind* the text: the line the caret is on, the statement
/// ⌘↵ will run, and the bracket pair the caret is touching.
struct EditorDecorations {
    var line: NSRange?
    var block: NSRange?
    var bracketA: NSRange?
    var bracketB: NSRange?
}

/// NSTextView blinks its caret forever while focused. That alone is 2
/// full-window repaints/sec and fails P12/P19/P20/P21/P22 simultaneously, so
/// the caret stops blinking after 10 s of no input and resumes on the next
/// keystroke. The 10 s arm is a ONE-SHOT `asyncAfter`, not a repeating
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

    // MARK: - decorations

    private(set) var decorations = EditorDecorations()

    /// Swaps the decorations and invalidates only the union of what moved —
    /// a caret move must not repaint the whole editor, which is the same
    /// discipline the parked caret above exists to enforce.
    func setDecorations(_ new: EditorDecorations) {
        let old = decorations
        decorations = new
        var dirty = NSRect.zero
        func add(_ r: NSRect?) {
            guard let r, !r.isEmpty else { return }
            dirty = dirty.isEmpty ? r : dirty.union(r)
        }
        for r in [old.line, old.block, new.line, new.block] { add(r.flatMap(bandRect)) }
        for r in [old.bracketA, old.bracketB, new.bracketA, new.bracketB] {
            add(r.flatMap(glyphRect))
        }
        if dirty.isEmpty {
            needsDisplay = true
        } else {
            setNeedsDisplay(dirty.insetBy(dx: -6, dy: -6))
        }
    }

    /// A full-width band covering every line fragment of `range`.
    private func bandRect(_ range: NSRange) -> NSRect? {
        guard let lm = layoutManager, let tc = textContainer, let ts = textStorage else {
            return nil
        }
        guard range.location >= 0, NSMaxRange(range) <= ts.length else { return nil }
        let glyphs = lm.glyphRange(forCharacterRange: range, actualCharacterRange: nil)
        var r = lm.boundingRect(forGlyphRange: glyphs, in: tc)
        if r.height <= 0 {
            // Empty line: the bounding rect of a zero-glyph range is empty, so
            // fall back to the fragment the caret would sit in.
            let g = min(glyphs.location, max(0, lm.numberOfGlyphs - 1))
            guard lm.numberOfGlyphs > 0 else { return nil }
            r = lm.lineFragmentRect(forGlyphAt: g, effectiveRange: nil)
        }
        guard r.height > 0 else { return nil }
        return NSRect(
            x: 0, y: r.origin.y + textContainerInset.height,
            width: bounds.width, height: r.height)
    }

    private func glyphRect(_ range: NSRange) -> NSRect? {
        guard let lm = layoutManager, let tc = textContainer, let ts = textStorage else {
            return nil
        }
        guard range.location >= 0, NSMaxRange(range) <= ts.length else { return nil }
        let glyphs = lm.glyphRange(forCharacterRange: range, actualCharacterRange: nil)
        let r = lm.boundingRect(forGlyphRange: glyphs, in: tc)
        guard r.width > 0, r.height > 0 else { return nil }
        return r.offsetBy(dx: textContainerInset.width, dy: textContainerInset.height)
    }

    override func drawBackground(in rect: NSRect) {
        super.drawBackground(in: rect)

        // The block first, so the current line reads as a stronger layer on top
        // of it rather than fighting it.
        if let block = decorations.block, let r = bandRect(block), r.intersects(rect) {
            SQLTheme.currentBlock.setFill()
            r.fill()
            // A 2 pt accent rule down the left edge: the subtle fill alone is
            // easy to miss, and "what will ⌘↵ run" should never be a guess.
            NSColor.controlAccentColor.withAlphaComponent(0.45).setFill()
            NSRect(x: 0, y: r.origin.y, width: 2, height: r.height).fill()
        }

        if let line = decorations.line, let r = bandRect(line), r.intersects(rect) {
            SQLTheme.currentLine.setFill()
            r.fill()
        }

        for br in [decorations.bracketA, decorations.bracketB] {
            guard let br, let r = glyphRect(br), r.intersects(rect) else { continue }
            SQLTheme.bracketMatch.setFill()
            NSBezierPath(roundedRect: r.insetBy(dx: -1, dy: 0), xRadius: 2, yRadius: 2).fill()
        }
    }
}

/// Hosts the tab bar and the scroll view, and is where ⌘T / ⌘W / ⌘S / ⌘1–9 are
/// caught. Key equivalents are handled here rather than in the main menu
/// because `AppDelegate` owns the menu and this file does not — see the
/// integration note on `SQLEditorController`.
final class EditorContainerView: NSView {
    var keyHandler: ((NSEvent) -> Bool)?

    override func performKeyEquivalent(with event: NSEvent) -> Bool {
        if keyHandler?(event) == true { return true }
        return super.performKeyEquivalent(with: event)
    }
}

/// Owns the real NSTextView. SwiftUI's `TextEditor` is deliberately NOT used:
/// this needs syntax highlighting driven off `NSTextStorage`, background
/// decoration behind the glyphs, and later completion popovers anchored to the
/// caret — all of which need the AppKit text system.
///
/// ## Integration points (hooks this controller offers; nothing else edits it)
///
/// `AppModel` already drives everything it needs through `setText`, `text`,
/// `currentBlock()` and `onSelectionChanged`, and the connection binding is
/// delivered through `currentBlock()`'s directives — so tabs work with no
/// change to `AppModel`. Two optional hooks make the picker complete:
///
/// ```swift
/// // in AppModel.boot(), after reloadProfiles():
/// editor.profilesProvider = { [weak self] in
///     (self?.roots ?? []).map { EditorConnectionOption(name: $0.name, driver: $0.driver) }
/// }
/// editor.onConnectionChanged = { [weak self] name in
///     if let name { self?.selectProfile(name) }
/// }
/// ```
///
/// Without them the picker still works, it just cannot list profiles it has
/// never seen. `reloadProfiles()` should also call `editor.refreshConnections()`.
final class SQLEditorController: NSViewController, NSTextViewDelegate {
    private(set) var textView: IdleCaretTextView!
    private let scroll = NSScrollView()
    private let highlighter = SQLHighlighter()
    private let store = SavedQueryStore()

    let tabs = EditorTabsModel()
    private var tabBarHost: NSHostingView<EditorTabBar>!

    var onSelectionChanged: (() -> Void)?

    /// Supplies the connection picker. See the class doc for the two lines that
    /// wire this up in `AppModel`.
    var profilesProvider: (() -> [EditorConnectionOption])?
    /// Fired when the user binds the active tab to a profile (`nil` = follow the
    /// window). Lets the window's own connection UI follow the tab.
    var onConnectionChanged: ((String?) -> Void)?

    /// The active tab's binding, or nil when it follows the window.
    var activeConnection: String? { tabs.active?.connection }

    private var didRestoreSession = false
    private var consumedInitialSetText = false
    private var autosaveItem: DispatchWorkItem?

    // MARK: - view

    override func loadView() {
        // The text system is built by hand rather than letting NSTextView make
        // its own: an explicit NSTextContainer selects TextKit 1, whose
        // NSLayoutManager geometry is what the background decorations and the
        // visible-range highlighting both need.
        let storage = NSTextStorage()
        let layout = NSLayoutManager()
        storage.addLayoutManager(layout)
        let container = NSTextContainer(
            size: NSSize(width: 0, height: CGFloat.greatestFiniteMagnitude))
        container.widthTracksTextView = true
        layout.addTextContainer(container)

        let tv = IdleCaretTextView(
            frame: NSRect(x: 0, y: 0, width: 600, height: 200), textContainer: container)
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
        tv.minSize = NSSize(width: 0, height: 0)
        tv.maxSize = NSSize(
            width: CGFloat.greatestFiniteMagnitude, height: CGFloat.greatestFiniteMagnitude)
        tv.drawsBackground = true
        tv.backgroundColor = .textBackgroundColor
        tv.delegate = self
        textView = tv

        highlighter.font = tv.font ?? NSFont.monospacedSystemFont(ofSize: 12, weight: .regular)
        highlighter.visibleRangeProvider = { [weak self] in self?.visibleCharacterRange() ?? NSRange() }
        highlighter.attach(to: storage)

        scroll.documentView = tv
        scroll.hasVerticalScroller = true
        scroll.borderType = .noBorder
        scroll.drawsBackground = false
        scroll.translatesAutoresizingMaskIntoConstraints = false

        // Scrolling re-attributes newly exposed lines. Event-driven — when the
        // view is still, this fires zero times.
        scroll.contentView.postsBoundsChangedNotifications = true
        NotificationCenter.default.addObserver(
            self, selector: #selector(clipViewDidScroll),
            name: NSView.boundsDidChangeNotification, object: scroll.contentView)

        wireTabCommands()
        tabBarHost = NSHostingView(rootView: EditorTabBar(model: tabs))
        tabBarHost.translatesAutoresizingMaskIntoConstraints = false

        let container2 = EditorContainerView()
        container2.keyHandler = { [weak self] e in self?.handleKeyEquivalent(e) ?? false }
        container2.addSubview(tabBarHost)
        container2.addSubview(scroll)
        NSLayoutConstraint.activate([
            tabBarHost.topAnchor.constraint(equalTo: container2.topAnchor),
            tabBarHost.leadingAnchor.constraint(equalTo: container2.leadingAnchor),
            tabBarHost.trailingAnchor.constraint(equalTo: container2.trailingAnchor),
            tabBarHost.heightAnchor.constraint(equalToConstant: 30),
            scroll.topAnchor.constraint(equalTo: tabBarHost.bottomAnchor),
            scroll.leadingAnchor.constraint(equalTo: container2.leadingAnchor),
            scroll.trailingAnchor.constraint(equalTo: container2.trailingAnchor),
            scroll.bottomAnchor.constraint(equalTo: container2.bottomAnchor),
        ])
        // Order matters: `view` must be assigned before anything can call back
        // out. Restoring a tab fires `onSelectionChanged`, and AppModel's
        // handler calls `currentBlock()`, which calls `loadViewIfNeeded()` — if
        // `isViewLoaded` were still false at that moment, `loadView()` would
        // re-enter and build a second text system.
        view = container2

        restoreSession()

        // Unsaved SQL must survive quit as reliably as it survives a crash.
        NotificationCenter.default.addObserver(
            self, selector: #selector(persistEverything),
            name: NSApplication.willTerminateNotification, object: nil)
        NotificationCenter.default.addObserver(
            self, selector: #selector(persistEverything),
            name: NSApplication.willResignActiveNotification, object: nil)
    }

    deinit { NotificationCenter.default.removeObserver(self) }

    /// One screen of overscan either side, so a flick-scroll lands on lines that
    /// are already coloured instead of colouring them in front of the user.
    private func visibleCharacterRange() -> NSRange {
        guard let lm = textView?.layoutManager, let tc = textView?.textContainer else {
            return NSRange(location: 0, length: 0)
        }
        var rect = textView.visibleRect
        guard rect.height > 0 else {
            return NSRange(location: 0, length: min(4096, textView.string.utf16.count))
        }
        rect = rect.insetBy(dx: 0, dy: -rect.height)
        rect.origin.y -= textView.textContainerInset.height
        let glyphs = lm.glyphRange(forBoundingRect: rect, in: tc)
        return lm.characterRange(forGlyphRange: glyphs, actualGlyphRange: nil)
    }

    @objc private func clipViewDidScroll() {
        highlighter.refreshVisible()
    }

    // MARK: - external API (unchanged surface for AppModel)

    /// Replaces the *active tab's* content. `AppModel.preview(node)` uses this
    /// to drop a generated `SELECT` into the editor, which is exactly right.
    ///
    /// The one exception is the very first call: `AppModel.boot()` seeds a
    /// welcome snippet, and a restored session must beat boilerplate — losing
    /// the query you were half way through writing because the app restarted is
    /// the failure this whole tab store exists to prevent. Once that first call
    /// has been absorbed, every later `setText` behaves normally.
    func setText(_ text: String) {
        loadViewIfNeeded()
        if didRestoreSession && !consumedInitialSetText {
            consumedInitialSetText = true
            return
        }
        consumedInitialSetText = true
        guard let tab = tabs.active else { return }
        tab.text = text
        tab.selectedRange = NSRange(location: 0, length: 0)
        if tab.name != nil { tab.isDirty = true }
        load(tab)
        scheduleAutosave()
    }

    var text: String {
        loadViewIfNeeded()
        return textView.string
    }

    /// The statement under the caret, with the tab's connection folded into its
    /// directives.
    ///
    /// This is how a tab's connection binding reaches the engine without any
    /// other file changing: `AppModel.execute()` already resolves the profile as
    /// `directives.connection ?? activeProfile`. An explicit `-- @connection` in
    /// the SQL still wins, because text the user wrote outranks a picker.
    func currentBlock() -> SQLBlock? {
        loadViewIfNeeded()
        let ns = textView.string as NSString
        guard ns.length > 0 else { return nil }
        let caret = min(textView.selectedRange().location, ns.length)

        var range = highlighter.blockRange(containing: caret)
        var body = ns.substring(with: range)
        if body.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty, range.location > 0 {
            // Caret parked in the whitespace after the final `;` — run the
            // statement that just ended, same fallback the old splitter had.
            range = highlighter.blockRange(containing: range.location - 1)
            body = ns.substring(with: range)
        }
        guard !body.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty else { return nil }

        var directives = SQLBlocks.directives(in: body)
        if directives.connection == nil, let bound = tabs.active?.connection, !bound.isEmpty {
            directives.connection = bound
        }
        return SQLBlock(
            text: body, range: range.location..<NSMaxRange(range), directives: directives)
    }

    func focus() { view.window?.makeFirstResponder(textView) }

    /// Re-reads the profile list. Call after profiles are added or removed.
    func refreshConnections() {
        tabs.connections = profilesProvider?() ?? fallbackConnections()
        refreshSavedList()
    }

    /// Named queries on disk that are not already open in a tab.
    private func refreshSavedList() {
        let open = Set(tabs.tabs.map(\.id))
        tabs.savedQueries = store.allSaved().filter { !open.contains($0.id) }
    }

    /// Reopens a saved query, or just focuses it when it is already open.
    private func openSaved(_ record: SavedQueryRecord) {
        loadViewIfNeeded()
        if let existing = tabs.tabs.first(where: { $0.id == record.id }) {
            activate(existing)
            return
        }
        guard let text = store.text(for: record) else { return }
        flushActive()
        let tab = EditorTab(
            id: record.id, name: record.name, connection: record.connection, text: text,
            selectedRange: NSRange(location: record.cursorLocation, length: record.cursorLength),
            isDirty: false)
        tabs.tabs.append(tab)
        tabs.activeID = tab.id
        load(tab)
        persistSession()
        refreshSavedList()
        focus()
    }

    /// With no provider installed the picker still has to name the profiles a
    /// restored tab is bound to, or a binding would display as a blank chip.
    private func fallbackConnections() -> [EditorConnectionOption] {
        let names = Set(tabs.tabs.compactMap(\.connection))
        return names.sorted().map { EditorConnectionOption(name: $0, driver: "") }
    }

    // MARK: - tab commands

    private func wireTabCommands() {
        tabs.onActivate = { [weak self] tab in self?.activate(tab) }
        tabs.onClose = { [weak self] tab in self?.close(tab) }
        tabs.onNew = { [weak self] in self?.newTab() }
        tabs.onSave = { [weak self] _ in self?.saveActiveTab() }
        tabs.onOpenSaved = { [weak self] record in self?.openSaved(record) }
        tabs.onBind = { [weak self] tab, name in
            guard let self else { return }
            tab.connection = name
            self.persist(tab)
            self.onConnectionChanged?(name)
        }
    }

    @discardableResult
    func newTab() -> EditorTab {
        loadViewIfNeeded()
        flushActive()
        let tab = EditorTab()
        tab.untitledNumber = tabs.nextUntitledNumber()
        tabs.tabs.append(tab)
        tabs.activeID = tab.id
        load(tab)
        persist(tab)
        persistSession()
        refreshConnections()
        return tab
    }

    private func activate(_ tab: EditorTab) {
        guard tab.id != tabs.activeID else {
            focus()
            return
        }
        flushActive()
        tabs.activeID = tab.id
        load(tab)
        persistSession()
        focus()
    }

    private func close(_ tab: EditorTab) {
        guard let idx = tabs.tabs.firstIndex(where: { $0.id == tab.id }) else { return }
        if tab.id == tabs.activeID { flushActive() }
        tabs.tabs.remove(at: idx)
        // A named query is a file the user asked us to keep, so closing its tab
        // must not delete it — it drops out of the session and stays in the
        // saved list. Only an untitled scratch tab is discarded, because
        // closing it is the only way the user has to say "throw this away".
        if tab.name == nil { store.delete(tab.record) }

        if tabs.tabs.isEmpty {
            // Never leave the user staring at no editor at all.
            newTab()
            return
        }
        if tab.id == tabs.activeID {
            let next = tabs.tabs[min(idx, tabs.tabs.count - 1)]
            tabs.activeID = next.id
            load(next)
        }
        persistSession()
        refreshSavedList()
    }

    private func selectTab(at index: Int) -> Bool {
        guard index >= 0, index < tabs.tabs.count else { return false }
        activate(tabs.tabs[index])
        return true
    }

    private func saveActiveTab() {
        loadViewIfNeeded()
        guard let tab = tabs.active else { return }
        flushActive()
        if tab.name == nil {
            guard let name = promptForName(suggestion: suggestedName(for: tab.text)) else { return }
            let previous = tab.record
            tab.name = name
            // Only after the new pair is safely on disk.
            store.save(tab.record, text: tab.text)
            store.delete(previous)
        }
        tab.isDirty = false
        store.save(tab.record, text: tab.text)
        persistSession()
        refreshSavedList()
    }

    /// First few words of the first statement, so the dialog opens with
    /// something better than "Untitled".
    private func suggestedName(for sql: String) -> String {
        let words =
            sql
            .split(whereSeparator: { $0.isWhitespace })
            .filter { !$0.hasPrefix("--") }
            .prefix(4)
            .joined(separator: " ")
        let trimmed = words.trimmingCharacters(in: CharacterSet(charactersIn: " ;,"))
        return trimmed.isEmpty ? "query" : String(trimmed.prefix(48))
    }

    private func promptForName(suggestion: String) -> String? {
        let alert = NSAlert()
        alert.messageText = "Save Query"
        alert.informativeText =
            "Saved as a plain .sql file in ~/Library/Application Support/datagrep/tabs — readable in any editor, and committable to git."
        alert.addButton(withTitle: "Save")
        alert.addButton(withTitle: "Cancel")
        let field = NSTextField(frame: NSRect(x: 0, y: 0, width: 280, height: 24))
        field.stringValue = suggestion
        field.placeholderString = "query name"
        alert.accessoryView = field
        alert.window.initialFirstResponder = field
        guard alert.runModal() == .alertFirstButtonReturn else { return nil }
        let name = field.stringValue.trimmingCharacters(in: .whitespacesAndNewlines)
        return name.isEmpty ? nil : name
    }

    // MARK: - document swap

    /// Copies the text view's state back into the tab that owns it.
    private func flushActive() {
        guard let tab = tabs.active, isViewLoaded, textView != nil else { return }
        tab.text = textView.string
        tab.selectedRange = textView.selectedRange()
    }

    private func load(_ tab: EditorTab) {
        // A wholesale replacement carries no incremental information, so the
        // highlighter is told to ignore the edit and re-seed once instead of
        // lexing the new document twice.
        highlighter.isSuspended = true
        textView.string = tab.text
        highlighter.isSuspended = false
        highlighter.documentDidChangeWholesale()

        let length = (textView.string as NSString).length
        let loc = min(max(0, tab.selectedRange.location), length)
        let len = min(max(0, tab.selectedRange.length), length - loc)
        textView.setSelectedRange(NSRange(location: loc, length: len))
        textView.scrollRangeToVisible(NSRange(location: loc, length: 0))
        // Undo is per-document; carrying a previous tab's edits into this one
        // would let ⌘Z type another tab's text into this buffer.
        textView.undoManager?.removeAllActions()

        updateDecorations()
        onSelectionChanged?()
    }

    // MARK: - persistence

    private func restoreSession() {
        let loaded = store.load()
        for (record, text) in loaded.tabs {
            let tab = EditorTab(
                id: record.id, name: record.name, connection: record.connection, text: text,
                selectedRange: NSRange(
                    location: record.cursorLocation, length: record.cursorLength),
                isDirty: record.isDirty)
            tabs.tabs.append(tab)
        }
        // Renumber untitled tabs in the order they were restored.
        var n = 1
        for tab in tabs.tabs where tab.name == nil {
            tab.untitledNumber = n
            n += 1
        }

        if tabs.tabs.isEmpty {
            let tab = EditorTab()
            tab.untitledNumber = 1
            tabs.tabs.append(tab)
            tabs.activeID = tab.id
        } else {
            didRestoreSession = true
            tabs.activeID = loaded.activeID ?? tabs.tabs.first?.id
        }
        if let active = tabs.active { load(active) }
        refreshConnections()
    }

    /// One-shot, cancelled and re-armed on every edit — the same discipline as
    /// the parked caret. Nothing polls; a settled editor schedules nothing.
    private func scheduleAutosave() {
        autosaveItem?.cancel()
        let item = DispatchWorkItem { [weak self] in
            guard let self, let tab = self.tabs.active else { return }
            self.flushActive()
            self.persist(tab)
        }
        autosaveItem = item
        DispatchQueue.main.asyncAfter(deadline: .now() + 1.2, execute: item)
    }

    private func persist(_ tab: EditorTab) {
        store.save(tab.record, text: tab.text)
    }

    private func persistSession() {
        store.saveSession(
            EditorSession(order: tabs.tabs.map(\.id), activeID: tabs.activeID))
    }

    @objc private func persistEverything() {
        autosaveItem?.cancel()
        autosaveItem = nil
        guard isViewLoaded else { return }
        flushActive()
        for tab in tabs.tabs { persist(tab) }
        persistSession()
    }

    // MARK: - key equivalents

    private func handleKeyEquivalent(_ event: NSEvent) -> Bool {
        // Caps Lock and the function/numeric-pad bits ride along on ordinary
        // keystrokes; only the intent-carrying modifiers may differ from ⌘.
        let mods = event.modifierFlags
            .intersection(.deviceIndependentFlagsMask)
            .subtracting([.capsLock, .function, .numericPad])
        guard mods == .command, let key = event.charactersIgnoringModifiers?.lowercased() else {
            return false
        }
        switch key {
        case "t":
            newTab()
            focus()
            return true
        case "s":
            saveActiveTab()
            return true
        case "w":
            // Only claimed while the editor has focus, so File ▸ Close Window
            // (⌘W, owned by AppDelegate's menu) still works everywhere else.
            guard view.window?.firstResponder === textView, let tab = tabs.active else {
                return false
            }
            close(tab)
            focus()
            return true
        default:
            if let n = Int(key), (1...9).contains(n) { return selectTab(at: n - 1) }
            return false
        }
    }

    // MARK: - NSTextViewDelegate

    func textDidChange(_ notification: Notification) {
        if let tab = tabs.active, tab.name != nil { tab.isDirty = true }
        // Safe here: the edit cycle has completed, so asking the layout manager
        // for the visible range will not re-enter it.
        highlighter.refreshVisible()
        updateDecorations()
        scheduleAutosave()
        onSelectionChanged?()
    }

    func textViewDidChangeSelection(_ notification: Notification) {
        updateDecorations()
        onSelectionChanged?()
    }

    private func updateDecorations() {
        guard isViewLoaded, textView != nil else { return }
        let caret = textView.selectedRange().location
        var d = EditorDecorations()
        d.line = highlighter.lineRange(containing: caret)
        d.block = highlighter.blockRange(containing: caret)
        if let (a, b) = highlighter.bracketPair(at: caret) {
            d.bracketA = a
            d.bracketB = b
        }
        textView.setDecorations(d)
    }
}

/// SwiftUI bridge. `makeNSViewController` hands back the model-owned instance so
/// SwiftUI re-evaluation never rebuilds the text system.
struct SQLEditorView: NSViewControllerRepresentable {
    let controller: SQLEditorController
    func makeNSViewController(context: Context) -> SQLEditorController { controller }
    func updateNSViewController(_ nsViewController: SQLEditorController, context: Context) {}
}
