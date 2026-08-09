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
    /// The welcome state, laid over the text view. Toggled by `isHidden` rather
    /// than added and removed, so the text system is built exactly once and the
    /// pane never reflows when the last editor closes.
    private var welcomeHost: NSHostingView<EditorWelcomeState>!

    /// Every open editor, across every connection. `tabs.tabs` is the slice of
    /// this belonging to `tabs.scope` — editors are per connection, so the tab
    /// bar shows one connection's editors and switching connection switches the
    /// whole bar.
    private var allTabs: [EditorTab] = []

    var onSelectionChanged: (() -> Void)?

    /// Supplies the connection picker. See the class doc for the two lines that
    /// wire this up in `AppModel`.
    var profilesProvider: (() -> [EditorConnectionOption])?
    /// Fired when the user binds the active tab to a profile (`nil` = follow the
    /// window). Lets the window's own connection UI follow the tab.
    var onConnectionChanged: ((String?) -> Void)?

    /// The active tab's binding, or nil when it follows the window.
    var activeConnection: String? { tabs.active?.connection }

    /// True when a previous session's editors came back. Nothing gates on it
    /// any more — `setText` no longer has boilerplate to defend against — but
    /// it is what `restoredEditorCount` reports for the status line.
    private(set) var didRestoreSession = false
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
        welcomeHost = NSHostingView(rootView: EditorWelcomeState(model: tabs))
        welcomeHost.translatesAutoresizingMaskIntoConstraints = false
        // An NSHostingView installs its own constraints to defend its SwiftUI
        // content's ideal size. That is what made this pane grow to fit the
        // welcome state and the tab bar instead of the split's height — the
        // same trap AppDelegate already sidesteps with
        // `NSHostingController.sizingOptions = []` for the window. Both hosts
        // here are laid out entirely by the constraints below, so neither
        // should contribute a size of its own.
        welcomeHost.sizingOptions = []
        tabBarHost.sizingOptions = []
        // An NSHostingView defends its SwiftUI content's intrinsic size at
        // `required` priority, and a hidden view still takes part in Auto
        // Layout. Pinned to all four of the scroll view's edges, that let the
        // welcome state's own height inflate the scroll view and with it this
        // whole controller's view — measured at 1151 pt inside a ~490 pt pane,
        // which pushed the tab bar off the top and left the visible area
        // showing the middle of an empty text view. It must never be the thing
        // that decides how tall the editor is.
        for axis in [NSLayoutConstraint.Orientation.horizontal, .vertical] {
            welcomeHost.setContentCompressionResistancePriority(.defaultLow, for: axis)
            welcomeHost.setContentHuggingPriority(.defaultLow, for: axis)
        }

        let container2 = EditorContainerView()
        container2.keyHandler = { [weak self] e in self?.handleKeyEquivalent(e) ?? false }
        // NSScrollView's fitting size follows its document view, and an
        // NSTextView's grows with its content, so this container reported a
        // fitting height far larger than the window. VSplitView honours a
        // child's ideal size, so SwiftUI laid the editor pane out at 1151 pt
        // inside a 673 pt window: the tab bar ended up above the visible area
        // and the pane showed the middle of an empty text view, with nothing
        // to click. The editor's height is the split's decision, never the
        // text's.
        for axis in [NSLayoutConstraint.Orientation.horizontal, .vertical] {
            scroll.setContentCompressionResistancePriority(.defaultLow, for: axis)
            scroll.setContentHuggingPriority(.defaultLow, for: axis)
            container2.setContentCompressionResistancePriority(.defaultLow, for: axis)
            container2.setContentHuggingPriority(.defaultLow, for: axis)
        }
        container2.addSubview(tabBarHost)
        container2.addSubview(scroll)
        container2.addSubview(welcomeHost)
        NSLayoutConstraint.activate([
            tabBarHost.topAnchor.constraint(equalTo: container2.topAnchor),
            tabBarHost.leadingAnchor.constraint(equalTo: container2.leadingAnchor),
            tabBarHost.trailingAnchor.constraint(equalTo: container2.trailingAnchor),
            tabBarHost.heightAnchor.constraint(equalToConstant: 30),
            scroll.topAnchor.constraint(equalTo: tabBarHost.bottomAnchor),
            scroll.leadingAnchor.constraint(equalTo: container2.leadingAnchor),
            scroll.trailingAnchor.constraint(equalTo: container2.trailingAnchor),
            scroll.bottomAnchor.constraint(equalTo: container2.bottomAnchor),
            welcomeHost.topAnchor.constraint(equalTo: scroll.topAnchor),
            welcomeHost.leadingAnchor.constraint(equalTo: scroll.leadingAnchor),
            welcomeHost.trailingAnchor.constraint(equalTo: scroll.trailingAnchor),
            welcomeHost.bottomAnchor.constraint(equalTo: scroll.bottomAnchor),
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
    /// With no editor open there is nothing to replace, and one is made — for
    /// the scoped connection — rather than the text being dropped on the floor.
    /// Nothing seeds boilerplate through here any more: the welcome state says
    /// what the sample statement used to, without putting text into a document
    /// the user then has to delete.
    func setText(_ text: String) {
        loadViewIfNeeded()
        let tab = tabs.active ?? newTab()
        tab.text = text
        tab.selectedRange = NSRange(location: 0, length: 0)
        tab.isDirty = true
        load(tab)
        scheduleAutosave()
    }

    // MARK: - scope: which connection's editors are showing

    /// Show `connection`'s editors.
    ///
    /// Called whenever the window's connection changes. The outgoing scope's
    /// frontmost tab is remembered, so coming back to a connection brings back
    /// the editor that was in front of it — not whichever tab was last touched
    /// anywhere in the app.
    func setScope(_ connection: String?) {
        loadViewIfNeeded()
        let next = (connection?.isEmpty ?? true) ? nil : connection
        guard next != tabs.scope else { return }
        flushActive()
        rememberActive()
        tabs.scope = next
        applyScope()
        persistSession()
    }

    /// Republish the visible tab slice and load whichever of them is frontmost.
    private func applyScope() {
        let scope = tabs.scope
        tabs.tabs = allTabs.filter { $0.connection == scope }
        let remembered = activeByScope[scope ?? EditorSession.unbound]
        let next = tabs.tabs.first { $0.id == remembered } ?? tabs.tabs.first
        tabs.activeID = next?.id
        if let next {
            load(next)
        } else {
            // Nothing to show. The text view is emptied so a stale buffer from
            // the previous connection cannot be read as this one's, and the
            // welcome state covers it.
            highlighter.isSuspended = true
            textView.string = ""
            highlighter.isSuspended = false
            highlighter.documentDidChangeWholesale()
            textView.undoManager?.removeAllActions()
            onSelectionChanged?()
        }
        updateWelcomeState()
        refreshSavedList()
    }

    private func rememberActive() {
        guard let id = tabs.activeID else { return }
        activeByScope[tabs.scope ?? EditorSession.unbound] = id
    }

    /// Frontmost tab per connection. Mirrored into `session.json` so it
    /// survives a relaunch.
    private var activeByScope: [String: String] = [:]


    private func updateWelcomeState() {
        welcomeHost?.isHidden = !tabs.tabs.isEmpty
        // A hidden overlay still sits in front of the text view for hit
        // testing in some releases; taking it out of the responder path
        // entirely is the only version that cannot swallow a click.
        scroll.isHidden = tabs.tabs.isEmpty
    }

    /// Every editor that belongs to `connection`, open or closed — what the
    /// connection's "Open Editor" menu lists.
    func editors(for connection: String) -> [SavedQueryRecord] {
        var seen = Set<String>()
        var out: [SavedQueryRecord] = []
        for tab in allTabs where tab.connection == connection {
            if seen.insert(tab.id).inserted { out.append(tab.record) }
        }
        for record in store.allRecords()
        where record.connection == connection && seen.insert(record.id).inserted {
            out.append(record)
        }
        return out
    }

    /// Open (or focus) one editor, switching scope to the connection it belongs
    /// to so the tab bar is showing the right set when it appears.
    func openEditor(_ record: SavedQueryRecord) {
        loadViewIfNeeded()
        if record.connection != tabs.scope { setScope(record.connection) }
        openSaved(record)
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

    /// Closed editors that could be reopened. Scoped like everything else — the
    /// menu on a connection must not offer another connection's queries.
    private func refreshSavedList() {
        let open = Set(allTabs.map(\.id))
        tabs.savedQueries =
            store.allRecords()
            .filter { !open.contains($0.id) && $0.connection == tabs.scope }
            .sorted { ($0.name ?? "\u{10FFFF}") < ($1.name ?? "\u{10FFFF}") }
    }

    /// Reopens a saved query, or just focuses it when it is already open.
    private func openSaved(_ record: SavedQueryRecord) {
        loadViewIfNeeded()
        if let existing = allTabs.first(where: { $0.id == record.id }) {
            activate(existing)
            return
        }
        guard let text = store.text(for: record) else { return }
        flushActive()
        let tab = EditorTab(
            id: record.id, name: record.name, connection: record.connection, text: text,
            selectedRange: NSRange(location: record.cursorLocation, length: record.cursorLength),
            isDirty: false)
        if tab.name == nil { tab.untitledNumber = nextUntitledNumber(in: tab.connection) }
        allTabs.append(tab)
        tabs.tabs = allTabs.filter { $0.connection == tabs.scope }
        tabs.activeID = tab.id
        activeByScope[tab.connection ?? EditorSession.unbound] = tab.id
        load(tab)
        updateWelcomeState()
        persistSession()
        refreshSavedList()
        focus()
    }

    /// Lowest unused number **within one connection**, so every connection
    /// starts at "Untitled 1" rather than continuing a global count.
    private func nextUntitledNumber(in connection: String?) -> Int {
        let used = Set(
            allTabs.filter { $0.name == nil && $0.connection == connection }.map(\.untitledNumber))
        var n = 1
        while used.contains(n) { n += 1 }
        return n
    }

    /// With no provider installed the picker still has to name the profiles a
    /// restored tab is bound to, or a binding would display as a blank chip.
    private func fallbackConnections() -> [EditorConnectionOption] {
        let names = Set(allTabs.compactMap(\.connection))
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
            tab.untitledNumber =
                tab.name == nil ? self.nextUntitledNumber(in: name) : tab.untitledNumber
            self.persist(tab)
            // Rebinding moves the editor into another connection's bar, so the
            // scope follows it — otherwise the tab the user just retargeted
            // would vanish from in front of them.
            self.activeByScope[name ?? EditorSession.unbound] = tab.id
            self.tabs.scope = name
            self.applyScope()
            self.persistSession()
            self.onConnectionChanged?(name)
        }
    }

    /// A new editor **for the scoped connection**. Empty: an editor is a blank
    /// page, and a sample statement in the wrong dialect is worse than none.
    @discardableResult
    func newTab(connection: String? = nil) -> EditorTab {
        loadViewIfNeeded()
        flushActive()
        // An explicit connection also moves the scope, so the tab appears in
        // the bar the user is looking at instead of silently in another one.
        if let connection, connection != tabs.scope { setScope(connection) }
        let tab = EditorTab(connection: tabs.scope)
        tab.untitledNumber = nextUntitledNumber(in: tab.connection)
        allTabs.append(tab)
        tabs.tabs = allTabs.filter { $0.connection == tabs.scope }
        tabs.activeID = tab.id
        activeByScope[tabs.scope ?? EditorSession.unbound] = tab.id
        load(tab)
        updateWelcomeState()
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
        rememberActive()
        load(tab)
        persistSession()
        focus()
    }

    private func close(_ tab: EditorTab) {
        // Closing an unsaved scratch tab is the ONLY action that destroys typed
        // SQL: quitting keeps everything (the session is restored verbatim,
        // dirty flags included) and closing a *named* query only drops it from
        // the session. So this is the one place a confirmation belongs, and
        // there is deliberately none on quit.
        if tab.isDirty, tab.name == nil,
            !tab.text.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty
        {
            confirmDiscard(tab)
            return
        }
        performClose(tab)
    }

    private func performClose(_ tab: EditorTab) {
        guard let all = allTabs.firstIndex(where: { $0.id == tab.id }) else { return }
        let idx = tabs.tabs.firstIndex { $0.id == tab.id } ?? 0
        let wasActive = tab.id == tabs.activeID
        if wasActive { flushActive() }
        allTabs.remove(at: all)
        // A named query is a file the user asked us to keep, so closing its tab
        // must not delete it — it drops out of the session and stays in the
        // saved list. Only an untitled scratch tab is discarded, because
        // closing it is the only way the user has to say "throw this away".
        if tab.name == nil { store.delete(tab.record) }

        tabs.tabs = allTabs.filter { $0.connection == tabs.scope }
        if wasActive {
            // Closing the last editor of a connection leaves the welcome state,
            // not a manufactured replacement tab. "Nothing open" is a state the
            // user asked for by closing it.
            let next = tabs.tabs.isEmpty ? nil : tabs.tabs[min(idx, tabs.tabs.count - 1)]
            tabs.activeID = next?.id
            if let next {
                activeByScope[tabs.scope ?? EditorSession.unbound] = next.id
                load(next)
            } else {
                activeByScope.removeValue(forKey: tabs.scope ?? EditorSession.unbound)
                highlighter.isSuspended = true
                textView.string = ""
                highlighter.isSuspended = false
                highlighter.documentDidChangeWholesale()
                textView.undoManager?.removeAllActions()
                onSelectionChanged?()
            }
        }
        updateWelcomeState()
        persistSession()
        refreshSavedList()
    }

    /// Drop every editor belonging to a connection that has just been deleted.
    /// Their `.sql` files stay on disk — a removed connection is not a reason
    /// to destroy SQL someone wrote — but nothing is left pointing at a profile
    /// that no longer exists.
    func forgetEditors(of connection: String) {
        loadViewIfNeeded()
        allTabs.removeAll { $0.connection == connection }
        activeByScope.removeValue(forKey: connection)
        if tabs.scope == connection {
            tabs.scope = nil
            applyScope()
        } else {
            tabs.tabs = allTabs.filter { $0.connection == tabs.scope }
            updateWelcomeState()
        }
        persistSession()
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

    /// Asks before throwing away an untitled tab's SQL, offering to keep it as
    /// a saved query instead. Returns true if the caller should go ahead and
    /// close.
    ///
    /// "Save…" runs the normal save path, which prompts for a name; if the user
    /// backs out of *that* prompt the tab is left untouched rather than closed,
    /// because cancelling a name prompt is not consent to discard.
    private func confirmDiscard(_ tab: EditorTab) {
        let alert = NSAlert()
        alert.alertStyle = .warning
        alert.messageText = "Discard this query?"
        alert.informativeText =
            "\(tab.displayTitle) has not been saved. Closing the tab deletes it — "
            + "quitting datagrep would keep it."
        alert.addButton(withTitle: "Discard")
        alert.addButton(withTitle: "Cancel")
        alert.addButton(withTitle: "Save…")
        // Destructive default: make Return cancel, not discard.
        alert.buttons[0].keyEquivalent = ""
        alert.buttons[1].keyEquivalent = "\r"

        // A SHEET, not `runModal()`. This is reached from a SwiftUI button
        // inside the tab bar's hosting view, and spinning a nested modal loop
        // from inside SwiftUI's own event handling left the alert on screen
        // with every button dead — it could not be dismissed at all. The sheet
        // hands control back immediately and reports the choice later.
        let handle: (NSApplication.ModalResponse) -> Void = { [weak self] response in
            guard let self else { return }
            switch response {
            case .alertFirstButtonReturn:
                self.performClose(tab)
            case .alertThirdButtonReturn:
                self.activate(tab)
                self.saveActiveTab()
                // Only close if the name prompt actually completed; backing out
                // of it is not consent to discard.
                if tab.name != nil { self.performClose(tab) }
            default:
                break  // Cancel: the tab stays exactly as it was.
            }
        }
        if let window = view.window {
            alert.beginSheetModal(for: window, completionHandler: handle)
        } else {
            handle(alert.runModal())
        }
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

    /// Reopen what was open, and **make nothing up**.
    ///
    /// A first launch restores zero tabs and that is the end of it: the welcome
    /// state takes the pane until the user asks for an editor. The app used to
    /// manufacture an "Untitled 1" here and `AppModel.boot()` filled it with a
    /// SQLite sample — which is text nobody asked for, in a dialect that is
    /// wrong for every connection except one.
    private func restoreSession() {
        let loaded = store.load()
        for (record, text) in loaded.tabs {
            let tab = EditorTab(
                id: record.id, name: record.name, connection: record.connection, text: text,
                selectedRange: NSRange(
                    location: record.cursorLocation, length: record.cursorLength),
                isDirty: record.isDirty)
            allTabs.append(tab)
        }
        // Renumber untitled tabs per connection, in the order they were
        // restored, so two connections can each have an "Untitled 1".
        var counters: [String: Int] = [:]
        for tab in allTabs where tab.name == nil {
            let key = tab.connection ?? EditorSession.unbound
            let n = (counters[key] ?? 0) + 1
            counters[key] = n
            tab.untitledNumber = n
        }

        didRestoreSession = !allTabs.isEmpty
        activeByScope = loaded.session.activeByConnection
        tabs.scope = loaded.session.activeConnection
        applyScope()
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
        rememberActive()
        store.saveSession(
            EditorSession(
                order: allTabs.map(\.id), activeByConnection: activeByScope,
                activeConnection: tabs.scope))
    }

    @objc private func persistEverything() {
        autosaveItem?.cancel()
        autosaveItem = nil
        guard isViewLoaded else { return }
        flushActive()
        for tab in allTabs { persist(tab) }
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
        // Every tab, not just named ones. An untitled scratch tab is the case
        // that actually needs the marker: `close(_:)` deletes its record, so
        // closing it is the one action in the app that destroys typed SQL for
        // good. Gating this on `name != nil` meant the tab most at risk was the
        // only one that never showed it.
        tabs.active?.isDirty = true
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

    /// Take the height offered, never the one AppKit would ask for.
    ///
    /// Without this SwiftUI sizes the representable from the controller view's
    /// Auto Layout fitting size, which follows the `NSScrollView`, which
    /// follows the `NSTextView` — so the pane grew with the document. In a
    /// 673 pt window the editor was laid out 1151 pt tall, putting the tab bar
    /// above the visible area and leaving a dead, unclickable band where the
    /// editor should be. `VSplitView` decides how tall this pane is; the text
    /// inside it does not get a vote.
    func sizeThatFits(
        _ proposal: ProposedViewSize,
        nsViewController: SQLEditorController,
        context: Context
    ) -> CGSize? {
        guard let width = proposal.width, let height = proposal.height else { return nil }
        return CGSize(width: width, height: height)
    }
}
