import AppKit
import DatagrepKit
import SwiftUI

/// A connection the tab picker can bind to. The editor does not talk to the
/// engine itself — `SQLEditorController.profilesProvider` supplies these, so the
/// picker always shows exactly the profiles the window knows about.
struct EditorConnectionOption: Identifiable, Hashable {
    let name: String
    let driver: String
    var id: String { name }
}

/// One open editor. A reference type with `@Published` so the tab bar can
/// re-render a single chip (a title, a dirty dot) without rebuilding the row.
///
/// `text` is authoritative only while the tab is *inactive*; the active tab's
/// text lives in the `NSTextView` and is flushed back here on every switch,
/// close, and autosave.
final class EditorTab: ObservableObject, Identifiable {
    let id: String
    /// `nil` until the tab is named with ⌘S. An untitled tab is still persisted.
    @Published var name: String?
    /// The profile this tab runs against. `nil` = follow the window.
    @Published var connection: String?
    @Published var isDirty: Bool = false

    var text: String
    var selectedRange: NSRange

    init(
        id: String = UUID().uuidString,
        name: String? = nil,
        connection: String? = nil,
        text: String = "",
        selectedRange: NSRange = NSRange(location: 0, length: 0),
        isDirty: Bool = false
    ) {
        self.id = id
        self.name = name
        self.connection = connection
        self.text = text
        self.selectedRange = selectedRange
        self.isDirty = isDirty
    }

    /// Untitled tabs are numbered by the model, not here, so the number stays
    /// stable for the life of the tab rather than shifting when a sibling closes.
    var untitledNumber: Int = 0

    var displayTitle: String {
        if let name, !name.isEmpty { return name }
        return untitledNumber > 0 ? "Untitled \(untitledNumber)" : "Untitled"
    }

    var record: SavedQueryRecord {
        SavedQueryRecord(
            id: id, name: name, connection: connection,
            cursorLocation: selectedRange.location, cursorLength: selectedRange.length,
            isDirty: isDirty)
    }
}

/// Tab list + which one is frontmost. The controller owns this and installs the
/// command closures; the SwiftUI bar only ever calls them.
final class EditorTabsModel: ObservableObject {
    @Published var tabs: [EditorTab] = []
    @Published var activeID: String?
    @Published var connections: [EditorConnectionOption] = []
    /// Named queries on disk that are not currently open in a tab.
    @Published var savedQueries: [SavedQueryRecord] = []

    var onActivate: ((EditorTab) -> Void)?
    var onClose: ((EditorTab) -> Void)?
    var onNew: (() -> Void)?
    var onSave: ((EditorTab) -> Void)?
    var onBind: ((EditorTab, String?) -> Void)?
    var onOpenSaved: ((SavedQueryRecord) -> Void)?

    var active: EditorTab? { tabs.first { $0.id == activeID } }

    var activeIndex: Int? { tabs.firstIndex { $0.id == activeID } }

    /// Lowest unused number, so closing "Untitled 2" frees the name again
    /// instead of marching the counter upward forever.
    func nextUntitledNumber() -> Int {
        let used = Set(tabs.filter { $0.name == nil }.map(\.untitledNumber))
        var n = 1
        while used.contains(n) { n += 1 }
        return n
    }
}

// MARK: - the bar

/// Sits directly above the `NSTextView`, hosted in an `NSHostingView`. SwiftUI
/// here and not AppKit because the engine mark (`EngineIcon`) is already a
/// SwiftUI view — reimplementing it in AppKit would be a second definition of
/// what an engine looks like, which `EngineStyle` exists to prevent.
struct EditorTabBar: View {
    @ObservedObject var model: EditorTabsModel

    var body: some View {
        HStack(spacing: 6) {
            ScrollView(.horizontal, showsIndicators: false) {
                HStack(spacing: 4) {
                    ForEach(model.tabs) { tab in
                        EditorTabChip(
                            tab: tab,
                            isActive: tab.id == model.activeID,
                            activate: { model.onActivate?(tab) },
                            close: { model.onClose?(tab) })
                    }
                }
                .padding(.horizontal, 2)
            }

            // Closing a named tab keeps its .sql on disk, so there has to be a
            // way back to it. This is that way.
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
    let isActive: Bool
    let activate: () -> Void
    let close: () -> Void

    @State private var hovering = false

    var body: some View {
        HStack(spacing: 5) {
            Text(tab.displayTitle)
                .font(.system(size: 11, weight: isActive ? .semibold : .regular))
                .lineLimit(1)

            // One dot, two meanings, never both at once: unsaved work, or a
            // close button once the pointer is over the chip.
            ZStack {
                if hovering {
                    Image(systemName: "xmark")
                        .font(.system(size: 8, weight: .bold))
                        .foregroundStyle(.secondary)
                        .onTapGesture(perform: close)
                } else if tab.isDirty {
                    Circle()
                        .fill(Color.secondary)
                        .frame(width: 6, height: 6)
                }
            }
            .frame(width: 10, height: 10)
        }
        .padding(.horizontal, 9)
        .frame(height: 22)
        .background(
            RoundedRectangle(cornerRadius: 6, style: .continuous)
                .fill(
                    isActive
                        ? Color(nsColor: .textBackgroundColor)
                        : Color(nsColor: .quaternaryLabelColor).opacity(0.22))
        )
        .overlay(
            RoundedRectangle(cornerRadius: 6, style: .continuous)
                .strokeBorder(
                    isActive ? Color.accentColor.opacity(0.55) : Color.clear, lineWidth: 1)
        )
        .contentShape(Rectangle())
        .onTapGesture(perform: activate)
        .onHover { hovering = $0 }
        .help(tab.name.map { "\($0)  ·  ⌘S saves" } ?? "Unsaved scratch tab — ⌘S names it")
    }
}

/// The heart of the request: *"the editor can be saved so the user can choose
/// which db"*. The binding lives on the tab, so switching tabs switches the
/// connection the next ⌘↵ runs against.
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
