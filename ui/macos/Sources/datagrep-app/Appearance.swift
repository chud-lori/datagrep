import AppKit

/// The user's appearance choice: follow macOS, or pin light or dark.
///
/// Comparable clients all offer this, and people do use it — a dark IDE beside
/// a light database client, or the reverse. The default stays `.system`, so a
/// user who never opens the menu sees exactly the behaviour they had before.
///
/// Stored in `UserDefaults` rather than the profile database on purpose: it is a
/// pure UI preference with no meaning to the engine or the CLI, and putting it
/// in `profiles.sqlite` would make a window setting part of the data the user
/// exports and syncs.
enum AppearanceMode: String, CaseIterable {
    case system
    case light
    case dark

    /// Key is namespaced because `UserDefaults.standard` for this bundle is
    /// shared with anything AppKit stores under its own names.
    private static let key = "datagrep.appearance"

    var title: String {
        switch self {
        case .system: return "Match System"
        case .light: return "Light"
        case .dark: return "Dark"
        }
    }

    /// `nil` means "inherit", which is what makes `.system` follow macOS live
    /// rather than freezing whatever the appearance happened to be at launch.
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

    /// Applies the stored choice to the running app.
    ///
    /// Nothing else needs to be told: assigning `NSApp.appearance` changes
    /// `effectiveAppearance`, and `EngineAppearanceObserver` is a KVO observer
    /// on exactly that — so the light/dark engine brand icons re-resolve on
    /// their own. Every view already uses semantic colours, so they follow too.
    @MainActor
    static func apply(_ mode: AppearanceMode = .current) {
        NSApp.appearance = mode.nsAppearance
    }
}
