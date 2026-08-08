import DatagrepKit
import SwiftUI

// MARK: - colour vocabulary

/// The connection colours offered in the editor.
///
/// Named, not free-form: the value is stored on the profile and read back by
/// the CLI, so it has to mean the same thing in both. Every swatch resolves to
/// a *system* colour, which is why the sidebar marker stays legible in dark
/// mode — §5 item 4 of the reference study is four competitors shipping safety
/// UI that disappears in one theme.
enum ConnectionColor {
    static let names = ["red", "orange", "yellow", "green", "blue", "purple", "graphite"]

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

// MARK: - draft

/// The editor's working copy of one connection.
///
/// Held apart from the saved profile on purpose: nothing typed here reaches the
/// engine until Save, and the patch sent on Save is the *difference* against
/// `original`, so a field this build did not report is never written back as an
/// empty value.
@MainActor
final class ConnectionDraft: ObservableObject, Identifiable {
    nonisolated let id = UUID()

    /// The name the connection is saved under. It is also the ABI's key, so a
    /// rename has to send the old one alongside the new.
    let originalName: String

    @Published var name: String
    @Published var url: String
    @Published var env: String
    @Published var readOnly: Bool
    @Published var confirmWrites: Bool
    @Published var autoLimitText: String
    @Published var idleTimeoutText: String
    @Published var color: String?

    /// Never pre-filled. The saved secret is in the keychain and does not come
    /// back through this ABI — round-tripping it through a text field would put
    /// a live password in the view hierarchy for no gain.
    @Published var password: String = ""
    @Published var hasSecret: Bool

    @Published var loading = false
    @Published var saving = false
    @Published var error: String?
    /// Set when the sheet opened against a build that cannot read a profile
    /// back, so the URL box is blank because we do not know it — not because
    /// the connection has no URL.
    @Published var urlUnknown = false

    @Published var enforcement: ReadOnlyEnforcement = .unknown
    private var original: ProfileDetail

    init(detail: ProfileDetail) {
        self.originalName = detail.name
        self.original = detail
        self.name = detail.name
        self.url = detail.url
        self.env = detail.env
        self.readOnly = detail.readOnly
        self.confirmWrites = detail.confirmWrites
        self.autoLimitText = detail.autoLimit.map(String.init) ?? ""
        self.idleTimeoutText = detail.idleTimeoutS.map(String.init) ?? ""
        self.color = detail.color
        self.hasSecret = detail.hasSecret
        self.enforcement = detail.enforcement
        self.urlUnknown = detail.url.isEmpty && !detail.reported.contains("url")
    }

    /// Re-seed an already-presented sheet once the background `_get_json` lands.
    func apply(_ detail: ProfileDetail) {
        original = detail
        name = detail.name
        url = detail.url
        env = detail.env
        readOnly = detail.readOnly
        confirmWrites = detail.confirmWrites
        autoLimitText = detail.autoLimit.map(String.init) ?? ""
        idleTimeoutText = detail.idleTimeoutS.map(String.init) ?? ""
        color = detail.color
        hasSecret = detail.hasSecret
        enforcement = detail.enforcement
        urlUnknown = detail.url.isEmpty && !detail.reported.contains("url")
        loading = false
    }

    var driver: String {
        let u = url.lowercased()
        for e in ConnectionEngines.all where u.hasPrefix(e.scheme) { return e.id }
        return original.driver
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
    ///
    /// `datagrep_profiles_add` already takes the secret this way and lifts it
    /// into the keychain before the profile is written, so this reuses the one
    /// path that is known to keep a password off disk. When the user types
    /// nothing, no URL password is sent and the stored secret is left alone.
    private var urlToSend: String? {
        let typed = password
        let urlChanged = trimmedURL != original.url && !trimmedURL.isEmpty
        guard !typed.isEmpty || urlChanged else { return nil }
        guard !typed.isEmpty else { return trimmedURL }
        return Self.embedPassword(typed, in: trimmedURL.isEmpty ? original.url : trimmedURL)
    }

    /// Inserts a password into `scheme://user@host/…` between the user and the
    /// `@`. Percent-encodes it, because a `@` or a `/` in a password otherwise
    /// re-points the URL at a different host.
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
        if env != original.env { p.set("env", env) }
        if readOnly != original.readOnly { p.set("read_only", readOnly) }
        if confirmWrites != original.confirmWrites { p.set("confirm_writes", confirmWrites) }
        if autoLimit != original.autoLimit { p.set("auto_limit", autoLimit) }
        if idleTimeout != original.idleTimeoutS { p.set("idle_timeout_s", idleTimeout) }
        if color != original.color { p.set("color", color) }
        return p
    }

