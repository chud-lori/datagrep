import Combine
import DatagrepKit
import SwiftUI

// MARK: - colour vocabulary

/// The connection colours offered in the editor.
enum ConnectionColor {
    static let names = ["red", "orange", "yellow", "green", "blue", "purple", "graphite"]

    static func nsColor(_ name: String?) -> NSColor? {
        switch name?.lowercased() {
        case "red": return .systemRed
        case "orange": return .systemOrange
        case "yellow": return .systemYellow
        case "green": return .systemGreen
        case "blue": return .systemBlue
        case "purple": return .systemPurple
        case "graphite", "gray", "grey": return .systemGray
        default: return nil
        }
    }

    static func color(_ name: String?) -> Color? {
        switch name?.lowercased() {
        case "red": return Color(nsColor: .systemRed)
        case "orange": return Color(nsColor: .systemOrange)
        case "yellow": return Color(nsColor: .systemYellow)
        case "green": return Color(nsColor: .systemGreen)
        case "blue": return Color(nsColor: .systemBlue)
        case "purple": return Color(nsColor: .systemPurple)
        case "graphite", "gray", "grey": return Color(nsColor: .systemGray)
        default: return nil
        }
    }
}

// MARK: - the connection form

@MainActor
final class ConnectionForm: ObservableObject {
    @Published var name: String = ""
    @Published private(set) var engineID: String
    @Published var host: String = ""
    @Published var port: String = ""
    @Published var database: String = ""
    @Published var username: String = ""
    @Published var password: String = ""
    @Published var filePath: String = ""
    @Published var useTLS: Bool = false
    @Published var extras: String = ""
    @Published var showsRawURL: Bool = false

    @Published private var rawURLDraft: String?

    init(engineID: String = "postgres") {
        self.engineID = engineID
    }

    var engine: ConnectionEngine? { ConnectionEngines.engine(id: engineID) }

    var fields: ConnectionFields {
        ConnectionFields(
            engineID: engineID, host: host, port: port, database: database, username: username,
            password: password, filePath: filePath, useTLS: useTLS, extras: extras)
    }

    /// What is shown, and what Save/Add sends — without the password.
    var url: String { rawURLDraft ?? fields.url() }

    var urlWithPassword: String {
        guard !password.isEmpty else { return url }
        guard rawURLDraft == nil else {
            return ConnectionDraft.embedPassword(password, in: url)
        }
        return fields.url(includingPassword: true)
    }

    var urlBinding: Binding<String> {
        Binding(
            get: { self.url },
            set: { text in
                self.rawURLDraft = text
                guard var parsed = ConnectionFields.parse(text) else { return }
                if !parsed.password.isEmpty {
                    self.password = parsed.password
                    parsed.password = ""
                    self.rawURLDraft = parsed.url()
                }
                self.applyParsed(parsed)
            })
    }

    func text(_ keyPath: ReferenceWritableKeyPath<ConnectionForm, String>) -> Binding<String> {
        Binding(
            get: { self[keyPath: keyPath] },
            set: { v in
                self.rawURLDraft = nil
                self[keyPath: keyPath] = v
            })
    }

    var tlsBinding: Binding<Bool> {
        Binding(
            get: { self.useTLS },
            set: { v in
                self.rawURLDraft = nil
                self.useTLS = v
            })
    }

    func selectEngine(_ id: String) {
        guard EngineStyle.canonicalID(id) != EngineStyle.canonicalID(engineID) else { return }
        rawURLDraft = nil
        engineID = ConnectionEngines.engine(id: id)?.id ?? id
        port = ""
        if !(engine?.tlsScheme != nil) { useTLS = false }
        extras = ""
    }

    /// Seed from a parsed URL or from a profile's stored config.
    func apply(_ f: ConnectionFields) {
        rawURLDraft = nil
        engineID = f.engineID
        applyParsed(f)
        password = f.password
    }

    private func applyParsed(_ f: ConnectionFields) {
        engineID = f.engineID
        host = f.host
        port = f.port
        database = f.database
        username = f.username
        filePath = f.filePath
        useTLS = f.useTLS
        extras = f.extras
    }

    /// Enough to attempt a connection with.
    var isComplete: Bool {
        guard let engine else { return false }
        return engine.isFileBased
            ? !filePath.trimmingCharacters(in: .whitespaces).isEmpty
            : !host.trimmingCharacters(in: .whitespaces).isEmpty
    }
}

