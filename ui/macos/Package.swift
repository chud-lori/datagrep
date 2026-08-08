// swift-tools-version: 6.1
//
// datagrep macOS UI. Built with Swift Package Manager, NOT Xcode — this machine has
// Command Line Tools 16.4 only (no Xcode.app, no xcodebuild). AppKit ships in
// the CLT SDK, so `swift build` is sufficient; `build-app.sh` assembles the
// .app bundle by hand afterwards.
//
// FFI selection (there is no SwiftPM trait here on purpose — traits are still
// experimental in 6.1 and this needs to work today):
//
//   swift build -c release                 -> links CDatagrepStub (synthetic data)
//   DATAGREP_FFI=real \
//   DATAGREP_FFI_LIB_DIR=/path/to/dir \
//   swift build -c release                 -> links libdatagrep_ffi.a instead
//
import PackageDescription
import Foundation

let env = ProcessInfo.processInfo.environment
let useRealFFI = (env["DATAGREP_FFI"]?.lowercased() == "real")
let realLibDir = env["DATAGREP_FFI_LIB_DIR"] ?? "../../target/release"

var targets: [Target] = [
    .target(name: "CDatagrepFFI"),
    .target(
        name: "DatagrepKit",
        dependencies: ["CDatagrepFFI"] + (useRealFFI ? [] : ["CDatagrepStub"]),
        // The engine brand marks. They live in DatagrepKit and not in datagrep-app on
        // purpose: `EngineStyle` is the single definition of what an engine
        // looks like, and `Bundle.module` only resolves inside the target that
        // declares the resources. build-app.sh copies the emitted
        // `datagrep-ui_DatagrepKit.bundle` into datagrep.app/Contents/Resources.
        resources: [.process("Resources")],
        // AppKit delegate protocols are not Swift 6 strict-concurrency clean;
        // v5 mode keeps the diagnostics honest instead of drowning in them.
        swiftSettings: [.swiftLanguageMode(.v5)],
        // The archive is named by full path, NOT `-L… -ldatagrep_ffi`: cargo emits
        // libdatagrep_ffi.a AND libdatagrep_ffi.dylib side by side, `-l` prefers the
        // dylib, and the resulting .app then depends on an absolute path inside
        // the build tree. Naming the .a gives one self-contained binary, which
        // is exactly what the crate builds a staticlib for.
        //
        // The system libraries after it are taken verbatim from the link line in
        // crates/datagrep-ffi/tests/run_smoke.sh: Security + CoreFoundation are the
        // keychain (datagrep-secrets), SystemConfiguration / libresolv / libiconv
        // come in via rustls and tokio, libc++ via the bundled SQLite.
        linkerSettings: useRealFFI
            ? [
                .unsafeFlags([
                    "\(realLibDir)/libdatagrep_ffi.a", "-lc++", "-lresolv", "-liconv",
                    // zlib: flate2 (via the mongodb and elasticsearch HTTP stacks)
                    // links the system libz. The stub build has no flate2, which is
                    // why omitting this only fails against the real FFI.
                    "-lz",
                ]
                    // The half of the ABI reached by `dlsym` (see
                    // `ProfileABI`) has NO Swift call site, so nothing in this
                    // package leaves an undefined symbol for the linker to
                    // resolve — and a static archive only contributes the
                    // members that resolve one. Without these the symbols are
                    // simply never pulled out of libdatagrep_ffi.a, every
                    // `dlsym` returns NULL, and the app quietly reports that
                    // the engine cannot edit or test a connection while the
                    // code for both sits in the archive unused. `-u` forces
                    // each one in. Verify with:
                    //   nm -g datagrep.app/Contents/MacOS/datagrep | grep _datagrep_
                    + [
                        "datagrep_profiles_update", "datagrep_profiles_get_json",
                        "datagrep_profiles_add_json", "datagrep_connection_info_json",
                        "datagrep_connection_test_json",
                    ].flatMap { ["-Xlinker", "-u", "-Xlinker", "_\($0)"] }),
                .linkedFramework("Security"),
                .linkedFramework("CoreFoundation"),
                .linkedFramework("SystemConfiguration"),
            ]
            : []
    ),
    .executableTarget(
        name: "datagrep-app",
        dependencies: ["DatagrepKit"],
        swiftSettings: [.swiftLanguageMode(.v5)],
        linkerSettings: [
            .linkedFramework("AppKit"),
            .linkedFramework("SwiftUI"),
            .linkedFramework("QuartzCore"),
        ]
    ),
]

if !useRealFFI {
    targets.insert(.target(name: "CDatagrepStub", dependencies: ["CDatagrepFFI"]), at: 1)
}

let package = Package(
    name: "datagrep-ui",
    // v14, not v13: `NSViewController.loadViewIfNeeded()` is macOS 14+, and so
    // are the NavigationSplitView column-width modifiers the workbench uses.
    platforms: [.macOS(.v14)],
    products: [
        .executable(name: "datagrep-app", targets: ["datagrep-app"])
    ],
    targets: targets
)
