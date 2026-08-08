import SwiftUI

/// Design §5.1, applied to the window instead of to a subcommand.
///
/// The CLI's rule is that nothing connects, opens the profile DB or initialises
/// TLS until a subcommand actually needs it — which is why it cold-starts in
/// ~17 ms. The GUI cannot literally do nothing (it has to show a window), but it
/// can hold the same line: **only the frame is on the critical path.**
///
/// Startup therefore runs in three turns of the run loop:
///
///   1. `applicationDidFinishLaunching` builds the menu, the window and the
///      chrome — sidebar, toolbar, status bar, empty panes — and paints it.
///      `MEASURE cold start exec -> window` is taken here.
///   2. `AppModel.boot()` opens `DatagrepCore`, lists profiles and restores the
///      editor session. All of it off the clock, with the window already up.
///   3. `contentReady` flips, and the two AppKit bridges — the `NSTextView`
///      editor and the `NSTableView` grid — are instantiated into panes that
///      were already drawn at the right size.
///
/// Splitting 3 out of 1 is what actually moves the number: building both AppKit
/// view hierarchies is the single most expensive thing SwiftUI does while
/// `NSWindow(contentViewController:)` loads the hosting controller's view.
///
/// The panes do not change size when the content arrives — `Chrome.pane` draws
/// the same rounded plane either way — so nothing reflows and there is no
/// spinner, only a pane that is briefly empty. (A `ProgressView()` here would
/// animate forever on a slow disk and fail P19 on its own, exactly as in the
/// status bar.)
@MainActor
final class StartupStage: ObservableObject {
    static let shared = StartupStage()

    /// False until the window has been painted at least once. Everything gated
    /// on it — the toolbar controls, the inspector column, the editor and the
    /// grid — is chrome the user cannot act on during the first frame anyway.
    @Published private(set) var contentReady: Bool

    private init() {
        // Measurement escape hatch: `--no-deferred-content` builds the whole
        // window on the critical path, the way the app did before this change,
        // so the before/after cold-start numbers come out of one binary instead
        // of two builds that might differ in other ways.
        contentReady = ProcessInfo.processInfo.arguments.contains("--no-deferred-content")
    }

    func markContentReady() {
        guard !contentReady else { return }
        contentReady = true
    }
}
