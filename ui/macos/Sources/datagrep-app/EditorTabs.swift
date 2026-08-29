import AppKit
import DatagrepKit
import SwiftUI

struct EditorConnectionOption: Identifiable, Hashable {
    let name: String
    let driver: String
    var id: String { name }
}

final class EditorTab: ObservableObject, Identifiable {
    let id: String
    /// `nil` until the tab is named with ⌘S. An untitled tab is still persisted.
    @Published var name: String?
    /// The catalog object this tab browses, when it was opened by a click.
    @Published var subject: String?
    /// The profile this tab runs against. `nil` = follow the window.
    @Published var connection: String?
    @Published var isDirty: Bool = false

    var text: String
    var selectedRange: NSRange

    init(
        id: String = UUID().uuidString,
        name: String? = nil,
        subject: String? = nil,
        connection: String? = nil,
        text: String = "",
        selectedRange: NSRange = NSRange(location: 0, length: 0),
        isDirty: Bool = false
    ) {
        self.id = id
        self.name = name
        self.subject = subject
        self.connection = connection
        self.text = text
        self.selectedRange = selectedRange
        self.isDirty = isDirty
    }

    var untitledNumber: Int = 0

    var displayTitle: String {
        if let name, !name.isEmpty { return name }
        if let subject, !subject.isEmpty { return subject }
        return untitledNumber > 0 ? "Untitled \(untitledNumber)" : "Untitled"
    }

    var record: SavedQueryRecord {
        SavedQueryRecord(
            id: id, name: name, subject: subject, connection: connection,
            cursorLocation: selectedRange.location, cursorLength: selectedRange.length,
            isDirty: isDirty)
    }
}

final class EditorTabsModel: ObservableObject {
    @Published var tabs: [EditorTab] = []
    @Published var activeID: String?
    @Published var connections: [EditorConnectionOption] = []
    /// Named queries on disk that are not currently open in a tab.
    @Published var savedQueries: [SavedQueryRecord] = []
    @Published var scope: String?

    var onActivate: ((EditorTab) -> Void)?
    var onClose: ((EditorTab) -> Void)?
    var onNew: (() -> Void)?
    var onSave: ((EditorTab) -> Void)?
    var onBind: ((EditorTab, String?) -> Void)?
    var onOpenSaved: ((SavedQueryRecord) -> Void)?
    var onNewConnection: (() -> Void)?
    /// Picking a connection from the welcome state.
    var onPickConnection: ((String) -> Void)?

    var active: EditorTab? { tabs.first { $0.id == activeID } }

    var activeIndex: Int? { tabs.firstIndex { $0.id == activeID } }

    /// The engine of one connection, for the mark on a tab chip.
    func driver(for connection: String?) -> String {
        guard let connection else { return "" }
        return connections.first { $0.name == connection }?.driver ?? ""
    }

}

// MARK: - the bar

struct EditorTabBar: View {
    @ObservedObject var model: EditorTabsModel

    var body: some View {
        HStack(spacing: 6) {
            HStack(spacing: 0) {
                ForEach(model.tabs) { tab in
                    EditorTabChip(
                        tab: tab,
                        driver: model.driver(for: tab.connection ?? model.scope),
                        isActive: tab.id == model.activeID,
                        activate: { model.onActivate?(tab) },
                        close: { model.onClose?(tab) })
                }
            }
            .frame(maxWidth: .infinity, alignment: .leading)

            Menu {
                Button("New Query Tab") { model.onNew?() }
                if !model.savedQueries.isEmpty {
                    Section("Saved Queries") {
                        ForEach(model.savedQueries, id: \.id) { record in
                            Button(record.name ?? record.id) { model.onOpenSaved?(record) }
                        }
                    }
                }
            } label: {
                Image(systemName: "plus")
                    .font(.system(size: 11, weight: .semibold))
            }
            .menuStyle(.borderlessButton)
            .menuIndicator(.hidden)
            .fixedSize()
            .foregroundStyle(.secondary)
            .help("New query tab (⌘T), or reopen a saved query")

            Divider().frame(height: 16)

            ConnectionPicker(model: model)
        }
        .padding(.horizontal, 8)
        .frame(height: 30)
        .background(Color(nsColor: .windowBackgroundColor).opacity(0.6))
    }
}

