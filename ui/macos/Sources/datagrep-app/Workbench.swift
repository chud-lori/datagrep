import AppKit
import DatagrepKit
import SwiftUI

/// The whole window, in SwiftUI.
///
/// Exactly two things in here are AppKit controls, and both are behind an
/// `NSViewControllerRepresentable`: the results grid (`NSTableView`) and the SQL
/// editor (`NSTextView`). Everything else — sidebar, toolbar, status bar,
/// inspector, sheets — is SwiftUI.
///
/// The grid is not SwiftUI `Table`/`List` because those do not virtualise
/// predictably at a million rows, and virtualisation is the product.
struct Workbench: View {
    @ObservedObject var model: AppModel

    /// Bound, not delegated to `toggleSidebar(_:)`. A `NavigationSplitView`
    /// dragged shut keeps its state privately, and there is then nothing to
    /// toggle back — this binding is the only version we own, so it is the one
    /// the button, the ⌃⌘S shortcut and the View menu all drive.
    private var visibility: Binding<NavigationSplitViewVisibility> {
        Binding(
            // Shown only when the user wants it AND the window is wide enough —
            // below that it collapses cleanly rather than clipping off-screen.
            get: { model.sidebarShown ? .all : .detailOnly },
            set: { v in model.sidebarVisible = (v != .detailOnly) })
    }

    var body: some View {
        NavigationSplitView(columnVisibility: visibility) {
            SidebarView(model: model)
                // A hard 190 pt floor: the drag cannot take the column to a
                // width the user can no longer grab.
                .navigationSplitViewColumnWidth(min: 190, ideal: 258, max: 460)
        } detail: {
            DetailArea(model: model)
        }
        .navigationSplitViewStyle(.balanced)
        // Renders nothing unless a newer release exists, and triggers its own
        // once-per-launch check. One line is the whole integration.
        .overlay(alignment: .bottomTrailing) { UpdateNoticeView() }
        // Production guardrail, layer 1: a connection marked production tints
        // every accent in the window red.
        .tint(model.markColor)
        .sheet(isPresented: $model.showNewConnection) { NewConnectionSheet(model: model) }
        // What the commit did, per document. Presented from the same place as
        // the other sheets so nothing about a write is drawn inside the grid's
        // AppKit host.
        .sheet(isPresented: $model.showMutationReport) {
            if let report = model.mutationReport {
                MutationReportSheet(model: model, report: report)
            }
        }
        .animation(.smooth(duration: 0.25), value: model.sidebarShown)
        // Feed the live content width so the sidebar can auto-collapse before the
        // balanced split would clip it. The window's own contentMinSize is the
        // hard floor (set on the NSWindow); this frame minimum only needs to
        // agree with it, not enforce the 900-wide "sidebar fits" width.
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
                    // A low floor, not a real minimum: the grid scrolls its own
                    // content, so the pane must be free to shrink well below a
                    // "show every row" height — otherwise a short window pushes
                    // the whole detail past the window edge instead of scrolling.
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
            // The window's own plane, so the two panes above read as raised
            // layers against it rather than as one continuous sheet of gray.
            .background(Color(nsColor: .underPageBackgroundColor))
            .frame(maxWidth: .infinity, maxHeight: .infinity)
            .safeAreaInset(edge: .top, spacing: 0) {
                if model.isRunning {
                    QueryProgressBar(model: model)
                        .transition(.opacity)
                }
            }
            // Attached BEFORE the progress-bar inset, so the inset places the
            // bar above it. As an overlay on the result of that inset it landed
            // on the composite's top edge — exactly over the 3 pt progress bar,
            // which made query progress invisible on any marked connection.
            .overlay(alignment: .top) { MarkStripe(color: model.markColor) }

            // A real bottom row, NOT a `.safeAreaInset`: an always-present bar
            // laid out via safeAreaInset did not reserve its height until the
            // window was resized, so the grid filled to the window edge and the
            // status bar sat off-screen. As a VStack row it is always laid out,
            // and the split above simply takes the remaining height.
            StatusBar(model: model)
        }
        // A low detail minimum so narrowing the window shrinks the EDITOR/GRID
        // first (the grid scrolls its own columns) while the sidebar keeps its
        // fixed width — `.balanced` shrinks the detail to make room for the
        // sidebar, so the sidebar never gets squeezed off its leading edge. The
        // window minimum (set on the NSWindow) is sidebar-min + this + slack, so
        // the two can always coexist without clipping.
        .frame(minWidth: 380, maxWidth: .infinity, maxHeight: .infinity)
        // `HistoryModel` is a nested ObservableObject, so the presentation flag
        // has to be observed by something that observes *it* — that is what the
        // modifier is for. One line here, no state in this view.
        .historySheet(model.history)
        // Deferred with the rest of the chrome — see `StartupStage`. Attaching
        // `.inspector` costs ~35 ms of view-graph construction for a column that
        // starts hidden, which is 14% of the 250 ms budget spent on something
        // nobody can see yet.
        .modifier(DeferredInspector(model: model))
        .animation(.smooth(duration: 0.22), value: model.isRunning)
        .animation(.smooth(duration: 0.2), value: model.isError)
        .navigationTitle(model.activeProfile.isEmpty ? "datagrep" : model.activeProfile)
        // NO `.navigationSubtitle`. Every fact it carried is now in the badge
        // (engine, database), the read-only accessory (the lock) and the status
        // bar (state, rows) — and it truncated mid-word even at a wide window,
        // which is the exact failure the status bar's density ladder exists to
        // avoid. It was also ~280 pt of a toolbar measured to be out of room.
        // The single most expensive thing in the window at launch (~80 ms of
        // the ~330 ms it used to take). The toolbar's *background* is a window
        // property and is painted from the first frame either way, so what is
        // deferred is the controls inside an already-correct bar — no reflow,
        // no height change.
        .toolbar { if stage.contentReady { WorkbenchToolbar(model: model) } }
        .toolbarBackground(.visible, for: .windowToolbar)
    }
}

