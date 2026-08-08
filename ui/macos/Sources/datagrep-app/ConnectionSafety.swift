import AppKit
import DatagrepKit
import SwiftUI

// MARK: - what one connection promises

/// The safety facts about a connection, resolved once so the sidebar, the
/// titlebar chip and the query path all answer the question the same way.
struct ConnectionSafety: Equatable {
    var name: String
    var isProd: Bool
    var readOnly: Bool
    var enforcement: ReadOnlyEnforcement
    var confirmWrites: Bool
    var color: String?

    static let empty = ConnectionSafety(
        name: "", isProd: false, readOnly: false, enforcement: .unknown, confirmWrites: false,
        color: nil)

    var hasAnything: Bool { isProd || readOnly }
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

/// Layer 2 of the production guardrail, in the sidebar. Large and always
/// visible: when Sequel Ace shrank its full-width production colour to a dot it
/// drew sustained backlash, so this is a filled band with a word in it, not a
/// tint.
struct ProdBanner: View {
    let name: String

    var body: some View {
        HStack(spacing: 6) {
            Image(systemName: "exclamationmark.octagon.fill")
                .font(.system(size: 12, weight: .bold))
            VStack(alignment: .leading, spacing: 0) {
                Text("PRODUCTION")
                    .font(.system(size: 11, weight: .heavy))
                    .tracking(1.1)
                Text(name)
                    .font(.system(size: 10))
                    .opacity(0.9)
                    .lineLimit(1)
                    .truncationMode(.middle)
            }
            Spacer(minLength: 0)
        }
        .foregroundStyle(.white)
        .padding(.horizontal, 10)
        .padding(.vertical, 6)
        .frame(maxWidth: .infinity, alignment: .leading)
        .background(Color(nsColor: .systemRed))
        .accessibilityLabel("Production connection \(name)")
    }
}

/// The per-row marker: a solid red bar down the leading edge plus the word.
/// Both, so it survives a narrow sidebar and a colour-blind reader alike.
struct ProdRowMarker: View {
    var body: some View {
        Text("PROD")
            .font(.system(size: 8.5, weight: .heavy))
            .tracking(0.5)
            .foregroundStyle(.white)
            .padding(.horizontal, 4)
            .padding(.vertical, 1)
            .background(Capsule().fill(Color(nsColor: .systemRed)))
            .accessibilityLabel("production")
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
            if safety.isProd {
                HStack(spacing: 4) {
                    Image(systemName: "exclamationmark.octagon.fill")
                        .font(.system(size: 10, weight: .bold))
                    Text("PRODUCTION")
                        .font(.system(size: 9.5, weight: .heavy))
                        .tracking(0.8)
                }
                .foregroundStyle(.white)
                .padding(.horizontal, 7)
                .padding(.vertical, 3)
                .background(Capsule().fill(Color(nsColor: .systemRed)))
                .help("This connection is marked production.")
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
