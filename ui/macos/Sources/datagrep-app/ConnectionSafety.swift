import AppKit
import DatagrepKit
import SwiftUI

// MARK: - what one connection promises

struct ConnectionSafety: Equatable {
    var name: String
    var readOnly: Bool
    var enforcement: ReadOnlyEnforcement
    var level: SafetyLevel
    var color: String?

    static let empty = ConnectionSafety(
        name: "", readOnly: false, enforcement: .unknown, level: .silent,
        color: nil)

    var isMarked: Bool { color != nil }

    var hasAnything: Bool { isMarked || readOnly || level.asksForAnything }
}

// MARK: - read-only badge

struct ReadOnlyBadge: View {
    let level: ReadOnlyEnforcement
    var compact = false

    private var tone: Color {
        switch level {
        case .server: return Color(nsColor: .systemGreen)
        case .client, .unknown: return Color(nsColor: .systemBlue)
        case .none: return Color(nsColor: .systemOrange)
        }
    }

    var body: some View {
        HStack(spacing: 3) {
            Image(systemName: level.symbol)
                .font(.system(size: compact ? 9 : 10.5, weight: .semibold))
            Text(compact ? "RO" : "READ-ONLY")
                .font(.system(size: compact ? 8 : 9.5, weight: .bold))
                .tracking(0.3)
            if !compact {
                Text(level.shortLabel)
                    .font(.system(size: 8.5, weight: .medium))
                    .opacity(0.85)
            }
        }
        .foregroundStyle(tone)
        .padding(.horizontal, compact ? 4 : 6)
        .padding(.vertical, compact ? 1 : 2)
        .background(Capsule().fill(tone.opacity(0.16)))
        .overlay(Capsule().strokeBorder(tone.opacity(0.45), lineWidth: 0.8))
        .accessibilityLabel(level.headline)
        .help("\(level.headline)\n\n\(level.detail)")
    }
}

// MARK: - production marker

struct MarkedBanner: View {
    let name: String
    let color: String

    var body: some View {
        HStack(spacing: 6) {
            Image(systemName: "exclamationmark.octagon.fill")
                .font(.system(size: 12, weight: .bold))
            Text(name)
                .font(.system(size: 11, weight: .heavy))
                .lineLimit(1)
                .truncationMode(.middle)
            Spacer(minLength: 0)
        }
        .foregroundStyle(.white)
        .padding(.horizontal, 10)
        .padding(.vertical, 6)
        .frame(maxWidth: .infinity, alignment: .leading)
        .background(ConnectionColor.color(color) ?? Color(nsColor: .systemGray))
        .accessibilityLabel("Marked connection \(name)")
    }
}

// MARK: - the titlebar chip

struct ConnectionSafetyChip: View {
    @ObservedObject var model: AppModel

    var body: some View {
        let safety = model.activeSafety
        HStack(spacing: 6) {
            if let color = safety.color {
                Circle()
                    .fill(ConnectionColor.color(color) ?? Color(nsColor: .systemGray))
                    .frame(width: 9, height: 9)
                    .overlay(Circle().strokeBorder(Color.primary.opacity(0.25), lineWidth: 0.5))
                    .help("This connection is marked \(color).")
            }
            if !model.activeProfile.isEmpty {
                SafetyLevelMenu(model: model)
            }
            if safety.readOnly {
                ReadOnlyBadge(level: safety.enforcement)
            }
        }
        .frame(height: 22)
        .fixedSize()
    }
}

// MARK: - the padlock: what rung this connection is on, and how to change it

struct SafetyLevelMenu: View {
    @ObservedObject var model: AppModel

    private var level: SafetyLevel { model.activeSafety.level }

    private var tone: Color {
        switch level {
        case .silent: return Color(nsColor: .tertiaryLabelColor)
        case .warnAll, .warnWrites: return Color(nsColor: .systemOrange)
        case .authAll, .authWrites: return Color(nsColor: .systemRed)
        }
    }

    var body: some View {
        Menu {
            Section("Safety for “\(model.activeProfile)”") {
                ForEach(SafetyLevel.allCases, id: \.self) { option in
                    Button {
                        model.setSafetyLevel(option, for: model.activeProfile)
                    } label: {
                        Label {
                            Text("\(option.title) — \(option.detail)")
                        } icon: {
                            Image(systemName: option == level ? "checkmark" : option.symbol)
                        }
                    }
                }
            }
        } label: {
            Image(systemName: level.symbol)
                .font(.system(size: 12, weight: .semibold))
                .foregroundStyle(tone)
                .frame(width: 22, height: 20)
                .contentShape(Rectangle())
        }
        .menuStyle(.button)
        .buttonStyle(.plain)
        .menuIndicator(.hidden)
        .fixedSize()
        .accessibilityLabel("Safety level: \(level.title)")
        .help("\(level.title) — \(level.detail)\n\nClick to change it for “\(model.activeProfile)”.")
    }
}

// MARK: - the rung, in a connection form

struct SafetyLevelPicker: View {
    @Binding var level: SafetyLevel
    var compact = false

    var body: some View {
        VStack(alignment: .leading, spacing: 5) {
            HStack(spacing: 6) {
                Image(systemName: level.symbol)
                Text(compact ? "Safety" : "Safety ladder")
                Spacer(minLength: 12)
                Picker("", selection: $level) {
                    ForEach(SafetyLevel.allCases, id: \.self) { option in
                        Text(option.title).tag(option)
                    }
                }
                .labelsHidden()
                .fixedSize()
            }
            Text(level.detail)
                .font(.caption)
                .foregroundStyle(.secondary)
                .fixedSize(horizontal: false, vertical: true)
        }
        .animation(.smooth(duration: 0.18), value: level)
    }
}
