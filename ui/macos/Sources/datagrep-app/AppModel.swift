import AppKit
import Combine
import DatagrepKit
import Foundation
import SwiftUI

final class CatalogNode: ObservableObject, Identifiable {
    let id = UUID()
    let profile: String
    let path: [String]
    let name: String
    let kind: String
    let enumeration: Enumeration
    let hasChildren: Bool
    let isProfile: Bool
    let driver: String
    let readOnly: Bool
    let enforcement: ReadOnlyEnforcement
    let colorName: String?

    @Published var children: [CatalogNode] = []
    @Published var isLoading = false
    @Published var loadError: String?
    @Published var scanPrefix: String = ""
    @Published var didLoad = false
    @Published var isExpanded = false {
        didSet {
            guard isExpanded, !didLoad, !isLoading else { return }
            if enumeration == .scanOnly { return }
            onExpand?(self, nil)
        }
    }
    var onExpand: ((CatalogNode, String?) -> Void)?

    var needsPrefix: Bool { enumeration == .scanOnly && !didLoad }

    init(profile: Profile) {
        self.profile = profile.name
        self.path = []
        self.name = profile.name
        self.kind = "profile"
        self.enumeration = .cheap
        self.hasChildren = true
        self.isProfile = true
        self.driver = profile.driver
        self.readOnly = profile.readOnly
        self.enforcement = profile.enforcement
        self.colorName = profile.color
    }

    init(profile: String, parentPath: [String], entry: CatalogEntry) {
        self.profile = profile
        self.path = parentPath + [entry.name]
        self.name = entry.name
        self.kind = entry.kind
        self.enumeration = entry.enumeration
        self.hasChildren = entry.hasChildren
        self.isProfile = false
        self.driver = ""
        self.readOnly = false
        self.enforcement = .unknown
        self.colorName = nil
    }

    var symbol: String {
        isProfile ? EngineStyle.symbol(for: driver) : NodeStyle.symbol(forKind: kind)
    }

    var tint: Color {
        isProfile ? EngineStyle.tint(for: driver) : Color.secondary
    }

    var badge: String? {
        if isProfile { return didLoad ? "\(children.count)" : nil }
        switch enumeration {
        case .cheap: return didLoad && !children.isEmpty ? "\(children.count)" : nil
        case .scanOnly: return "scan"
        case .paged: return "paged"
        case .onDemand: return nil
        }
    }

    var subtitle: String? {
        guard isProfile else { return nil }
        var s = EngineStyle.displayName(for: driver)
        if readOnly { s += " · \(enforcement.headline)" }
        return s
    }

    var isPreviewable: Bool {
        ["table", "collection", "view", "key", "hash", "string"].contains(kind)
    }

    var isDescribable: Bool {
        !isProfile
            && [
                "table", "collection", "view", "key", "hash", "string", "list", "set", "zset",
                "stream",
            ].contains(kind)
    }

    var schemaCacheKey: String {
        ([profile] + path).joined(separator: "\u{1}")
    }
}

enum InspectorMode: String, Hashable {
    case cell, schema
}

struct SchemaTarget: Equatable {
    let profile: String
    let path: [String]
    let name: String
    let kind: String
    var cacheKey: String { ([profile] + path).joined(separator: "\u{1}") }
    var breadcrumb: String { path.joined(separator: " › ") }
}

enum SchemaLoad {
    case idle
    case loading
    case loaded(SchemaDetail)
    case failed(String)
}

@MainActor
final class AppModel: ObservableObject {
    // The two AppKit bridges. Owned here so SwiftUI never re-creates them.
    let results = ResultsViewController()
    let editor = SQLEditorController()

    let history = HistoryModel()

    let edits = PendingEdits()

    @Published var roots: [CatalogNode] = []
    @Published var activeProfile: String = ""
    @Published var sqlText: String = ""
    @Published var searchText: String = ""

    @Published var state: QueryState? = nil
    @Published var rowsLoaded: UInt64 = 0
    @Published var resultGeneration: Int = 0
    @Published var showResultAsText = false
    @Published var totalKnown = true
    @Published var elapsedMs: UInt64 = 0
    @Published var message: String = "starting…"
    @Published var isError = false
    @Published var directives = BlockDirectives()
    @Published var hiddenColumns = 0

    @Published var residentPages = 0
    @Published var residentRows: UInt64 = 0
    @Published var footprintMB: Double = 0

    @Published var detailTitle: String = ""
    @Published var detailBody: String = ""
    @Published var showDetail = false

    @Published var isCommitting = false
    @Published var mutationReport: MutationReport?
    @Published var showMutationReport = false
    @Published var stagingGeneration = 0

    @Published var conflictReview: ConflictReview?
    @Published var showConflictReview = false
    @Published var isRereading = false

    @Published var inspectorMode: InspectorMode = .cell
    @Published var schemaTarget: SchemaTarget?
    @Published var schemaLoad: SchemaLoad = .idle

    @Published var showNewConnection = false
    let newForm = ConnectionForm()
    let newTest = ConnectionTestState()
    @Published var newError: String?

    /// The Edit Connection sheet, or nil when it is closed.
    @Published var editDraft: ConnectionDraft?

    @Published private(set) var profilesByName: [String: Profile] = [:]

    @Published var sidebarVisible = true {
        didSet { UserDefaults.standard.set(sidebarVisible, forKey: Self.sidebarKey) }
    }
    private static let sidebarKey = "datagrep.sidebarVisible"

    @Published var windowContentWidth: CGFloat = 1180
    static let sidebarFitsWidth: CGFloat = 900

    var sidebarShown: Bool { sidebarVisible && windowContentWidth >= Self.sidebarFitsWidth }

    @Published var progressPhase: Double = 0

    /// Sorting is a re-issued query, so these are query state, not view state.
    @Published var sortColumn: String?
    @Published var sortAscending = true
    private var baseSQL: String = ""
    private var baseFilters: [(column: String, value: String)] = []

    var showsGrid: Bool { rowsLoaded > 0 }

    var activeDriver: String { roots.first { $0.name == activeProfile }?.driver ?? "" }

    @Published var connectionInfo: DatagrepCoreHandle.ConnectionInfo?

    func refreshConnectionInfo() {
        let name = activeProfile
        guard let core, !name.isEmpty else {
            connectionInfo = nil
            return
        }
        catalogQueue.async { [weak self] in
            let info = try? core.connectionInfo(profile: name)
            DispatchQueue.main.async {
                guard let self else { return }
                guard self.activeProfile == name else { return }
                self.connectionInfo = info
            }
        }
    }
    var canSortInEngine: Bool { EngineStyle.supportsSubqueryOrderBy(activeDriver) }

