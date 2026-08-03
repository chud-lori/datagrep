import AppKit
import DbxKit
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

    var symbol: String {
        if isProfile {
            switch driver {
            case "redis": return "bolt.horizontal.circle"
            case "mongo": return "doc.text.magnifyingglass"
            case "sqlite": return "internaldrive"
            default: return "server.rack"
            }
        }
        switch kind {
        case "database", "schema": return "cylinder"
        case "table", "view": return "tablecells"
        case "collection": return "doc.text"
        case "key", "hash", "string": return "key"
        case "column", "field": return "tag"
        default: return "circle"
        }
    }

    var badge: String? {
        if isProfile { return "\(driver) · \(env)" }
        switch enumeration {
        case .cheap: return nil
        case .scanOnly: return "scan"
        case .paged: return "paged"
        case .onDemand: return nil
        }
    }

    var isPreviewable: Bool {
        ["table", "collection", "view", "key", "hash", "string"].contains(kind)
    }
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

    var isProd: Bool { activeEnv == "prod" }
    var isRunning: Bool { state.map { !$0.isTerminal } ?? false }

    private let queryQueue = DispatchQueue(label: "dbx.query", qos: .userInitiated)
    private let catalogQueue = DispatchQueue(label: "dbx.catalog", qos: .userInitiated)
    private var core: DbxCoreHandle?
    private var query: DbxQueryHandle?

    // MARK: - boot

    func boot() {
        results.onNestedCell = { [weak self] row, col, window in
            guard let self else { return }
            self.detailTitle = "row \(row + 1) · column \(col + 1)"
            self.detailBody =
                Self.prettify(window.detailJSON(absoluteRow: UInt64(row), col: col))
                ?? "(no detail available)"
            self.showDetail = true
        }
        results.onHiddenColumnsChanged = { [weak self] n in
            guard let self else { return }
            self.hiddenColumns = n
        }
        editor.onSelectionChanged = { [weak self] in self?.refreshDirectives() }

        sqlText = """
            -- ⌘⏎ runs the statement under the caret.
            -- Block directives (design §3.6) are parsed and shown in the status bar:
            -- @limit 1000000
            -- @timeout 30s
            SELECT * FROM public.events;

            -- A smaller bounded result:
            -- @limit 500
            SELECT * FROM public.users;
            """
        editor.setText(sqlText)

        let dbPath = FileManager.default.temporaryDirectory
            .appendingPathComponent("dbx-ui-profiles.sqlite").path
        do {
            let c = try DbxCoreHandle(profilesDBPath: dbPath)
            core = c
            let profiles = try c.profiles()
            roots = profiles.map { p in
                let n = CatalogNode(profile: p)
                n.onExpand = { [weak self] node, prefix in self?.load(node, prefix: prefix) }
                return n
            }
            activeProfile = profiles.first?.name ?? ""
            activeEnv = profiles.first?.env ?? "dev"
            message = "core ready · \(profiles.count) profiles · nothing connected yet"
        } catch {
            message = "dbx_core_new failed: \(error)"
            isError = true
        }
        refreshFootprint()
        refreshDirectives()
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

    func run(sql: String, directives: BlockDirectives) {
        guard let core else { return }
        let profile = directives.connection ?? activeProfile
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

    private func adopt(_ handle: DbxQueryHandle) {
        query = handle  // the previous handle deinits here -> dbx_query_free
        results.beginNewResult(pager: RowPager(query: handle, pageSize: 512, maxPages: 4))
        handle.onProgress { [weak self, weak handle] in
            // Already hopped to main and coalesced by DbxQueryHandle.
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
        rowsLoaded = status.rowsLoaded
        totalKnown = status.totalKnown
        elapsedMs = status.elapsedMs
        if let e = status.error {
            message = e
            isError = true
        } else if status.state == .done && status.columns.isEmpty && status.rowsLoaded == 0 {
            message =
                "statement completed; this ABI carries no Shape::Ack, so an affected-row count is not available (dbx-cli README gap #3)"
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
