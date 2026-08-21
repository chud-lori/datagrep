import AppKit
import DatagrepKit
import SwiftUI

// MARK: - what one connection promises

struct ConnectionSafety: Equatable {
    var name: String
    var readOnly: Bool
    var enforcement: ReadOnlyEnforcement
    var confirmWrites: Bool
    var color: String?

    static let empty = ConnectionSafety(
        name: "", readOnly: false, enforcement: .unknown, confirmWrites: false,
        color: nil)

    var isMarked: Bool { color != nil }

    var hasAnything: Bool { isMarked || readOnly }
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
            if safety.readOnly {
                ReadOnlyBadge(level: safety.enforcement)
            }
        }
        .padding(.trailing, 10)
        .frame(height: 22)
        .fixedSize()
    }
}

/// Installs `ConnectionSafetyChip` as a titlebar accessory.
@MainActor
enum ConnectionSafetyTitlebar {
    private static var controller: NSTitlebarAccessoryViewController?

    static func install(model: AppModel) {
        guard controller == nil else { return }
        guard let window = NSApp.mainWindow ?? NSApp.windows.first(where: { $0.isVisible })
        else { return }

        let host = NSHostingView(rootView: ConnectionSafetyChip(model: model))
        host.frame = NSRect(x: 0, y: 0, width: 240, height: 24)
        let vc = NSTitlebarAccessoryViewController()
        vc.view = host
        vc.layoutAttribute = .right
        window.addTitlebarAccessoryViewController(vc)
        controller = vc
    }
}