// MARK: - the two AppKit bridges, held back until the window is up

/// The SQL editor pane. See `StartupStage`: on the very first pass the pane is
/// drawn empty, so `NSWindow(contentViewController:)` does not have to build an
/// `NSTextView`, a TextKit 1 stack, the tab bar's own hosting view and a
/// restored session before the user sees anything. The pane's frame is
/// identical either way, so the editor appearing moves no pixels.
///
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

/// The results pane. Same deal as `EditorPane` — the `NSTableView`, its ruler
/// and its 30 columns are not built before first paint. The empty state was
/// already what an unstarted session shows, so holding the grid back changes
/// nothing the user sees.
private struct ResultsPane: View {
    @ObservedObject var model: AppModel
    @ObservedObject private var stage = StartupStage.shared

    var body: some View {
        // A real bottom row, NOT a `.safeAreaInset` — the same lesson the
        // window's status bar carries: a bar attached as a safe-area inset did
        // not reserve its height until something forced a relayout, and here
        // that would put it over the last row of the grid, which is exactly the
        // row someone has just been editing.
        VStack(spacing: 0) {
            grid
            StagedEditsSlot(model: model, edits: model.edits)
        }
    }

    private var grid: some View {
        Chrome.pane(
            ZStack {
                // `.opacity()` and NOT an `if`, so the grid is built once and
                // keeps its scroll position, column widths and selection across
                // an empty result. But NO implicit animation on it: animating
                // opacity on a hosted AppKit view left the layer stuck at 0
                // after fading in, so rows appeared for a frame and then
                // vanished with the data still loaded underneath.
                // The grid is ALWAYS rendered at full opacity, and the empty
                // state is an opaque cover on top of it. Never `.opacity(0)`:
                // SwiftUI takes a zero-opacity platform view out of the render
                // tree, and the moment a result arrived and flipped it back to
                // 1 the host was re-attached with fresh, EMPTY layer contents —
                // drawn milliseconds earlier, wiped on arrival. That is why the
                // pane stayed blank until a window resize forced AppKit to
                // redraw everything.
                if stage.contentReady {
                    ResultsGridView(controller: model.results, generation: model.resultGeneration)
                }
                // Text view: an opaque cover over the grid (same reasoning as the
                // empty state — the grid stays in the render tree underneath).
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

/// The staged-edits bar's place in the layout, and nothing else.
///
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
/// `NSTextView` rather than a SwiftUI `Text` in a `ScrollView`: a two-axis
/// SwiftUI scroll view centres content smaller than the viewport (the table
/// floated in the middle of the pane), while a text view top-left-aligns and
/// scrolls both axes natively — and it selects/copies for free.
///
/// Styled by line so it does not read as a wall of grey: the header row is bold
/// in the accent colour, the rule under it dimmed, the "N more rows" footer
/// secondary — the structure a plain string loses.
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

/// `.inspector` is a structural modifier, so it cannot be added and removed
/// freely — doing that mid-session would re-identify the whole detail subtree.
/// It is attached exactly once, in the same state change that attaches the
/// editor and the grid, and never detached again.
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

/// Where you are, in one place: engine, connection, database.
///
/// The same three facts used to be spread over the connection picker, the
/// window subtitle and the status bar, with the database named in none of them.
/// Splitting an identity across three weak signals is how someone runs a
/// statement against the wrong server, so this consolidates them into the one
/// saturated element in an otherwise monochrome toolbar.
///
/// The connection's own colour fills it, which is what finally makes that
/// colour *readable* rather than an ambient wash.
private struct ConnectionBadge: View {
    @ObservedObject var model: AppModel

    /// What the server calls itself, preferred over the driver id: `product` is
    /// the only thing that separates MariaDB from MySQL, or OpenSearch from
    /// Elasticsearch — a distinction the driver id cannot make.
    private var product: String {
        if let p = model.connectionInfo?.product, !p.isEmpty, !Self.isUnknown(p) { return p }
        return EngineStyle.displayName(for: model.activeDriver)
    }

    /// `MySQL 8.0.36`, or just `MySQL` until a handshake has confirmed a
    /// version. Never a guess: an unconfirmed version is the number someone
    /// would quote when asking whether a feature exists on their server.
    private var engine: String {
        guard let v = model.connectionInfo?.version, !v.isEmpty, !Self.isUnknown(v) else {
            return product
        }
        // `@@version` arrives as e.g. `8.0.36-0ubuntu0.22.04.1`. The build
        // suffix is packaging trivia in the one element the eye lands on; the
        // whole string stays in the tooltip.
        let short = v.split(separator: "-", maxSplits: 1).first.map(String.init) ?? v
        return "\(product) \(short)"
    }

    /// Some drivers hand up the literal string `unknown` instead of declining
    /// to answer, which would otherwise render as `PostgreSQL unknown` in the
    /// most saturated element in the window. Treated as absent here so the
    /// badge keeps its promise; the drivers should report nothing at all.
    private static func isUnknown(_ s: String) -> Bool {
        s.caseInsensitiveCompare("unknown") == .orderedSame
    }

    /// The connection's own colour, grey when it has none.
    ///
    /// This used to short-circuit on `isProd` and return red first — but
    /// `isProd` *was* "has a colour", so the branch below was reachable only
    /// when the colour was nil, and the badge could therefore only ever be red
    /// or grey. A connection marked green got a red pill while the window tint
    /// and the sidebar band both painted it green correctly.
    private var fill: Color {
        model.markColor ?? Color(nsColor: .systemGray)
    }

    /// One segment of the pill. Split out so the three densities below stay
    /// readable rather than three copies of the same modifier stack.
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

    /// Carries what the densest pill had to drop, including the full
    /// unshortened version string.
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
        return parts.joined(separator: " · ")
    }

    /// The connection name, which must survive every density: it is the field
    /// that answers "am I about to run this on the right server".
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
            // Not an EmptyView. The most saturated element in the window
            // pointing at the thing you have to do first is worth more than a
            // hole in the middle of the toolbar.
            pill {
                Text("No connection")
            }
            .help("No connection selected — ⌘N adds one")
        } else {
            // Sheds detail rather than truncating. Left to itself SwiftUI was
            // free to cut the connection NAME while keeping `PostgreSQL 16.2`,
            // which is precisely backwards. Same idiom the status bar already
            // uses for its four densities.
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
            // The badge must never be the widest thing in the toolbar. 420 pt
            // was not a cap on a ~1000 pt toolbar — it let the pill keep full
            // width while Run, Cancel and New Connection went to the overflow
            // chevron, which is the reverse of the intended priority.
            .frame(maxWidth: 260)
            .help(tooltip)
        }
    }
}

// MARK: - toolbar

private struct WorkbenchToolbar: ToolbarContent {
    @ObservedObject var model: AppModel

