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

    /// The lazy-startup rule applied to the GUI: **the window is on screen
    /// before anything is loaded.** The CLI honours it by never opening the
    /// profile DB until a subcommand needs it and cold-starts in ~17 ms; the
    /// GUI's equivalent is that nothing which touches the engine, the profile
    /// store or the disk may sit between `exec` and first paint.
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
        // Before anything is built: this one prints and leaves, so there is no
        // window to put it in. See `MutationProbe`.
        if MutationProbe.runIfRequested() {
            NSApp.terminate(nil)
            return
        }
        // Before the window exists, so the first frame is already in the right
        // appearance and nothing flashes. The screenshot paths further down
        // assign `NSApp.appearance` themselves and deliberately run after this,
        // so they still win for the shot they are taking.
        AppearanceMode.apply()
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
        // `--window-size 960x600` opens at a chosen size instead of 1180×760.
        // A measurement affordance, in the same family as `--screenshot`: the
        // status bar has to degrade gracefully at widths this machine's screen
        // cannot be dragged to on demand, and "I could not look at it" is not
        // an answer. It relaxes contentMinSize only as far as it has to.
        let size = Self.requestedWindowSize() ?? NSSize(width: 1180, height: 760)
        window.setContentSize(size)
        // Sidebar-min (190) + detail-min (380, set on the SwiftUI detail) + a
        // little slack. The window cannot be dragged narrower than the sidebar
        // and a usable detail need TOGETHER, so the sidebar never clips; above
        // this, narrowing shrinks the detail first and the grid scrolls.
        window.contentMinSize = NSSize(
            width: min(600, size.width), height: min(480, size.height))
        window.title = "datagrep"
        Startup.mark("NSWindow size + title")
        window.styleMask.insert(.fullSizeContentView)
        window.titlebarAppearsTransparent = true
        Startup.mark("fullSizeContentView + transparent titlebar")
        window.toolbarStyle = .unified
        window.tabbingMode = .disallowed
        window.delegate = self
        Startup.mark("toolbar style + delegate")
        if let size = Self.requestedWindowSize() {
            // No autosave name under `--window-size`: a measurement run must not
            // write a test frame into the size the user's real window reopens at.
            window.setContentSize(size)
        } else {
            window.setFrameAutosaveName("datagrep.main")
            // A frame restored from a previous session (or an older build with a
            // smaller minimum) can come back BELOW contentMinSize, and AppKit does
            // not re-clamp a restored frame — which let the window open narrow
            // enough to push the sidebar off its own leading edge. Clamp it back
            // up to the content minimum so the sidebar is never clipped.
            let minC = window.contentMinSize
            let cur = window.contentRect(forFrameRect: window.frame).size
            if cur.width < minC.width || cur.height < minC.height {
                window.setContentSize(
                    NSSize(width: max(cur.width, minC.width), height: max(cur.height, minC.height)))
            }
        }
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

        // Everything below here is off the critical path, in two more turns of
        // the run loop so neither of them blocks the other: chrome, then engine.
        DispatchQueue.main.async { [weak self] in self?.finishChrome() }
    }

    /// Turn 2: the chrome the first frame did without — toolbar controls, the
    /// inspector column, the editor's `NSTextView` and the grid's `NSTableView`.
    /// Nothing here can move the window's layout, so it fills in rather than
    /// reflows.
    ///
    /// It runs BEFORE `model.boot()` on purpose: the controls are what makes the
    /// window look finished, and the connection list arriving a frame later is
    /// the part a user reads as "loading", not the part they read as "broken".
    private func finishChrome() {
        StartupStage.shared.markContentReady()
        // Flipping the flag only invalidates the view; forcing the redraw here
        // keeps the cost inside this run-loop turn instead of smearing it into
        // whichever turn happens to draw next.
        window.contentView?.displayIfNeeded()
        Startup.mark("toolbar + inspector + editor + grid attached")

        DispatchQueue.main.async { [weak self] in self?.finishBooting() }
    }

    /// Turn 3: the engine. `DatagrepCore` (tokio runtime), `profiles.sqlite`,
    /// the profile list and the editor's session restore — the only things here
    /// that touch a socket, a database or the disk, and none of them are between
    /// `exec` and first paint any more.
    private func finishBooting() {
        model.boot()
        Startup.mark("model.boot() — core + profiles + editor session")

        // The window chrome follows the connection's colour, not a timer, and
        // only fires when the colour actually changes. Colour is the user's own
        // marker — datagrep does not decide which connections are dangerous.
        model.$activeProfile
            .combineLatest(model.$profilesByName)
            .map { profile, byName in byName[profile]?.color }
            .removeDuplicates()
            .sink { [weak self] color in self?.applyChromeTint(color) }
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

    /// `--window-size 960x600`, or nil for the default frame.
    private static func requestedWindowSize() -> NSSize? {
        let args = ProcessInfo.processInfo.arguments
        guard let i = args.firstIndex(of: "--window-size"), i + 1 < args.count else { return nil }
        let parts = args[i + 1].lowercased().split(separator: "x")
        guard parts.count == 2, let w = Double(parts[0]), let h = Double(parts[1]),
            w > 200, h > 150
        else { return nil }
        return NSSize(width: w, height: h)
    }

    /// Layer 1 of the production guardrail. `titlebarAppearsTransparent` means
    /// the window background *is* the titlebar background, so one property
    /// paints the whole chrome.
    private func applyChromeTint(_ colorName: String?) {
        guard let window else { return }
        // Blended well down: this has to be unmistakable at a glance without
        // making the window unreadable to work in all day.
        window.backgroundColor =
            colorName
            .flatMap(ConnectionColor.nsColor)
            .flatMap { $0.blended(withFraction: 0.78, of: .windowBackgroundColor) }
            ?? .windowBackgroundColor
        // The title says which connection, not which tier — the colour is the
        // user's own marker and only they know what it stands for.
        window.title =
            colorName == nil ? "datagrep" : "datagrep — \(model.activeProfile)"
    }

    /// Writes a PNG of the app's own window for headless verification.
    ///
    /// Primary path is `CGWindowListCreateImage` of THIS window — the real
    /// composited pixels from the window server, i.e. exactly what is on screen.
    /// That matters because the obvious in-process alternative, `cacheDisplay`,
    /// is BLIND to the SwiftUI-hosted results grid: it copies layer contents and
    /// the scroll view's document subtree never shows up, so a cacheDisplay shot
    /// of a populated grid comes back blank. `cacheDisplay` is kept only as a
    /// fallback for the (rare) case the window-server capture returns nil.
    private func writeScreenshot(to path: String) {
        if let w = window,
            let cg = CGWindowListCreateImage(
                .null, .optionIncludingWindow, CGWindowID(w.windowNumber),
                [.boundsIgnoreFraming, .bestResolution])
        {
            let rep = NSBitmapImageRep(cgImage: cg)
            if let data = rep.representation(using: .png, properties: [:]) {
                try? data.write(to: URL(fileURLWithPath: path))
                FileHandle.standardError.write(
                    Data("SHOT wrote \(path) (\(cg.width)×\(cg.height))\n".utf8))
                return
            }
        }
        // Fallback only: this path cannot capture the results grid.
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

    /// Kept so the checkmark can be moved when the choice changes.
    private var appearanceMenuItems: [NSMenuItem] = []

    @objc func setAppearanceMode(_ sender: NSMenuItem) {
        guard let raw = sender.representedObject as? String,
            let mode = AppearanceMode(rawValue: raw)
        else { return }
        AppearanceMode.current = mode
        AppearanceMode.apply(mode)
        for item in appearanceMenuItems {
            item.state = (item.representedObject as? String) == raw ? .on : .off
        }
    }

    @objc func toggleSidebar(_ sender: Any?) {
        withAnimation(.smooth(duration: 0.25)) { model.sidebarVisible.toggle() }
    }

    /// ⌘Y. In the menu as well as the toolbar because the toolbar controls are
    /// deferred past first paint, and a shortcut that only works once the
    /// chrome has caught up is a shortcut people stop trusting.
    @objc func showQueryHistory(_ sender: Any?) { model.history.isPresented = true }

    /// The user asked, so this one reports its outcome instead of failing
    /// silently the way the launch check does.
    @objc func checkForUpdates(_ sender: Any?) {
        UpdateCheck.shared.checkNow { newer, failed in
            let alert = NSAlert()
            if failed {
                alert.messageText = "Could not check for updates"
                alert.informativeText =
                    "datagrep could not reach the release manifest. You are running \(UpdateCheck.shared.currentVersion)."
            } else if let newer {
                alert.messageText = "datagrep \(UpdateCheck.normalize(newer.version)) is available"
                alert.informativeText =
                    "You are running \(UpdateCheck.shared.currentVersion). Updates are never downloaded or installed automatically — the notice at the bottom of the window links to the release."
            } else {
                alert.messageText = "datagrep is up to date"
                alert.informativeText = "You are running \(UpdateCheck.shared.currentVersion)."
            }
            alert.runModal()
        }
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
        add(appMenu, "Check for Updates…", #selector(checkForUpdates(_:)), "")
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

        // Appearance lives in View rather than behind a Preferences window:
        // there is no Preferences window yet, and inventing one to hold a
        // single three-way choice would be more chrome than setting.
        viewMenu.addItem(.separator())
        let appearanceItem = NSMenuItem(title: "Appearance", action: nil, keyEquivalent: "")
        let appearanceMenu = NSMenu(title: "Appearance")
        appearanceMenu.autoenablesItems = false
        for mode in AppearanceMode.allCases {
            let item = NSMenuItem(
                title: mode.title, action: #selector(setAppearanceMode(_:)), keyEquivalent: "")
            item.target = self
            item.representedObject = mode.rawValue
            item.state = (mode == AppearanceMode.current) ? .on : .off
            appearanceMenu.addItem(item)
        }
        appearanceItem.submenu = appearanceMenu
        appearanceMenuItems = appearanceMenu.items
        viewMenu.addItem(appearanceItem)

        viewItem.submenu = viewMenu
        main.addItem(viewItem)

        let queryItem = NSMenuItem()
        let queryMenu = NSMenu(title: "Query")
        add(queryMenu, "Run Statement Under Caret", #selector(runStatement(_:)), "\r")
        add(queryMenu, "Cancel", #selector(cancelQuery(_:)), ".")
        queryMenu.addItem(.separator())
        add(queryMenu, "Query History…", #selector(showQueryHistory(_:)), "y")
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
    // It stays in the binary permanently: the cold-start budget (P1, ≤250 ms
    // target / >600 ms FAIL) is a number that regresses silently, and a
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

/// Drives the app from the command line so the performance numbers can be
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
    /// `--select-path public/orders`: catalog path under the active profile to
    /// expand and then select, exactly as clicking it in the sidebar would.
    private let selectPath: [String]
    private var settleTicks = 0

    init(
        model: AppModel?, profile: (name: String, url: String)?, sql: String?, bench: Bool,
        selectPath: [String] = []
    ) {
        self.model = model
        self.profile = profile
        self.sql = sql
        self.bench = bench
        self.selectPath = selectPath
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
        let selectPath =
            value("--select-path")?.split(separator: "/").map(String.init) ?? []
        guard profile != nil || sql != nil || bench || !selectPath.isEmpty else { return nil }
        return Autopilot(
            model: model, profile: profile, sql: sql, bench: bench, selectPath: selectPath)
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
        if !selectPath.isEmpty { return descend(from: nil, remaining: selectPath[...]) }
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

    /// Walks the catalog one level at a time — the same `load(_:prefix:)` the
    /// disclosure triangle calls — and finishes with `select(_:)`, the same call
    /// a sidebar click makes. Nothing here reaches past the UI's own entry
    /// points, so a green result really does mean "clicking that table works".
    private func descend(from parent: CatalogNode?, remaining: ArraySlice<String>) {
        guard let model else { return }
        guard let wanted = remaining.first else {
            if let parent {
                FileHandle.standardError.write(
                    Data("AUTOPILOT selecting \(parent.profile)/\(parent.path.joined(separator: "/"))\n".utf8))
                model.select(parent)
            }
            return finishOrBench()
        }

        let siblings: [CatalogNode]
        if let parent {
            siblings = parent.children
        } else {
            siblings = model.roots.filter { $0.name == model.activeProfile }
            // The first hop is the profile row itself, whose "children" are the
            // top-level schemas — so start by expanding the active profile.
            if let root = siblings.first, !root.didLoad, !root.isLoading {
                model.load(root, prefix: nil)
            }
            if let root = siblings.first, root.didLoad {
                return descend(from: root, remaining: remaining)
            }
            return retryDescend(parent: nil, remaining: remaining)
        }

        guard let match = siblings.first(where: { $0.name == wanted }) else {
            if parent?.didLoad == true {
                FileHandle.standardError.write(
                    Data(
                        "AUTOPILOT no child named `\(wanted)` under /\(parent?.path.joined(separator: "/") ?? "") — have: \(siblings.map(\.name).joined(separator: ", "))\n"
                            .utf8))
                return finishOrBench()
            }
            return retryDescend(parent: parent, remaining: remaining)
        }

        if remaining.count == 1 { return descend(from: match, remaining: remaining.dropFirst()) }
        if !match.didLoad && !match.isLoading { model.load(match, prefix: nil) }
        if match.didLoad { return descend(from: match, remaining: remaining.dropFirst()) }
        retryDescend(parent: parent, remaining: remaining)
    }

    private func retryDescend(parent: CatalogNode?, remaining: ArraySlice<String>) {
        settleTicks += 1
        guard settleTicks < 100 else {
            FileHandle.standardError.write(Data("AUTOPILOT catalog walk timed out\n".utf8))
            return finishOrBench()
        }
        DispatchQueue.main.asyncAfter(deadline: .now() + 0.1) { [weak self] in
            self?.descend(from: parent, remaining: remaining)
        }
    }

    private func finishOrBench() {
        guard bench, let model else { return }
        DispatchQueue.main.asyncAfter(deadline: .now() + 0.3) { model.runScrollBench() }
    }
}
