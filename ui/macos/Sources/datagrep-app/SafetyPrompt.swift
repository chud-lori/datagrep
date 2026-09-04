import DatagrepKit
import LocalAuthentication
import SwiftUI

/// One open ceremony: the engine's decision, and whatever the user has done about it so far.
@MainActor
final class SafetyPrompt: ObservableObject, Identifiable {
    nonisolated let id = UUID()
    let decision: SafetyDecision
    let profile: String

    @Published var typed = ""
    @Published var working = false
    @Published var error: String?

    init(decision: SafetyDecision, profile: String) {
        self.decision = decision
        self.profile = profile
    }

    var needsAuthentication: Bool { decision.requires == .authenticate }
}

/// Touch ID, and nothing else — the typed connection name is the only fallback.
enum SystemAuth {
    static var isAvailable: Bool {
        let context = LAContext()
        var error: NSError?
        guard context.canEvaluatePolicy(.deviceOwnerAuthenticationWithBiometrics, error: &error)
        else { return false }
        return context.biometryType == .touchID
    }

    static func authenticate(reason: String, then done: @escaping (String?, String?) -> Void) {
        let context = LAContext()
        context.localizedCancelTitle = "Cancel"
        context.evaluatePolicy(.deviceOwnerAuthenticationWithBiometrics, localizedReason: reason) {
            ok, error in
            DispatchQueue.main.async {
                done(ok ? "touch_id" : nil, ok ? nil : error?.localizedDescription)
            }
        }
    }
}

struct SafetyPromptSheet: View {
    @ObservedObject var model: AppModel
    @ObservedObject var prompt: SafetyPrompt

    private var decision: SafetyDecision { prompt.decision }

    var body: some View {
        VStack(alignment: .leading, spacing: 14) {
            header
            statements
            if prompt.needsAuthentication {
                authentication
            } else {
                Text(
                    "Nothing has been sent yet. Running it sends exactly what is listed above, once."
                )
                .font(.caption)
                .foregroundStyle(.secondary)
                .fixedSize(horizontal: false, vertical: true)
            }
            if let error = prompt.error {
                Callout(symbol: "exclamationmark.triangle.fill", tone: .error, text: error)
            }
            footer
        }
        .padding(20)
        .frame(width: 512)
        .animation(.smooth(duration: 0.18), value: prompt.error)
    }

    private var tone: Color {
        prompt.needsAuthentication ? Color(nsColor: .systemRed) : Color(nsColor: .systemOrange)
    }

    private var header: some View {
        HStack(alignment: .top, spacing: 10) {
            Image(systemName: decision.level.symbol)
                .font(.system(size: 22, weight: .semibold))
                .foregroundStyle(tone)
                .frame(width: 26)
            VStack(alignment: .leading, spacing: 2) {
                Text(
                    prompt.needsAuthentication
                        ? "Authenticate to run this on “\(prompt.profile)”"
                        : "Run this on “\(prompt.profile)”?"
                )
                .font(.headline)
                Text("\(decision.level.title) — \(decision.level.detail)")
                    .font(.caption)
                    .foregroundStyle(.secondary)
                    .fixedSize(horizontal: false, vertical: true)
            }
            Spacer(minLength: 0)
        }
    }

    private var listed: [SafetyStatement] { Array(decision.statements.prefix(6)) }

    private var statements: some View {
        VStack(alignment: .leading, spacing: 6) {
            ForEach(Array(listed.enumerated()), id: \.offset) { _, statement in
                StatementRow(statement: statement)
            }
            if decision.statements.count > listed.count {
                Text("and \(decision.statements.count - listed.count) more in this script")
                    .font(.caption2)
                    .foregroundStyle(.secondary)
            }
        }
        .padding(10)
        .frame(maxWidth: .infinity, alignment: .leading)
        .background(
            RoundedRectangle(cornerRadius: 8, style: .continuous)
                .fill(Color(nsColor: .quaternaryLabelColor).opacity(0.22)))
    }

    private var authentication: some View {
        VStack(alignment: .leading, spacing: 8) {
            if SystemAuth.isAvailable {
                Button {
                    model.authenticateWithTouchID(prompt)
                } label: {
                    Label("Confirm with Touch ID", systemImage: "touchid")
                        .frame(maxWidth: .infinity)
                }
                .controlSize(.large)
                .keyboardShortcut(.defaultAction)
                .disabled(prompt.working)

                Text("or type the connection name")
                    .font(.caption)
                    .foregroundStyle(.secondary)
            }
            HStack(spacing: 8) {
                TextField("\(prompt.profile)", text: $prompt.typed)
                    .textFieldStyle(.roundedBorder)
                    .font(.system(size: 12, design: .monospaced))
                    .onSubmit { model.answerSafetyPrompt(prompt, with: .typedPhrase(prompt.typed)) }
                Button("Confirm") {
                    model.answerSafetyPrompt(prompt, with: .typedPhrase(prompt.typed))
                }
                .disabled(prompt.typed.isEmpty || prompt.working)
            }
            Text(
                "datagrep never checks what you type — the engine compares it against the name it holds."
            )
            .font(.caption2)
            .foregroundStyle(.secondary)
            .fixedSize(horizontal: false, vertical: true)
        }
    }

    private var footer: some View {
        HStack(spacing: 8) {
            if prompt.working {
                ProgressView().controlSize(.small)
            }
            Spacer()
            Button("Cancel") { model.cancelSafetyPrompt(prompt) }
                .keyboardShortcut(.cancelAction)
            if !prompt.needsAuthentication {
                Button("Run") { model.answerSafetyPrompt(prompt, with: .acknowledged) }
                    .keyboardShortcut(.defaultAction)
                    .disabled(prompt.working)
            }
        }
    }
}

private struct StatementRow: View {
    let statement: SafetyStatement

    private var tone: Color {
        switch statement.kind {
        case .read: return Color(nsColor: .systemBlue)
        case .unknown: return Color(nsColor: .systemRed)
        default: return Color(nsColor: .systemOrange)
        }
    }

    var body: some View {
        HStack(alignment: .firstTextBaseline, spacing: 8) {
            Text(statement.kind.label)
                .font(.system(size: 8.5, weight: .bold))
                .tracking(0.3)
                .foregroundStyle(tone)
                .padding(.horizontal, 5)
                .padding(.vertical, 2)
                .background(Capsule().fill(tone.opacity(0.16)))
                .overlay(Capsule().strokeBorder(tone.opacity(0.45), lineWidth: 0.8))
            VStack(alignment: .leading, spacing: 1) {
                Text(statement.text)
                    .font(.system(size: 11.5, design: .monospaced))
                    .lineLimit(2)
                    .truncationMode(.middle)
                Text(
                    statement.requires == .none
                        ? "\(statement.kind.note) · goes straight through"
                        : statement.kind.note
                )
                .font(.caption2)
                .foregroundStyle(.secondary)
            }
            Spacer(minLength: 0)
        }
        .opacity(statement.requires == .none ? 0.55 : 1)
    }
}