    var body: some ToolbarContent {
        ToolbarItemGroup(placement: .navigation) {
            // The fix for "I collapsed the sidebar and could not get it back".
            Button {
                withAnimation(.smooth(duration: 0.25)) { model.sidebarVisible.toggle() }
            } label: {
                Label("Toggle Sidebar", systemImage: "sidebar.leading")
            }
            // NO `.keyboardShortcut`: ⌃⌘S is owned by the View menu item in
            // AppDelegate. Binding it here too is the same double-binding that
            // made one ⌘↩ run a statement twice.
            .help("Show or hide the connections sidebar  ⌃⌘S")

            // New Connection lives in the window toolbar, as its own icon.
            //
            // It used to be reachable only from the File menu and from inside
            // the toolbar's connection menu — but that menu is labelled with
            // the CURRENT connection, so it reads as a switcher and the add
            // hides inside it. A + on the sidebar's section header was tried
            // and rejected: at that size, next to a secondary-grey label, it
            // reads as decoration however it is coloured. A toolbar icon is
            // where a window-level verb belongs, and it stays visible with the
            // sidebar collapsed.
            Button {
                model.showNewConnection = true
            } label: {
                Label("New Connection", systemImage: "powerplug")
            }
            .help("New connection (⌘N)")

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
                // Icon AND name: a bare chevron is not a control.
                HStack(spacing: 5) {
                    EngineIcon(model.activeDriver, size: 15)
                    Text(model.activeProfile.isEmpty ? "No connection" : model.activeProfile)
                        .font(.callout)
                        .lineLimit(1)
                }
            }
            .menuStyle(.borderlessButton)
            .fixedSize()
            .help("Switch, add or remove a connection")
        }

