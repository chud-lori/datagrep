import AppKit
import Combine
import DatagrepKit
import SwiftUI

/// Entry point.
///
/// AppKit owns the lifecycle (`NSApplication` + a hand-built main menu) and
/// SwiftUI owns every pixel below the titlebar, via one `NSHostingController`
/// whose root view is `Workbench`. Doing it this way rather than with the
/// SwiftUI `App` protocol buys two things this app actually needs: a real main
/// menu with working ⌘Z/⌘X/⌘C/⌘V for the `NSTextView`, and direct access to the
/// `NSWindow` so a production connection can tint the titlebar itself.
@main
enum DatagrepMain {
    @MainActor
    static func main() {
        // First line of Swift that runs. Everything before it — dyld, the Swift
        // and SwiftUI runtime init, framework mapping — is the floor no amount
        // of application code can move, so it is measured separately.
        Startup.mark("pre-main (dyld + runtime)")
        let app = NSApplication.shared
        Startup.mark("NSApplication.shared")
        let delegate = AppDelegate()
        Startup.mark("AppDelegate() — includes AppModel()")
        app.delegate = delegate
        app.setActivationPolicy(.regular)
        app.run()
    }
}

@MainActor
final class AppDelegate: NSObject, NSApplicationDelegate, NSWindowDelegate {
    let model = AppModel()
    private var window: NSWindow!
    private var cancellables: Set<AnyCancellable> = []
    private var autopilot: Autopilot?
    private var sidebarMenuItem: NSMenuItem!
    private var inspectorMenuItem: NSMenuItem!

    /// Design §5.1 applied to the GUI: **the window is on screen before anything
    /// is loaded.** The CLI honours the rule by never opening the profile DB
    /// until a subcommand needs it and cold-starts in ~17 ms; the GUI's
    /// equivalent is that nothing which touches the engine, the profile store or
    /// the disk may sit between `exec` and first paint.
    ///
    /// So this method does exactly three things on the critical path — build the
    /// menu, build the window, paint it — and everything else (`model.boot()`,
    /// which creates `DatagrepCore`, opens `profiles.sqlite`, lists the profiles
    /// and restores the editor session) runs in `finishBooting()` on the next
    /// run-loop turn, after the user is already looking at the window.
    ///
    /// The `MEASURE cold start` line is emitted from a forced first display, not
    /// from an `async` hop that would report a window that has not drawn yet.
    func applicationDidFinishLaunching(_ notification: Notification) {
        buildMainMenu()
        Startup.mark("buildMainMenu")

        let host = NSHostingController(rootView: Workbench(model: model))
        // Empty sizing options: otherwise the hosting controller pushes the
        // SwiftUI ideal size onto the window and the frame set below is
        // silently overridden (it opened at 872×572 instead of 1180×760).
        host.sizingOptions = []
        Startup.mark("NSHostingController(Workbench)")

        window = NSWindow(contentViewController: host)
        Startup.mark("NSWindow(contentViewController:) — SwiftUI loadView")
        window.setContentSize(NSSize(width: 1180, height: 760))
        window.contentMinSize = NSSize(width: 900, height: 560)
        window.title = "datagrep"
        Startup.mark("NSWindow size + title")
        window.styleMask.insert(.fullSizeContentView)
        window.titlebarAppearsTransparent = true
        Startup.mark("fullSizeContentView + transparent titlebar")
        window.toolbarStyle = .unified
        window.tabbingMode = .disallowed
        window.delegate = self
        Startup.mark("toolbar style + delegate")
        window.setFrameAutosaveName("datagrep.main")
        window.center()
        Startup.mark("frame autosave + center")

        window.makeKeyAndOrderFront(nil)
        NSApp.activate(ignoringOtherApps: true)
        Startup.mark("makeKeyAndOrderFront")

        // Force the first pass through SwiftUI layout + draw NOW, so the number
        // printed below is a window the user could actually see and not merely
        // one that has been ordered in. Without this the measurement moves the
        // cost off the clock instead of off the critical path.
        window.contentView?.displayIfNeeded()
        Startup.mark("first paint")

        let ms = Startup.millisSinceProcessStart()
        FileHandle.standardError.write(
            Data(String(format: "MEASURE cold start exec -> window: %.0f ms\n", ms).utf8))

        // Everything below here is off the critical path.
        DispatchQueue.main.async { [weak self] in self?.finishBooting() }
    }

