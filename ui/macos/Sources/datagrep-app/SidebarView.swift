import DatagrepKit
import SwiftUI

/// Profiles + a lazy schema tree. A node's children are fetched only when its
/// disclosure group opens, and only for THAT level — never a crawl.
struct SidebarView: View {
    @ObservedObject var model: AppModel

    var body: some View {
        List(selection: .constant(nil as UUID?)) {
            Section {
                ForEach(model.roots) { node in
                    NodeRow(node: node, model: model, filter: model.searchText, depth: 0)
                }
                if model.roots.isEmpty {
                    Button {
                        model.showNewConnection = true
                    } label: {
                        Label("Add a connection…", systemImage: "plus.circle")
                            .font(.callout)
                    }
                    .buttonStyle(.plain)
                    .foregroundStyle(Color.accentColor)
                    .padding(.vertical, 4)
                }
            } header: {
                Text("Connections")
                    .font(.caption2)
                    .fontWeight(.semibold)
                    .textCase(.uppercase)
                    .foregroundStyle(.secondary)
                    .tracking(0.6)
            }
        }
        .listStyle(.sidebar)
        // A band, not a tint. Sequel Ace shrinking the same signal to a dot
        // produced sustained backlash, so a marked connection gets full width.
        .safeAreaInset(edge: .top, spacing: 0) {
            if let color = model.activeSafety.color, !model.activeProfile.isEmpty {
                MarkedBanner(name: model.activeProfile, color: color)
                    .transition(.move(edge: .top).combined(with: .opacity))
            }
        }
        // ⌘E on the selected connection. A zero-size button rather than a menu
        // item because the main menu is built in AppDelegate; the shortcut is
        // live for as long as the window is.
        .background {
            Button("Edit Connection") { model.editActiveConnection() }
                .keyboardShortcut("e", modifiers: .command)
                .disabled(model.activeProfile.isEmpty)
                // Not `.hidden()`: a hidden view is dropped from the responder
                // chain in some releases and the shortcut goes with it.
                .opacity(0)
                .frame(width: 0, height: 0)
                .allowsHitTesting(false)
                .accessibilityHidden(true)
        }
        .sheet(item: $model.editDraft) { draft in
            ConnectionEditorSheet(model: model, draft: draft)
        }
        .animation(.smooth(duration: 0.2), value: model.activeSafety.color)
        // Real window vibrancy. `.listStyle(.sidebar)` alone gives the sidebar
        // *metrics*, not the material — hosted in an NSHostingView it renders
        // as flat window gray without this.
        .scrollContentBackground(.hidden)
        .background(VisualEffect(material: .sidebar).ignoresSafeArea())
        .searchable(
            text: $model.searchText, placement: .sidebar,
            prompt: "Filter loaded nodes"
        )
        .safeAreaInset(edge: .bottom) {
            if !model.searchText.isEmpty {
                Text("Filtering nodes already loaded — this never triggers a server scan.")
                    .font(.caption2)
                    .foregroundStyle(.secondary)
                    .padding(.horizontal, 10)
                    .padding(.vertical, 6)
                    .frame(maxWidth: .infinity, alignment: .leading)
                    .background(.ultraThinMaterial)
                    .transition(.move(edge: .bottom).combined(with: .opacity))
            }
        }
        .animation(.smooth(duration: 0.2), value: model.searchText.isEmpty)
    }
}

private struct NodeRow: View {
    @ObservedObject var node: CatalogNode
    @ObservedObject var model: AppModel
    let filter: String
    let depth: Int

    private var visibleChildren: [CatalogNode] {
        guard !filter.isEmpty else { return node.children }
        return node.children.filter { $0.name.localizedCaseInsensitiveContains(filter) }
    }

