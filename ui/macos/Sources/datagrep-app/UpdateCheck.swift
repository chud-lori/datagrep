import AppKit
import Foundation

/// Shape of `latest.json` (docs/latest.json in the repo). Extra keys ignored.
struct UpdateManifest: Decodable, Equatable {
    let version: String
    let tag: String
    let releaseURL: URL?

    enum CodingKeys: String, CodingKey {
        case version, tag
        case releaseURL = "release_url"
    }
}

enum UpdatePrefs {
    static let checkOnLaunchKey = "updateCheckOnLaunch"
    static let skippedVersionKey = "updateSkippedVersion"

    /// Defaults to true when the key has never been written.
    static var checkOnLaunch: Bool {
        get { UserDefaults.standard.object(forKey: checkOnLaunchKey) as? Bool ?? true }
        set { UserDefaults.standard.set(newValue, forKey: checkOnLaunchKey) }
    }

    static var skippedVersion: String? {
        get { UserDefaults.standard.string(forKey: skippedVersionKey) }
        set {
            if let v = newValue {
                UserDefaults.standard.set(v, forKey: skippedVersionKey)
            } else {
                UserDefaults.standard.removeObject(forKey: skippedVersionKey)
            }
        }
    }
}

@MainActor
final class UpdateCheck: ObservableObject {
    static let shared = UpdateCheck()

    static let manifestURL = URL(string: "https://chud-lori.github.io/datagrep/latest.json")!

    static let fallbackVersion = "0.4.0"

    @Published private(set) var available: UpdateManifest?

    private var didCheckThisLaunch = false

    private init() {}

    var currentVersion: String {
        (Bundle.main.object(forInfoDictionaryKey: "CFBundleShortVersionString") as? String)
            ?? Self.fallbackVersion
    }

    func checkOnLaunchIfEnabled() {
        guard UpdatePrefs.checkOnLaunch, !didCheckThisLaunch else { return }
        didCheckThisLaunch = true
        fetchManifest { [weak self] manifest in
            guard let self, let manifest else { return } // silence on any failure
            guard Self.isNewer(manifest.version, than: self.currentVersion) else { return }
            if let skipped = UpdatePrefs.skippedVersion,
                Self.normalize(skipped) == Self.normalize(manifest.version)
            {
                return
            }
            self.available = manifest
        }
    }

    func checkNow(completion: @escaping (_ newer: UpdateManifest?, _ failed: Bool) -> Void) {
        fetchManifest { [weak self] manifest in
            guard let self else { return }
            guard let manifest else {
                completion(nil, true)
                return
            }
            if Self.isNewer(manifest.version, than: self.currentVersion) {
                self.available = manifest
                completion(manifest, false)
            } else {
                completion(nil, false)
            }
        }
    }

    /// "Skip this version": remember it, drop the current notice.
    func skip(_ manifest: UpdateManifest) {
        UpdatePrefs.skippedVersion = manifest.version
        if available == manifest { available = nil }
    }

    /// Dismiss for this launch only (the next launch may notify again).
    func dismiss() {
        available = nil
    }

    // MARK: - fetch

    /// One GET, short timeout, ephemeral session (nothing persisted).
    private func fetchManifest(_ completion: @escaping @MainActor (UpdateManifest?) -> Void) {
        let config = URLSessionConfiguration.ephemeral
        config.timeoutIntervalForRequest = 6
        config.timeoutIntervalForResource = 10
        config.httpAdditionalHeaders = [
            "Accept": "application/json",
            "User-Agent": "datagrep/\(currentVersion)",
        ]
        let session = URLSession(configuration: config)
        let task = session.dataTask(with: Self.manifestURL) { data, response, error in
            var manifest: UpdateManifest?
            if error == nil,
                let http = response as? HTTPURLResponse, (200..<300).contains(http.statusCode),
                let data
            {
                manifest = try? JSONDecoder().decode(UpdateManifest.self, from: data)
            }
            let result = manifest
            DispatchQueue.main.async {
                completion(result)
            }
            session.finishTasksAndInvalidate()
        }
        task.resume()
    }

    // MARK: - version comparison

    static func normalize(_ v: String) -> String {
        v.hasPrefix("v") ? String(v.dropFirst()) : v
    }

    static func isNewer(_ a: String, than b: String) -> Bool {
        func parse(_ s: String) -> (UInt64, UInt64, UInt64) {
            let parts = normalize(s).split(separator: ".", maxSplits: 2)
            func num(_ i: Int) -> UInt64 {
                guard i < parts.count else { return 0 }
                let digits = parts[i].prefix { $0.isNumber }
                return UInt64(digits) ?? 0
            }
            return (num(0), num(1), num(2))
        }
        let x = parse(a)
        let y = parse(b)
        if x.0 != y.0 { return x.0 > y.0 }
        if x.1 != y.1 { return x.1 > y.1 }
        return x.2 > y.2
    }
}
