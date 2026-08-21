import AppKit
import DatagrepKit
import SwiftUI

/// The whole window, in SwiftUI. Exactly two things are AppKit controls behind
/// an `NSViewControllerRepresentable`: the results grid (`NSTableView`) and
/// the SQL editor (`NSTextView`). The grid is not SwiftUI `Table`/`List`
/// because those do not virtualise predictably at a million rows.
struct Workbench: View {
    @ObservedObject var model: AppModel

    /// Bound, not delegated to `toggleSidebar(_:)`: a `NavigationSplitView`
    /// dragged shut keeps its state privately, leaving nothing to toggle back.
    /// This binding is the one the button, ⌃⌘S and the View menu all drive.
    private var visibility: Binding<NavigationSplitViewVisibility> {
        Binding(
            get: { model.sidebarShown ? .all : .detailOnly },
            set: { v in model.sidebarVisible = (v != .detailOnly) })
    }

    var body: some View {
        NavigationSplitView(columnVisibility: visibility) {
            SidebarView(model: model)
                .navigationSplitViewColumnWidth(min: 190, ideal: 258, max: 460)
        } detail: {
            DetailArea(model: model)
        }
        .navigationSplitViewStyle(.balanced)
        .overlay(alignment: .bottomTrailing) { UpdateNoticeView() }
        // A connection marked production tints every accent in the window red.
        .tint(model.markColor)
        .sheet(isPresented: $model.showNewConnection) { NewConnectionSheet(model: model) }
        // What the commit did, per document.
        .sheet(isPresented: $model.showMutationReport) {
            if let report = model.mutationReport {
                MutationReportSheet(model: model, report: report)
            }
        }
        // What the server holds now, for the documents the guard refused to
        // overwrite. Never up at the same time as the report sheet — the
        // re-read lands a runloop turn after the report is told to close.
        .sheet(isPresented: $model.showConflictReview) {
            if let review = model.conflictReview {
                ConflictReviewSheet(model: model, review: review)
            }
        }
        .animation(.smooth(duration: 0.25), value: model.sidebarShown)
        // Feed the live content width so the sidebar can auto-collapse before
        // the balanced split would clip it. The NSWindow's contentMinSize is
        // the hard floor; this minimum only needs to agree with it.
        .frame(minWidth: 480, minHeight: 400)
        .background {
            GeometryReader { proxy in
                Color.clear
                    .onChange(of: proxy.size.width, initial: true) { _, w in
                        model.windowContentWidth = w
                    }
            }
        }
    }
}

// MARK: - detail column

private struct DetailArea: View {
    @ObservedObject var model: AppModel
    @ObservedObject var stage = StartupStage.shared

