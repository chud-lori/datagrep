import AppKit
import Foundation

// datagrep's update check. Same pattern as rusty-requester: a static
// `latest.json` on GitHub Pages is the single source of truth, written by
// scripts/deploy.sh and re-asserted by the release workflow, so the manifest
// can never describe a release that doesn't exist.
//
// The contract, in order of importance:
//
//   1. ONE silent GET of a static JSON file per app launch. No timer, no
//      retry loop, no background schedule — datagrep's design bans polling
//      outright (ci/gates.sh hard-fails on `tokio::time::interval`; the idle
//      budget is ≤2 wakeups/sec) and an update poller on the Swift side would
//      break the same promise by other means. `checkOnLaunchIfEnabled()` is
//      guarded by a per-process flag; calling it twice does nothing.
//
//   2. Never downloads, never installs. The app is ad-hoc signed, not
//      notarized — a binary silently replacing itself is exactly what
//      Gatekeeper exists to prevent. This code compares two version strings
//      and, when the remote one is newer, exposes the manifest so the UI can
//      show a quiet, dismissible notice with a link. The user decides.
//
//   3. Opt-out, honestly described. `UpdatePrefs.checkOnLaunch` defaults to
//      true; the settings wording (UpdateNotice.swift) says plainly what the
//      check sends: nothing but the GET itself. No identifiers, no telemetry,
//      nothing about the user's databases.
//
//   4. Fail silently. Offline, DNS failure, 404, bad JSON — the user must
//      never see an error dialog because a version check didn't work.

/// Shape of https://chud-lori.github.io/datagrep/latest.json (docs/latest.json
/// in the repo). Extra keys (`release_notes_url`, `install_url`) are ignored.
struct UpdateManifest: Decodable, Equatable {
    let version: String
    let tag: String
    let releaseURL: URL?

    enum CodingKeys: String, CodingKey {
        case version, tag
        case releaseURL = "release_url"
    }
}

/// UserDefaults-backed preferences for the update check. Also scriptable:
/// `defaults write com.lori.datagrep updateCheckOnLaunch -bool NO`.
enum UpdatePrefs {
    static let checkOnLaunchKey = "updateCheckOnLaunch"
    static let skippedVersionKey = "updateSkippedVersion"

    /// Defaults to true when the key has never been written.
    static var checkOnLaunch: Bool {
        get { UserDefaults.standard.object(forKey: checkOnLaunchKey) as? Bool ?? true }
        set { UserDefaults.standard.set(newValue, forKey: checkOnLaunchKey) }
    }

    /// A version the user chose "Skip this version" on. Only suppresses the
    /// launch notice for exactly that version — a newer release notifies again.
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

    /// Used only when the process has no bundle Info.plist (bare `swift build`
    /// binary). Inside datagrep.app, CFBundleShortVersionString wins.
    /// scripts/deploy.sh bumps this literal — keep it on its own line.
    static let fallbackVersion = "0.1.0"

    /// Non-nil when the manifest advertises a strictly newer version than the
    /// one running (and the user hasn't skipped it). Set at most once per
    /// launch, on the main actor — the notice view observes this.
    @Published private(set) var available: UpdateManifest?

    private var didCheckThisLaunch = false

    private init() {}

    var currentVersion: String {
        (Bundle.main.object(forInfoDictionaryKey: "CFBundleShortVersionString") as? String)
            ?? Self.fallbackVersion
    }

    /// The once-per-launch check. Second and later calls are no-ops, so it is
    /// safe to trigger from a view's `onAppear` even if that view re-appears.
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

    /// Explicit user-initiated check (for a "Check for Updates…" menu item).
    /// Ignores the skip list and the once-per-launch guard — the user asked.
    /// Reports the outcome instead of failing silently, because this time the
    /// user is watching.
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

    /// One GET, 6-second timeout, ephemeral session (no cookies, no cache,
    /// nothing persisted). Completion runs on the main actor; `nil` means any
    /// failure whatsoever.
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

    /// True when `a` is strictly newer than `b`. Both accept an optional `v`
    /// prefix; unparseable components count as 0 and pre-release suffixes on
    /// the patch (`0.2.0-rc1`) are stripped. Good enough for a notice — this
    /// never gates anything security-relevant.
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