    /// True when the user has put a colour on this connection.
    var isMarked: Bool { activeSafety.isMarked }

    var markColor: Color? { ConnectionColor.color(activeSafety.color) }
    var isRunning: Bool { state.map { !$0.isTerminal } ?? false }

    // MARK: - safety, resolved once

    func safety(for name: String) -> ConnectionSafety {
        let p = profilesByName[name]
        return ConnectionSafety(
            name: name,
            readOnly: p?.readOnly ?? false,
            enforcement: p?.enforcement ?? .unknown,
            confirmWrites: p?.confirmWrites ?? false,
            color: p?.color)
    }

    var activeSafety: ConnectionSafety {
        activeProfile.isEmpty ? .empty : safety(for: activeProfile)
    }

    /// `.navigationSubtitle` — the connection state, in the titlebar, always.
    var connectionSubtitle: String {
        guard !activeProfile.isEmpty else { return "no connection" }
        let driver = roots.first { $0.name == activeProfile }?.driver ?? "?"
        let lock = activeSafety.readOnly ? " · \(activeSafety.enforcement.headline)" : ""
        guard let state else { return "\(driver)\(lock) · idle" }
        return "\(driver)\(lock) · \(state.rawValue) · \(rowsLoaded.formatted()) rows"
    }

    static var profilesDBPath: String {
        SupportDirectory.ensured().appendingPathComponent("profiles.sqlite").path
    }

    private var sinks: Set<AnyCancellable> = []

    private let queryQueue = DispatchQueue(label: "datagrep.query", qos: .userInitiated)
    private let catalogQueue = DispatchQueue(label: "datagrep.catalog", qos: .userInitiated)
    private var core: DatagrepCoreHandle?
    private var query: DatagrepQueryHandle?

    private var schemaCache: [String: SchemaDetail] = [:]
    private var schemaGeneration = 0

    // MARK: - boot

    func boot() {
        results.onNestedCell = { [weak self] row, col, window in
            guard let self else { return }
            self.detailTitle = "row \(row + 1) · column \(col + 1)"
            let value =
                Self.prettify(window.detailJSON(absoluteRow: UInt64(row), col: col))
                ?? "(no detail available)"
            if let envelope = window.envelope(absoluteRow: UInt64(row)),
                let text = Self.prettifyObject(envelope), !envelope.isEmpty
            {
                self.detailBody = "// document\n\(text)\n\n// value\n\(value)"
            } else {
                self.detailBody = value
            }
            self.inspectorMode = .cell
            withAnimation(.smooth(duration: 0.22)) { self.showDetail = true }
        }
        results.onHiddenColumnsChanged = { [weak self] n in
            guard let self else { return }
            self.hiddenColumns = n
        }
        results.onSortRequested = { [weak self] col in self?.sort(by: col) }
        results.onFilterRequested = { [weak self] col, value in self?.filter(col, equals: value) }
        results.onCopied = { [weak self] label in
            self?.message = label
            self?.isError = false
        }
        editor.onSelectionChanged = { [weak self] in self?.refreshDirectives() }

        editor.profilesProvider = { [weak self] in
            (self?.roots ?? []).map { EditorConnectionOption(name: $0.name, driver: $0.driver) }
        }
        editor.onConnectionChanged = { [weak self] name in
            guard let name, !name.isEmpty else { return }
            self?.selectProfile(name, scopeEditors: false)
        }
        editor.tabs.onNewConnection = { [weak self] in self?.showNewConnection = true }
        editor.tabs.onPickConnection = { [weak self] name in self?.selectProfile(name) }

        // Every path that changes the active tab or closes one runs through these two.
        editor.tabs.$activeID
            .sink { [weak self] id in self?.activeTabChanged(to: id) }
            .store(in: &sinks)
        editor.tabs.$tabs
            .sink { [weak self] tabs in self?.forgetResults(outside: Set(tabs.map(\.id))) }
            .store(in: &sinks)

        history.onOpenInEditor = { [weak self] sql, connection in
            self?.openInNewEditorTab(sql: sql, connection: connection)
        }
        history.onRerun = { [weak self] sql, connection in
            self?.rerunFromHistory(sql: sql, connection: connection)
        }
        history.onStatus = { [weak self] text in
            self?.message = text
            self?.isError = false
        }

        if UserDefaults.standard.object(forKey: Self.sidebarKey) != nil {
            sidebarVisible = UserDefaults.standard.bool(forKey: Self.sidebarKey)
        }

        do {
            let c = try DatagrepCoreHandle(profilesDBPath: Self.profilesDBPath)
            core = c
            reloadProfiles()
            message = "core ready · \(roots.count) profiles · nothing connected yet"
        } catch {
            message = "datagrep_core_new failed: \(error)"
            isError = true
        }
        refreshFootprint()
        refreshDirectives()

        // The safety chip and the inspector toggle, pinned to the trailing edge.
        DispatchQueue.main.async { TitlebarTrailingAccessory.install(model: self) }

        // Companion to DATAGREP_SAFETY_FIXTURE: opens the editor on one
        if let n = ProcessInfo.processInfo.environment["DATAGREP_EDIT_FIXTURE"], !n.isEmpty {
            DispatchQueue.main.async { [weak self] in self?.editConnection(named: n) }
        }
        if let out = ProcessInfo.processInfo.environment["DATAGREP_EDIT_SHOT"] {
            NSApp.appearance = NSAppearance(named: .aqua)
            DispatchQueue.main.asyncAfter(deadline: .now() + 1.6) {
                let sheet = NSApp.windows.first { $0.isSheet && $0.isVisible }
                if let content = sheet?.contentView,
                    let rep = content.bitmapImageRepForCachingDisplay(in: content.bounds)
                {
                    content.cacheDisplay(in: content.bounds, to: rep)
                    if let png = rep.representation(using: .png, properties: [:]) {
                        try? png.write(to: URL(fileURLWithPath: out))
                    }
                }
                NSApp.terminate(nil)
            }
        }

        if let mode = ProcessInfo.processInfo.environment["DATAGREP_HISTORY_FIXTURE"] {
            DispatchQueue.main.asyncAfter(deadline: .now() + 0.8) { [weak self] in
                guard let self else { return }
                if mode == "open", let entry = self.history.entries.first {
                    self.history.openInEditor(entry)
                } else {
                    self.history.isPresented = true
                }
            }
        }

        if let f = ProcessInfo.processInfo.environment["DATAGREP_SCHEMA_FIXTURE"],
            let text = try? String(contentsOfFile: f, encoding: .utf8),
            let d = SchemaDetail.decode(text, fallbackName: "fixture", fallbackKind: "table")
        {
            schemaTarget = SchemaTarget(
                profile: "local_pg", path: d.path.isEmpty ? [d.name] : d.path, name: d.name,
                kind: d.kind)
            schemaLoad = .loaded(d)
            inspectorMode = .schema
            showDetail = true
        }
        if let out = ProcessInfo.processInfo.environment["DATAGREP_SCHEMA_SHOT"] {
            DispatchQueue.main.asyncAfter(deadline: .now() + 1.2) { [weak self] in
                guard let self else { return }
                let renderer = ImageRenderer(
                    content: DetailPanel(model: self)
                        .frame(width: 380, height: 860)
                        .background(Color(nsColor: .windowBackgroundColor)))
                renderer.scale = 2
                if let img = renderer.nsImage, let tiff = img.tiffRepresentation,
                    let rep = NSBitmapImageRep(data: tiff),
                    let png = rep.representation(using: .png, properties: [:])
                {
                    try? png.write(to: URL(fileURLWithPath: out))
                }
                NSApp.terminate(nil)
            }
        }
    }