    var body: some View {
        VStack(spacing: 0) {
            VSplitView {
                EditorPane(model: model)
                    .frame(minHeight: 90, idealHeight: 190, maxHeight: .infinity)
                    .padding(.horizontal, 10)
                    .padding(.top, 8)
                    .padding(.bottom, 5)

                ResultsPane(model: model)
                    // A low floor: the grid scrolls its own content, so the
                    // pane must be free to shrink well below its content
                    // height — otherwise a short window pushes the detail past
                    // the window edge instead of scrolling.
                    .frame(minHeight: 80, maxHeight: .infinity)
                    .padding(.horizontal, 10)
                    .padding(.top, 5)
                    .padding(.bottom, 8)
                    .overlay(alignment: .top) {
                        if model.isError, model.state == .failed {
                            ErrorCard(message: model.message) {
                                withAnimation(.smooth(duration: 0.2)) { model.isError = false }
                            }
                            .transition(.move(edge: .top).combined(with: .opacity))
                        }
                    }
            }
            .background(Color(nsColor: .underPageBackgroundColor))
            .frame(maxWidth: .infinity, maxHeight: .infinity)
            .safeAreaInset(edge: .top, spacing: 0) {
                if model.isRunning {
                    QueryProgressBar(model: model)
                        .transition(.opacity)
                }
            }
            // Attached BEFORE the progress-bar inset, so the inset places the
            // bar above it — as an overlay after the inset it landed exactly
            // over the 3 pt progress bar on any marked connection.
            .overlay(alignment: .top) { MarkStripe(color: model.markColor) }

            // A real bottom row, NOT a `.safeAreaInset`: a bar laid out via
            // safeAreaInset did not reserve its height until the window was
            // resized, leaving the status bar off-screen.
            StatusBar(model: model)
        }
        // A low detail minimum so narrowing the window shrinks the editor/grid
        // (which scroll) while the sidebar keeps its fixed width. The NSWindow
        // minimum is sidebar-min + this + slack, so the two always coexist.
        .frame(minWidth: 380, maxWidth: .infinity, maxHeight: .infinity)
        // `HistoryModel` is a nested ObservableObject, so the presentation flag
        // must be observed by something that observes *it* — that is what the
        // modifier is for.
        .historySheet(model.history)
        // Deferred with the rest of the chrome — see `StartupStage`. Attaching
        // `.inspector` costs ~35 ms of view-graph construction for a column
        // that starts hidden.
        .modifier(DeferredInspector(model: model))
        .animation(.smooth(duration: 0.22), value: model.isRunning)
        .animation(.smooth(duration: 0.2), value: model.isError)
        // NO `.navigationTitle` or `.navigationSubtitle`: they duplicate the
        // badge and cost ~280 pt of a toolbar that is out of room. The window
        // still has a title (AppDelegate owns it); the toolbar just does not
        // draw it (`RemoveToolbarTitle` below).
        // Deferring the toolbar controls saves ~80 ms at launch; the toolbar
        // *background* is a window property painted from the first frame, so
        // there is no reflow or height change when they arrive.
        .toolbar { if stage.contentReady { WorkbenchToolbar(model: model) } }
        // SwiftUI draws `NSWindow.title` inside the toolbar even with no
        // `.navigationTitle`, and re-shows it over an AppKit
        // `titleVisibility = .hidden` — the removal has to be said in
        // SwiftUI's own vocabulary.
        .modifier(RemoveToolbarTitle())
        .toolbarBackground(.visible, for: .windowToolbar)
    }
}

/// `.toolbar(removing: .title)` is macOS 15 API and the deployment target is
/// 14. On 14 the title simply draws again — crowded, not broken.
private struct RemoveToolbarTitle: ViewModifier {
    func body(content: Content) -> some View {
        if #available(macOS 15.0, *) {
            content.toolbar(removing: .title)
        } else {
            content
        }
    }
}

// MARK: - the two AppKit bridges, held back until the window is up

/// The SQL editor pane, drawn empty until `StartupStage` flips (see there).
/// Only this view and `ResultsPane` observe `StartupStage`, so the flip
/// re-renders two subtrees rather than the whole window.
private struct EditorPane: View {
    @ObservedObject var model: AppModel
    @ObservedObject private var stage = StartupStage.shared

    var body: some View {
        Chrome.pane(
            Group {
                if stage.contentReady {
                    SQLEditorView(controller: model.editor)
                } else {
                    Color.clear
                }
            }
        )
    }
}

/// The results pane. Same deal as `EditorPane` — the `NSTableView` is not
/// built before first paint.
private struct ResultsPane: View {
    @ObservedObject var model: AppModel
    @ObservedObject private var stage = StartupStage.shared

    var body: some View {
        // A real bottom row, NOT a `.safeAreaInset` — same lesson as the
        // window's status bar: an inset bar did not reserve its height until a
        // relayout, and here that would cover the last row of the grid.
        VStack(spacing: 0) {
            grid
            StagedEditsSlot(model: model, edits: model.edits)
        }
    }

