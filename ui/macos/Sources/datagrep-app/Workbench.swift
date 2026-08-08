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
            get: { model.sidebarVisible ? .all : .detailOnly },
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
        // §3.8 guardrail, layer 1: a connection marked production tints every
        // accent in the window red.
        .tint(model.isProd ? Color.red : nil)
        .sheet(isPresented: $model.showNewConnection) { NewConnectionSheet(model: model) }
        .animation(.smooth(duration: 0.25), value: model.sidebarVisible)
        .frame(minWidth: 900, minHeight: 560)
    }
}

// MARK: - detail column

private struct DetailArea: View {
    @ObservedObject var model: AppModel

    var body: some View {
        VSplitView {
            EditorPane(model: model)
                .frame(minHeight: 90, idealHeight: 190, maxHeight: .infinity)
                .padding(.horizontal, 10)
                .padding(.top, 8)
                .padding(.bottom, 5)

            ResultsPane(model: model)
            .frame(minHeight: 160, maxHeight: .infinity)
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
        // The window's own plane, so the two panes above read as raised layers
        // against it rather than as one continuous sheet of gray.
        .background(Color(nsColor: .underPageBackgroundColor))
        .frame(minWidth: 560, maxWidth: .infinity, maxHeight: .infinity)
        // Inset, not a VStack row: the grid keeps the height it thinks it has,
        // so the status bar appearing can never shift the rows.
        .safeAreaInset(edge: .bottom, spacing: 0) { StatusBar(model: model) }
        .safeAreaInset(edge: .top, spacing: 0) {
            if model.isRunning {
                QueryProgressBar(model: model)
                    .transition(.opacity)
            }
        }
        .overlay(alignment: .top) { ProdStripe(isProd: model.isProd) }
        .inspector(isPresented: $model.showDetail) {
            DetailPanel(model: model)
                .inspectorColumnWidth(min: 260, ideal: 380, max: 720)
        }
        .animation(.smooth(duration: 0.22), value: model.isRunning)
        .animation(.smooth(duration: 0.22), value: model.showsGrid)
        .animation(.smooth(duration: 0.2), value: model.isError)
        .navigationTitle(model.activeProfile.isEmpty ? "datagrep" : model.activeProfile)
        .navigationSubtitle(model.connectionSubtitle)
        .toolbar { WorkbenchToolbar(model: model) }
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
                if stage.contentReady || StartupStage.deferralDisabled {
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
                if stage.contentReady || StartupStage.deferralDisabled {
                    ResultsGridView(controller: model.results)
                        .opacity(model.showsGrid ? 1 : 0)
                }
                if !model.showsGrid {
                    ResultsEmptyState(model: model)
                }
            }
        )
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
                .help("This connection is marked production — client-side marker only.")
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
                Divider()
                Toggle(
                    "Treat as Production",
                    isOn: Binding(
                        get: { model.prodMarked.contains(model.activeProfile) },
                        set: { _ in model.toggleProdMark(model.activeProfile) })
                )
                .disabled(model.activeProfile.isEmpty)
                .help(
                    "Client-side marker only — this ABI cannot set a profile's env, so datagrep cannot learn it from the engine."
                )
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
            .keyboardShortcut(.return, modifiers: .command)
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

/// `datagrep_profiles_add` parses the URL and puts any inline password in the
/// keychain, so this sheet is deliberately one text field: the URL is the
/// profile format, and it is the thing that is committable to git.
struct NewConnectionSheet: View {
    @ObservedObject var model: AppModel

    private static let engines: [(id: String, scheme: String, example: String)] = [
        ("postgres", "postgres://", "postgres://user@localhost/mydb"),
        ("mysql", "mysql://", "mysql://user@localhost/mydb"),
        ("sqlite", "sqlite://", "sqlite:///Users/me/data.db"),
        ("redis", "redis://", "redis://localhost:6379"),
        ("mongo", "mongodb://", "mongodb://localhost/mydb"),
    ]

    private var detectedDriver: String {
        let u = model.newURL.lowercased()
        return Self.engines.first { u.hasPrefix($0.scheme) }?.id ?? ""
    }

    var body: some View {
        VStack(alignment: .leading, spacing: 16) {
            HStack(spacing: 9) {
                EngineIcon(detectedDriver, size: 22)
                VStack(alignment: .leading, spacing: 1) {
                    Text("New Connection").font(.headline)
                    Text(
                        detectedDriver.isEmpty
                            ? "Pick an engine, or paste a connection URL"
                            : EngineStyle.displayName(for: detectedDriver)
                    )
                    .font(.caption)
                    .foregroundStyle(.secondary)
                }
                Spacer()
            }

            // One tap fills in the scheme. Not a driver dropdown: the URL is
            // the profile format, and a dropdown that disagreed with the URL
            // would just be a second source of truth.
            HStack(spacing: 6) {
                ForEach(Self.engines, id: \.id) { e in
                    Button {
                        model.newURL = e.example
                        if model.newName.isEmpty { model.newName = e.id }
                    } label: {
                        VStack(spacing: 4) {
                            EngineIcon(e.id, size: 20)
                            Text(EngineStyle.displayName(for: e.id))
                                .font(.system(size: 9))
                                .foregroundStyle(.secondary)
                        }
                        .frame(width: 74, height: 50)
                        .background(
                            RoundedRectangle(cornerRadius: 7, style: .continuous)
                                .fill(
                                    detectedDriver == e.id
                                        ? Color.accentColor.opacity(0.16)
                                        : Color(nsColor: .quaternaryLabelColor).opacity(0.3))
                        )
                        .overlay(
                            RoundedRectangle(cornerRadius: 7, style: .continuous)
                                .strokeBorder(
                                    detectedDriver == e.id
                                        ? Color.accentColor : Color.clear, lineWidth: 1.5)
                        )
                    }
                    .buttonStyle(.plain)
                }
            }
            .animation(.smooth(duration: 0.18), value: detectedDriver)

            Grid(alignment: .leading, horizontalSpacing: 10, verticalSpacing: 8) {
                GridRow {
                    Text("Name").foregroundStyle(.secondary)
                    TextField("local", text: $model.newName)
                        .textFieldStyle(.roundedBorder)
                }
                GridRow {
                    Text("URL").foregroundStyle(.secondary)
                    TextField("sqlite:///Users/me/data.db", text: $model.newURL)
                        .textFieldStyle(.roundedBorder)
                        .font(.system(size: 11, design: .monospaced))
                }
            }
            .frame(width: 430)

            Label(
                "A password in the URL is moved into the macOS keychain before the profile is written — it never reaches disk in plain text.",
                systemImage: "lock.fill"
            )
            .font(.caption)
            .foregroundStyle(.secondary)
            .fixedSize(horizontal: false, vertical: true)
            .frame(width: 470, alignment: .leading)

            if let err = model.newError {
                Label(err, systemImage: "exclamationmark.triangle.fill")
                    .font(.caption)
                    .foregroundStyle(.red)
                    .fixedSize(horizontal: false, vertical: true)
                    .frame(width: 470, alignment: .leading)
                    .transition(.opacity)
            }

            HStack {
                Spacer()
                Button("Cancel") {
                    model.newError = nil
                    model.showNewConnection = false
                }
                .keyboardShortcut(.cancelAction)
                Button("Add") { model.addProfile(name: model.newName, url: model.newURL) }
                    .keyboardShortcut(.defaultAction)
                    .disabled(model.newName.isEmpty || model.newURL.isEmpty)
            }
        }
        .padding(20)
        .animation(.smooth(duration: 0.2), value: model.newError)
    }
}