    // MARK: - profiles

    func reloadProfiles() {
        guard let core else { return }
        schemaCache.removeAll()
        schemaTarget = nil
        schemaLoad = .idle
        do {
            let profiles = Self.applySafetyFixture(to: try core.profiles())
            roots = profiles.map { p in
                let n = CatalogNode(profile: p)
                n.onExpand = { [weak self] node, prefix in self?.load(node, prefix: prefix) }
                return n
            }
            profilesByName = Dictionary(uniqueKeysWithValues: profiles.map { ($0.name, $0) })
            if !roots.contains(where: { $0.name == activeProfile }) {
                activeProfile = profiles.first?.name ?? ""
            }
            editor.refreshConnections()
            editor.setScope(activeProfile.isEmpty ? nil : activeProfile)
            refreshConnectionInfo()
        } catch {
            message = "could not list profiles: \(error)"
            isError = true
        }
    }

    func addProfileFromForm() {
        addProfile(name: newForm.name, url: newForm.urlWithPassword)
    }

    func addProfile(name: String, url: String) {
        guard let core else { return }
        let n = name.trimmingCharacters(in: .whitespacesAndNewlines)
        let u = url.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !n.isEmpty, !u.isEmpty else {
            newError = "a name and either a host or a file are required"
            return
        }
        newError = nil
        queryQueue.async { [weak self] in
            var failure: String?
            do { try core.addProfile(name: n, url: u) } catch { failure = "\(error)" }
            DispatchQueue.main.async {
                guard let self else { return }
                if let failure {
                    self.newError = failure
                    return
                }
                self.showNewConnection = false
                self.newForm.apply(ConnectionFields(engineID: "postgres"))
                self.newForm.name = ""
                self.newTest.clear()
                self.reloadProfiles()
                self.selectProfile(n)
                self.message = "added connection `\(n)`"
                self.isError = false
            }
        }
    }

    // MARK: - testing a connection

    func testNewConnection() {
        runTest(newTest, name: nil, url: newForm.urlWithPassword)
    }

    /// Test what the Edit sheet currently describes.
    func testConnection(_ draft: ConnectionDraft) {
        let typed = draft.urlToTest.trimmingCharacters(in: .whitespacesAndNewlines)
        let unchanged = typed == draft.originalURL && draft.password.isEmpty
        runTest(draft.test, name: unchanged ? draft.originalName : nil, url: unchanged ? nil : typed)
    }

    private func runTest(_ state: ConnectionTestState, name: String?, url: String?) {
        guard let core else { return }
        state.begin()
        queryQueue.async { [weak self] in
            var result: ConnectionTestResult?
            var failure: String?
            do { result = try core.testConnection(name: name, url: url) } catch {
                failure = "\(error)"
            }
            DispatchQueue.main.async {
                guard self != nil else { return }
                state.running = false
                state.result = result
                state.failure = failure
            }
        }
    }

    /// `DATAGREP_SAFETY_FIXTURE` overlays read-only / enforcement / colour onto the
    static func applySafetyFixture(to profiles: [Profile]) -> [Profile] {
        guard let text = ProcessInfo.processInfo.environment["DATAGREP_SAFETY_FIXTURE"],
            let data = text.data(using: .utf8),
            let map = try? JSONSerialization.jsonObject(with: data) as? [String: [String: Any]]
        else { return profiles }
        return profiles.map { p in
            guard let o = map[p.name] else { return p }
            return Profile(
                name: p.name, driver: p.driver,
                hasSecret: o["has_secret"] as? Bool ?? p.hasSecret,
                readOnly: o["read_only"] as? Bool ?? p.readOnly,
                confirmWrites: o["confirm_writes"] as? Bool ?? p.confirmWrites,
                enforcement: (o["enforcement"] as? String).map { ReadOnlyEnforcement(abi: $0) }
                    ?? p.enforcement,
                color: o["color"] as? String ?? p.color)
        }
    }

    // MARK: - editing a connection

    func editConnection(named name: String) {
        guard let core, profilesByName[name] != nil else { return }
        let seed =
            profilesByName[name].map {
                ProfileDetail(
                    name: $0.name, url: "", driver: $0.driver, readOnly: $0.readOnly,
                    confirmWrites: $0.confirmWrites, color: $0.color, hasSecret: $0.hasSecret,
                    enforcement: $0.enforcement, reported: ["name", "driver", "read_only"])
            } ?? ProfileDetail(name: name)
        let draft = ConnectionDraft(detail: seed)
        draft.loading = ProfileABI.canPrefill
        editDraft = draft

        guard ProfileABI.canPrefill else { return }
        queryQueue.async { [weak self] in
            var detail: ProfileDetail?
            var failure: String?
            do { detail = try core.profileDetail(name: name) } catch { failure = "\(error)" }
            DispatchQueue.main.async {
                guard let self, self.editDraft === draft else { return }
                if let detail {
                    draft.apply(detail)
                } else {
                    draft.loading = false
                    draft.error = failure
                }
            }
        }
    }

    func editActiveConnection() {
        guard !activeProfile.isEmpty else { return }
        editConnection(named: activeProfile)
    }

    func closeConnectionEditor() { editDraft = nil }

