import AppKit
import SwiftUI

/// The one place that knows what an engine looks like.
public enum EngineStyle {
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
        case "elasticsearch": return "Elasticsearch"
        default: return driverID
        }
    }

    /// True when `SELECT * FROM (<query>) ORDER BY …` is a legal thing to send.
    public static func supportsSubqueryOrderBy(_ driverID: String) -> Bool {
        ["postgres", "mysql", "sqlite"].contains(normalise(driverID))
    }

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

@MainActor
public final class EngineAppearanceObserver: NSObject, ObservableObject {
    public static let shared = EngineAppearanceObserver()

    @Published public private(set) var isDark: Bool

    private var observation: NSKeyValueObservation?

    override private init() {
        isDark = Self.currentIsDark()
        super.init()
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
