import DatagrepKit
import SwiftUI

struct SidebarView: View {
    @ObservedObject var model: AppModel

    var body: some View {
        List(selection: .constant(nil as UUID?)) {
            Section {
                ForEach(model.roots) { node in
                    NodeRow(node: node, model: model, filter: model.searchText, depth: 0)
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
        .safeAreaInset(edge: .top, spacing: 0) {
            if let color = model.activeSafety.color, !model.activeProfile.isEmpty {
                MarkedBanner(name: model.activeProfile, color: color)
                    .transition(.move(edge: .top).combined(with: .opacity))
            }
        }
        .background {
            Button("Edit Connection") { model.editActiveConnection() }
                .keyboardShortcut("e", modifiers: .command)
                .disabled(model.activeProfile.isEmpty)
                .opacity(0)
                .frame(width: 0, height: 0)
                .allowsHitTesting(false)
                .accessibilityHidden(true)
        }
        .sheet(item: $model.editDraft) { draft in
            ConnectionEditorSheet(model: model, draft: draft)
        }
        .animation(.smooth(duration: 0.2), value: model.activeSafety.color)
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
                    set: { v in withAnimation(.smooth(duration: 0.22)) { node.isExpanded = v } })
            ) {
                if node.isLoading {
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

private struct NodeLabel: View {
    @ObservedObject var node: CatalogNode
    @ObservedObject var model: AppModel
    @State private var hovering = false

    private var isActive: Bool { node.isProfile && node.name == model.activeProfile }

    private var isInspected: Bool {
        !node.isProfile && model.schemaTarget?.cacheKey == node.schemaCacheKey
    }

    private var safety: ConnectionSafety? {
        node.isProfile ? model.safety(for: node.name) : nil
    }

    var body: some View {
        HStack(spacing: 7) {
            if let safety, let color = safety.color {
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
        .onTapGesture(count: 2) {
            if node.isProfile {
                model.openSQLEditor(for: node.name)
            } else if node.isBrowsable {
                model.browse(node)
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
                if node.isBrowsable {
                    if node.isDescribable { Divider() }
                    Button("Browse Rows") { model.browse(node) }
                }
            }
        }
        .help(node.subtitle ?? node.kind)
    }
}

/// Everything you can do to one connection, in one menu.
private struct ConnectionMenu: View {
    @ObservedObject var model: AppModel
    let name: String

    private var editors: [SavedQueryRecord] { model.editors(for: name) }

    var body: some View {
        Button("Connect") { model.selectProfile(name) }
            .disabled(model.activeProfile == name)
        Button("Reconnect") { model.reconnect(name) }
            .help("Drop the pooled socket so the next statement dials the server again")

        Divider()

        Button("New SQL Editor") { model.openSQLEditor(for: name) }
        if editors.isEmpty {
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

        Button("Edit…") { model.editConnection(named: name) }
        Button("Duplicate") { model.duplicateProfile(named: name) }

        Divider()

        Button("Remove…", role: .destructive) { model.removeProfile(named: name) }
    }
}

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