@MainActor
final class ConnectionTestState: ObservableObject {
    @Published var running = false
    @Published var result: ConnectionTestResult?
    @Published var failure: String?

    func begin() {
        running = true
        result = nil
        failure = nil
    }

    func clear() {
        running = false
        result = nil
        failure = nil
    }
}

// MARK: - draft

/// The editor's working copy of one connection.
@MainActor
final class ConnectionDraft: ObservableObject, Identifiable {
    nonisolated let id = UUID()

    let originalName: String

    let form = ConnectionForm()
    let test = ConnectionTestState()
    private var nested: [AnyCancellable] = []

    @Published var name: String
    @Published var readOnly: Bool
    @Published var confirmWrites: Bool
    @Published var autoLimitText: String
    @Published var idleTimeoutText: String
    @Published var color: String?

    var password: String {
        get { form.password }
        set { form.password = newValue }
    }
    @Published var hasSecret: Bool

    @Published var loading = false
    @Published var saving = false
    @Published var error: String?
    @Published var urlUnknown = false

    @Published var enforcement: ReadOnlyEnforcement = .unknown
    private var original: ProfileDetail

    init(detail: ProfileDetail) {
        self.originalName = detail.name
        self.original = detail
        self.name = detail.name
        self.readOnly = detail.readOnly
        self.confirmWrites = detail.confirmWrites
        self.autoLimitText = detail.autoLimit.map(String.init) ?? ""
        self.idleTimeoutText = detail.idleTimeoutS.map(String.init) ?? ""
        self.color = detail.color
        self.hasSecret = detail.hasSecret
        self.enforcement = detail.enforcement
        self.urlUnknown = detail.url.isEmpty && !detail.reported.contains("url")
        seedForm(detail)
        nested = [
            form.objectWillChange.sink { [weak self] in self?.objectWillChange.send() },
            test.objectWillChange.sink { [weak self] in self?.objectWillChange.send() },
        ]
    }

    /// Re-seed an already-presented sheet once the background `_get_json` lands.
    func apply(_ detail: ProfileDetail) {
        original = detail
        name = detail.name
        readOnly = detail.readOnly
        confirmWrites = detail.confirmWrites
        autoLimitText = detail.autoLimit.map(String.init) ?? ""
        idleTimeoutText = detail.idleTimeoutS.map(String.init) ?? ""
        color = detail.color
        hasSecret = detail.hasSecret
        enforcement = detail.enforcement
        urlUnknown = detail.url.isEmpty && !detail.reported.contains("url")
        seedForm(detail)
        loading = false
    }

    /// Fill the fields from whatever the engine reported.
    private func seedForm(_ detail: ProfileDetail) {
        if let fields = detail.fields {
            form.apply(fields)
        } else if let parsed = ConnectionFields.parse(detail.url) {
            form.apply(parsed)
        } else if let engine = ConnectionEngines.engine(id: detail.driver) {
            form.selectEngine(engine.id)
        }
        // Never seeded from the profile: the secret does not cross this ABI.
        form.password = ""
        form.name = detail.name
    }

    var url: String { form.url }

    var originalURL: String { original.url }

    var driver: String {
        let id = form.engineID
        return id.isEmpty ? original.driver : id
    }

    private var trimmedName: String { name.trimmingCharacters(in: .whitespacesAndNewlines) }
    private var trimmedURL: String { url.trimmingCharacters(in: .whitespacesAndNewlines) }
    private var autoLimit: Int? { Int(autoLimitText.trimmingCharacters(in: .whitespaces)) }
    private var idleTimeout: Int? { Int(idleTimeoutText.trimmingCharacters(in: .whitespaces)) }

    var canSave: Bool {
        guard !trimmedName.isEmpty, !saving, !loading else { return false }
        return !changedKeys.isEmpty
    }

    /// The URL actually sent, with a freshly typed password spliced in.
    private var urlToSend: String? {
        let typed = password
        let urlChanged = trimmedURL != original.url && !trimmedURL.isEmpty
        guard !typed.isEmpty || urlChanged else { return nil }
        guard !typed.isEmpty else { return trimmedURL }
        return Self.embedPassword(typed, in: trimmedURL.isEmpty ? original.url : trimmedURL)
    }

    var urlToTest: String { form.urlWithPassword }

