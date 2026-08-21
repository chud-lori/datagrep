import SwiftUI

/// Keeps everything but the window frame off the startup critical path.
///
/// Startup runs in three turns of the run loop:
///
///   1. `applicationDidFinishLaunching` builds and paints the chrome.
///      `MEASURE cold start exec -> window` is taken here.
///   2. `AppModel.boot()` opens `DatagrepCore` and restores the session, with
///      the window already up.
///   3. `contentReady` flips, and the two AppKit bridges — the `NSTextView`
///      editor and the `NSTableView` grid — are instantiated into panes that
///      were already drawn at the right size.
///
/// Building those two AppKit view hierarchies is the single most expensive
/// thing SwiftUI does while the window loads, so deferring them (3 out of 1)
/// is what moves the cold-start number. The panes keep their size when content
/// arrives, so nothing reflows — and no spinner: a `ProgressView()` here would
/// animate forever on a slow disk and fail P19 on its own.
@MainActor
final class StartupStage: ObservableObject {
    static let shared = StartupStage()

    /// False until the window has been painted at least once.
    @Published private(set) var contentReady: Bool

    private init() {
        // Measurement escape hatch: `--no-deferred-content` builds the whole
        // window on the critical path, so before/after cold-start numbers come
        // out of one binary.
        contentReady = ProcessInfo.processInfo.arguments.contains("--no-deferred-content")
    }

    func markContentReady() {
        guard !contentReady else { return }
        contentReady = true
    }
}
