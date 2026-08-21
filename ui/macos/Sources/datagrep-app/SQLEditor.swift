import AppKit
import DatagrepKit
import SwiftUI

struct EditorDecorations {
    var line: NSRange?
    var block: NSRange?
    var bracketA: NSRange?
    var bracketB: NSRange?
}

final class IdleCaretTextView: NSTextView {
    private var caretParked = false
    private var parkWorkItem: DispatchWorkItem?
    static let idleParkSeconds: TimeInterval = 10

    override func updateInsertionPointStateAndRestartTimer(_ restartFlag: Bool) {
        super.updateInsertionPointStateAndRestartTimer(caretParked ? false : restartFlag)
    }

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

        if let block = decorations.block, let r = bandRect(block), r.intersects(rect) {
            SQLTheme.currentBlock.setFill()
            r.fill()
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

final class EditorContainerView: NSView {
    var keyHandler: ((NSEvent) -> Bool)?

    override func performKeyEquivalent(with event: NSEvent) -> Bool {
        if keyHandler?(event) == true { return true }
        return super.performKeyEquivalent(with: event)
    }
}

private final class TabBarHostingView: NSHostingView<EditorTabBar> {
    override func acceptsFirstMouse(for event: NSEvent?) -> Bool { true }
}

final class SQLEditorController: NSViewController, NSTextViewDelegate {
    private(set) var textView: IdleCaretTextView!
    /// The document, held strongly.
    private var textStorage: NSTextStorage!
    private let scroll = NSScrollView()
    private let highlighter = SQLHighlighter()
    private let store = SavedQueryStore()

    let tabs = EditorTabsModel()
    private var tabBarHost: NSHostingView<EditorTabBar>!
    private var welcomeHost: NSHostingView<EditorWelcomeState>!

    private var allTabs: [EditorTab] = []

    var onSelectionChanged: (() -> Void)?

    var profilesProvider: (() -> [EditorConnectionOption])?
    var onConnectionChanged: ((String?) -> Void)?

    /// The active tab's binding, or nil when it follows the window.
    var activeConnection: String? { tabs.active?.connection }

    private(set) var didRestoreSession = false
    private var autosaveItem: DispatchWorkItem?

    private var isSwappingDocument = false

    // MARK: - view

    override func loadView() {
        let storage = NSTextStorage()
        textStorage = storage
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

        scroll.contentView.postsBoundsChangedNotifications = true
        NotificationCenter.default.addObserver(
            self, selector: #selector(clipViewDidScroll),
            name: NSView.boundsDidChangeNotification, object: scroll.contentView)

        wireTabCommands()
        tabBarHost = TabBarHostingView(rootView: EditorTabBar(model: tabs))
        tabBarHost.translatesAutoresizingMaskIntoConstraints = false
        welcomeHost = NSHostingView(rootView: EditorWelcomeState(model: tabs))
        welcomeHost.translatesAutoresizingMaskIntoConstraints = false
        welcomeHost.sizingOptions = []
        tabBarHost.sizingOptions = []
        for axis in [NSLayoutConstraint.Orientation.horizontal, .vertical] {
            welcomeHost.setContentCompressionResistancePriority(.defaultLow, for: axis)
            welcomeHost.setContentHuggingPriority(.defaultLow, for: axis)
        }

        let container2 = EditorContainerView()
        container2.keyHandler = { [weak self] e in self?.handleKeyEquivalent(e) ?? false }
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
    func setScope(_ connection: String?) {
        loadViewIfNeeded()
        let next = (connection?.isEmpty ?? true) ? nil : connection
        guard next != tabs.scope else { return }
        flushActive()
        tabs.scope = next
        updateWelcomeState()
        refreshSavedList()
        persistSession()
    }

    /// Publish every open editor to the tab bar, whatever connection it targets.
    private func publishTabs() {
        tabs.tabs = allTabs
    }

    /// Empty the text view — no editor is active (the welcome state covers it).
    private func showEmptyEditor() {
        tabs.activeID = nil
        isSwappingDocument = true
        highlighter.isSuspended = true
        textView.string = ""
        highlighter.isSuspended = false
        highlighter.documentDidChangeWholesale()
        textView.undoManager?.removeAllActions()
        isSwappingDocument = false
        onSelectionChanged?()
    }

    private func updateWelcomeState() {
        welcomeHost?.isHidden = !tabs.tabs.isEmpty
        scroll.isHidden = tabs.tabs.isEmpty
    }

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

    func openEditor(_ record: SavedQueryRecord) {
        loadViewIfNeeded()
        openSaved(record)
    }

    var text: String {
        loadViewIfNeeded()
        return textView.string
    }

    func currentBlock() -> SQLBlock? {
        loadViewIfNeeded()
        let ns = textView.string as NSString
        guard ns.length > 0 else { return nil }
        let caret = min(textView.selectedRange().location, ns.length)

        func clamped(_ r: NSRange) -> NSRange {
            NSIntersectionRange(r, NSRange(location: 0, length: ns.length))
        }
        var range = clamped(highlighter.blockRange(containing: caret))
        var body = ns.substring(with: range)
        if body.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty, range.location > 0 {
            range = clamped(highlighter.blockRange(containing: range.location - 1))
            body = ns.substring(with: range)
        }
        if body.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty {
            guard let rescued = SQLBlocks.block(at: caret, in: ns as String),
                !rescued.text.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty
            else { return nil }
            range = clamped(
                NSRange(
                    location: rescued.range.lowerBound,
                    length: rescued.range.count))
            body = ns.substring(with: range)
            guard !body.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty else { return nil }
        }

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
        pruneOrphanEditors()
        refreshSavedList()
    }

    private func pruneOrphanEditors() {
        guard profilesProvider != nil else { return }
        let known = Set(tabs.connections.map(\.name))
        guard !known.isEmpty else { return }
        let orphanIDs = Set(
            allTabs.compactMap { tab -> String? in
                guard let c = tab.connection, !c.isEmpty, !known.contains(c) else { return nil }
                return tab.id
            })
        guard !orphanIDs.isEmpty else { return }
        let losingActive = tabs.activeID.map(orphanIDs.contains) ?? false
        allTabs.removeAll { orphanIDs.contains($0.id) }
        if let scope = tabs.scope, !known.contains(scope) { tabs.scope = nil }
        publishTabs()
        if losingActive {
            if let next = tabs.tabs.first {
                tabs.activeID = next.id
                load(next)
            } else {
                showEmptyEditor()
            }
        }
        updateWelcomeState()
        persistSession()
    }

    private func refreshSavedList() {
        let open = Set(allTabs.map(\.id))
        let known: Set<String>? =
            profilesProvider != nil ? Set(tabs.connections.map(\.name)) : nil
        tabs.savedQueries =
            store.allRecords()
            .filter { !open.contains($0.id) }
            .filter { record in
                guard let known, !known.isEmpty, let c = record.connection, !c.isEmpty else {
                    return true
                }
                return known.contains(c)
            }
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
        publishTabs()
        tabs.activeID = tab.id
        load(tab)
        updateWelcomeState()
        persistSession()
        refreshSavedList()
        focus()
    }

    private func nextUntitledNumber(in connection: String?) -> Int {
        let used = Set(
            allTabs.filter { $0.name == nil && $0.connection == connection }.map(\.untitledNumber))
        var n = 1
        while used.contains(n) { n += 1 }
        return n
    }

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
            self.publishTabs()
            self.persistSession()
            self.refreshConnections()
            self.refreshSavedList()
        }
    }

    @discardableResult
    func newTab(connection: String? = nil) -> EditorTab {
        loadViewIfNeeded()
        flushActive()
        if let connection, connection != tabs.scope { setScope(connection) }
        let tab = EditorTab(connection: tabs.scope)
        tab.untitledNumber = nextUntitledNumber(in: tab.connection)
        allTabs.append(tab)
        publishTabs()
        tabs.activeID = tab.id
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
        load(tab)
        persistSession()
        focus()
    }

    private func close(_ tab: EditorTab) {
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
        if tab.name == nil { store.delete(tab.record) }

        publishTabs()
        if wasActive {
            if let next = tabs.tabs.isEmpty ? nil : tabs.tabs[min(idx, tabs.tabs.count - 1)] {
                tabs.activeID = next.id
                load(next)
            } else {
                showEmptyEditor()
            }
        }
        updateWelcomeState()
        persistSession()
        refreshSavedList()
    }

    /// Drop every editor belonging to a connection that has just been deleted.
    func forgetEditors(of connection: String) {
        loadViewIfNeeded()
        let losingActive = allTabs.first { $0.id == tabs.activeID }?.connection == connection
        allTabs.removeAll { $0.connection == connection }
        if tabs.scope == connection { tabs.scope = nil }
        publishTabs()
        if losingActive {
            if let next = tabs.tabs.first {
                tabs.activeID = next.id
                load(next)
            } else {
                showEmptyEditor()
            }
        }
        updateWelcomeState()
        refreshSavedList()
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

        let handle: (NSApplication.ModalResponse) -> Void = { [weak self] response in
            guard let self else { return }
            switch response {
            case .alertFirstButtonReturn:
                self.performClose(tab)
            case .alertThirdButtonReturn:
                self.activate(tab)
                self.saveActiveTab()
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
        isSwappingDocument = true
        highlighter.isSuspended = true
        textView.string = tab.text
        highlighter.isSuspended = false
        highlighter.documentDidChangeWholesale()

        let length = (textView.string as NSString).length
        let loc = min(max(0, tab.selectedRange.location), length)
        let len = min(max(0, tab.selectedRange.length), length - loc)
        textView.setSelectedRange(NSRange(location: loc, length: len))
        textView.scrollRangeToVisible(NSRange(location: loc, length: 0))
        textView.undoManager?.removeAllActions()
        isSwappingDocument = false

        updateDecorations()
        onSelectionChanged?()
    }

    // MARK: - persistence

    /// Reopen what was open, and **make nothing up**.
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
        var counters: [String: Int] = [:]
        for tab in allTabs where tab.name == nil {
            let key = tab.connection ?? EditorSession.unbound
            let n = (counters[key] ?? 0) + 1
            counters[key] = n
            tab.untitledNumber = n
        }

        didRestoreSession = !allTabs.isEmpty
        tabs.scope = loaded.session.activeConnection
        publishTabs()
        let active = allTabs.first { $0.id == loaded.session.activeID } ?? allTabs.first
        tabs.activeID = active?.id
        if let active {
            load(active)
        } else {
            showEmptyEditor()
        }
        updateWelcomeState()
        refreshConnections()
        refreshSavedList()
    }

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
            EditorSession(
                order: allTabs.map(\.id), activeID: tabs.activeID,
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
        guard !isSwappingDocument else { return }
        tabs.active?.isDirty = true
        highlighter.refreshVisible()
        updateDecorations()
        scheduleAutosave()
        onSelectionChanged?()
    }

    func textViewDidChangeSelection(_ notification: Notification) {
        guard !isSwappingDocument else { return }
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

struct SQLEditorView: NSViewControllerRepresentable {
    let controller: SQLEditorController
    func makeNSViewController(context: Context) -> SQLEditorController { controller }
    func updateNSViewController(_ nsViewController: SQLEditorController, context: Context) {}

    /// Take the height offered, never the one AppKit would ask for.
    func sizeThatFits(
        _ proposal: ProposedViewSize,
        nsViewController: SQLEditorController,
        context: Context
    ) -> CGSize? {
        CGSize(width: proposal.width ?? 10, height: proposal.height ?? 10)
    }
}
