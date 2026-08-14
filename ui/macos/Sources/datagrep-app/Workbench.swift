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
        .tint(model.isProd ? Color.red : nil)
        .sheet(isPresented: $model.showNewConnection) { NewConnectionSheet(model: model) }
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
            .overlay(alignment: .top) { ProdStripe(isProd: model.isProd) }

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
        .navigationSubtitle(model.connectionSubtitle)
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
                    ResultsEmptyState(model: model)
                        .frame(maxWidth: .infinity, maxHeight: .infinity)
                        .background(Color(nsColor: .textBackgroundColor))
                }
            }
            .overlay(alignment: .topTrailing) {
                if model.showsGrid {
                    ResultViewModeToggle(model: model)
                        .padding(8)
                }
            }
        )
    }
}

/// Grid ⇄ Text switch, floated in the results pane's top-right corner.
private struct ResultViewModeToggle: View {
    @ObservedObject var model: AppModel

    var body: some View {
        Picker("View", selection: $model.showResultAsText) {
            Image(systemName: "tablecells").tag(false)
            Image(systemName: "text.alignleft").tag(true)
        }
        .pickerStyle(.segmented)
        .labelsHidden()
        .frame(width: 82)
        .help("Switch between the grid and a copyable plain-text table")
        .background(.regularMaterial, in: RoundedRectangle(cornerRadius: 7, style: .continuous))
    }
}

/// The result as a selectable, column-aligned monospaced table. Rebuilt only
/// when a new result lands (keyed on `resultGeneration`), never per frame.
private struct ResultTextView: View {
    @ObservedObject var model: AppModel
    @State private var text = ""

    var body: some View {
        ScrollView([.horizontal, .vertical]) {
            Text(text.isEmpty ? "No rows." : text)
                .font(.system(size: 12, weight: .regular, design: .monospaced))
                .textSelection(.enabled)
                .foregroundStyle(.primary)
                .padding(12)
                .frame(maxWidth: .infinity, maxHeight: .infinity, alignment: .topLeading)
        }
        .frame(maxWidth: .infinity, maxHeight: .infinity)
        .background(Color(nsColor: .textBackgroundColor))
        .task(id: model.resultGeneration) { text = model.results.resultAsAlignedText() }
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

/// Three points of red along the top edge of the content. Drawn as an overlay
/// so it costs no layout and never moves anything.
private struct ProdStripe: View {
    let isProd: Bool
    var body: some View {
        if isProd {
            Rectangle()
                .fill(Color.red)
                .frame(height: 3)
                .accessibilityLabel("production connection")
                .help("This connection is marked production.")
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
            .keyboardShortcut("s", modifiers: [.control, .command])
            .help("Show or hide the connections sidebar (⌃⌘S)")

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
                Button("New Connection…") { model.showNewConnection = true }
                    .keyboardShortcut("n", modifiers: .command)
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

        ToolbarItemGroup(placement: .primaryAction) {
            if model.hasDerivedClauses {
                Button {
                    model.clearDerived()
                } label: {
                    Label("Clear sort & filters", systemImage: "line.3.horizontal.decrease.circle")
                }
                .help("Remove the ORDER BY / WHERE datagrep added by clicking headers and cells")
            }

            Button {
                model.runStatementUnderCaret()
            } label: {
                Label("Run", systemImage: "play.fill")
            }
            // NO `.keyboardShortcut` here. ⌘↩ is owned by the Query menu item
            // in AppDelegate, and binding it in both places fired the statement
            // TWICE on one press: the first result painted, then the second
            // run's opening status — zero rows, zero columns — wiped the grid a
            // frame later and left "running on …" behind. The menu keeps it
            // because that is where the shortcut is discoverable.
            .disabled(model.activeProfile.isEmpty || model.isRunning)
            .help("Run the statement under the caret  ⌘↩")

            Button {
                model.cancel()
            } label: {
                Label("Cancel", systemImage: "stop.fill")
            }
            .keyboardShortcut(".", modifiers: .command)
            .disabled(!model.isRunning)
            .help("Cancel — returns instantly, then reports what the server actually did  ⌘.")

            HistoryToolbarButton(history: model.history)
                .keyboardShortcut("y", modifiers: .command)

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