    private var grid: some View {
        Chrome.pane(
            ZStack {
                // The grid is ALWAYS in the render tree at full opacity, and
                // the empty/text states are opaque covers on top of it. Never
                // `.opacity(0)` and never an `if`: SwiftUI takes a
                // zero-opacity platform view out of the render tree, and
                // re-attaching it on the next result gives the host fresh,
                // EMPTY layer contents — a blank pane until a window resize
                // forces AppKit to redraw. Keeping it mounted also preserves
                // scroll position, column widths and selection.
                if stage.contentReady {
                    ResultsGridView(controller: model.results, generation: model.resultGeneration)
                }
                if model.showsGrid && model.showResultAsText {
                    ResultTextView(model: model)
                }
                if !model.showsGrid {
                    ResultsEmptyState(model: model, tabs: model.editor.tabs)
                        .frame(maxWidth: .infinity, maxHeight: .infinity)
                        .background(Color(nsColor: .textBackgroundColor))
                }
            }
        )
    }
}

/// Its own view because it has to *observe* `PendingEdits`: the staging store
/// is written from the AppKit side of the grid, and a parent that merely reads
/// `model.edits` would not re-render when a cell was typed into.
private struct StagedEditsSlot: View {
    @ObservedObject var model: AppModel
    @ObservedObject var edits: PendingEdits

    var body: some View {
        if !edits.isEmpty {
            StagedEditsBar(model: model, edits: edits)
                .transition(.move(edge: .bottom).combined(with: .opacity))
        }
    }
}

/// The result as a selectable, column-aligned monospaced table. An AppKit
/// `NSTextView` rather than SwiftUI `Text` in a `ScrollView`: a two-axis
/// SwiftUI scroll view centres content smaller than the viewport, while a
/// text view top-left-aligns, scrolls both axes natively, and selects/copies
/// for free.
private struct ResultTextView: NSViewControllerRepresentable {
    @ObservedObject var model: AppModel

    func makeNSViewController(context: Context) -> ResultTextController { ResultTextController() }

    func updateNSViewController(_ vc: ResultTextController, context: Context) {
        _ = model.resultGeneration
        vc.setText(Self.style(model.results.resultAsAlignedText()))
    }

    static func style(_ raw: String) -> NSAttributedString {
        let mono = NSFont.monospacedSystemFont(ofSize: 12, weight: .regular)
        let monoBold = NSFont.monospacedSystemFont(ofSize: 12, weight: .semibold)
        let para = NSMutableParagraphStyle()
        para.lineSpacing = 3
        guard !raw.isEmpty else {
            return NSAttributedString(
                string: "No rows.",
                attributes: [.font: mono, .foregroundColor: NSColor.secondaryLabelColor])
        }
        let out = NSMutableAttributedString()
        for (i, line) in raw.components(separatedBy: "\n").enumerated() {
            let color: NSColor
            var font = mono
            if i == 0 {
                color = .controlAccentColor
                font = monoBold
            } else if i == 1 {
                color = .tertiaryLabelColor
            } else if line.hasPrefix("…") {
                color = .secondaryLabelColor
            } else {
                color = .labelColor
            }
            out.append(
                NSAttributedString(
                    string: i == 0 ? line : "\n" + line,
                    attributes: [.font: font, .foregroundColor: color, .paragraphStyle: para]))
        }
        return out
    }
}

/// Hosts the read-only, selectable text view for `ResultTextView`.
final class ResultTextController: NSViewController {
    private let textView = NSTextView()

    override func loadView() {
        let scroll = NSScrollView()
        scroll.hasVerticalScroller = true
        scroll.hasHorizontalScroller = true
        scroll.autohidesScrollers = true
        scroll.drawsBackground = true
        scroll.backgroundColor = .textBackgroundColor
        scroll.borderType = .noBorder

        textView.isEditable = false
        textView.isSelectable = true
        textView.drawsBackground = true
        textView.backgroundColor = .textBackgroundColor
        textView.textContainerInset = NSSize(width: 16, height: 16)
        textView.isVerticallyResizable = true
        textView.isHorizontallyResizable = true
        textView.autoresizingMask = []
        textView.maxSize = NSSize(
            width: CGFloat.greatestFiniteMagnitude, height: CGFloat.greatestFiniteMagnitude)
        textView.textContainer?.widthTracksTextView = false
        textView.textContainer?.containerSize = NSSize(
            width: CGFloat.greatestFiniteMagnitude, height: CGFloat.greatestFiniteMagnitude)

        scroll.documentView = textView
        self.view = scroll
    }