    func saveConnectionDraft() {
        guard let core, let draft = editDraft else { return }
        let oldName = draft.originalName
        let patchJSON = draft.patch.json
        let changed = draft.changedKeys
        guard !changed.isEmpty else {
            closeConnectionEditor()
            return
        }
        let newName = draft.finalName
        draft.error = nil
        draft.saving = true
        queryQueue.async { [weak self] in
            var failure: String?
            do { try core.updateProfile(name: oldName, patchJSON: patchJSON) } catch {
                failure = "\(error)"
            }
            DispatchQueue.main.async {
                guard let self else { return }
                draft.saving = false
                if let failure {
                    draft.error = failure
                    return
                }
                self.editDraft = nil
                if self.activeProfile == oldName { self.activeProfile = newName }
                self.reloadProfiles()
                self.message =
                    "saved `\(newName)` — \(changed.joined(separator: ", "))"
                self.isError = false
            }
        }
    }

    // MARK: - production marking

    /// Reconcile the window's idea of production with the profile store's.
    func removeActiveProfile() {
        guard !activeProfile.isEmpty else { return }
        removeProfile(named: activeProfile)
    }

    /// Delete a connection, after asking.
    func removeProfile(named name: String) {
        guard core != nil, profilesByName[name] != nil else { return }
        let safety = safety(for: name)
        let alert = NSAlert()
        alert.alertStyle = .critical
        alert.messageText = "Remove the connection “\(name)”?"
        var detail =
            "The connection is deleted from datagrep. The database itself is untouched."
        if profilesByName[name]?.hasSecret == true {
            detail += " Its saved password is removed from the macOS keychain as well."
        }
        detail += " Editors you wrote for it are kept on disk."
        alert.informativeText = detail
        alert.addButton(withTitle: "Remove")
        alert.addButton(withTitle: "Cancel")
        alert.buttons.first?.hasDestructiveAction = true
        alert.buttons.last?.keyEquivalent = "\r"

        let commit: (NSApplication.ModalResponse) -> Void = { [weak self] response in
            guard response == .alertFirstButtonReturn else { return }
            self?.commitRemoval(of: name)
        }
        guard let window = NSApp.mainWindow ?? NSApp.windows.first(where: { $0.isVisible }) else {
            commit(alert.runModal())
            return
        }
        alert.beginSheetModal(for: window, completionHandler: commit)
    }

    private func commitRemoval(of name: String) {
        guard let core else { return }
        do {
            try core.removeProfile(name: name)
            profilesByName.removeValue(forKey: name)
            editor.forgetEditors(of: name)
            if activeProfile == name { activeProfile = "" }
            reloadProfiles()
            message = "removed connection `\(name)`"
            isError = false
        } catch {
            message = "\(error)"
            isError = true
        }
    }

    func duplicateProfile(named name: String) {
        guard let core, let original = profilesByName[name] else { return }
        guard ProfileABI.canPrefill else {
            message =
                "this engine build cannot read a connection back, so `\(name)` cannot be copied — add the connection again instead"
            isError = true
            return
        }
        let copyName = uniqueProfileName(basedOn: name)
        queryQueue.async { [weak self] in
            var failure: String?
            var url = ""
            do {
                url = try core.profileDetail(name: name).url
                if url.isEmpty {
                    failure = "the engine reported no connection URL for `\(name)`"
                } else {
                    try core.addProfile(name: copyName, url: url)
                }
            } catch {
                failure = "\(error)"
            }
            DispatchQueue.main.async {
                guard let self else { return }
                if let failure {
                    self.message = "could not copy `\(name)`: \(failure)"
                    self.isError = true
                    return
                }
                self.reloadProfiles()
                self.selectProfile(copyName)
                self.message =
                    original.hasSecret
                    ? "copied `\(name)` to `\(copyName)` — the saved password was not copied, so set one with ⌘E"
                    : "copied `\(name)` to `\(copyName)`"
                self.isError = !original.hasSecret ? false : true
            }
        }
    }

    private func uniqueProfileName(basedOn name: String) -> String {
        var candidate = name + " copy"
        var n = 2
        while profilesByName[candidate] != nil {
            candidate = "\(name) copy \(n)"
            n += 1
        }
        return candidate
    }

    /// Drop this connection's pooled socket so the next statement dials again.
    func reconnect(_ name: String) {
        guard let core, profilesByName[name] != nil else { return }
        var patch = ProfilePatch()
        patch.set("name", name)
        let json = patch.json
        message = "reconnecting `\(name)`…"
        isError = false
        queryQueue.async { [weak self] in
            var failure: String?
            do { try core.updateProfile(name: name, patchJSON: json) } catch {
                failure = "\(error)"
            }
            DispatchQueue.main.async {
                guard let self else { return }
                if let failure {
                    self.message = "could not reconnect `\(name)`: \(failure)"
                    self.isError = true
                    return
                }
                self.reloadProfiles()
                self.message = "`\(name)` will dial again on the next statement"
                self.isError = false
            }
        }
    }

    /// Make `name` the window's connection, and show its editors.
    func selectProfile(_ name: String, scopeEditors: Bool = true) {
        guard activeProfile != name else { return }
        activeProfile = name
        if scopeEditors { editor.setScope(name.isEmpty ? nil : name) }
        connectionInfo = nil
        refreshConnectionInfo()
        syncVisibleResult(tab: editor.tabs.activeID)
    }

    // MARK: - catalog, one level per call

    func load(_ node: CatalogNode, prefix: String?) {
        guard let core, !node.isLoading else { return }
        node.isLoading = true
        var path = node.path
        if let prefix, !prefix.isEmpty { path.append(prefix) }
        let profile = node.profile
        catalogQueue.async { [weak self] in
            var entries: [CatalogEntry] = []
            var failure: String?
            do { entries = try core.children(profile: profile, path: path) } catch let e {
                failure = "\(e)"
            }
            DispatchQueue.main.async {
                guard let self else { return }
                node.isLoading = false
                node.loadError = failure
                node.children = entries.map { e in
                    let child = CatalogNode(profile: profile, parentPath: path, entry: e)
                    child.onExpand = { [weak self] n, p in self?.load(n, prefix: p) }
                    return child
                }
                node.didLoad = failure == nil
                if let failure {
                    self.message = "catalog error: \(failure)"
                    self.isError = true
                } else {
                    self.message =
                        "\(profile): \(entries.count) children at /\(path.joined(separator: "/"))"
                    self.isError = false
                }
            }
        }
    }