private struct EditorTabChip: View {
    @ObservedObject var tab: EditorTab
    let driver: String
    let isActive: Bool
    let activate: () -> Void
    let close: () -> Void

    @State private var hovering = false

    var body: some View {
        Button(action: activate) {
            HStack(spacing: 5) {
                // 12 pt brand mark — square, so anything larger crowds the title.
                if !driver.isEmpty {
                    EngineIcon(driver, size: 12)
                }
                Text(tab.displayTitle)
                    .font(.system(size: 11, weight: isActive ? .semibold : .regular))
                    .foregroundStyle(isActive ? Color.primary : Color.secondary)
                    .lineLimit(1)

                if let conn = tab.connection, !conn.isEmpty {
                    Text(conn)
                        .font(.system(size: 9, weight: .medium))
                        .foregroundStyle(.tertiary)
                        .lineLimit(1)
                        .padding(.horizontal, 4)
                        .padding(.vertical, 1)
                        .background(
                            Capsule().fill(Color(nsColor: .quaternaryLabelColor).opacity(0.5)))
                }

                HStack(spacing: 3) {
                    if tab.isDirty {
                        Circle()
                            .fill(Color.accentColor)
                            .frame(width: 6, height: 6)
                            .help("Unsaved changes")
                    }
                    ZStack {
                        if hovering {
                            Button(action: close) {
                                Image(systemName: "xmark")
                                    .font(.system(size: 8, weight: .bold))
                                    .foregroundStyle(.secondary)
                            }
                            .buttonStyle(.plain)
                            .help("Close tab")
                        }
                    }
                    .frame(width: 10, height: 10)
                }
            }
            .padding(.horizontal, 11)
            .frame(maxHeight: .infinity)
            .contentShape(Rectangle())
        }
        .buttonStyle(.plain)
        .background(isActive ? Color(nsColor: .textBackgroundColor) : Color.clear)
        .overlay(alignment: .bottom) {
            Rectangle()
                .fill(isActive ? Color.accentColor : Color.clear)
                .frame(height: 2)
                .allowsHitTesting(false)
        }
        .overlay(alignment: .trailing) {
            if !isActive {
                Rectangle()
                    .fill(Color(nsColor: .separatorColor))
                    .frame(width: 1)
                    .padding(.vertical, 6)
                    .allowsHitTesting(false)
            }
        }
        .onHover { hovering = $0 }
        .help(tab.name.map { "\($0)  ·  ⌘S saves" } ?? "Unsaved scratch tab — ⌘S names it")
    }
}

/// What fills the editor pane when the scoped connection has no editor open.
struct EditorWelcomeState: View {
    @ObservedObject var model: EditorTabsModel

    private var reopenable: [SavedQueryRecord] {
        model.savedQueries.filter { $0.connection == model.scope }
    }