    func setText(_ attributed: NSAttributedString) {
        textView.textStorage?.setAttributedString(attributed)
    }
}

/// `.inspector` is a structural modifier — adding/removing it mid-session
/// would re-identify the whole detail subtree. It is attached exactly once,
/// with the rest of the deferred content, and never detached.
private struct DeferredInspector: ViewModifier {
    @ObservedObject var model: AppModel
    @ObservedObject var stage = StartupStage.shared

    func body(content: Content) -> some View {
        if stage.contentReady {
            content.inspector(isPresented: $model.showDetail) {
                DetailPanel(model: model)
                    .inspectorColumnWidth(min: 260, ideal: 380, max: 720)
            }
        } else {
            content
        }
    }
}

/// Three points of the connection's own colour along the top edge of the
/// content. Drawn as an overlay so it costs no layout and never moves anything.
private struct MarkStripe: View {
    let color: Color?
    var body: some View {
        if let color {
            Rectangle()
                .fill(color)
                .frame(height: 3)
                .accessibilityLabel("marked connection")
                .help("This connection is marked.")
        }
    }
}

// MARK: - connection badge

/// Where you are, in one place: engine, connection, database. Splitting that
/// identity across weak signals is how someone runs a statement against the
/// wrong server, so it is consolidated into the one saturated element in an
/// otherwise monochrome toolbar, filled with the connection's own colour.
private struct ConnectionBadge: View {
    @ObservedObject var model: AppModel

    /// What the server calls itself, preferred over the driver id: `product`
    /// distinguishes forks and compatible servers the driver id cannot.
    private var product: String {
        if let p = model.connectionInfo?.product, !p.isEmpty, !Self.isUnknown(p) { return p }
        return EngineStyle.displayName(for: model.activeDriver)
    }

    /// Product plus version, or just the product until a handshake has
    /// confirmed a version — never a guess.
    private var engine: String {
        guard let v = model.connectionInfo?.version, !v.isEmpty, !Self.isUnknown(v) else {
            return product
        }
        // `@@version` arrives as e.g. `8.0.36-0ubuntu0.22.04.1`; the build
        // suffix stays in the tooltip only.
        let short = v.split(separator: "-", maxSplits: 1).first.map(String.init) ?? v
        return "\(product) \(short)"
    }

    /// Some drivers hand up the literal string `unknown` instead of declining
    /// to answer; treated as absent so the badge never renders it.
    private static func isUnknown(_ s: String) -> Bool {
        s.caseInsensitiveCompare("unknown") == .orderedSame
    }

    /// The connection's own colour, grey when it has none.
    private var fill: Color {
        model.markColor ?? Color(nsColor: .systemGray)
    }

    /// One segment of the pill, shared by the three densities below.
    private func pill(@ViewBuilder _ content: () -> some View) -> some View {
        HStack(spacing: 6) { content() }
            .font(.system(size: 11, weight: .regular, design: .monospaced))
            .lineLimit(1)
            .foregroundStyle(.white)
            .padding(.horizontal, 10)
            .padding(.vertical, 4)
            .background(fill, in: RoundedRectangle(cornerRadius: 5, style: .continuous))
    }

    private var separator: some View { Text("·").foregroundStyle(.secondary) }

    /// Carries what the densest pill had to drop, including the full version.
    private var tooltip: String {
        var parts = [product]
        if let v = model.connectionInfo?.version, !v.isEmpty, !Self.isUnknown(v) {
            parts.append(v)
        } else {
            parts.append("version unknown — run a statement to learn it")
        }
        parts.append(model.activeProfile)
        if let db = model.connectionInfo?.database { parts.append(db) }
        if model.activeSafety.readOnly { parts.append(model.activeSafety.enforcement.headline) }
        // The pill has no chevron, so the tooltip is what says it opens.
        parts.append("click to switch connection")
        return parts.joined(separator: " · ")
    }