    func scan(_ node: CatalogNode) {
        let prefix = node.scanPrefix.trimmingCharacters(in: .whitespaces)
        guard !prefix.isEmpty else {
            message = "a prefix is required — listing every key would be a full keyspace scan"
            isError = true
            return
        }
        load(node, prefix: prefix)
    }

    func select(_ node: CatalogNode) {
        selectProfile(node.profile)
        if node.isDescribable { showSchema(for: node) }
    }

    // MARK: - editors, per connection

    func openSQLEditor(for profile: String) {
        guard profilesByName[profile] != nil || roots.contains(where: { $0.name == profile })
        else { return }
        selectProfile(profile)
        editor.newTab(connection: profile)
        editor.focus()
        message = "new editor for `\(profile)`"
        isError = false
    }

    func editors(for profile: String) -> [SavedQueryRecord] { editor.editors(for: profile) }

    func openEditor(_ record: SavedQueryRecord) {
        if let connection = record.connection { selectProfile(connection) }
        editor.openEditor(record)
        editor.focus()
    }

    // MARK: - schema, one describe() per object, cached

    /// Point the inspector at an object and make sure its schema is there.
    func showSchema(for node: CatalogNode, force: Bool = false) {
        let target = SchemaTarget(
            profile: node.profile, path: node.path, name: node.name, kind: node.kind)
        showSchema(target, force: force)
    }

    func showSchema(_ target: SchemaTarget, force: Bool = false) {
        schemaTarget = target
        inspectorMode = .schema
        if !showDetail { withAnimation(.smooth(duration: 0.22)) { showDetail = true } }

        if force {
            schemaCache.removeValue(forKey: target.cacheKey)
        } else if let hit = schemaCache[target.cacheKey] {
            schemaLoad = .loaded(hit)
            return
        }

        guard let core else {
            schemaLoad = .failed("the engine core is not open")
            return
        }
        schemaLoad = .loading
        schemaGeneration &+= 1
        let generation = schemaGeneration
        catalogQueue.async { [weak self] in
            var detail: SchemaDetail?
            var failure: String?
            do {
                let json = try core.describe(profile: target.profile, path: target.path)
                detail = SchemaDetail.decode(
                    json, fallbackName: target.name, fallbackKind: target.kind)
                if detail == nil { failure = "describe() returned something that is not an object" }
            } catch {
                failure = "\(error)"
            }
            DispatchQueue.main.async {
                guard let self, self.schemaGeneration == generation else { return }
                if let detail {
                    self.schemaCache[target.cacheKey] = detail
                    self.schemaLoad = .loaded(detail)
                } else {
                    self.schemaLoad = .failed(failure ?? "describe() failed")
                }
            }
        }
    }

    /// Explicit refresh — the only thing that invalidates a cached schema.
    func refreshSchema() {
        guard let schemaTarget else { return }
        showSchema(schemaTarget, force: true)
    }

    func showCellDetail() {
        inspectorMode = .cell
        if !showDetail { withAnimation(.smooth(duration: 0.22)) { showDetail = true } }
    }

    func preview(_ node: CatalogNode) {
        select(node)
        let sql = """
            -- @limit 500
            SELECT * FROM \(node.path.joined(separator: "."));
            """
        sqlText = sql
        editor.setText(sql)
        refreshDirectives()
        run(sql: sql, directives: SQLBlocks.directives(in: sql))
    }

    // MARK: - history

    private func driverID(for profile: String) -> String {
        profilesByName[profile]?.driver ?? roots.first { $0.name == profile }?.driver ?? ""
    }

    private func openInNewEditorTab(sql: String, connection: String?) {
        let known = connection.flatMap { profilesByName[$0] != nil ? $0 : nil }
        let tab = editor.newTab(connection: known)
        editor.setText(sql)
        if let known {
            editor.tabs.onBind?(tab, known)
            selectProfile(known)
        }
        sqlText = sql
        refreshDirectives()
        editor.focus()
        message = "opened a history entry in a new tab — your other tabs are untouched"
        isError = false
    }

    private func rerunFromHistory(sql: String, connection: String?) {
        openInNewEditorTab(sql: sql, connection: connection)
        var d = SQLBlocks.directives(in: sql)
        if let connection, !connection.isEmpty, profilesByName[connection] != nil {
            d.connection = connection
        }
        directives = d
        run(sql: sql, directives: d)
    }

    // MARK: - query

    func refreshDirectives() {
        directives = editor.currentBlock()?.directives ?? BlockDirectives()
    }

    func runStatementUnderCaret() {
        guard let block = editor.currentBlock() else {
            message = "nothing to run"
            isError = true
            return
        }
        directives = block.directives
        run(sql: block.text, directives: block.directives)
    }

    func run(sql: String, directives: BlockDirectives) {
        baseSQL = sql
        sortColumn = nil
        sortAscending = true
        baseFilters = []
        execute(directives: directives)
    }

    func sort(by column: String) {
        guard !baseSQL.isEmpty else { return }
        guard canSortInEngine else {
            message =
                "sorting is only offered where datagrep can push ORDER BY to the engine — \(EngineStyle.displayName(for: activeDriver)) results would have to be sorted client-side, which would only sort the rows already loaded"
            isError = true
            return
        }
        if sortColumn == column {
            sortAscending.toggle()
        } else {
            sortColumn = column
            sortAscending = true
        }
        execute(directives: directives)
    }

    func filter(_ column: String, equals value: String) {
        guard !baseSQL.isEmpty, canSortInEngine else {
            message = "filtering needs an engine datagrep can wrap the statement for"
            isError = true
            return
        }
        baseFilters.removeAll { $0.column == column }
        baseFilters.append((column, value))
        execute(directives: directives)
    }

    /// Run the same statement again — what "Reload" offers after a commit.
    func reloadResult() {
        guard !baseSQL.isEmpty else { return }
        execute(directives: directives)
    }

    func clearDerived() {
        sortColumn = nil
        baseFilters = []
        execute(directives: directives)
    }

    var hasDerivedClauses: Bool { sortColumn != nil || !baseFilters.isEmpty }

    var effectiveSQL: String {
        var inner = baseSQL.trimmingCharacters(in: .whitespacesAndNewlines)
        while inner.hasSuffix(";") { inner = String(inner.dropLast()) }
        guard hasDerivedClauses else { return baseSQL }
        var q = "SELECT * FROM (\n\(inner)\n) AS datagrep_result"
        if !baseFilters.isEmpty {
            let clauses = baseFilters.map { f -> String in
                f.value.isEmpty
                    ? "\(quoteIdent(f.column)) IS NULL OR \(quoteIdent(f.column)) = ''"
                    : "\(quoteIdent(f.column)) = '\(f.value.replacingOccurrences(of: "'", with: "''"))'"
            }
            q += "\nWHERE (" + clauses.joined(separator: ") AND (") + ")"
        }
        if let sortColumn {
            q += "\nORDER BY \(quoteIdent(sortColumn)) \(sortAscending ? "ASC" : "DESC")"
        }
        return q
    }