    var finalName: String { trimmedName }
}

/// The engines the URL field understands, shared with the New Connection sheet's
/// list so the two read as one thing.
enum ConnectionEngines {
    static let all: [(id: String, scheme: String, example: String)] = [
        ("postgres", "postgres://", "postgres://user@localhost/mydb"),
        ("mysql", "mysql://", "mysql://user@localhost/mydb"),
        ("sqlite", "sqlite://", "sqlite:///Users/me/data.db"),
        ("redis", "redis://", "redis://localhost:6379"),
        ("mongo", "mongodb://", "mongodb://localhost/mydb"),
    ]
}

// MARK: - the sheet

/// Edit Connection. Deliberately the same silhouette as `NewConnectionSheet` —
/// same header, same field grid, same footer — because a user who has added a
/// connection should recognise this instantly rather than learn a second dialog.
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
                        "This engine build cannot read a saved connection back, so the URL is not shown. Leave it blank to keep the one already saved; typing one replaces it."
                )
            }

            fields

            safetySection

            if draft.env == "prod" { prodWarning }

            if let err = draft.error {
                Callout(symbol: "exclamationmark.triangle.fill", tone: .error, text: err)
            }

            footer
        }
        .padding(20)
        .frame(width: 512)
        .animation(.smooth(duration: 0.18), value: draft.readOnly)
        .animation(.smooth(duration: 0.18), value: draft.env)
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
        Grid(alignment: .leading, horizontalSpacing: 10, verticalSpacing: 8) {
            GridRow {
                Text("Name").foregroundStyle(.secondary)
                TextField("local", text: $draft.name)
                    .textFieldStyle(.roundedBorder)
            }
            GridRow {
                Text("URL").foregroundStyle(.secondary)
                TextField(
                    draft.urlUnknown ? "unchanged" : "sqlite:///Users/me/data.db",
                    text: $draft.url
                )
                .textFieldStyle(.roundedBorder)
                .font(.system(size: 11, design: .monospaced))
            }
            GridRow {
                Text("Password").foregroundStyle(.secondary)
                VStack(alignment: .leading, spacing: 2) {
                    SecureField(draft.hasSecret ? "••••••••" : "none saved", text: $draft.password)
                        .textFieldStyle(.roundedBorder)
                    Text(
                        draft.hasSecret
                            ? "A password is saved in the macOS keychain. Leave this blank to keep it — datagrep never reads it back into the window."
                            : "Optional. Whatever you type is moved into the macOS keychain before the connection is written; it never reaches disk in plain text."
                    )
                    .font(.caption2)
                    .foregroundStyle(.secondary)
                    .fixedSize(horizontal: false, vertical: true)
                }
            }
            GridRow {
                Text("Environment").foregroundStyle(.secondary)
                Picker("", selection: $draft.env) {
                    Text("Development").tag("dev")
                    Text("Staging").tag("staging")
                    Text("Production").tag("prod")
                }
                .pickerStyle(.segmented)
                .labelsHidden()
            }
            GridRow {
                Text("Colour").foregroundStyle(.secondary)
                HStack(spacing: 6) {
                    SwatchButton(name: nil, selected: draft.color == nil) { draft.color = nil }
                    ForEach(ConnectionColor.names, id: \.self) { n in
                        SwatchButton(name: n, selected: draft.color == n) { draft.color = n }
                    }
                }
            }
            GridRow {
                Text("Row limit").foregroundStyle(.secondary)
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
                Text("Idle timeout").foregroundStyle(.secondary)
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

    /// The reason this dialog exists. Read-only first, and it says which kind
    /// of read-only it is buying you before you rely on it.
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

    private var prodWarning: some View {
        Callout(
            symbol: "exclamationmark.octagon.fill", tone: .error,
            text:
                "Marked production. datagrep will paint the whole window red and flag this connection everywhere it appears, so a statement is never run against it by accident."
        )
    }

    // MARK: footer

    private var footer: some View {
        HStack {
            if !draft.changedKeys.isEmpty {
                // What the patch will contain, spelled out. Nothing this dialog
                // does not name here is written.
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

/// Says which protection is really in force. Never phrased so that a
/// client-side guard could be mistaken for the server refusing writes (§3.8).
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