    static func embedPassword(_ password: String, in url: String) -> String {
        var allowed = CharacterSet.alphanumerics
        allowed.insert(charactersIn: "-._~")
        let escaped = password.addingPercentEncoding(withAllowedCharacters: allowed) ?? password
        guard let schemeEnd = url.range(of: "://") else { return url }
        let rest = url[schemeEnd.upperBound...]
        let head = url[..<schemeEnd.upperBound]
        // Authority is everything before the first '/', '?' or '#'.
        let authorityEnd =
            rest.firstIndex(where: { $0 == "/" || $0 == "?" || $0 == "#" }) ?? rest.endIndex
        var authority = String(rest[..<authorityEnd])
        let tail = String(rest[authorityEnd...])
        if let at = authority.lastIndex(of: "@") {
            let userinfo = String(authority[..<at])
            let host = String(authority[authority.index(after: at)...])
            let user = userinfo.split(separator: ":", maxSplits: 1).first.map(String.init) ?? ""
            authority = "\(user):\(escaped)@\(host)"
        } else {
            authority = ":\(escaped)@\(authority)"
        }
        return head + authority + tail
    }

    var changedKeys: [String] { patch.changedKeys }

    /// Only what moved. See `ProfilePatch`.
    var patch: ProfilePatch {
        var p = ProfilePatch()
        if trimmedName != original.name { p.set("name", trimmedName) }
        if let u = urlToSend { p.set("url", u) }
        if readOnly != original.readOnly { p.set("read_only", readOnly) }
        if confirmWrites != original.confirmWrites { p.set("confirm_writes", confirmWrites) }
        if autoLimit != original.autoLimit { p.set("auto_limit", autoLimit) }
        if idleTimeout != original.idleTimeoutS { p.set("idle_timeout_s", idleTimeout) }
        if color != original.color { p.set("color", color) }
        return p
    }

    var finalName: String { trimmedName }
}

// MARK: - shared field views

struct EnginePicker: View {
    @ObservedObject var form: ConnectionForm

    var body: some View {
        HStack(spacing: 6) {
            ForEach(ConnectionEngines.all) { e in
                let selected = EngineStyle.canonicalID(form.engineID) == EngineStyle.canonicalID(e.id)
                Button {
                    form.selectEngine(e.id)
                } label: {
                    VStack(spacing: 4) {
                        EngineIcon(e.id, size: 20)
                        Text(EngineStyle.displayName(for: e.id))
                            .font(.system(size: 9))
                            .foregroundStyle(.secondary)
                            .lineLimit(1)
                            .minimumScaleFactor(0.8)
                    }
                    .frame(width: 74, height: 50)
                    .background(
                        RoundedRectangle(cornerRadius: 7, style: .continuous)
                            .fill(
                                selected
                                    ? Color.accentColor.opacity(0.16)
                                    : Color(nsColor: .quaternaryLabelColor).opacity(0.3))
                    )
                    .overlay(
                        RoundedRectangle(cornerRadius: 7, style: .continuous)
                            .strokeBorder(
                                selected ? Color.accentColor : Color.clear, lineWidth: 1.5)
                    )
                }
                .buttonStyle(.plain)
            }
        }
        .animation(.smooth(duration: 0.18), value: form.engineID)
    }
}

struct ConnectionFieldsView: View {
    @ObservedObject var form: ConnectionForm
    var name: Binding<String>?
    var hasStoredSecret: Bool = false

    private var engine: ConnectionEngine? { form.engine }

    static let labelWidth: CGFloat = 88

    private func label(_ text: String) -> some View {
        Text(text)
            .foregroundStyle(.secondary)
            .frame(width: Self.labelWidth, alignment: .leading)
    }

