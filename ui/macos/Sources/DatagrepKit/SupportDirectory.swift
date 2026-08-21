import Foundation

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