    private func quoteIdent(_ name: String) -> String {
        let escaped = name.replacingOccurrences(of: "\"", with: "\"\"")
        if EngineStyle.displayName(for: activeDriver) == "MySQL" {
            return "`\(name.replacingOccurrences(of: "`", with: "``"))`"
        }
        return "\"\(escaped)\""
    }

    private func execute(directives: BlockDirectives) {
        guard core != nil else { return }
        let sql = effectiveSQL
        let profile = directives.connection ?? activeProfile
        results.sortColumn = sortColumn
        results.sortAscending = sortAscending
        if directives.readOnly && SQLBlocks.isWriteStatement(sql) {
            message =
                "blocked by `-- @readonly` — client-side classifier only; the server was not asked to enforce read-only"
            isError = true
            history.executionBlocked(
                sql: sql, connection: profile, engine: driverID(for: profile),
                reason: message)
            return
        }
        let safety = safety(for: profile)
        if safety.readOnly, SQLBlocks.isWriteStatement(sql) {
            let verb = Self.statementVerb(sql)
            message =
                "`\(profile)` is read-only — \(verb) not sent (\(safety.enforcement.refusalClause)). ⌘E to change it."
            isError = true
            state = nil
            history.executionBlocked(
                sql: sql, connection: profile, engine: driverID(for: profile), reason: message)
            return
        }
        if safety.confirmWrites, SQLBlocks.isWriteStatement(sql) {
            confirmWrite(verb: Self.statementVerb(sql), profile: profile, safety: safety) {
                [weak self] in
                self?.send(sql: sql, profile: profile)
            }
            return
        }
        send(sql: sql, profile: profile)
    }

    /// The first word of the statement, for a refusal that says what it refused.
    static func statementVerb(_ sql: String) -> String {
        var s = sql
        while true {
            s = s.trimmingCharacters(in: .whitespacesAndNewlines)
            if s.hasPrefix("--") {
                if let nl = s.firstIndex(of: "\n") {
                    s = String(s[s.index(after: nl)...])
                } else {
                    s = ""
                }
                continue
            }
            break
        }
        let head = s.prefix(while: { $0.isLetter }).uppercased()
        return head.isEmpty ? "statement" : head
    }

    /// The `confirm_writes` profile setting, as a real window-modal sheet.
    private func confirmWrite(
        verb: String, profile: String, safety: ConnectionSafety, proceed: @escaping () -> Void
    ) {
        let alert = NSAlert()
        alert.alertStyle = .warning
        alert.messageText = "Run a \(verb) against `\(profile)`?"
        alert.informativeText =
            "This connection is set to ask before every write. The statement has not been sent yet."
        alert.addButton(withTitle: "Run \(verb)")
        alert.addButton(withTitle: "Cancel")
        guard let window = NSApp.mainWindow ?? NSApp.windows.first(where: { $0.isVisible }) else {
            if alert.runModal() == .alertFirstButtonReturn { proceed() }
            return
        }
        alert.beginSheetModal(for: window) { [weak self] response in
            guard response == .alertFirstButtonReturn else {
                self?.message = "not sent — `\(profile)` asks before every write"
                self?.isError = false
                self?.state = nil
                return
            }
            proceed()
        }
    }

    private func send(sql: String, profile: String) {
        guard let core else { return }
        // A `-- @connection` directive may aim elsewhere; the badge follows the statement.
        selectProfile(profile, scopeEditors: false)
        resultProfile = profile
        results.allowsEditing = !safety(for: profile).readOnly
        message = "running on \(profile)…"
        isError = false
        history.executionStarted(sql: sql, connection: profile, engine: driverID(for: profile))
        state = .streaming

        queryQueue.async { [weak self] in
            do {
                let handle = try core.run(profile: profile, sql: sql)
                DispatchQueue.main.async { self?.adopt(handle) }
            } catch {
                DispatchQueue.main.async {
                    guard let self else { return }
                    self.state = .failed
                    self.message = "\(error)"
                    self.isError = true
                    // Nothing ran, so nothing is owned: drop the previous handle too.
                    self.query = nil
                    self.resultTab = nil
                    self.results.clear()
                    self.history.executionFailedToStart("\(error)")
                }
            }
        }
    }

    private func adopt(_ handle: DatagrepQueryHandle) {
        query = handle  // the previous handle deinits here -> datagrep_query_free
        resultTab = editor.tabs.activeID
        edits.discardAll()
        stagingGeneration &+= 1
        results.beginNewResult(pager: RowPager(query: handle, pageSize: 512, maxPages: 4))
        handle.onProgress { [weak self, weak handle] in
            // Already hopped to main and coalesced by DatagrepQueryHandle.
            guard let self, let handle, self.query === handle else { return }
            MainActor.assumeIsolated { self.refreshFromCore() }
        }
        refreshFromCore()
    }

    /// The only redraw trigger besides user input. Nothing polls this.
    private func refreshFromCore() {
        guard let query, let status = try? query.status() else { return }
        apply(status, recordHistory: true)
    }

    private func apply(_ status: QueryStatus, recordHistory: Bool) {
        results.apply(status: status)
        resultGeneration &+= 1
        state = status.state
        if !status.state.isTerminal, status.rowsLoaded != rowsLoaded {
            progressPhase = progressPhase >= 0.999 ? 0 : min(1, progressPhase + 0.14)
        }
        rowsLoaded = status.rowsLoaded
        totalKnown = status.totalKnown
        elapsedMs = status.elapsedMs
        if status.state.isTerminal, connectionInfo?.version == nil {
            refreshConnectionInfo()
        }
        // Safe on every tick: this records once, when the query goes terminal.
        if recordHistory {
            history.executionProgressed(
                state: status.state, rowsLoaded: status.rowsLoaded, elapsedMs: status.elapsedMs,
                error: status.error)
        }
        if let e = status.error {
            message = e
            isError = true
        } else if status.state == .done && status.columns.isEmpty && status.rowsLoaded == 0 {
            message =
                "statement completed; this ABI carries no Shape::Ack, so an affected-row count is not available (datagrep-cli README gap #3)"
            isError = false
        }
        refreshFootprint()
    }

