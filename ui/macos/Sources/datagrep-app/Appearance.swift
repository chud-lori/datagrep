import AppKit

/// The user's appearance choice: follow macOS, or pin light or dark.
enum AppearanceMode: String, CaseIterable {
    case system
    case light
    case dark

    private static let key = "datagrep.appearance"

    var title: String {
        switch self {
        case .system: return "Match System"
        case .light: return "Light"
        case .dark: return "Dark"
        }
    }

    var nsAppearance: NSAppearance? {
        switch self {
        case .system: return nil
        case .light: return NSAppearance(named: .aqua)
        case .dark: return NSAppearance(named: .darkAqua)
        }
    }

    static var current: AppearanceMode {
        get { AppearanceMode(rawValue: UserDefaults.standard.string(forKey: key) ?? "") ?? .system }
        set { UserDefaults.standard.set(newValue.rawValue, forKey: key) }
    }

    @MainActor
    static func apply(_ mode: AppearanceMode = .current) {
        NSApp.appearance = mode.nsAppearance
    }
}