        ToolbarItem(placement: .principal) {
            ConnectionBadge(model: model)
        }

        // DECLARATION ORDER IS SURVIVAL PRIORITY. A toolbar group overflows
        // from its tail, so whatever is declared last is what disappears into
        // the `»` chevron first. Run was previously declared third, behind two
        // items that only sometimes exist, and went to the chevron at ordinary
        // window widths. The primary verb of the app now comes first.
        ToolbarItemGroup(placement: .primaryAction) {
            // Run and Cancel were two buttons that could never both be live —
            // one was always greyed out. One button that swaps its glyph costs
            // half the width and never shows a dead control.
            Button {
                if model.isRunning { model.cancel() } else { model.runStatementUnderCaret() }
            } label: {
                Label(
                    model.isRunning ? "Cancel" : "Run",
                    systemImage: model.isRunning ? "stop.fill" : "play.fill")
            }
            // NO `.keyboardShortcut` here. ⌘↩ and ⌘. are owned by the Query
            // menu items in AppDelegate, and binding ⌘↩ in both places fired
            // the statement TWICE on one press: the first result painted, then
            // the second run's opening status — zero rows, zero columns — wiped
            // the grid a frame later and left "running on …" behind.
            .disabled(model.activeProfile.isEmpty && !model.isRunning)
            .help(
                model.isRunning
                    ? "Cancel — returns instantly, then reports what the server actually did  ⌘."
                    : "Run the statement under the caret  ⌘↩")

            // Grid ⇄ Text result view. In the toolbar (the way Finder keeps its
            // icon/list/column switcher) rather than floated over the grid, where
            // it covered the last column's header.
            //
            // Shown-and-disabled rather than hidden, so Run does not slide
            // sideways the moment a result lands. The editor's tab menu already
            // states the rule: an entry that appears only sometimes is harder
            // to learn than one that says why it is off.
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

/// Add a connection by describing it — host, port, database, user, password —
/// the way every other client asks, with the URL one disclosure away for
/// anyone who already has one to paste.
///
/// The two representations are the same value (see `ConnectionForm`), and the
/// same `ConnectionFieldsView` the Edit sheet uses is what draws them, so
/// adding and editing a connection are visibly one design. `datagrep_profiles_add`
/// still takes a URL and still lifts any password in it into the keychain
/// before anything is written — that path is unchanged.
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