    /// The connection name — the one field that must survive every density.
    private var name: some View {
        Text(model.activeProfile)
            .fontWeight(.semibold)
            .fixedSize()
            .layoutPriority(3)
    }

    private var lock: some View {
        Group {
            if model.activeSafety.readOnly {
                Image(systemName: "lock.fill")
                    .help(model.activeSafety.enforcement.headline)
            }
        }
    }

    var body: some View {
        if model.activeProfile.isEmpty {
            pill {
                Text("No connection")
            }
            .help("No connection selected — ⌘N adds one")
        } else {
            // Sheds detail rather than truncating: left to itself SwiftUI is
            // free to cut the connection NAME while keeping the engine
            // segment, which is precisely backwards.
            ViewThatFits(in: .horizontal) {
                pill {
                    Text(engine).fixedSize()
                    separator
                    name
                    if let db = model.connectionInfo?.database {
                        separator
                        Text(db).fixedSize()
                    }
                    lock
                }
                pill {
                    name
                    if let db = model.connectionInfo?.database {
                        separator
                        Text(db)
                    }
                    lock
                }
                pill {
                    name
                    lock
                }
            }
            // The cap follows the window because NSToolbar budgets the badge's
            // CAP, not its drawn width, when deciding what to evict — it never
            // compresses an item, only hides tail items. A narrow window
            // trades the engine segment (still in the tooltip) for keeping
            // every icon on screen.
            .frame(maxWidth: model.windowContentWidth < 1080 ? 170 : 260)
            .help(tooltip)
        }
    }
}

// MARK: - toolbar

private struct WorkbenchToolbar: ToolbarContent {
    @ObservedObject var model: AppModel

    var body: some ToolbarContent {
        ToolbarItemGroup(placement: .navigation) {
            Button {
                withAnimation(.smooth(duration: 0.25)) { model.sidebarVisible.toggle() }
            } label: {
                Label("Toggle Sidebar", systemImage: "sidebar.leading")
            }
            // NO `.keyboardShortcut`: ⌃⌘S is owned by the View menu item in
            // AppDelegate. Binding it here too is the same double-binding that
            // made one ⌘↩ run a statement twice.
            .help("Show or hide the connections sidebar  ⌃⌘S")

            Button {
                model.showNewConnection = true
            } label: {
                Label("New Connection", systemImage: "powerplug")
            }
            .help("New connection (⌘N)")

            // The badge IS the connection switcher — one pill in an ordinary
            // slot, never `.principal`: a `.principal` item is not displaced
            // by narrowing, it EVICTS every other item into the `»` chevron.
            Menu {
                if model.roots.isEmpty {
                    Text("No connections yet")
                }
                ForEach(model.roots) { node in
                    Button {
                        model.selectProfile(node.name)
                    } label: {
                        Label {
                            Text("\(node.name)  ·  \(EngineStyle.displayName(for: node.driver))")
                        } icon: {
                            if node.name == model.activeProfile {
                                Image(systemName: "checkmark")
                            } else {
                                EngineIcon(node.driver, size: 14)
                            }
                        }
                    }
                }
                Divider()
                // ⌘N belongs to the File menu item; this entry only names it.
                Button("New Connection…") { model.showNewConnection = true }
                Button("Remove “\(model.activeProfile)”") { model.removeActiveProfile() }
                    .disabled(model.activeProfile.isEmpty)
            } label: {
                ConnectionBadge(model: model)
            }
            .menuStyle(.button)
            .buttonStyle(.plain)
            // No chevron (the tooltip says it opens), and no `.fixedSize()` —
            // that would pin the badge at its ideal width and starve the
            // ViewThatFits ladder above.
            .menuIndicator(.hidden)

            // DECLARATION ORDER IS SURVIVAL PRIORITY: a toolbar group
            // overflows from its tail, so whatever is declared last goes into
            // the `»` chevron first. ONE group, so that order holds for the
            // whole toolbar — a section boundary would reorder who dies first.
            // The primary verb comes right after the identity pill.
            //
            // One button that swaps Run/Cancel: the two can never both be
            // live, and one glyph-swapping button never shows a dead control.
            Button {
                if model.isRunning { model.cancel() } else { model.runStatementUnderCaret() }
            } label: {
                Label(
                    model.isRunning ? "Cancel" : "Run",
                    systemImage: model.isRunning ? "stop.fill" : "play.fill")
            }
            // NO `.keyboardShortcut` here. ⌘↩ and ⌘. are owned by the Query
            // menu items in AppDelegate; binding ⌘↩ in both places fires the
            // statement TWICE on one press, and the second run's empty opening
            // status wipes the first result's grid a frame later.
            .disabled(model.activeProfile.isEmpty && !model.isRunning)
            .help(
                model.isRunning
                    ? "Cancel — returns instantly, then reports what the server actually did  ⌘."
                    : "Run the statement under the caret  ⌘↩")

            // Grid ⇄ Text result view. Shown-and-disabled rather than hidden,
            // so Run does not slide sideways the moment a result lands.
            Picker("Result view", selection: $model.showResultAsText) {
                Image(systemName: "tablecells").tag(false)
                Image(systemName: "text.alignleft").tag(true)
            }
            .pickerStyle(.segmented)
            .labelsHidden()
            .disabled(!model.showsGrid)
            .help("Show the result as a grid or as a copyable plain-text table")

            Button {
                model.clearDerived()
            } label: {
                Label("Clear sort & filters", systemImage: "line.3.horizontal.decrease.circle")
            }
            .disabled(!model.hasDerivedClauses)
            .help("Remove the ORDER BY / WHERE datagrep added by clicking headers and cells")

            // ⌘Y is the Query menu's; not re-bound here.
            HistoryToolbarButton(history: model.history)

            Button {
                withAnimation(.smooth(duration: 0.22)) { model.showDetail.toggle() }
            } label: {
                Label("Inspector", systemImage: "sidebar.trailing")
            }
            .help("Show or hide the cell inspector  ⌘I")
        }
    }
}