    // MARK: - a result belongs to the editor tab that ran it

    /// Everything needed to put one tab's result back on screen.
    private struct TabResult {
        let query: DatagrepQueryHandle
        let profile: String
        let sql: String
        let filters: [(column: String, value: String)]
        let sortColumn: String?
        let sortAscending: Bool
        let allowsEditing: Bool
        let message: String
        let isError: Bool
        let edits: PendingEdits.Snapshot
    }

    /// The tab whose result is on screen, and the connection that produced it.
    private var resultTab: String?
    private var resultProfile: String = ""
    private var resultsByTab: [String: TabResult] = [:]

    /// Park the visible result under its own tab so switching back restores it.
    private func parkVisibleResult() {
        guard let query, let tab = resultTab else { return }
        resultsByTab[tab] = TabResult(
            query: query, profile: resultProfile, sql: baseSQL, filters: baseFilters,
            sortColumn: sortColumn, sortAscending: sortAscending,
            allowsEditing: results.allowsEditing, message: message, isError: isError,
            edits: edits.snapshot())
    }

    /// Back to "No result yet" — nothing on screen is attributed to anything.
    private func clearVisibleResult() {
        let had = query != nil
        query = nil
        resultTab = nil
        resultProfile = ""
        results.clear()
        edits.discardAll()
        stagingGeneration &+= 1
        baseSQL = ""
        baseFilters = []
        sortColumn = nil
        sortAscending = true
        results.sortColumn = nil
        state = nil
        rowsLoaded = 0
        totalKnown = true
        elapsedMs = 0
        hiddenColumns = 0
        resultGeneration &+= 1
        if had {
            isError = false
            message = "no result in this tab yet"
        }
        refreshFootprint()
    }

    private func restore(_ saved: TabResult, forTab tab: String) {
        query = saved.query
        resultTab = tab
        resultProfile = saved.profile
        baseSQL = saved.sql
        baseFilters = saved.filters
        sortColumn = saved.sortColumn
        sortAscending = saved.sortAscending
        results.sortColumn = saved.sortColumn
        results.sortAscending = saved.sortAscending
        results.allowsEditing = saved.allowsEditing
        edits.restore(saved.edits)
        stagingGeneration &+= 1
        results.beginNewResult(pager: RowPager(query: saved.query, pageSize: 512, maxPages: 4))
        message = saved.message
        isError = saved.isError
        // Not refreshFromCore: this run was recorded in history when it first ran.
        if let status = try? saved.query.status() { apply(status, recordHistory: false) }
    }

    /// The tab is the unit: its connection and its result arrive together.
    private func activeTabChanged(to tab: String?) {
        if let tab, let bound = editor.tabs.tabs.first(where: { $0.id == tab })?.connection,
            !bound.isEmpty
        {
            selectProfile(bound, scopeEditors: false)
        }
        syncVisibleResult(tab: tab)
    }

    /// Put `tab`'s result on screen — and only if this connection produced it.
    private func syncVisibleResult(tab: String?) {
        if query != nil, resultTab == tab, resultProfile == activeProfile { return }
        parkVisibleResult()
        clearVisibleResult()
        guard let tab, let saved = resultsByTab[tab], saved.profile == activeProfile else { return }
        restore(saved, forTab: tab)
    }

    /// Closed tabs free their result — and with it the core-side result store.
    private func forgetResults(outside live: Set<String>) {
        resultsByTab = resultsByTab.filter { live.contains($0.key) }
    }

    // MARK: - committing staged edits

    /// The one destructive step. Everything before it is staging.
    func commitStagedEdits() {
        guard let core, !isCommitting else { return }
        let pending = edits.pending
        guard !pending.isEmpty else { return }
        let profile = activeProfile
        guard !profile.isEmpty else { return }
        let atomic = results.editable?.atomicBatch ?? false

        let alert = NSAlert()
        alert.alertStyle = .warning
        alert.messageText =
            pending.count == 1
            ? "Commit 1 document edit to `\(profile)`?"
            : "Commit \(pending.count) document edits to `\(profile)`?"
        alert.informativeText = Self.commitWarning(count: pending.count, atomic: atomic)
        alert.addButton(
            withTitle: pending.count == 1 ? "Commit 1 Document" : "Commit \(pending.count) Documents"
        )
        alert.addButton(withTitle: "Cancel")
        // Nothing is written by pressing return on a sheet nobody read.
        alert.buttons.first?.hasDestructiveAction = true
        alert.buttons.last?.keyEquivalent = "\r"

        let proceed: (NSApplication.ModalResponse) -> Void = { [weak self] response in
            guard response == .alertFirstButtonReturn else { return }
            self?.send(mutations: pending, profile: profile, core: core)
        }
        guard let window = NSApp.mainWindow ?? NSApp.windows.first(where: { $0.isVisible }) else {
            proceed(alert.runModal())
            return
        }
        alert.beginSheetModal(for: window, completionHandler: proceed)
    }

    /// The sentence that has to be read before the click.
    static func commitWarning(count: Int, atomic: Bool) -> String {
        if atomic {
            return
                "This connection applies the batch atomically: either all \(count) are written, or none are."
        }
        if count == 1 {
            return
                "The document is written on its own. If it fails, nothing is written and the edit stays staged for another try."
        }
        let example = min(3, count)
        let before = example - 1
        return
            "\(count) documents will be written one by one, and there is no transaction: if #\(example) fails, the \(before == 1 ? "one" : "\(before)") before it stay written and nothing is rolled back. The report then names every document — written, refused, or never attempted — and anything not written stays staged."
    }

    private func send(mutations: [StagedDocument], profile: String, core: DatagrepCoreHandle) {
        isCommitting = true
        message = "committing \(mutations.count) document(s) to \(profile)…"
        isError = false
        let rows = mutations.map(\.row)
        let ids = mutations.map(\.id)
        let batch = mutations.map(\.mutation)
        queryQueue.async { [weak self] in
            var report: MutationReport?
            var failure: String?
            do { report = try core.mutate(profile: profile, mutations: batch) } catch {
                failure = "\(error)"
            }
            DispatchQueue.main.async {
                guard let self else { return }
                self.isCommitting = false
                guard let report else {
                    self.message = failure ?? "the commit failed without a message"
                    self.isError = true
                    return
                }
                let linedUp = self.edits.apply(report, committed: ids)
                self.mutationReport = report
                self.showMutationReport = true
                self.results.refreshStagedRows(rows)
                self.stagingGeneration &+= 1
                self.resultGeneration &+= 1
                self.message =
                    linedUp
                    ? Self.reportHeadline(report)
                    : "the engine reported \(report.rows.count) outcome(s) for \(ids.count) document(s), so datagrep cannot say which is which — read the report, and re-run the statement to see what was written"
                self.isError = !report.isClean || !linedUp
            }
        }
    }