    var body: some View {
        Grid(alignment: .leading, horizontalSpacing: 10, verticalSpacing: 8) {
            if let name {
                GridRow {
                    label("Name")
                    TextField(form.engineID, text: name)
                        .textFieldStyle(.roundedBorder)
                }
            }
            if engine?.isFileBased == true {
                GridRow {
                    label(engine?.databaseLabel ?? "File")
                    HStack(spacing: 6) {
                        TextField(
                            engine?.databasePlaceholder ?? "/Users/me/data.db",
                            text: form.text(\.filePath)
                        )
                        .textFieldStyle(.roundedBorder)
                        Button("Choose…") { chooseFile() }
                            .controlSize(.small)
                    }
                }
                GridRow {
                    label("")
                    Text(
                        "SQLite is a file on disk, not a server — there is no host, port or password to give it."
                    )
                    .font(.caption2)
                    .foregroundStyle(.secondary)
                    .fixedSize(horizontal: false, vertical: true)
                }
            } else {
                GridRow {
                    label("Host")
                    HStack(spacing: 6) {
                        TextField("localhost", text: form.text(\.host))
                            .textFieldStyle(.roundedBorder)
                        Text("Port").foregroundStyle(.secondary).font(.callout)
                        TextField(
                            engine?.defaultPort.map(String.init) ?? "", text: form.text(\.port)
                        )
                        .textFieldStyle(.roundedBorder)
                        .frame(width: 62)
                    }
                }
                GridRow {
                    label(engine?.databaseLabel ?? "Database")
                    TextField(
                        engine?.databasePlaceholder ?? "mydb", text: form.text(\.database)
                    )
                    .textFieldStyle(.roundedBorder)
                }
                GridRow {
                    label("Username")
                    TextField("", text: form.text(\.username))
                        .textFieldStyle(.roundedBorder)
                }
                GridRow {
                    label("Password")
                    VStack(alignment: .leading, spacing: 2) {
                        SecureField(
                            hasStoredSecret ? "••••••••" : "optional", text: $form.password
                        )
                        .textFieldStyle(.roundedBorder)
                        Text(
                            hasStoredSecret
                                ? "A password is saved in the macOS keychain. Leave this blank to keep it — datagrep never reads it back into the window."
                                : "Moved into the macOS keychain before the connection is written; it never reaches disk in plain text, and it is never shown in the URL below."
                        )
                        .font(.caption2)
                        .foregroundStyle(.secondary)
                        .fixedSize(horizontal: false, vertical: true)
                    }
                }
                if engine?.tlsScheme != nil {
                    GridRow {
                        label("")
                        Toggle("Use TLS (https)", isOn: form.tlsBinding)
                            .toggleStyle(.checkbox)
                    }
                }
            }

            GridRow {
                label("")
                DisclosureGroup(isExpanded: $form.showsRawURL) {
                    VStack(alignment: .leading, spacing: 3) {
                        TextField(engine?.example ?? "", text: form.urlBinding)
                            .textFieldStyle(.roundedBorder)
                            .font(.system(size: 11, design: .monospaced))
                        Text(
                            "The profile's storage format, and what the CLI reads. Paste one and the fields above fill in; edit the fields and this follows. Any password stays out of it."
                        )
                        .font(.caption2)
                        .foregroundStyle(.secondary)
                        .fixedSize(horizontal: false, vertical: true)
                    }
                    .padding(.top, 3)
                } label: {
                    Text("Connection URL").font(.callout).foregroundStyle(.secondary)
                }
            }
        }
        .animation(.smooth(duration: 0.18), value: form.engineID)
        .animation(.smooth(duration: 0.18), value: form.showsRawURL)
    }

    private func chooseFile() {
        let panel = NSOpenPanel()
        panel.canChooseFiles = true
        panel.canChooseDirectories = false
        panel.allowsMultipleSelection = false
        panel.message = "Choose a SQLite database file"
        guard panel.runModal() == .OK, let url = panel.url else { return }
        form.text(\.filePath).wrappedValue = url.path
    }
}

/// The Test Connection button and whatever the last test said.
struct ConnectionTestRow: View {
    @ObservedObject var state: ConnectionTestState
    let enabled: Bool
    let run: () -> Void

    var body: some View {
        VStack(alignment: .leading, spacing: 8) {
            HStack(spacing: 8) {
                Button {
                    run()
                } label: {
                    Label("Test Connection", systemImage: "bolt.horizontal.circle")
                }
                .disabled(!enabled || state.running || !ProfileABI.canTest)
                .help(
                    ProfileABI.canTest
                        ? "Open one connection with these settings and report what answers"
                        : "This build of the datagrep engine cannot test a connection")

                if state.running {
                    ProgressView().controlSize(.small)
                    Text("connecting…").font(.caption).foregroundStyle(.secondary)
                }
                Spacer(minLength: 0)
            }

            if let result = state.result {
                Callout(
                    symbol: "checkmark.circle.fill", tone: .good,
                    title: result.headline,
                    text: result.details.isEmpty
                        ? "The engine accepted the connection and it was closed again — nothing was saved by testing."
                        : result.details.map { "\($0.0): \($0.1)" }.joined(separator: " · "))
                .transition(.opacity)
            } else if let failure = state.failure {
                Callout(
                    symbol: "xmark.octagon.fill", tone: .error,
                    title: "Could not connect", text: failure
                )
                .transition(.opacity)
            }
        }
        .animation(.smooth(duration: 0.18), value: state.running)
        .animation(.smooth(duration: 0.18), value: state.result)
        .animation(.smooth(duration: 0.18), value: state.failure)
    }
}

