import DbxKit
import SwiftUI

/// Profiles + a lazy schema tree. A node's children are fetched only when its
/// disclosure group opens, and only for THAT level — never a crawl.
struct SidebarView: View {
    @ObservedObject var model: AppModel

    var body: some View {
        List(selection: .constant(nil as UUID?)) {
            Section("Connections") {
                ForEach(model.roots) { node in
                    NodeRow(node: node, model: model, filter: model.searchText)
                }
            }
        }
        .listStyle(.sidebar)
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
                    .background(Color(nsColor: .controlBackgroundColor))
            }
        }
    }
}

private struct NodeRow: View {
    @ObservedObject var node: CatalogNode
    @ObservedObject var model: AppModel
    let filter: String

    private var visibleChildren: [CatalogNode] {
        guard !filter.isEmpty else { return node.children }
        return node.children.filter { $0.name.localizedCaseInsensitiveContains(filter) }
    }

    var body: some View {
        if node.hasChildren {
            DisclosureGroup(isExpanded: $node.isExpanded) {
                if node.isLoading {
                    Label("loading…", systemImage: "ellipsis").foregroundStyle(.secondary)
                } else if node.needsPrefix {
                    ScanPrompt(node: node, model: model)
                } else if let err = node.loadError {
                    Label(err, systemImage: "exclamationmark.triangle")
                        .foregroundStyle(.red)
                } else if node.didLoad && visibleChildren.isEmpty {
                    Text(filter.isEmpty ? "no children" : "no match")
                        .foregroundStyle(.tertiary)
                } else {
                    ForEach(visibleChildren) { child in
                        NodeRow(node: child, model: model, filter: filter)
                    }
                }
            } label: {
                NodeLabel(node: node)
                    .contentShape(Rectangle())
                    .onTapGesture { model.select(node) }
                    .onTapGesture(count: 2) {
                        if node.isPreviewable { model.preview(node) }
                    }
            }
        } else {
            NodeLabel(node: node)
                .contentShape(Rectangle())
                .onTapGesture { model.select(node) }
                .onTapGesture(count: 2) {
                    if node.isPreviewable { model.preview(node) }
                }
        }
    }
}

private struct NodeLabel: View {
    @ObservedObject var node: CatalogNode

    var body: some View {
        HStack(spacing: 6) {
            Image(systemName: node.symbol)
                .foregroundStyle(node.isProfile && node.env == "prod" ? Color.red : Color.accentColor)
                .frame(width: 16)
            Text(node.name)
                .fontWeight(node.isProfile ? .semibold : .regular)
                .foregroundStyle(Color.primary)
                .lineLimit(1)
            Spacer(minLength: 4)
            if let badge = node.badge {
                Text(badge)
                    .font(.caption2)
                    .foregroundStyle(.secondary)
                    .padding(.horizontal, 5)
                    .padding(.vertical, 1)
                    .background(
                        RoundedRectangle(cornerRadius: 4)
                            .fill(Color(nsColor: .quaternaryLabelColor).opacity(0.5)))
            }
        }
        .help(node.isProfile ? "\(node.driver) · \(node.env)" : node.kind)
    }
}

/// `Enumeration::ScanOnly` — a prefix is mandatory. This is the control that
/// stops the app firing `KEYS *` at a 40 GB Redis.
private struct ScanPrompt: View {
    @ObservedObject var node: CatalogNode
    @ObservedObject var model: AppModel

    var body: some View {
        VStack(alignment: .leading, spacing: 4) {
            Label("Scan required", systemImage: "magnifyingglass")
                .font(.caption)
                .foregroundStyle(.secondary)
            Text("‘\(node.name)’ has no cheap listing. Enter a key prefix — enumerating everything would be a full keyspace scan.")
                .font(.caption2)
                .foregroundStyle(.secondary)
                .fixedSize(horizontal: false, vertical: true)
            HStack(spacing: 4) {
                TextField("prefix, e.g. session:", text: $node.scanPrefix)
                    .textFieldStyle(.roundedBorder)
                    .font(.caption)
                    .onSubmit { model.scan(node) }
                Button("Scan") { model.scan(node) }
                    .buttonStyle(.borderless)
                    .font(.caption)
            }
        }
        .padding(.vertical, 4)
    }
}
