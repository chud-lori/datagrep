import AppKit
import DatagrepKit
import SwiftUI

struct Workbench: View {
    @ObservedObject var model: AppModel

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
        .sheet(isPresented: $model.showConflictReview) {
            if let review = model.conflictReview {
                ConflictReviewSheet(model: model, review: review)
            }
        }
        .animation(.smooth(duration: 0.25), value: model.sidebarShown)
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
            .overlay(alignment: .top) { MarkStripe(color: model.markColor) }

            StatusBar(model: model)
        }
        .frame(minWidth: 380, maxWidth: .infinity, maxHeight: .infinity)
        .historySheet(model.history)
        .modifier(DeferredInspector(model: model))
        .animation(.smooth(duration: 0.22), value: model.isRunning)
        .animation(.smooth(duration: 0.2), value: model.isError)
        .toolbar { if stage.contentReady { WorkbenchToolbar(model: model) } }
        .modifier(RemoveToolbarTitle())
        .toolbarBackground(.visible, for: .windowToolbar)
    }
}

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

private struct ResultsPane: View {
    @ObservedObject var model: AppModel
    @ObservedObject private var stage = StartupStage.shared

    var body: some View {
        VStack(spacing: 0) {
            grid
            StagedEditsSlot(model: model, edits: model.edits)
        }
    }

    private var grid: some View {
        Chrome.pane(
            ZStack {
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

private struct ConnectionBadge: View {
    @ObservedObject var model: AppModel

    private var product: String {
        if let p = model.connectionInfo?.product, !p.isEmpty, !Self.isUnknown(p) { return p }
        return EngineStyle.displayName(for: model.activeDriver)
    }

    private var engine: String {
        guard let v = model.connectionInfo?.version, !v.isEmpty, !Self.isUnknown(v) else {
            return product
        }
        let short = v.split(separator: "-", maxSplits: 1).first.map(String.init) ?? v
        return "\(product) \(short)"
    }

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
            // NSToolbar budgets this declared cap, not the drawn width — keep it width-aware.
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
            .help("Show or hide the connections sidebar  ⌃⌘S")

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
                ConnectionBadge(model: model)
            }
            .menuStyle(.button)
            .buttonStyle(.plain)
            .menuIndicator(.hidden)

            Button {
                if model.isRunning { model.cancel() } else { model.runStatementUnderCaret() }
            } label: {
                Label(
                    model.isRunning ? "Cancel" : "Run",
                    systemImage: model.isRunning ? "stop.fill" : "play.fill")
            }
            .disabled(model.activeProfile.isEmpty && !model.isRunning)
            .help(
                model.isRunning
                    ? "Cancel — returns instantly, then reports what the server actually did  ⌘."
                    : "Run the statement under the caret  ⌘↩")

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
