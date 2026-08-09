import AppKit
import DatagrepKit
import SwiftUI

// MARK: - what one connection promises

/// The safety facts about a connection, resolved once so the sidebar, the
/// titlebar chip and the query path all answer the question the same way.
struct ConnectionSafety: Equatable {
    var name: String
    var readOnly: Bool
    var enforcement: ReadOnlyEnforcement
    var confirmWrites: Bool
    var color: String?

    static let empty = ConnectionSafety(
        name: "", readOnly: false, enforcement: .unknown, confirmWrites: false,
        color: nil)

    /// A colour is the user's own "this one matters" marker — datagrep does
    /// not decide what red means, it just shows it where a mistake would hurt.
    var isMarked: Bool { color != nil }

    var hasAnything: Bool { isMarked || readOnly }
}

// MARK: - read-only badge

/// A lock and a word, at two sizes. Never a tint on its own: a colour-only
/// signal disappears in whichever theme it was not tuned for, and it would also
/// collide with the connection colour the user picked.
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

/// The banner for a connection the user has given a colour. Large and always
/// visible: when Sequel Ace shrank its full-width production colour to a dot it
/// drew sustained backlash, so this is a filled band with the connection's name
/// in it, not a tint.
///
/// It says the name and nothing else. datagrep does not know what the colour
/// means — that is the point of letting the user choose it.
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

/// What sits at the trailing end of the toolbar. It is the same two facts as
/// the sidebar — production, and how real the read-only promise is — carried
/// into the one piece of chrome that is on screen no matter which pane has
/// focus or whether the sidebar is collapsed.
struct ConnectionSafetyChip: View {
    @ObservedObject var model: AppModel

    var body: some View {
        let safety = model.activeSafety
        HStack(spacing: 6) {
            if let color = safety.color {
                // A dot, not a word: the colour is the user's own marker and
                // datagrep has no name for what it means.
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
///
/// It goes here rather than into the SwiftUI `.toolbar` because the toolbar is
/// owned by another part of the window; an accessory view is additive, attaches
/// once, and never re-lays-out the controls already in the bar.
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