    static func reportHeadline(_ report: MutationReport) -> String {
        var parts = ["\(report.applied) applied"]
        if report.failed > 0 {
            parts.append(
                report.conflicts > 0
                    ? "\(report.failed) failed (\(report.conflicts) a version conflict)"
                    : "\(report.failed) failed")
        }
        if report.notAttempted > 0 {
            parts.append("\(report.notAttempted) never attempted, still staged")
        }
        return parts.joined(separator: " · ")
    }

    // MARK: - resolving a version conflict

    func reviewConflicts() {
        guard let core, !isRereading, !isCommitting else { return }
        let conflicted = edits.conflicted
        guard !conflicted.isEmpty else { return }
        let profile = activeProfile
        guard !profile.isEmpty else { return }
        guard let editable = results.editable else {
            message =
                "this result no longer says how its documents are identified, so datagrep cannot read them back — re-run the statement"
            isError = true
            return
        }
        let addresses = conflicted.map(\.address)
        isRereading = true
        showMutationReport = false
        message = "reading what the server holds now…"
        isError = false

        queryQueue.async { [weak self] in
            var server: [ServerDocument]?
            var failure: String?
            do { server = try core.reread(profile: profile, addresses: addresses) } catch {
                failure = "\(error)"
            }
            DispatchQueue.main.async {
                guard let self else { return }
                self.isRereading = false
                guard let server else {
                    self.message = failure ?? "the re-read failed without a message"
                    self.isError = true
                    return
                }
                guard server.count == conflicted.count else {
                    self.message =
                        "the engine answered for \(server.count) of \(conflicted.count) documents, so datagrep cannot say which answer belongs to which — re-run the statement"
                    self.isError = true
                    return
                }
                self.conflictReview = ConflictReview(
                    conflicted: conflicted, server: server, editable: editable)
                self.showConflictReview = true
                self.message = "\(conflicted.count) conflict(s) to resolve"
                self.isError = false
            }
        }
    }

    /// Re-apply one document's edits onto the version just shown.
    func rebaseConflicted(_ document: ConflictDocument) {
        guard let guardValues = document.rebaseGuard else {
            message =
                "the server did not return a version for this document, so the edit could only be re-sent unguarded — which would overwrite whatever is there now"
            isError = true
            return
        }
        let row = edits.rebase(id: document.id, onto: guardValues)
        resolved(document, repainting: row)
        message =
            "re-applied onto the current version — still staged, and still not written. Commit to write it."
        isError = false
    }

    func discardConflicted(_ document: ConflictDocument) {
        let row = edits.discard(id: document.id)
        resolved(document, repainting: row)
        message = "edit discarded — the server's version is untouched"
        isError = false
    }

    private func resolved(_ document: ConflictDocument, repainting row: Int?) {
        if let row { results.refreshStagedRows([row]) }
        stagingGeneration &+= 1
        resultGeneration &+= 1
        let remaining = conflictReview?.removing(document.id)
        conflictReview = remaining
        if remaining?.isEmpty ?? true { showConflictReview = false }
    }

    func discardStagedEdits() {
        let rows = edits.documents.map(\.row)
        guard !rows.isEmpty else { return }
        let alert = NSAlert()
        alert.alertStyle = .warning
        alert.messageText = "Discard \(edits.pendingCount) staged document edit(s)?"
        alert.informativeText =
            "Nothing has been written yet, and nothing will be. The values you typed are lost."
        alert.addButton(withTitle: "Discard")
        alert.addButton(withTitle: "Keep Editing")
        alert.buttons.first?.hasDestructiveAction = true
        alert.buttons.last?.keyEquivalent = "\r"
        let clear: (NSApplication.ModalResponse) -> Void = { [weak self] response in
            guard response == .alertFirstButtonReturn, let self else { return }
            self.edits.discardAll()
            self.results.refreshStagedRows(rows)
            self.stagingGeneration &+= 1
            self.resultGeneration &+= 1
            self.message = "staged edits discarded"
            self.isError = false
        }
        guard let window = NSApp.mainWindow ?? NSApp.windows.first(where: { $0.isVisible }) else {
            clear(alert.runModal())
            return
        }
        alert.beginSheetModal(for: window, completionHandler: clear)
    }

    func cancel() {
        guard let query else { return }
        message = query.cancel() ?? "cancel requested"
        isError = false
        refreshFromCore()
    }

    func refreshFootprint() {
        let s = Footprint.sample()
        footprintMB = s.physMB
        residentPages = results.pager?.residentPages ?? 0
        residentRows = results.pager?.residentRows ?? 0
    }

    func reportFootprint() {
        let s = Footprint.sample()
        let p = results.pager
        let line = String(
            format:
                "phys_footprint %.1f MB · rss %.1f MB · cpu %.3f s · resident %d pages / %llu rows · fetches %llu (avg %.2f ms, max %.2f ms) · evictions %llu",
            s.physMB, s.rssMB, Footprint.cpuSeconds(), p?.residentPages ?? 0, p?.residentRows ?? 0,
            p?.fetches ?? 0,
            Double(p?.totalFetchNanos ?? 0) / Double(max(p?.fetches ?? 1, 1)) / 1e6,
            Double(p?.maxFetchNanos ?? 0) / 1e6, p?.evictions ?? 0)
        message = line
        isError = false
        FileHandle.standardError.write(Data(("MEASURE " + line + "\n").utf8))
    }

    func runScrollBench() {
        ScrollBench.run(on: results, model: self)
    }

    static func prettifyObject(_ object: [String: Any]) -> String? {
        guard JSONSerialization.isValidJSONObject(object),
            let data = try? JSONSerialization.data(
                withJSONObject: object, options: [.prettyPrinted, .sortedKeys])
        else { return nil }
        return String(data: data, encoding: .utf8)
    }

    static func prettify(_ json: String?) -> String? {
        guard let json, let data = json.data(using: .utf8) else { return json }
        guard let obj = try? JSONSerialization.jsonObject(with: data, options: [.fragmentsAllowed]),
            let pretty = try? JSONSerialization.data(
                withJSONObject: obj, options: [.prettyPrinted, .sortedKeys, .fragmentsAllowed]),
            let text = String(data: pretty, encoding: .utf8)
        else { return json }
        return text
    }
}
