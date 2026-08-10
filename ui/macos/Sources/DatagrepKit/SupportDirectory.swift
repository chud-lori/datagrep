import Foundation

/// Where datagrep keeps everything it owns: `profiles.sqlite`, the editor's
/// `tabs/`, and `history/`.
///
/// Normally `~/Library/Application Support/datagrep`. Set `DATAGREP_CONFIG_DIR`
/// to point all three somewhere else — the CLI has always honoured that
/// variable, and the app not honouring it meant there was no way to run the GUI
/// against anything but the real connections on the machine. Debugging a UI bug
/// then required driving somebody's live database, which is not an acceptable
/// price for a screenshot.
public enum SupportDirectory {
    public static var base: URL {
        if let override = ProcessInfo.processInfo.environment["DATAGREP_CONFIG_DIR"],
            !override.isEmpty
        {
            return URL(fileURLWithPath: (override as NSString).expandingTildeInPath, isDirectory: true)
        }
        return FileManager.default
            .urls(for: .applicationSupportDirectory, in: .userDomainMask)[0]
            .appendingPathComponent("datagrep", isDirectory: true)
    }

    /// The base directory, created if it is not there yet.
    public static func ensured() -> URL {
        let dir = base
        try? FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
        return dir
    }
}