// MARK: - new connection

/// Add a connection by fields, with the URL one disclosure away. The two
/// representations are the same value (see `ConnectionForm`), drawn by the
/// same `ConnectionFieldsView` the Edit sheet uses. `datagrep_profiles_add`
/// still takes a URL and still lifts any password in it into the keychain
/// before anything is written.
struct NewConnectionSheet: View {
    @ObservedObject var model: AppModel
    @ObservedObject var form: ConnectionForm

    init(model: AppModel) {
        self.model = model
        self.form = model.newForm
    }

    var body: some View {
        VStack(alignment: .leading, spacing: 14) {
            HStack(spacing: 9) {
                EngineIcon(form.engineID, size: 22)
                VStack(alignment: .leading, spacing: 1) {
                    Text("New Connection").font(.headline)
                    Text(EngineStyle.displayName(for: form.engineID))
                        .font(.caption)
                        .foregroundStyle(.secondary)
                }
                Spacer()
            }

            EnginePicker(form: form)

            ConnectionFieldsView(form: form, name: $form.name)

            ConnectionTestRow(state: model.newTest, enabled: form.isComplete) {
                model.testNewConnection()
            }

            if let err = model.newError {
                Callout(symbol: "exclamationmark.triangle.fill", tone: .error, text: err)
                    .transition(.opacity)
            }

            HStack {
                Spacer()
                Button("Cancel") {
                    model.newError = nil
                    model.newTest.clear()
                    model.showNewConnection = false
                }
                .keyboardShortcut(.cancelAction)
                Button("Add") { model.addProfileFromForm() }
                    .keyboardShortcut(.defaultAction)
                    .disabled(form.name.isEmpty || !form.isComplete)
            }
        }
        .padding(20)
        .frame(width: 512)
        .animation(.smooth(duration: 0.2), value: model.newError)
    }
}