// MARK: - the sheet

struct ConnectionEditorSheet: View {
    @ObservedObject var model: AppModel
    @ObservedObject var draft: ConnectionDraft

    var body: some View {
        VStack(alignment: .leading, spacing: 14) {
            header

            if let reason = ProfileABI.unavailableReason {
                Callout(
                    symbol: "exclamationmark.triangle.fill", tone: .warning,
                    text: reason)
            }
            if draft.urlUnknown {
                Callout(
                    symbol: "questionmark.circle.fill", tone: .info,
                    text:
                        "This engine build cannot read a saved connection back, so the fields below are empty. Leave them alone to keep what is already saved; filling them in replaces it."
                )
            }

            EnginePicker(form: draft.form)

            fields

            ConnectionTestRow(state: draft.test, enabled: draft.form.isComplete) {
                model.testConnection(draft)
            }

            safetySection

            if let err = draft.error {
                Callout(symbol: "exclamationmark.triangle.fill", tone: .error, text: err)
            }

            footer
        }
        .padding(20)
        .frame(width: 512)
        .animation(.smooth(duration: 0.18), value: draft.readOnly)
        .animation(.smooth(duration: 0.18), value: draft.error)
    }

    // MARK: header

    private var header: some View {
        HStack(spacing: 9) {
            EngineIcon(draft.driver, size: 22)
            VStack(alignment: .leading, spacing: 1) {
                Text("Edit Connection").font(.headline)
                Text(
                    draft.driver.isEmpty
                        ? "Paste a connection URL"
                        : EngineStyle.displayName(for: draft.driver)
                )
                .font(.caption)
                .foregroundStyle(.secondary)
            }
            Spacer()
            if draft.loading {
                ProgressView().controlSize(.small)
            }
        }
    }

    // MARK: fields

    private var fields: some View {
        VStack(alignment: .leading, spacing: 8) {
            ConnectionFieldsView(
                form: draft.form, name: $draft.name, hasStoredSecret: draft.hasSecret)
            settings
        }
    }

    private func label(_ text: String) -> some View {
        Text(text)
            .foregroundStyle(.secondary)
            .frame(width: ConnectionFieldsView.labelWidth, alignment: .leading)
    }

    private var settings: some View {
        Grid(alignment: .leading, horizontalSpacing: 10, verticalSpacing: 8) {
            GridRow {
                label("Colour")
                HStack(spacing: 6) {
                    SwatchButton(name: nil, selected: draft.color == nil) { draft.color = nil }
                    ForEach(ConnectionColor.names, id: \.self) { n in
                        SwatchButton(name: n, selected: draft.color == n) { draft.color = n }
                    }
                }
            }
            GridRow {
                label("Row limit")
                HStack(spacing: 8) {
                    TextField("none", text: $draft.autoLimitText)
                        .textFieldStyle(.roundedBorder)
                        .frame(width: 90)
                    Text("rows fetched before datagrep stops on its own")
                        .font(.caption2)
                        .foregroundStyle(.secondary)
                }
            }
            GridRow {
                label("Idle timeout")
                HStack(spacing: 8) {
                    TextField("none", text: $draft.idleTimeoutText)
                        .textFieldStyle(.roundedBorder)
                        .frame(width: 90)
                    Text("seconds before an unused connection is dropped")
                        .font(.caption2)
                        .foregroundStyle(.secondary)
                }
            }
        }
    }

    // MARK: safety

