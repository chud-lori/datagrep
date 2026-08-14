import AppKit
import DatagrepKit
import Foundation
import SwiftUI

/// One catalog node. A reference type with `@Published` so a lazily-loaded
/// subtree can update in place without rebuilding the whole sidebar.
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
    /// Safety facts, copied off the profile so a row can draw its own badge
    /// without reaching back into the model for every cell.
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
            // ScanOnly refuses to enumerate without a prefix — this is what stops
            // the app firing `KEYS *` at a 40 GB Redis.
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

    /// Engine glyphs and node glyphs come from `DatagrepKit.EngineStyle` /
    /// `NodeStyle` — one definition, used by the sidebar, the toolbar picker
    /// and the connection sheet alike, so an engine looks the same everywhere.
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

    /// Objects `describe()` has something to say about. Databases and schemas
    /// are deliberately excluded: selecting one would spend a round trip to be
    /// told its name back. Redis keys are in — `describe_key` is a `TYPE` plus a
    /// `MEMORY USAGE`, and it is the only structure Redis will ever report.
    var isDescribable: Bool {
        !isProfile
            && [
                "table", "collection", "view", "key", "hash", "string", "list", "set", "zset",
                "stream",
            ].contains(kind)
    }

    /// Identity for the schema cache. The path is already unique within a
    /// profile, and `\u{1}` cannot occur in an identifier.
    var schemaCacheKey: String {
        ([profile] + path).joined(separator: "\u{1}")
    }
}

/// What the inspector is showing. Two modes, one pane — and switching between
/// them throws neither away.
enum InspectorMode: String, Hashable {
    case cell, schema
}

/// The object the schema pane is pointed at. Held as plain values rather than a
/// `CatalogNode` reference so a reloaded tree cannot leave the pane holding a
/// node that is no longer in it.
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

/// The whole application state. Everything published here is written on the
/// main queue only; the FFI work happens on `queryQueue` / `catalogQueue`.
@MainActor
final class AppModel: ObservableObject {
    // The two AppKit bridges. Owned here so SwiftUI never re-creates them.
    let results = ResultsViewController()
    let editor = SQLEditorController()

    /// The automatic log of every statement this window has run. Its own object
    /// rather than more fields here: the query path's whole contact with it is
    /// the three `execution*` calls below, and nothing else in the model reads
    /// it back.
    let history = HistoryModel()

    @Published var roots: [CatalogNode] = []
    @Published var activeProfile: String = ""
    @Published var sqlText: String = ""
    @Published var searchText: String = ""

    @Published var state: QueryState? = nil
    @Published var rowsLoaded: UInt64 = 0
    /// Bumped every time a result status is applied, purely so the SwiftUI
    /// host of the results grid re-runs its update and re-snapshots the table.
    @Published var resultGeneration: Int = 0
    /// The results pane can render as the grid (default) or as a plain,
    /// column-aligned monospaced text table the user can select and copy.
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

    /// The inspector's two modes. Cell detail and schema hold their own state;
    /// flipping between them is a view change, never a load.
    @Published var inspectorMode: InspectorMode = .cell
    @Published var schemaTarget: SchemaTarget?
    @Published var schemaLoad: SchemaLoad = .idle

    // New-connection sheet. The same `ConnectionForm` the Edit sheet uses, so
    // adding and editing a connection ask for the same things in the same
    // order rather than being two dialogs that happen to be about one subject.
    @Published var showNewConnection = false
    let newForm = ConnectionForm()
    let newTest = ConnectionTestState()
    @Published var newError: String?

    /// The Edit Connection sheet, or nil when it is closed.
    @Published var editDraft: ConnectionDraft?

    /// Every saved connection by name, including the safety fields. The sidebar
    /// draws from `roots`; this is what the *query path* asks before it sends a
    /// statement, so a read-only refusal can name the profile that refused.
    @Published private(set) var profilesByName: [String: Profile] = [:]




    /// Sidebar visibility, bound to `NavigationSplitView`'s `columnVisibility`
    /// and persisted. Bound rather than driven by `toggleSidebar(_:)` because
    /// the bound value is the only version we can guarantee is recoverable —
    /// a split view dragged shut has no state we own.
    @Published var sidebarVisible = true {
        didSet { UserDefaults.standard.set(sidebarVisible, forKey: Self.sidebarKey) }
    }
    private static let sidebarKey = "datagrep.sidebarVisible"