    /// Runs on the first run-loop turn after the window is up. Ordered by how
    /// soon the user can notice it is missing.
    private func finishBooting() {
        model.boot()
        Startup.mark("model.boot() — core + profiles + editor text")

        // Turn 3: the NSTextView and the NSTableView get built into panes that
        // are already on screen at their final size. `boot()` has already forced
        // the editor's `loadView` via `setText`, so this is mostly SwiftUI
        // adopting an existing controller.
        StartupStage.shared.markContentReady()
        Startup.mark("editor + grid attached")

        // The window chrome follows the connection, not a timer: this fires only
        // when one of these three values actually changes.
        model.$activeProfile
            .combineLatest(model.$activeEnv, model.$prodMarked)
            .map { profile, env, marked in env == "prod" || marked.contains(profile) }
            .removeDuplicates()
            .sink { [weak self] isProd in self?.applyChromeTint(isProd) }
            .store(in: &cancellables)

        model.$sidebarVisible
            .removeDuplicates()
            .sink { [weak self] visible in
                self?.sidebarMenuItem.title = visible ? "Hide Sidebar" : "Show Sidebar"
            }
            .store(in: &cancellables)

        model.$showDetail
            .removeDuplicates()
            .sink { [weak self] visible in
                self?.inspectorMenuItem.title = visible ? "Hide Inspector" : "Show Inspector"
            }
            .store(in: &cancellables)

        model.editor.focus()
        Startup.mark("combine sinks + editor focus")

        let ms = Startup.millisSinceProcessStart()
        FileHandle.standardError.write(
            Data(String(format: "MEASURE cold start exec -> loaded: %.0f ms\n", ms).utf8))
        Startup.dumpTrace()

        autopilot = Autopilot.fromArguments(model: model)
        autopilot?.start()

        installLaunchHarnesses()
    }

    /// The screenshot / diagnostic launch flags. They are all timers hung off an
    /// already-visible window, so they belong here rather than on the critical
    /// path — parsing `ProcessInfo.arguments` three times before first paint was
    /// free, but scheduling from here keeps the launch method to one job.
    private func installLaunchHarnesses() {
        // `--screenshot <path> [delay]`: the app renders ITSELF to a PNG.
        // `screencapture` needs Screen Recording consent, which a headless
        // agent session does not have, and "I could not look at it" is not an
        // acceptable answer for a UI change.
        let args = ProcessInfo.processInfo.arguments
        if let i = args.firstIndex(of: "--screenshot"), i + 1 < args.count {
            let path = args[i + 1]
            let delay = (i + 2 < args.count ? Double(args[i + 2]) : nil) ?? 2.0
            DispatchQueue.main.asyncAfter(deadline: .now() + delay) { [weak self] in
                self?.writeScreenshot(to: path)
                if args.contains("--quit-after-shot") {
                    DispatchQueue.main.asyncAfter(deadline: .now() + 0.3) { NSApp.terminate(nil) }
                }
            }
        }

        // `--theme-flip-shot <light.png> <dark.png>`: forces aqua, snapshots,
        // then flips NSApp.appearance to darkAqua and snapshots again. Setting
        // `NSApp.appearance` changes `effectiveAppearance`, which fires the
        // same KVO that a System Settings appearance change does — so the dark
        // screenshot proves the engine artwork re-resolves at runtime, not
        // just at launch.
        if let i = args.firstIndex(of: "--theme-flip-shot"), i + 2 < args.count {
            let lightPath = args[i + 1]
            let darkPath = args[i + 2]
            NSApp.appearance = NSAppearance(named: .aqua)
            DispatchQueue.main.asyncAfter(deadline: .now() + 2.5) { [weak self] in
                self?.writeScreenshot(to: lightPath)
                NSApp.appearance = NSAppearance(named: .darkAqua)
                DispatchQueue.main.asyncAfter(deadline: .now() + 1.2) {
                    self?.writeScreenshot(to: darkPath)
                    DispatchQueue.main.asyncAfter(deadline: .now() + 0.3) {
                        NSApp.terminate(nil)
                    }
                }
            }
        }

        if ProcessInfo.processInfo.arguments.contains("--diag") {
            DispatchQueue.main.asyncAfter(deadline: .now() + 1.5) { [weak self] in
                guard let w = self?.window else { return }
                let items = w.toolbar?.items.map(\.itemIdentifier.rawValue) ?? []
                FileHandle.standardError.write(
                    Data(
                        """
                        DIAG window \(w.isVisible ? "visible" : "hidden") frame=\(w.frame) \
                        title=\(w.title) subtitle=\(w.subtitle)
                        DIAG toolbar items (\(items.count)): \(items.joined(separator: ", "))
                        DIAG contentView=\(String(describing: type(of: w.contentView!))) \
                        size=\(w.contentView!.frame.size)

                        """.utf8))
            }
        }
    }

