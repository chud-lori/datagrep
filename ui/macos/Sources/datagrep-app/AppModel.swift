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
    let env: String

    @Published var children: [CatalogNode] = []
    @Published var isLoading = false
    @Published var loadError: String?
    @Published var scanPrefix: String = ""
    @Published var didLoad = false
    @Published var isExpanded = false {
        didSet {
            guard isExpanded, !didLoad, !isLoading else { return }
            // ScanOnly refuses to enumerate without a prefix — this is what stops
            // the app firing `KEYS *` at a 40 GB Redis (design §3.1).
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
        self.env = profile.env
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
        self.env = ""
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
        isProfile ? "\(EngineStyle.displayName(for: driver)) · \(env)" : nil
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

    @Published var roots: [CatalogNode] = []
    @Published var activeProfile: String = ""
    @Published var activeEnv: String = "dev"
    @Published var sqlText: String = ""
    @Published var searchText: String = ""

    @Published var state: QueryState? = nil
    @Published var rowsLoaded: UInt64 = 0
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

    // New-connection sheet.
    @Published var showNewConnection = false
    @Published var newName: String = ""
    @Published var newURL: String = ""
    @Published var newError: String?

    /// FFI gap: `datagrep_profiles_add` hard-codes `env: Env::Dev`, so no profile
    /// created through this ABI can ever report `prod`. The §3.8 red-chrome
    /// guardrail would be unreachable dead code without a client-side marker,
    /// so one profile name per line is remembered here (UserDefaults, never the
    /// profile store — the engine owns that file).
    @Published var prodMarked: Set<String> = [] {
        didSet { UserDefaults.standard.set(Array(prodMarked), forKey: Self.prodKey) }
    }
    private static let prodKey = "datagrep.prodMarkedProfiles"

    /// Sidebar visibility, bound to `NavigationSplitView`'s `columnVisibility`
    /// and persisted. Bound rather than driven by `toggleSidebar(_:)` because
    /// the bound value is the only version we can guarantee is recoverable —
    /// a split view dragged shut has no state we own.
    @Published var sidebarVisible = true {
        didSet { UserDefaults.standard.set(sidebarVisible, forKey: Self.sidebarKey) }
    }
    private static let sidebarKey = "datagrep.sidebarVisible"

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

    var isProd: Bool { activeEnv == "prod" || prodMarked.contains(activeProfile) }
    var isRunning: Bool { state.map { !$0.isTerminal } ?? false }

    /// `.navigationSubtitle` — the connection state, in the titlebar, always.
    var connectionSubtitle: String {
        guard !activeProfile.isEmpty else { return "no connection" }
        let driver = roots.first { $0.name == activeProfile }?.driver ?? "?"
        let env = isProd ? "PRODUCTION" : activeEnv
        guard let state else { return "\(driver) · \(env) · idle" }
        return "\(driver) · \(env) · \(state.rawValue) · \(rowsLoaded.formatted()) rows"
    }

    /// Where the engine keeps its profile store. Not the temp directory: a
    /// connection you added has to still be there tomorrow.
    static var profilesDBPath: String {
        let dir = FileManager.default
            .urls(for: .applicationSupportDirectory, in: .userDomainMask)[0]
            .appendingPathComponent("datagrep", isDirectory: true)
        try? FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
        return dir.appendingPathComponent("profiles.sqlite").path
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

        prodMarked = Set(UserDefaults.standard.stringArray(forKey: Self.prodKey) ?? [])
        if UserDefaults.standard.object(forKey: Self.sidebarKey) != nil {
            sidebarVisible = UserDefaults.standard.bool(forKey: Self.sidebarKey)
        }

        sqlText = """
            -- ⌘⏎ runs the statement under the caret.
            -- Block directives (design §3.6) are parsed and shown in the status bar:
            -- @limit 1000000
            -- @timeout 30s
            SELECT name, type FROM sqlite_master ORDER BY name;
            """
        editor.setText(sqlText)

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
            let profiles = try core.profiles()
            roots = profiles.map { p in
                let n = CatalogNode(profile: p)
                n.onExpand = { [weak self] node, prefix in self?.load(node, prefix: prefix) }
                return n
            }
            if !roots.contains(where: { $0.name == activeProfile }) {
                activeProfile = profiles.first?.name ?? ""
                activeEnv = profiles.first?.env ?? "dev"
            }
        } catch {
            message = "could not list profiles: \(error)"
            isError = true
        }
    }

    func addProfile(name: String, url: String) {
        guard let core else { return }
        let n = name.trimmingCharacters(in: .whitespacesAndNewlines)
        let u = url.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !n.isEmpty, !u.isEmpty else {
            newError = "both a name and a connection URL are required"
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
                self.newName = ""
                self.newURL = ""
                self.reloadProfiles()
                self.activeProfile = n
                self.activeEnv = self.roots.first { $0.name == n }?.env ?? "dev"
                self.message = "added profile `\(n)`"
                self.isError = false
            }
        }
    }

    func removeActiveProfile() {
        guard let core, !activeProfile.isEmpty else { return }
        let n = activeProfile
        do {
            try core.removeProfile(name: n)
            prodMarked.remove(n)
            reloadProfiles()
            message = "removed profile `\(n)`"
            isError = false
        } catch {
            message = "\(error)"
            isError = true
        }
    }

    func selectProfile(_ name: String) {
        activeProfile = name
        activeEnv = roots.first { $0.name == name }?.env ?? "dev"
    }

    /// §3.8 layer 1: the window turns red for a connection the user has said is
    /// production. Client-side only, and the UI says exactly that.
    func toggleProdMark(_ name: String) {
        if prodMarked.contains(name) {
            prodMarked.remove(name)
        } else {
            prodMarked.insert(name)
        }
        objectWillChange.send()
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
                node.didLoad = true
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
        activeProfile = node.profile
        if let root = roots.first(where: { $0.profile == node.profile }) { activeEnv = root.env }
        if node.isDescribable { showSchema(for: node) }
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
        guard let core else { return }
        let sql = effectiveSQL
        let profile = directives.connection ?? activeProfile
        results.sortColumn = sortColumn
        results.sortAscending = sortAscending
        if directives.readOnly && SQLBlocks.isWriteStatement(sql) {
            message =
                "blocked by `-- @readonly` — client-side classifier only; the server was not asked to enforce read-only"
            isError = true
            return
        }
        message = "running on \(profile)…"
        isError = false
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
        state = status.state
        // One notch per progress event. This is the ONLY thing that moves the
        // activity bar — no timer, no display link.
        if !status.state.isTerminal, status.rowsLoaded != rowsLoaded {
            progressPhase = progressPhase >= 0.999 ? 0 : min(1, progressPhase + 0.14)
        }
        rowsLoaded = status.rowsLoaded
        totalKnown = status.totalKnown
        elapsedMs = status.elapsedMs
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
