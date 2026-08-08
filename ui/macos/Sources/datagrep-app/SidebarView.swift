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
                    HStack(spacing: 6) {
                        Image(systemName: "ellipsis")
                            .foregroundStyle(.tertiary)
                        Text("loading…").foregroundStyle(.secondary)
                    }
                    .font(.callout)
                } else if node.needsPrefix {
                    ScanPrompt(node: node, model: model)
                } else if let err = node.loadError {
                    Label(err, systemImage: "exclamationmark.triangle")
                        .font(.caption)
                        .foregroundStyle(.red)
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

    var body: some View {
        HStack(spacing: 7) {
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

            if node.isProfile && model.prodMarked.contains(node.name) {
                Text("PROD")
                    .font(.system(size: 8, weight: .bold))
                    .foregroundStyle(.white)
                    .padding(.horizontal, 4)
                    .padding(.vertical, 1)
                    .background(Capsule().fill(Color.red))
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
                    isActive
                        ? Color.accentColor.opacity(0.16)
                        : (hovering ? Color.secondary.opacity(0.11) : Color.clear))
        )
        .contentShape(Rectangle())
        .onHover { h in hovering = h }
        .onTapGesture { model.select(node) }
        .onTapGesture(count: 2) {
            if node.isPreviewable { model.preview(node) }
        }
        .contextMenu {
            if node.isProfile {
                Button("Set as Active Connection") { model.selectProfile(node.name) }
                Toggle(
                    "Treat as Production",
                    isOn: Binding(
                        get: { model.prodMarked.contains(node.name) },
                        set: { _ in model.toggleProdMark(node.name) }))
                Divider()
                Button("Remove Connection…") {
                    model.selectProfile(node.name)
                    model.removeActiveProfile()
                }
            } else if node.isPreviewable {
                Button("Preview 500 Rows") { model.preview(node) }
            }
        }
        .help(node.subtitle ?? node.kind)
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