    func applicationShouldTerminateAfterLastWindowClosed(_ sender: NSApplication) -> Bool { true }

    /// §3.8 layer 1. `titlebarAppearsTransparent` means the window background
    /// *is* the titlebar background, so one property paints the whole chrome.
    private func applyChromeTint(_ isProd: Bool) {
        guard let window else { return }
        window.backgroundColor = isProd
            ? NSColor.systemRed.blended(withFraction: 0.72, of: .windowBackgroundColor)
                ?? .windowBackgroundColor
            : .windowBackgroundColor
        window.title = isProd ? "datagrep — PRODUCTION" : "datagrep"
    }

    /// Renders the window's own theme frame (titlebar + toolbar + content) into
    /// a PNG. Not a screen grab — the app draws itself, so no capture consent
    /// is involved and the result is exactly the pixels this app produced.
    private func writeScreenshot(to path: String) {
        guard let content = window.contentView else { return }
        let target = content.superview ?? content
        guard let rep = target.bitmapImageRepForCachingDisplay(in: target.bounds) else { return }
        target.cacheDisplay(in: target.bounds, to: rep)
        guard let data = rep.representation(using: .png, properties: [:]) else { return }
        do {
            try data.write(to: URL(fileURLWithPath: path))
            FileHandle.standardError.write(
                Data("SHOT wrote \(path) (\(Int(target.bounds.width))×\(Int(target.bounds.height)))\n".utf8))
        } catch {
            FileHandle.standardError.write(Data("SHOT failed: \(error)\n".utf8))
        }
    }

    // MARK: - actions wired to the main menu

    @objc func runStatement(_ sender: Any?) { model.runStatementUnderCaret() }
    @objc func cancelQuery(_ sender: Any?) { model.cancel() }
    @objc func newConnection(_ sender: Any?) { model.showNewConnection = true }
    @objc func focusEditor(_ sender: Any?) { model.editor.focus() }
    @objc func reportFootprint(_ sender: Any?) { model.reportFootprint() }
    @objc func runScrollBench(_ sender: Any?) { model.runScrollBench() }
    @objc func toggleDetail(_ sender: Any?) {
        withAnimation(.smooth(duration: 0.22)) { model.showDetail.toggle() }
    }

    @objc func toggleSidebar(_ sender: Any?) {
        withAnimation(.smooth(duration: 0.25)) { model.sidebarVisible.toggle() }
    }

    // MARK: - main menu