    /// Live content width, fed by a GeometryReader on the split view. Below the
    /// width where the sidebar and a usable detail can coexist, SwiftUI's
    /// `.balanced` split squeezes the sidebar off its own leading edge instead
    /// of collapsing it — so we collapse it ourselves. This is the macOS-native
    /// answer (HIG: a sidebar auto-hides when its window gets too narrow), not a
    /// horizontally-scrolling sidebar.
    @Published var windowContentWidth: CGFloat = 1180
    /// The width at/above which the sidebar is allowed to show. Empirically the
    /// balanced split starts clipping the sidebar below ~900.
    static let sidebarFitsWidth: CGFloat = 900

    /// Whether the sidebar is actually shown: the user wants it AND the window
    /// is wide enough to hold it without clipping. Narrowing past the threshold
    /// collapses it (revealed again by widening, or by the toolbar toggle / ⌃⌘S
    /// once there is room). The user's own show/hide choice is remembered in
    /// `sidebarVisible` and re-applied the moment the window is wide enough.
    var sidebarShown: Bool { sidebarVisible && windowContentWidth >= Self.sidebarFitsWidth }

    /// Advances one notch per progress callback. The only thing driving the
    /// activity bar — no timer anywhere.
    @Published var progressPhase: Double = 0

    /// Sorting is a re-issued query, so these are query state, not view state.
    @Published var sortColumn: String?
    @Published var sortAscending = true
    /// The last statement the user actually asked for, before datagrep wrapped it in
    /// an ORDER BY or a WHERE. Sorting twice must not nest two subqueries.
    private var baseSQL: String = ""
    private var baseFilters: [(column: String, value: String)] = []

    /// The grid is only shown when it has something to show; otherwise the pane
    /// holds a `ContentUnavailableView`, so "nothing here" is a stated fact and
    /// not an empty rectangle the user has to interpret.
    var showsGrid: Bool { rowsLoaded > 0 }

    var activeDriver: String { roots.first { $0.name == activeProfile }?.driver ?? "" }
    var canSortInEngine: Bool { EngineStyle.supportsSubqueryOrderBy(activeDriver) }

    var isProd: Bool { activeSafety.isMarked }
    var isRunning: Bool { state.map { !$0.isTerminal } ?? false }

    // MARK: - safety, resolved once