    var body: some View {
        if node.hasChildren {
            DisclosureGroup(
                isExpanded: Binding(
                    get: { node.isExpanded },
                    // The expand animation is on the binding, not the view, so
                    // the fetch that follows does not get animated with it.
                    set: { v in withAnimation(.smooth(duration: 0.22)) { node.isExpanded = v } })
            ) {
                if node.isLoading {
                    // A real spinner, not three static dots. Fetching 200 tables
                    // off a remote server takes seconds, and a row that says
                    // "loading…" without moving is indistinguishable from a row
                    // that has hung — which is exactly what it looked like.
                    HStack(spacing: 6) {
                        ProgressView()
                            .controlSize(.small)
                            .scaleEffect(0.7)
                            .frame(width: 14, height: 14)
                        Text(node.isProfile ? "connecting…" : "loading…")
                            .foregroundStyle(.secondary)
                    }
                    .font(.callout)
                } else if node.needsPrefix {
                    ScanPrompt(node: node, model: model)
                } else if let err = node.loadError {
                    // Named, and retryable. A failure used to leave the row
                    // marked loaded, so collapsing and reopening it did nothing
                    // and the only way back was relaunching the app.
                    VStack(alignment: .leading, spacing: 4) {
                        Label(err, systemImage: "exclamationmark.triangle")
                            .font(.caption)
                            .foregroundStyle(.red)
                            .fixedSize(horizontal: false, vertical: true)
                        Button("Try Again") { model.load(node, prefix: nil) }
                            .controlSize(.small)
                    }
                    .padding(.vertical, 2)
                } else if node.didLoad && visibleChildren.isEmpty {
                    Text(filter.isEmpty ? "no children" : "no match")
                        .font(.callout)
                        .foregroundStyle(.tertiary)
                } else {
                    ForEach(visibleChildren) { child in
                        NodeRow(node: child, model: model, filter: filter, depth: depth + 1)
                    }
                }
            } label: {
                NodeLabel(node: node, model: model)
            }
        } else {
            NodeLabel(node: node, model: model)
        }
    }
}

/// One row. Hover is the whole point: before this, nothing in the sidebar
/// acknowledged the pointer at all.
private struct NodeLabel: View {
    @ObservedObject var node: CatalogNode
    @ObservedObject var model: AppModel
    @State private var hovering = false

    private var isActive: Bool { node.isProfile && node.name == model.activeProfile }

    /// The object the inspector is currently describing. Selection in this tree
    /// is otherwise invisible — a schema pane with no indication of *which*
    /// table it belongs to is a pane you have to guess at.
    private var isInspected: Bool {
        !node.isProfile && model.schemaTarget?.cacheKey == node.schemaCacheKey
    }

    /// The connection's own facts, asked of the model so the row and the query
    /// path can never disagree about whether this thing is protected.
    private var safety: ConnectionSafety? {
        node.isProfile ? model.safety(for: node.name) : nil
    }

    var body: some View {
        HStack(spacing: 7) {
            if let safety, let color = safety.color {
                // A solid bar down the leading edge. Wide enough to see at a
                // glance, and it does not depend on the row being selected.
                RoundedRectangle(cornerRadius: 1.5, style: .continuous)
                    .fill(ConnectionColor.color(color) ?? Color.clear)
                    .frame(width: 3, height: 17)
            }
            if node.isProfile {
                EngineIcon(node.driver, size: 15)
            } else {
                Image(systemName: node.symbol)
                    .font(.system(size: 11.5))
                    .foregroundStyle(.secondary)
                    .frame(width: 15)
            }

            Text(node.name)
                .font(node.isProfile ? .callout.weight(.medium) : .callout)
                .foregroundStyle(Color.primary)
                .lineLimit(1)
                .truncationMode(.middle)

            Spacer(minLength: 4)

            // The pencil only appears under the pointer, so the resting row
            // stays quiet — but the lock and the PROD marker never hide.
            if node.isProfile, hovering {
                Button {
                    model.editConnection(named: node.name)
                } label: {
                    Image(systemName: "pencil")
                        .font(.system(size: 10, weight: .semibold))
                        .foregroundStyle(.secondary)
                        .frame(width: 16, height: 16)
                        .contentShape(Rectangle())
                }
                .buttonStyle(.plain)
                .help("Edit this connection  ⌘E")
                .transition(.opacity)
            }

            if let safety, safety.readOnly {
                ReadOnlyBadge(level: safety.enforcement, compact: true)
            }
            if let badge = node.badge {
                Text(badge)
                    .font(.system(size: 9, weight: .medium))
                    .monospacedDigit()
                    .foregroundStyle(.secondary)
                    .padding(.horizontal, 5)
                    .padding(.vertical, 1)
                    .background(
                        Capsule().fill(Color(nsColor: .quaternaryLabelColor).opacity(0.5)))
            }
        }
        .padding(.vertical, 2)
        .padding(.horizontal, 5)
        .background(
            RoundedRectangle(cornerRadius: 5, style: .continuous)
                .fill(
                    isActive || isInspected
                        ? Color.accentColor.opacity(isInspected && !isActive ? 0.12 : 0.16)
                        : (hovering ? Color.secondary.opacity(0.11) : Color.clear))
        )
        .contentShape(Rectangle())
        .onHover { h in withAnimation(.smooth(duration: 0.12)) { hovering = h } }
        .onTapGesture { model.select(node) }
        // Double-click a connection: a new editor for it, DBeaver-style. The
        // previewable branch is unchanged — a profile row is never previewable
        // (its kind is `profile`), so these two have never overlapped.
        .onTapGesture(count: 2) {
            if node.isProfile {
                model.openSQLEditor(for: node.name)
            } else if node.isPreviewable {
                model.preview(node)
            }
        }
        .contextMenu {
            if node.isProfile {
                ConnectionMenu(model: model, name: node.name)
            } else {
                if node.isDescribable {
                    Button("Show Schema") { model.showSchema(for: node) }
                    Button("Refresh Schema") { model.showSchema(for: node, force: true) }
                        .help("Re-read the columns and indexes from the server")
                }
                if node.isPreviewable {
                    if node.isDescribable { Divider() }
                    Button("Preview 500 Rows") { model.preview(node) }
                }
            }
        }
        .help(node.subtitle ?? node.kind)
    }
}