    /// Hand-built because there is no Xcode here and therefore no MainMenu.nib.
    /// The Edit menu is not decoration: without it the `NSTextView` has no
    /// undo, no copy and no paste.
    private func buildMainMenu() {
        let main = NSMenu()

        let appItem = NSMenuItem()
        let appMenu = NSMenu()
        appMenu.addItem(
            withTitle: "About datagrep", action: #selector(NSApplication.orderFrontStandardAboutPanel(_:)),
            keyEquivalent: "")
        appMenu.addItem(.separator())
        appMenu.addItem(
            withTitle: "Hide datagrep", action: #selector(NSApplication.hide(_:)), keyEquivalent: "h")
        appMenu.addItem(.separator())
        appMenu.addItem(
            withTitle: "Quit datagrep", action: #selector(NSApplication.terminate(_:)), keyEquivalent: "q")
        appItem.submenu = appMenu
        main.addItem(appItem)

        let fileItem = NSMenuItem()
        let fileMenu = NSMenu(title: "File")
        add(fileMenu, "New Connection…", #selector(newConnection(_:)), "n")
        fileMenu.addItem(.separator())
        add(fileMenu, "Close Window", #selector(NSWindow.performClose(_:)), "w")
        fileItem.submenu = fileMenu
        main.addItem(fileItem)

        let editItem = NSMenuItem()
        let editMenu = NSMenu(title: "Edit")
        add(editMenu, "Undo", Selector(("undo:")), "z")
        add(editMenu, "Redo", Selector(("redo:")), "Z")
        editMenu.addItem(.separator())
        add(editMenu, "Cut", #selector(NSText.cut(_:)), "x")
        add(editMenu, "Copy", #selector(NSText.copy(_:)), "c")
        add(editMenu, "Paste", #selector(NSText.paste(_:)), "v")
        add(editMenu, "Select All", #selector(NSText.selectAll(_:)), "a")
        editItem.submenu = editMenu
        main.addItem(editItem)

        // View: the third of the three routes back to a collapsed sidebar
        // (toolbar button, ⌃⌘S, and this). The title flips with the state so
        // the menu always says what the click will do.
        let viewItem = NSMenuItem()
        let viewMenu = NSMenu(title: "View")
        viewMenu.autoenablesItems = false
        sidebarMenuItem = NSMenuItem(
            title: "Hide Sidebar", action: #selector(toggleSidebar(_:)), keyEquivalent: "s")
        sidebarMenuItem.keyEquivalentModifierMask = [.control, .command]
        sidebarMenuItem.target = self
        viewMenu.addItem(sidebarMenuItem)
        inspectorMenuItem = NSMenuItem(
            title: "Show Inspector", action: #selector(toggleDetail(_:)), keyEquivalent: "i")
        inspectorMenuItem.target = self
        viewMenu.addItem(inspectorMenuItem)
        viewItem.submenu = viewMenu
        main.addItem(viewItem)

        let queryItem = NSMenuItem()
        let queryMenu = NSMenu(title: "Query")
        add(queryMenu, "Run Statement Under Caret", #selector(runStatement(_:)), "\r")
        add(queryMenu, "Cancel", #selector(cancelQuery(_:)), ".")
        queryMenu.addItem(.separator())
        add(queryMenu, "Focus Editor", #selector(focusEditor(_:)), "l")
        add(queryMenu, "Toggle Cell Detail", #selector(toggleDetail(_:)), "i")
        queryMenu.addItem(.separator())
        add(queryMenu, "Report Footprint", #selector(reportFootprint(_:)), "m")
        add(queryMenu, "Run Scroll Benchmark", #selector(runScrollBench(_:)), "B")
        queryItem.submenu = queryMenu
        main.addItem(queryItem)

        let windowItem = NSMenuItem()
        let windowMenu = NSMenu(title: "Window")
        add(windowMenu, "Minimize", #selector(NSWindow.performMiniaturize(_:)), "m")
        add(windowMenu, "Zoom", #selector(NSWindow.performZoom(_:)), "")
        windowItem.submenu = windowMenu
        main.addItem(windowItem)

        NSApp.mainMenu = main
        NSApp.windowsMenu = windowMenu
    }

    private func add(_ menu: NSMenu, _ title: String, _ action: Selector, _ key: String) {
        let item = NSMenuItem(title: title, action: action, keyEquivalent: key)
        if key.range(of: "[A-Z]", options: .regularExpression) != nil && key.count == 1 {
            item.keyEquivalentModifierMask = [.command, .shift]
        }
        menu.addItem(item)
    }
}

// MARK: - startup timing

enum Startup {
    /// Real exec -> now, read from the kernel, so it includes dyld and the
    /// Swift/SwiftUI runtime init that an in-process stopwatch cannot see.
    static func millisSinceProcessStart() -> Double {
        var info = kinfo_proc()
        var size = MemoryLayout<kinfo_proc>.stride
        var mib: [Int32] = [CTL_KERN, KERN_PROC, KERN_PROC_PID, getpid()]
        let rc = sysctl(&mib, 4, &info, &size, nil, 0)
        guard rc == 0 else { return 0 }
        let start =
            Double(info.kp_proc.p_starttime.tv_sec)
            + Double(info.kp_proc.p_starttime.tv_usec) / 1e6
        return (Date().timeIntervalSince1970 - start) * 1000
    }

    // MARK: - phase trace
    //
    // `--trace-startup` (or DATAGREP_TRACE_STARTUP=1) prints one line per phase
    // with both the elapsed-since-exec clock and the cost of that phase alone.
    // It stays in the binary permanently: the cold-start budget (design §5 P1,
    // ≤250 ms target / >600 ms FAIL) is a number that regresses silently, and a
    // breakdown is the only way to see WHICH phase moved.

    nonisolated(unsafe) private static var lastMark: Double = 0
    nonisolated(unsafe) private static var marks: [(String, Double, Double)] = []

    static let tracing: Bool = {
        ProcessInfo.processInfo.arguments.contains("--trace-startup")
            || ProcessInfo.processInfo.environment["DATAGREP_TRACE_STARTUP"] == "1"
    }()

    /// Records the end of a startup phase. Cheap enough (one sysctl) to leave
    /// unconditional, but the sysctl is skipped entirely when not tracing.
    static func mark(_ label: String) {
        guard tracing else { return }
        let now = millisSinceProcessStart()
        marks.append((label, now, now - lastMark))
        lastMark = now
    }

    /// Times one phase and marks it. The return value is passed through so a
    /// phase that produces a value can be wrapped without restructuring.
    @discardableResult
    static func phase<T>(_ label: String, _ body: () throws -> T) rethrows -> T {
        guard tracing else { return try body() }
        let out = try body()
        mark(label)
        return out
    }

    static func dumpTrace() {
        guard tracing, !marks.isEmpty else { return }
        var out = "MEASURE startup breakdown (ms since exec / phase cost)\n"
        for (label, at, cost) in marks {
            out += String(format: "  %7.1f  +%7.1f  %@\n", at, cost, label)
        }
        FileHandle.standardError.write(Data(out.utf8))
        marks.removeAll()
    }
}

// MARK: - autopilot (measurement harness)

/// Drives the app from the command line so the design §5 numbers can be
/// produced repeatably instead of by hand-scrolling and guessing.
///
///   datagrep --add-profile bench=sqlite:///tmp/bench.db \
///       --sql "SELECT * FROM big" --bench --quit-after-bench
///
/// This is the ONLY thing in the app that polls, it only exists while a launch
/// flag asked for it, and it stops the moment the run finishes.
@MainActor
final class Autopilot {
    private weak var model: AppModel?
    private let profile: (name: String, url: String)?
    private let sql: String?
    private let bench: Bool
    private var settleTicks = 0

    init(model: AppModel?, profile: (name: String, url: String)?, sql: String?, bench: Bool) {
        self.model = model
        self.profile = profile
        self.sql = sql
        self.bench = bench
    }

    static func fromArguments(model: AppModel?) -> Autopilot? {
        let args = ProcessInfo.processInfo.arguments
        func value(_ flag: String) -> String? {
            guard let i = args.firstIndex(of: flag), i + 1 < args.count else { return nil }
            return args[i + 1]
        }
        var profile: (String, String)?
        if let spec = value("--add-profile"), let eq = spec.firstIndex(of: "=") {
            profile = (String(spec[spec.startIndex..<eq]), String(spec[spec.index(after: eq)...]))
        }
        let sql = value("--sql")
        let bench = args.contains("--bench")
        guard profile != nil || sql != nil || bench else { return nil }
        return Autopilot(model: model, profile: profile, sql: sql, bench: bench)
    }

    func start() {
        guard let model else { return }
        if let profile {
            if model.roots.contains(where: { $0.name == profile.name }) {
                model.selectProfile(profile.name)
            } else {
                model.addProfile(name: profile.name, url: profile.url)
            }
        }
        DispatchQueue.main.asyncAfter(deadline: .now() + 1.0) { [weak self] in self?.afterConnect() }
    }

    private func afterConnect() {
        guard let model else { return }
        if let profile, model.activeProfile != profile.name,
            model.roots.contains(where: { $0.name == profile.name })
        {
            model.selectProfile(profile.name)
        }
        guard let sql else { return finishOrBench() }
        FileHandle.standardError.write(Data("AUTOPILOT running: \(sql)\n".utf8))
        model.run(sql: sql, directives: SQLBlocks.directives(in: sql))
        waitForRows()
    }

    private func waitForRows() {
        DispatchQueue.main.asyncAfter(deadline: .now() + 0.5) { [weak self] in
            guard let self, let model = self.model else { return }
            self.settleTicks += 1
            let settled = !model.isRunning && model.rowsLoaded > 0
            if settled || self.settleTicks > 240 {
                FileHandle.standardError.write(
                    Data(
                        "AUTOPILOT state=\(model.state?.rawValue ?? "nil") rows=\(model.rowsLoaded) after \(Double(self.settleTicks) * 0.5)s\n"
                            .utf8))
                model.reportFootprint()
                self.finishOrBench()
            } else {
                self.waitForRows()
            }
        }
    }

    private func finishOrBench() {
        guard bench, let model else { return }
        DispatchQueue.main.asyncAfter(deadline: .now() + 0.3) { model.runScrollBench() }
    }
}
