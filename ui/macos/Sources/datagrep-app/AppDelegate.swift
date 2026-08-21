import AppKit
import Combine
import DatagrepKit
import SwiftUI

/// Entry point.
@main
enum DatagrepMain {
    @MainActor
    static func main() {
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

    func applicationDidFinishLaunching(_ notification: Notification) {
        if MutationProbe.runIfRequested() {
            NSApp.terminate(nil)
            return
        }
        AppearanceMode.apply()
        buildMainMenu()
        Startup.mark("buildMainMenu")

        let host = NSHostingController(rootView: Workbench(model: model))
        host.sizingOptions = []
        Startup.mark("NSHostingController(Workbench)")

        window = NSWindow(contentViewController: host)
        Startup.mark("NSWindow(contentViewController:) — SwiftUI loadView")
        // `--window-size 960x600` opens at a chosen size instead of 1180×760.
        let size = Self.requestedWindowSize() ?? NSSize(width: 1180, height: 760)
        window.setContentSize(size)
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
            window.setContentSize(size)
        } else {
            window.setFrameAutosaveName("datagrep.main")
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

        window.contentView?.displayIfNeeded()
        Startup.mark("first paint")

        let ms = Startup.millisSinceProcessStart()
        FileHandle.standardError.write(
            Data(String(format: "MEASURE cold start exec -> window: %.0f ms\n", ms).utf8))

        DispatchQueue.main.async { [weak self] in self?.finishChrome() }
    }

    private func finishChrome() {
        StartupStage.shared.markContentReady()
        window.contentView?.displayIfNeeded()
        Startup.mark("toolbar + inspector + editor + grid attached")

        DispatchQueue.main.async { [weak self] in self?.finishBooting() }
    }

    private func finishBooting() {
        model.boot()
        Startup.mark("model.boot() — core + profiles + editor session")

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

    private func installLaunchHarnesses() {
        // `--screenshot <path> [delay]`: the app renders ITSELF to a PNG.
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

    private func applyChromeTint(_ colorName: String?) {
        guard let window else { return }
        window.backgroundColor =
            colorName
            .flatMap(ConnectionColor.nsColor)
            .flatMap { $0.blended(withFraction: 0.78, of: .windowBackgroundColor) }
            ?? .windowBackgroundColor
        window.title =
            colorName == nil ? "datagrep" : "datagrep — \(model.activeProfile)"
    }

    /// Writes a PNG of the app's own window for headless verification.
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

    @objc func showQueryHistory(_ sender: Any?) { model.history.isPresented = true }

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
        add(queryMenu, "Report Footprint", #selector(reportFootprint(_:)), "")
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

    nonisolated(unsafe) private static var lastMark: Double = 0
    nonisolated(unsafe) private static var marks: [(String, Double, Double)] = []

    static let tracing: Bool = {
        ProcessInfo.processInfo.arguments.contains("--trace-startup")
            || ProcessInfo.processInfo.environment["DATAGREP_TRACE_STARTUP"] == "1"
    }()

    static func mark(_ label: String) {
        guard tracing else { return }
        let now = millisSinceProcessStart()
        marks.append((label, now, now - lastMark))
        lastMark = now
    }

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

@MainActor
final class Autopilot {
    private weak var model: AppModel?
    private let profile: (name: String, url: String)?
    private let sql: String?
    private let bench: Bool
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