    var body: some View {
        VStack(spacing: 12) {
            Image(systemName: "square.and.pencil")
                .font(.system(size: 26, weight: .light))
                .foregroundStyle(.tertiary)

            VStack(spacing: 3) {
                Text("No editor open")
                    .font(.callout.weight(.medium))
                Text(
                    model.scope.map {
                        "⌘T opens a new SQL editor for \($0). Every editor you open stays in the tab bar, whatever connection it targets."
                    }
                        ?? "Add a connection, or pick one in the sidebar, then ⌘T opens an editor. Every editor stays in the tab bar, whatever connection it targets."
                )
                .font(.caption)
                .foregroundStyle(.secondary)
                .multilineTextAlignment(.center)
                .fixedSize(horizontal: false, vertical: true)
                .frame(maxWidth: 440)
            }

            HStack(spacing: 8) {
                if model.scope != nil {
                    Button { model.onNew?() } label: {
                        Label("New SQL Editor", systemImage: "plus")
                    }
                    .controlSize(.small)
                }
                Button { model.onNewConnection?() } label: {
                    Label("New Connection…", systemImage: "externaldrive.badge.plus")
                }
                .controlSize(.small)
            }

            if !reopenable.isEmpty {
                VStack(spacing: 4) {
                    Text("Editors you made earlier")
                        .font(.caption2)
                        .foregroundStyle(.tertiary)
                    HStack(spacing: 6) {
                        ForEach(reopenable.prefix(4), id: \.id) { record in
                            Button(record.name ?? "Untitled") { model.onOpenSaved?(record) }
                                .buttonStyle(.link)
                                .font(.caption)
                                .lineLimit(1)
                        }
                    }
                }
                .padding(.top, 2)
            } else if model.scope == nil, !model.connections.isEmpty {
                VStack(spacing: 4) {
                    Text("Connections")
                        .font(.caption2)
                        .foregroundStyle(.tertiary)
                    HStack(spacing: 6) {
                        ForEach(model.connections.prefix(4)) { option in
                            Button {
                                model.onPickConnection?(option.name)
                            } label: {
                                HStack(spacing: 4) {
                                    EngineIcon(option.driver, size: 12)
                                    Text(option.name).font(.caption).lineLimit(1)
                                }
                            }
                            .buttonStyle(.link)
                        }
                    }
                }
                .padding(.top, 2)
            }

            Text("⌘⏎ runs the statement under the caret · -- @limit and -- @timeout set per-statement limits")
                .font(.caption2)
                .foregroundStyle(.tertiary)
                .multilineTextAlignment(.center)
                .lineLimit(2)
                .fixedSize(horizontal: false, vertical: true)
                .frame(maxWidth: 480)
                .padding(.top, 2)
        }
        .frame(maxWidth: .infinity, maxHeight: .infinity)
        .padding(20)
        .background(Color(nsColor: .textBackgroundColor))
    }
}

private struct ConnectionPicker: View {
    @ObservedObject var model: EditorTabsModel

    private var tab: EditorTab? { model.active }

    private var boundName: String? { tab?.connection }

    private var driver: String {
        guard let boundName else { return "" }
        return model.connections.first { $0.name == boundName }?.driver ?? ""
    }

    var body: some View {
        Menu {
            Button {
                if let tab { model.onBind?(tab, nil) }
            } label: {
                Label(
                    "Follow window connection",
                    systemImage: boundName == nil ? "checkmark" : "arrow.triangle.branch")
            }
            if !model.connections.isEmpty { Divider() }
            ForEach(model.connections) { option in
                Button {
                    if let tab { model.onBind?(tab, option.name) }
                } label: {
                    Label {
                        Text("\(option.name)  ·  \(EngineStyle.displayName(for: option.driver))")
                    } icon: {
                        if option.name == boundName {
                            Image(systemName: "checkmark")
                        } else {
                            EngineIcon(option.driver, size: 13)
                        }
                    }
                }
            }
        } label: {
            HStack(spacing: 4) {
                if let boundName {
                    EngineIcon(driver, size: 13)
                    Text(boundName)
                        .font(.system(size: 11))
                        .lineLimit(1)
                } else {
                    Image(systemName: "arrow.triangle.branch")
                        .font(.system(size: 10))
                        .foregroundStyle(.secondary)
                    Text("window")
                        .font(.system(size: 11))
                        .foregroundStyle(.secondary)
                }
            }
        }
        .menuStyle(.borderlessButton)
        .fixedSize()
        .disabled(tab == nil)
        .help(
            "The connection this tab runs against. `-- @connection` inside the statement still wins."
        )
    }
}