    /// The safety facts for one connection: production, read-only, and *which*
    /// read-only. Everything that draws a lock or refuses a statement reads
    /// this, so the badge and the refusal can never disagree.
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
        // The lock is in the toolbar chip too, but the subtitle is the one line
        // that survives a collapsed sidebar and a narrow window.
        let lock = activeSafety.readOnly ? " · \(activeSafety.enforcement.headline)" : ""
        guard let state else { return "\(driver)\(lock) · idle" }
        return "\(driver)\(lock) · \(state.rawValue) · \(rowsLoaded.formatted()) rows"
    }

    /// Where the engine keeps its profile store. Not the temp directory: a
    /// connection you added has to still be there tomorrow.
    static var profilesDBPath: String {
        SupportDirectory.ensured().appendingPathComponent("profiles.sqlite").path
    }

    private let queryQueue = DispatchQueue(label: "datagrep.query", qos: .userInitiated)
    private let catalogQueue = DispatchQueue(label: "datagrep.catalog", qos: .userInitiated)
    private var core: DatagrepCoreHandle?
    private var query: DatagrepQueryHandle?

    /// One entry per object the user has looked at. Dropped wholesale when the
    /// profile list changes — a schema is only true of the connection it came
    /// from. Nothing evicts it on a timer; a describe payload is a few KB.
    private var schemaCache: [String: SchemaDetail] = [:]
    private var schemaGeneration = 0

    // MARK: - boot

    func boot() {
        results.onNestedCell = { [weak self] row, col, window in
            guard let self else { return }
            self.detailTitle = "row \(row + 1) · column \(col + 1)"
            self.detailBody =
                Self.prettify(window.detailJSON(absoluteRow: UInt64(row), col: col))
                ?? "(no detail available)"
            // Clicking a chip is an unambiguous request for the cell, so the
            // inspector switches to it — the loaded schema stays put behind
            // the mode switch, one click away.
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

        // The editor's view of the connection list. Without this the tab chips
        // and the welcome state can only name the profiles a restored tab
        // happens to be bound to, so neither could draw an engine mark.
        editor.profilesProvider = { [weak self] in
            (self?.roots ?? []).map { EditorConnectionOption(name: $0.name, driver: $0.driver) }
        }
        editor.onConnectionChanged = { [weak self] name in
            guard let name, !name.isEmpty else { return }
            self?.selectProfile(name, scopeEditors: false)
        }
        editor.tabs.onNewConnection = { [weak self] in self?.showNewConnection = true }
        editor.tabs.onPickConnection = { [weak self] name in self?.selectProfile(name) }

        // Query history. Opening an entry gets a NEW tab rather than replacing
        // the active buffer — a history panel that silently overwrites the SQL
        // someone was half way through writing has cost them more than it saved.
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

        // No seed text, and no starter tab. An editor is something the user
        // makes — for a connection — and until then the pane holds the welcome
        // state. The old boilerplate here was a SQLite `sqlite_master` query
        // that was wrong for every other engine, dropped into a document the
        // user then had to clear before they could type.

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

        // The read-only / production chip at the trailing end of the toolbar.
        // Attached after the window is up, on the next turn, for the same
        // reason the toolbar itself is: nothing that draws chrome belongs
        // between `exec` and first paint.
        DispatchQueue.main.async { ConnectionSafetyTitlebar.install(model: self) }

        // Companion to DATAGREP_SAFETY_FIXTURE: opens the editor on one
        // connection so the sheet can be rendered and looked at.
        if let n = ProcessInfo.processInfo.environment["DATAGREP_EDIT_FIXTURE"], !n.isEmpty {
            DispatchQueue.main.async { [weak self] in self?.editConnection(named: n) }
        }
        // The sheet is its own window, so the app's `--screenshot` (which draws
        // the main window) cannot see it. Same escape hatch as
        // `DATAGREP_SCHEMA_SHOT`: render the view itself.
        if let out = ProcessInfo.processInfo.environment["DATAGREP_EDIT_SHOT"] {
            // A cached rep carries no window background of its own, so dark-mode
            // label text would come out white on nothing. Pin the shot to aqua.
            NSApp.appearance = NSAppearance(named: .aqua)
            DispatchQueue.main.asyncAfter(deadline: .now() + 1.6) {
                // `cacheDisplay`, not `ImageRenderer`: the sheet is full of
                // AppKit-backed controls (TextField, Toggle, Picker) that an
                // `ImageRenderer` draws as placeholder glyphs, which is exactly
                // the part that has to be looked at.
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

        // Same family as DATAGREP_EDIT_FIXTURE, and for the same reason: the
        // history panel is a sheet, which is its own window, so `--screenshot`
        // (which draws the main window) cannot see it.
        //   DATAGREP_HISTORY_FIXTURE=panel  opens the sheet — capture it with
        //                                   DATAGREP_EDIT_SHOT.
        //   DATAGREP_HISTORY_FIXTURE=open   replays the newest entry into a new
        //                                   editor tab, visible to --screenshot.
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

    /// One `datagrep_profiles_list_json` call. Any subtree already expanded is
    /// dropped with it — profiles changed, so the tree below them is stale.
    func reloadProfiles() {
        guard let core else { return }
        // The tree below the profiles is about to be dropped; the schemas that
        // came out of it go with it.
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
            // Editors are scoped by connection, so the tab bar has to follow
            // the profile list: a connection that just appeared (or vanished)
            // changes which editors are showing.
            editor.refreshConnections()
            editor.setScope(activeProfile.isEmpty ? nil : activeProfile)
        } catch {
            message = "could not list profiles: \(error)"
            isError = true
        }
    }

    /// Save the New Connection sheet. The URL sent carries the typed password,
    /// which `datagrep_profiles_add` lifts into the keychain before the profile
    /// is written — the one path that is known to keep it off disk.
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

    /// Test what the New Connection sheet currently describes. Nothing is
    /// saved, and no password reaches the keychain — a failed test leaves no
    /// wreckage behind to clean up.
    func testNewConnection() {
        runTest(newTest, name: nil, url: newForm.urlWithPassword)
    }

    /// Test what the Edit sheet currently describes.
    ///
    /// The edited URL is dialled rather than the saved profile whenever the
    /// user has typed anything, so the button answers "will this work?" and not
    /// "did the old one work?". With nothing typed it falls back to the saved
    /// profile, which is the only way to test the keychain password.
    func testConnection(_ draft: ConnectionDraft) {
        let typed = draft.urlToTest.trimmingCharacters(in: .whitespacesAndNewlines)
        let unchanged = typed == draft.originalURL && draft.password.isEmpty
        runTest(draft.test, name: unchanged ? draft.originalName : nil, url: unchanged ? nil : typed)
    }

    private func runTest(_ state: ConnectionTestState, name: String?, url: String?) {
        guard let core else { return }
        state.begin()
        // The dial happens on `queryQueue` and can take the engine's full
        // connect timeout: the sheet stays live and says "connecting…" instead
        // of the window beachballing on a host that is not there.
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
    /// listed profiles, in the same family as `DATAGREP_SCHEMA_FIXTURE`: the
    /// stub engine reports neither, and "I could not look at the red chrome or
    /// the lock badge" is not an acceptable answer for safety UI. It is read
    /// once per profile reload and does nothing when the variable is unset.
    ///
    /// Shape: `{"local_pg":{"read_only":true,"enforcement":"client","color":"red"}}`
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

    /// Open the sheet on a saved connection. The sheet appears immediately with
    /// what the profile list already knows; `datagrep_profiles_get_json` fills
    /// in the rest from `queryQueue`, because it opens the profile store and the
    /// main thread is never allowed to wait on that.
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

    /// Save. Everything below the first line runs on `queryQueue`; the sheet
    /// stays live and says "saving" rather than freezing.
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
    ///
    func removeActiveProfile() {
        guard !activeProfile.isEmpty else { return }
        removeProfile(named: activeProfile)
    }

    /// Delete a connection, after asking.
    ///
    /// Destructive and irreversible — the engine's `datagrep_profiles_remove`
    /// deletes the keychain entry along with the row — so it is confirmed, the
    /// alert is `.critical`, and it says out loud that the saved password goes
    /// too. The editors written for this connection are *not* deleted: their
    /// `.sql` files stay where they are, because removing a connection is not
    /// permission to destroy SQL someone wrote.
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
        // The destructive button is the one that has to be chosen deliberately;
        // ⏎ must not delete a connection.
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

    /// Copy a connection, so a second one against the same server (a different
    /// database, a read-only twin) does not have to be retyped.
    ///
    /// The copy carries the URL and the safety settings; it deliberately does
    /// **not** carry the password — the secret never crosses this ABI, so the
    /// only honest thing to do is say so and let the user type it once.
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
    ///
    /// A connection that has gone stale — a laptop that slept, a server that
    /// restarted, a VPN that dropped — otherwise keeps failing on the socket it
    /// already has. The engine closes the pool when a profile is forgotten,
    /// which is what a no-op update triggers, so this is a real reconnect and
    /// not a cosmetic one.
    func reconnect(_ name: String) {
        guard let core, profilesByName[name] != nil else { return }
        var patch = ProfilePatch()
        // A no-op patch is what makes the engine forget the profile and close
        // its pool, so the next statement genuinely re-dials.
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
                // Drop the cached catalog too: the tree came off the old
                // connection, and half of a reconnect is forgetting what the
                // stale one told us.
                self.reloadProfiles()
                self.message = "`\(name)` will dial again on the next statement"
                self.isError = false
            }
        }
    }

    /// Make `name` the window's connection, and show its editors.
    ///
    /// `scopeEditors: false` is for the one caller that is already inside the
    /// editor — binding a tab to a connection — where re-scoping from here
    /// would fight the move the editor has just made.
    func selectProfile(_ name: String, scopeEditors: Bool = true) {
        activeProfile = name
        if scopeEditors { editor.setScope(name.isEmpty ? nil : name) }
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
                // Only a level that actually came back counts as loaded. Marking
                // a failed fetch loaded made the failure permanent: the expand
                // hook fires once and `didLoad` is what stops it firing again,
                // so a dropped connection could never be retried by reopening
                // the node.
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

    /// New SQL Editor, owned by one connection. The double-click and the
    /// context-menu entry on a connection row both land here.
    func openSQLEditor(for profile: String) {
        guard profilesByName[profile] != nil || roots.contains(where: { $0.name == profile })
        else { return }
        selectProfile(profile)
        editor.newTab(connection: profile)
        editor.focus()
        message = "new editor for `\(profile)`"
        isError = false
    }

    /// Every editor that belongs to a connection, open or closed — what the
    /// connection's Open Editor submenu lists.
    func editors(for profile: String) -> [SavedQueryRecord] { editor.editors(for: profile) }

    func openEditor(_ record: SavedQueryRecord) {
        if let connection = record.connection { selectProfile(connection) }
        editor.openEditor(record)
        editor.focus()
    }

    // MARK: - schema, one describe() per object, cached

    /// Point the inspector at an object and make sure its schema is there.
    ///
    /// A cache hit is synchronous and costs no round trip, so re-selecting a
    /// table the user already looked at is instant and silent. A miss goes to
    /// `catalogQueue`; `describe()` can open a connection and talk to a server,
    /// and the main thread is never allowed to wait on that.
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
        // Every in-flight describe carries the generation it was issued in. A
        // slow answer for a table the user has already clicked away from is
        // dropped instead of overwriting the pane.
        schemaGeneration &+= 1
        let generation = schemaGeneration
        catalogQueue.async { [weak self] in
            var detail: SchemaDetail?
            var failure: String?
            do {
                let json = try core.describe(profile: target.profile, path: target.path)
                // Decoding happens here too — it is the caller's JSON parse, not
                // the main thread's.
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

    /// The driver id recorded alongside a statement. Taken from the profile
    /// rather than looked up later: history has to stay readable after the
    /// connection it ran on has been deleted.
    private func driverID(for profile: String) -> String {
        profilesByName[profile]?.driver ?? roots.first { $0.name == profile }?.driver ?? ""
    }

    /// Opens a recorded statement in a **new** editor tab, bound to the
    /// connection it originally ran on.
    ///
    /// `editor.setText` replaces the *active* tab's text, so calling it alone
    /// would destroy whatever unsaved SQL happened to be in front of the user.
    /// `newTab()` makes a fresh tab active first, so nothing is overwritten and
    /// ⌘W still throws the copy away.
    private func openInNewEditorTab(sql: String, connection: String?) {
        let known = connection.flatMap { profilesByName[$0] != nil ? $0 : nil }
        // The tab is created *in* the connection it ran on, so it lands in that
        // connection's bar rather than being made somewhere else and rebound.
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

    /// Run Again: same statement, same connection, in its own tab. The tab is
    /// deliberate — the results that appear should have visible SQL next to
    /// them, and it still costs no one their buffer.
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

    /// What the user asked for. Resets the derived state, because a new
    /// statement is a new question — carrying the old sort into it would be
    /// applying an ORDER BY to a column that may not exist any more.
    func run(sql: String, directives: BlockDirectives) {
        baseSQL = sql
        sortColumn = nil
        sortAscending = true
        baseFilters = []
        execute(directives: directives)
    }

    /// Click a header: re-issue the SAME question with an ORDER BY pushed to
    /// the engine. Sorting the 2 048 rows that happen to be in the page cache
    /// would reorder 0.4% of a 500 000-row result and call it sorted.
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

    /// Right-click a cell -> "Filter by this value". Same rule as sorting: the
    /// predicate goes to the engine, so it is true of the whole result.
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

    func clearDerived() {
        sortColumn = nil
        baseFilters = []
        execute(directives: directives)
    }

    var hasDerivedClauses: Bool { sortColumn != nil || !baseFilters.isEmpty }

    /// `baseSQL` wrapped in whatever ORDER BY / WHERE the user has clicked
    /// together. One level of wrapping only — sorting twice re-wraps the base,
    /// it never nests.
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
        // MySQL only accepts double-quoted identifiers under ANSI_QUOTES, which
        // datagrep does not set; backticks are unambiguous there.
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
            // Recorded: a statement that was refused is exactly the one people
            // come back looking for, and it never reaches the engine to be
            // recorded any other way.
            history.executionBlocked(
                sql: sql, connection: profile, engine: driverID(for: profile),
                reason: message)
            return
        }
        // The per-connection read-only guard. It names the profile that refused
        // and which protection did the refusing — a bare "permission denied" is
        // the thing this feature exists to replace.
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
    /// `beginSheetModal` rather than `runModal`: the main thread keeps running.
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
        message = "running on \(profile)…"
        isError = false
        // Four string copies, no I/O: the entry itself is only written once the
        // statement reaches a terminal state.
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
                    self.results.clear()
                    // Never got a query handle, so `refreshFromCore` will never
                    // see this one go terminal.
                    self.history.executionFailedToStart("\(error)")
                }
            }
        }
    }

    private func adopt(_ handle: DatagrepQueryHandle) {
        query = handle  // the previous handle deinits here -> datagrep_query_free
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
        results.apply(status: status)
        resultGeneration &+= 1
        state = status.state
        // One notch per progress event. This is the ONLY thing that moves the
        // activity bar — no timer, no display link.
        if !status.state.isTerminal, status.rowsLoaded != rowsLoaded {
            progressPhase = progressPhase >= 0.999 ? 0 : min(1, progressPhase + 0.14)
        }
        rowsLoaded = status.rowsLoaded
        totalKnown = status.totalKnown
        elapsedMs = status.elapsedMs
        // Safe on every tick: this records once, when the query goes terminal.
        history.executionProgressed(
            state: status.state, rowsLoaded: status.rowsLoaded, elapsedMs: status.elapsedMs,
            error: status.error)
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