/// Everything you can do to one connection, in one menu.
///
/// Ordered the way macOS orders a contextual menu: the thing you almost always
/// want first (open it, work in it), then the editors that already exist for
/// it, then the settings, and the irreversible one last behind its own
/// separator. Verbs, and no "Connection" suffix on every line — the menu is
/// already on a connection, so repeating the noun six times only makes the
/// destructive entry harder to pick out.
private struct ConnectionMenu: View {
    @ObservedObject var model: AppModel
    let name: String

    /// Editors this connection already owns, open or closed. Reopening one
    /// beats making a third copy of the same query.
    private var editors: [SavedQueryRecord] { model.editors(for: name) }

    var body: some View {
        Button("Connect") { model.selectProfile(name) }
            .disabled(model.activeProfile == name)
        Button("Reconnect") { model.reconnect(name) }
            .help("Drop the pooled socket so the next statement dials the server again")

        Divider()

        Button("New SQL Editor") { model.openSQLEditor(for: name) }
        if editors.isEmpty {
            // Shown and disabled rather than hidden: the entry appearing only
            // sometimes is harder to learn than one that says why it is off.
            Button("Open Editor") {}
                .disabled(true)
        } else {
            Menu("Open Editor") {
                ForEach(editors, id: \.id) { record in
                    Button(record.name ?? "Untitled") { model.openEditor(record) }
                }
            }
        }

        Divider()

        // No `.keyboardShortcut` here: ⌘E is already registered window-wide by
        // the sidebar, and registering it twice is ambiguous.
        Button("Edit…") { model.editConnection(named: name) }
        Button("Duplicate") { model.duplicateProfile(named: name) }

        Divider()

        Button("Remove…", role: .destructive) { model.removeProfile(named: name) }
    }
}

/// `Enumeration::ScanOnly` — a prefix is mandatory. This is the control that
/// stops the app firing `KEYS *` at a 40 GB Redis.
private struct ScanPrompt: View {
    @ObservedObject var node: CatalogNode
    @ObservedObject var model: AppModel

    var body: some View {
        VStack(alignment: .leading, spacing: 5) {
            Label("Scan required", systemImage: "magnifyingglass")
                .font(.caption)
                .foregroundStyle(.secondary)
            Text(
                "‘\(node.name)’ has no cheap listing. Enter a key prefix — enumerating everything would be a full keyspace scan."
            )
            .font(.caption2)
            .foregroundStyle(.secondary)
            .fixedSize(horizontal: false, vertical: true)
            HStack(spacing: 4) {
                TextField("prefix, e.g. session:", text: $node.scanPrefix)
                    .textFieldStyle(.roundedBorder)
                    .font(.caption)
                    .onSubmit { model.scan(node) }
                Button("Scan") { model.scan(node) }
                    .controlSize(.small)
            }
        }
        .padding(.vertical, 5)
        .padding(.horizontal, 6)
        .background(
            RoundedRectangle(cornerRadius: 6, style: .continuous)
                .fill(Color(nsColor: .quaternaryLabelColor).opacity(0.25)))
    }
}
