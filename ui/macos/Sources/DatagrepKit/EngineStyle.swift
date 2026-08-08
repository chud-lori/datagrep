import AppKit
import SwiftUI

/// The one place that knows what an engine looks like.
///
/// Every view that shows a connection — the toolbar picker, the sidebar, the
/// New Connection sheet — asks here. There is deliberately no `switch driver`
/// anywhere else: engine identity has to be pixel-identical in all of them or
/// it stops working as a recognition cue.
///
/// The tints are the engines' own brand colours, which is the one intentional
/// exception to "semantic colours only". They are used for **glyphs only**,
/// never for text or backgrounds, so contrast in light and dark mode is still
/// carried entirely by the semantic palette.
public enum EngineStyle {
    /// Brand artwork, if we have it. Cached: `Bundle.module.image` hits the
    /// filesystem, and the sidebar asks for this once per row per redraw.
    ///
    /// `dark` selects the luminance-raised `<engine>-dark` variant — the
    /// simple-icons brand colours (e.g. SQLite `#003B57`) are tuned for a
    /// light background and are nearly invisible on a dark sidebar. Falls back
    /// to the light artwork if no dark variant shipped for this engine, and to
    /// `symbol(for:)` + `tint(for:)` if neither did.
    ///
    /// The `-dark` suffix, not a `dark/` subdirectory: SwiftPM's `.process()`
    /// resource bundler rejects two files sharing a basename anywhere under one
    /// target (`multiple resources named 'postgresql.png'`), even in different
    /// subdirectories, so light and dark artwork have to live side by side with
    /// distinct names.
    public static func logo(for driverID: String, dark: Bool = false) -> NSImage? {
        let key = normalise(driverID)
        let cacheKey = dark ? "\(key)#dark" : key
        if let cached = logoCache[cacheKey] { return cached }
        let file: String?
        switch key {
        case "postgres": file = "postgresql"
        case "mysql": file = "mysql"
        case "sqlite": file = "sqlite"
        case "redis": file = "redis"
        case "mongo": file = "mongodb"
        default: file = nil
        }
        var image: NSImage?
        if let file {
            // Full-colour brand marks, NOT template glyphs: tinting them to
            // labelColor would erase the thing that makes them recognisable.
            if dark, let img = loadImage(named: "\(file)-dark") {
                image = img
            } else if let img = loadImage(named: file) {
                image = img
            }
            if let image {
                image.isTemplate = false
                image.accessibilityDescription = displayName(for: key)
            }
        }
        logoCache[cacheKey] = image
        return image
    }

    private nonisolated(unsafe) static var logoCache: [String: NSImage?] = [:]

    /// Assembles one `NSImage` from the 1×/2×/3× PNGs for `name`, rather than
    /// leaning on `Bundle.image(forResource:)`'s undocumented @2x/@3x
    /// resolution, so retina selection is exactly as explicit for the dark set
    /// as for the light one.
    private static func loadImage(named name: String) -> NSImage? {
        guard let moduleBundle else { return nil }
        let scales: [(suffix: String, scale: CGFloat)] = [("", 1), ("@2x", 2), ("@3x", 3)]
        var reps: [NSBitmapImageRep] = []
        var baseSize: NSSize?
        for (suffix, scale) in scales {
            guard
                let url = moduleBundle.url(forResource: name + suffix, withExtension: "png"),
                let data = try? Data(contentsOf: url),
                let rep = NSBitmapImageRep(data: data)
            else { continue }
            rep.size = NSSize(
                width: CGFloat(rep.pixelsWide) / scale, height: CGFloat(rep.pixelsHigh) / scale)
            reps.append(rep)
            if scale == 1 { baseSize = rep.size }
        }
        guard !reps.isEmpty else { return nil }
        let image = NSImage(size: baseSize ?? reps[0].size)
        for r in reps { image.addRepresentation(r) }
        return image
    }

    /// `Bundle.module` traps when the resource bundle is missing. Reaching it
    /// through a failable lookup instead means a mis-assembled .app degrades to
    /// SF Symbols rather than dying on the first sidebar row.
    private static let moduleBundle: Bundle? = {
        let name = "datagrep-ui_DatagrepKit"
        let candidates = [
            Bundle.main.resourceURL,
            Bundle(for: BundleToken.self).resourceURL,
            Bundle.main.bundleURL,
        ]
        for base in candidates {
            if let url = base?.appendingPathComponent(name + ".bundle"),
                let b = Bundle(url: url)
            {
                return b
            }
        }
        return nil
    }()

    private final class BundleToken {}

    public static func symbol(for driverID: String) -> String {
        switch normalise(driverID) {
        case "postgres": return "cylinder.fill"
        case "mysql": return "cylinder.split.1x2.fill"
        case "sqlite": return "internaldrive.fill"
        case "redis": return "bolt.fill"
        case "mongo": return "leaf.fill"
        // No brand artwork shipped for Elastic, so this glyph is what the
        // engine looks like everywhere — a magnifier, because a search index
        // is what it is, and nothing else in `NodeStyle` uses one.
        case "elasticsearch": return "magnifyingglass.circle.fill"
        default: return "cylinder.fill"
        }
    }

