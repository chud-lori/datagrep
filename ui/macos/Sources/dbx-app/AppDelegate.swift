import AppKit
import Combine
import DbxKit
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
enum DbxMain {
    @MainActor
    static func main() {
        let app = NSApplication.shared
        let delegate = AppDelegate()
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

    func applicationDidFinishLaunching(_ notification: Notification) {
        buildMainMenu()

        model.boot()

        let host = NSHostingController(rootView: Workbench(model: model))
        // Empty sizing options: otherwise the hosting controller pushes the
        // SwiftUI ideal size onto the window and the frame set below is
        // silently overridden (it opened at 872×572 instead of 1180×760).
        host.sizingOptions = []
        window = NSWindow(contentViewController: host)
        window.setContentSize(NSSize(width: 1180, height: 760))
        window.contentMinSize = NSSize(width: 900, height: 560)
        window.title = "dbx"
        window.styleMask.insert(.fullSizeContentView)
        window.titlebarAppearsTransparent = true
        window.toolbarStyle = .unified
        window.tabbingMode = .disallowed
        window.delegate = self
        window.setFrameAutosaveName("dbx.main")
        window.center()
        window.makeKeyAndOrderFront(nil)
        NSApp.activate(ignoringOtherApps: true)

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

        model.editor.focus()

        DispatchQueue.main.async { [weak self] in
            let ms = Startup.millisSinceProcessStart()
            FileHandle.standardError.write(
                Data(String(format: "MEASURE cold start exec -> window: %.0f ms\n", ms).utf8))
            self?.autopilot = Autopilot.fromArguments(model: self?.model)
            self?.autopilot?.start()
        }

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
        window.title = isProd ? "dbx — PRODUCTION" : "dbx"
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
            withTitle: "About dbx", action: #selector(NSApplication.orderFrontStandardAboutPanel(_:)),
            keyEquivalent: "")
        appMenu.addItem(.separator())
        appMenu.addItem(
            withTitle: "Hide dbx", action: #selector(NSApplication.hide(_:)), keyEquivalent: "h")
        appMenu.addItem(.separator())
        appMenu.addItem(
            withTitle: "Quit dbx", action: #selector(NSApplication.terminate(_:)), keyEquivalent: "q")
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
        let inspectorItem = NSMenuItem(
            title: "Show Inspector", action: #selector(toggleDetail(_:)), keyEquivalent: "i")
        inspectorItem.target = self
        viewMenu.addItem(inspectorItem)
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
}

// MARK: - autopilot (measurement harness)

/// Drives the app from the command line so the design §5 numbers can be
/// produced repeatably instead of by hand-scrolling and guessing.
///
///   dbx --add-profile bench=sqlite:///tmp/bench.db \
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