    private var safetySection: some View {
        VStack(alignment: .leading, spacing: 8) {
            Toggle(isOn: $draft.readOnly) {
                HStack(spacing: 6) {
                    Image(systemName: draft.readOnly ? "lock.fill" : "lock.open")
                    Text("Read-only")
                }
            }
            .toggleStyle(.switch)
            .help("Refuse writes on this connection even when the account is allowed to write")

            Text(
                "Refuses writes on this connection even when the database account is allowed to make them."
            )
            .font(.caption)
            .foregroundStyle(.secondary)
            .fixedSize(horizontal: false, vertical: true)

            if draft.readOnly {
                EnforcementNote(level: draft.enforcement)
                    .transition(.opacity)
            }

            Divider().padding(.vertical, 2)

            Toggle(isOn: $draft.confirmWrites) {
                HStack(spacing: 6) {
                    Image(systemName: "hand.raised.fill")
                    Text("Ask before running a write")
                }
            }
            .toggleStyle(.switch)
            .disabled(draft.readOnly)
            .help(
                draft.readOnly
                    ? "Not needed while read-only is on — writes are refused outright"
                    : "Show a confirmation before INSERT / UPDATE / DELETE / DROP")
        }
        .padding(12)
        .frame(maxWidth: .infinity, alignment: .leading)
        .background(
            RoundedRectangle(cornerRadius: 8, style: .continuous)
                .fill(Color(nsColor: .quaternaryLabelColor).opacity(0.22))
        )
    }

    // MARK: footer

    private var footer: some View {
        HStack {
            if !draft.changedKeys.isEmpty {
                Text("Will save: \(draft.changedKeys.joined(separator: ", "))")
                    .font(.caption2)
                    .foregroundStyle(.secondary)
                    .lineLimit(1)
                    .truncationMode(.tail)
            }
            Spacer()
            Button("Cancel") { model.closeConnectionEditor() }
                .keyboardShortcut(.cancelAction)
            Button("Save") { model.saveConnectionDraft() }
                .keyboardShortcut(.defaultAction)
                .disabled(!draft.canSave || !ProfileABI.canEdit)
        }
    }
}

// MARK: - pieces

struct EnforcementNote: View {
    let level: ReadOnlyEnforcement

    private var tone: Callout.Tone {
        switch level {
        case .server: return .good
        case .client, .unknown: return .info
        case .none: return .warning
        }
    }

    var body: some View {
        Callout(symbol: level.symbol, tone: tone, title: level.headline, text: level.detail)
    }
}

struct Callout: View {
    enum Tone {
        case good, info, warning, error
        var color: Color {
            switch self {
            case .good: return Color(nsColor: .systemGreen)
            case .info: return Color(nsColor: .systemBlue)
            case .warning: return Color(nsColor: .systemOrange)
            case .error: return Color(nsColor: .systemRed)
            }
        }
    }

    let symbol: String
    let tone: Tone
    var title: String? = nil
    let text: String

    var body: some View {
        HStack(alignment: .top, spacing: 8) {
            Image(systemName: symbol)
                .foregroundStyle(tone.color)
                .font(.system(size: 12))
                .padding(.top, 1)
            VStack(alignment: .leading, spacing: 2) {
                if let title {
                    Text(title).font(.caption.weight(.semibold))
                }
                Text(text)
                    .font(.caption)
                    .foregroundStyle(.secondary)
                    .fixedSize(horizontal: false, vertical: true)
            }
            Spacer(minLength: 0)
        }
        .padding(9)
        .frame(maxWidth: .infinity, alignment: .leading)
        .background(
            RoundedRectangle(cornerRadius: 7, style: .continuous)
                .fill(tone.color.opacity(0.11))
        )
        .overlay(
            RoundedRectangle(cornerRadius: 7, style: .continuous)
                .strokeBorder(tone.color.opacity(0.35), lineWidth: 1)
        )
    }
}

private struct SwatchButton: View {
    let name: String?
    let selected: Bool
    let action: () -> Void

    var body: some View {
        Button(action: action) {
            ZStack {
                Circle()
                    .fill(ConnectionColor.color(name) ?? Color(nsColor: .quaternaryLabelColor))
                    .frame(width: 17, height: 17)
                if name == nil {
                    Image(systemName: "slash.circle")
                        .font(.system(size: 11))
                        .foregroundStyle(.secondary)
                }
            }
            .overlay(
                Circle()
                    .strokeBorder(selected ? Color.primary : Color.clear, lineWidth: 2)
                    .padding(-2.5)
            )
        }
        .buttonStyle(.plain)
        .help(name?.capitalized ?? "No colour")
    }
}