    public static func tint(for driverID: String) -> Color {
        switch normalise(driverID) {
        case "postgres": return Color(red: 0.200, green: 0.404, blue: 0.569)  // #336791
        case "mysql": return Color(red: 0.000, green: 0.459, blue: 0.561)  // #00758F
        case "sqlite": return Color(red: 0.541, green: 0.541, blue: 0.557)  // #8A8A8E
        case "redis": return Color(red: 0.863, green: 0.220, blue: 0.176)  // #DC382D
        case "mongo": return Color(red: 0.278, green: 0.635, blue: 0.282)  // #47A248
        case "elasticsearch": return Color(red: 0.000, green: 0.749, blue: 0.702)  // #00BFB3
        default: return Color.secondary
        }
    }

    public static func displayName(for driverID: String) -> String {
        switch normalise(driverID) {
        case "postgres": return "PostgreSQL"
        case "mysql": return "MySQL"
        case "sqlite": return "SQLite"
        case "redis": return "Redis"
        case "mongo": return "MongoDB"
        // One driver, two products — the handshake decides which, so the name
        // shown before a connection exists has to cover both.
        case "elasticsearch": return "Elasticsearch"
        default: return driverID
        }
    }

    /// True when `SELECT * FROM (<query>) ORDER BY …` is a legal thing to send.
    /// Sorting is offered only where it can be pushed to the engine, because
    /// sorting the 2 048 rows currently in the page cache would be a lie about
    /// a 500 000-row result.
    public static func supportsSubqueryOrderBy(_ driverID: String) -> Bool {
        ["postgres", "mysql", "sqlite"].contains(normalise(driverID))
    }

    /// Driver ids arrive from the engine (`postgres`, `sqlite`, `redis`, …) but
    /// URLs and human typing bring variants; fold them once, here.
    ///
    /// Public because `ConnectionEngines` matches a saved profile's driver
    /// against its own engine list, and a second folding table would be exactly
    /// the "two definitions of what an engine is" this type exists to prevent.
    public static func canonicalID(_ id: String) -> String { normalise(id) }

    private static func normalise(_ id: String) -> String {
        let s = id.lowercased()
        if s.hasPrefix("postgres") || s == "pg" || s == "psql" { return "postgres" }
        if s.hasPrefix("mysql") || s.hasPrefix("maria") { return "mysql" }
        if s.hasPrefix("sqlite") { return "sqlite" }
        if s.hasPrefix("redis") || s.hasPrefix("rediss") { return "redis" }
        if s.hasPrefix("mongo") { return "mongo" }
        if s.hasPrefix("elastic") || s.hasPrefix("opensearch") { return "elasticsearch" }
        return s
    }
}

/// Watches the app's effective appearance so engine artwork swaps between the
/// light- and dark-background variant the moment the user flips System
/// Settings' appearance — not just at next launch. KVO on
/// `NSApp.effectiveAppearance` rather than `viewDidChangeEffectiveAppearance()`
/// because every call site is a SwiftUI `View` (`EngineIcon`, below), not an
/// `NSView` subclass; publishing here lets SwiftUI's own diffing do the redraw.
@MainActor
public final class EngineAppearanceObserver: NSObject, ObservableObject {
    public static let shared = EngineAppearanceObserver()

    @Published public private(set) var isDark: Bool

    private var observation: NSKeyValueObservation?

    override private init() {
        isDark = Self.currentIsDark()
        super.init()
        // The `NSKeyValueObservation` callback predates structured concurrency
        // and is not itself actor-isolated, so the actual read of `NSApp`
        // (a `@MainActor`-isolated global) happens inside a `@MainActor` Task
        // rather than a raw `DispatchQueue.main.async`, which the type system
        // cannot see is main-thread-only.
        observation = NSApp.observe(\.effectiveAppearance, options: [.new]) { [weak self] _, _ in
            Task { @MainActor [weak self] in
                guard let self else { return }
                let dark = Self.currentIsDark()
                if self.isDark != dark { self.isDark = dark }
            }
        }
    }

    private static func currentIsDark() -> Bool {
        NSApp.effectiveAppearance.bestMatch(from: [.aqua, .darkAqua]) == .darkAqua
    }
}

/// The engine mark, wherever an engine is named: toolbar picker, sidebar
/// connection row, New Connection sheet. Brand artwork when we have it, the SF
/// Symbol + brand tint when we do not — the caller never has to know which.
public struct EngineIcon: View {
    private let driverID: String
    private let size: CGFloat
    @ObservedObject private var appearance = EngineAppearanceObserver.shared

    public init(_ driverID: String, size: CGFloat = 16) {
        self.driverID = driverID
        self.size = size
    }

    public var body: some View {
        Group {
            if let logo = EngineStyle.logo(for: driverID, dark: appearance.isDark) {
                Image(nsImage: logo)
                    .resizable()
                    .interpolation(.high)
                    .aspectRatio(contentMode: .fit)
            } else {
                Image(systemName: EngineStyle.symbol(for: driverID))
                    .font(.system(size: size * 0.86))
                    .foregroundStyle(EngineStyle.tint(for: driverID))
            }
        }
        .frame(width: size, height: size)
        .accessibilityLabel(EngineStyle.displayName(for: driverID))
    }
}

/// Catalog node glyphs, kept deliberately disjoint from `EngineStyle.symbol`
/// so a node kind can never be mistaken for an engine.
public enum NodeStyle {
    public static func symbol(forKind kind: String) -> String {
        switch kind.lowercased() {
        case "database": return "cylinder.split.1x2"
        case "schema": return "folder"
        case "table": return "tablecells"
        case "view": return "eye"
        case "collection": return "doc.text"
        case "key", "hash", "string", "list", "set", "zset": return "key"
        case "column", "field": return "number"
        case "index": return "line.3.horizontal.decrease"
        case "function": return "function"
        default: return "circle.dashed"
        }
    }
}
