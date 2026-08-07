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
    /// Returns nil for an engine with no artwork, and for a build whose
    /// resource bundle did not make it into the .app — callers then fall back
    /// to `symbol(for:)` + `tint(for:)`, so a missing asset degrades to a
    /// perfectly good SF Symbol instead of a blank space or a crash.
    public static func logo(for driverID: String) -> NSImage? {
        let key = normalise(driverID)
        if let cached = logoCache[key] { return cached }
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
        if let file, let img = moduleBundle?.image(forResource: file) {
            // Full-colour brand marks, NOT template glyphs: tinting them to
            // labelColor would erase the thing that makes them recognisable.
            img.isTemplate = false
            img.accessibilityDescription = displayName(for: key)
            image = img
        }
        logoCache[key] = image
        return image
    }

    private nonisolated(unsafe) static var logoCache: [String: NSImage?] = [:]

    /// `Bundle.module` traps when the resource bundle is missing. Reaching it
    /// through a failable lookup instead means a mis-assembled .app degrades to
    /// SF Symbols rather than dying on the first sidebar row.
    private static let moduleBundle: Bundle? = {
        let name = "dbx-macos_DbxKit"
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
    private static func normalise(_ id: String) -> String {
        let s = id.lowercased()
        if s.hasPrefix("postgres") || s == "pg" || s == "psql" { return "postgres" }
        if s.hasPrefix("mysql") || s.hasPrefix("maria") { return "mysql" }
        if s.hasPrefix("sqlite") { return "sqlite" }
        if s.hasPrefix("redis") || s.hasPrefix("rediss") { return "redis" }
        if s.hasPrefix("mongo") { return "mongo" }
        return s
    }
}

/// The engine mark, wherever an engine is named: toolbar picker, sidebar
/// connection row, New Connection sheet. Brand artwork when we have it, the SF
/// Symbol + brand tint when we do not — the caller never has to know which.
public struct EngineIcon: View {
    private let driverID: String
    private let size: CGFloat

    public init(_ driverID: String, size: CGFloat = 16) {
        self.driverID = driverID
        self.size = size
    }

    public var body: some View {
        Group {
            if let logo = EngineStyle.logo(for: driverID) {
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
