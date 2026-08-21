import AppKit

/// The user's appearance choice: follow macOS, or pin light or dark.
///
/// Stored in `UserDefaults`, not the profile database: it is a pure UI
/// preference, and `profiles.sqlite` is data the user exports and syncs.
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

    /// `nil` means "inherit" — what makes `.system` follow macOS live rather
    /// than freezing the appearance at launch.
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

    /// Applies the stored choice. Nothing else needs to be told: assigning
    /// `NSApp.appearance` changes `effectiveAppearance`, which
    /// `EngineAppearanceObserver` observes via KVO, so the engine icons
    /// re-resolve on their own.
    @MainActor
    static func apply(_ mode: AppearanceMode = .current) {
        NSApp.appearance = mode.nsAppearance
    }
}
