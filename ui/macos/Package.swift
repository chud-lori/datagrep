// swift-tools-version: 6.1
//
// dbx macOS UI. Built with Swift Package Manager, NOT Xcode — this machine has
// Command Line Tools 16.4 only (no Xcode.app, no xcodebuild). AppKit ships in
// the CLT SDK, so `swift build` is sufficient; `build-app.sh` assembles the
// .app bundle by hand afterwards.
//
// FFI selection (there is no SwiftPM trait here on purpose — traits are still
// experimental in 6.1 and this needs to work today):
//
//   swift build -c release                 -> links CDbxStub (synthetic data)
//   DBX_FFI=real \
//   DBX_FFI_LIB_DIR=/path/to/dir \
//   swift build -c release                 -> links libdbx_ffi.a instead
//
import PackageDescription
import Foundation

let env = ProcessInfo.processInfo.environment
let useRealFFI = (env["DBX_FFI"]?.lowercased() == "real")
let realLibDir = env["DBX_FFI_LIB_DIR"] ?? "../../target/release"

var targets: [Target] = [
    .target(name: "CDbxFFI"),
    .target(
        name: "DbxKit",
        dependencies: ["CDbxFFI"] + (useRealFFI ? [] : ["CDbxStub"]),
        // The engine brand marks. They live in DbxKit and not in dbx-app on
        // purpose: `EngineStyle` is the single definition of what an engine
        // looks like, and `Bundle.module` only resolves inside the target that
        // declares the resources. build-app.sh copies the emitted
        // `dbx-macos_DbxKit.bundle` into dbx.app/Contents/Resources.
        resources: [.process("Resources")],
        // AppKit delegate protocols are not Swift 6 strict-concurrency clean;
        // v5 mode keeps the diagnostics honest instead of drowning in them.
        swiftSettings: [.swiftLanguageMode(.v5)],
        // The archive is named by full path, NOT `-L… -ldbx_ffi`: cargo emits
        // libdbx_ffi.a AND libdbx_ffi.dylib side by side, `-l` prefers the
        // dylib, and the resulting .app then depends on an absolute path inside
        // the build tree. Naming the .a gives one self-contained binary, which
        // is exactly what the crate builds a staticlib for.
        //
        // The system libraries after it are taken verbatim from the link line in
        // crates/dbx-ffi/tests/run_smoke.sh: Security + CoreFoundation are the
        // keychain (dbx-secrets), SystemConfiguration / libresolv / libiconv
        // come in via rustls and tokio, libc++ via the bundled SQLite.
        linkerSettings: useRealFFI
            ? [
                .unsafeFlags([
                    "\(realLibDir)/libdbx_ffi.a", "-lc++", "-lresolv", "-liconv",
                ]),
                .linkedFramework("Security"),
                .linkedFramework("CoreFoundation"),
                .linkedFramework("SystemConfiguration"),
            ]
            : []
    ),
    .executableTarget(
        name: "dbx-app",
        dependencies: ["DbxKit"],
        swiftSettings: [.swiftLanguageMode(.v5)],
        linkerSettings: [
            .linkedFramework("AppKit"),
            .linkedFramework("SwiftUI"),
            .linkedFramework("QuartzCore"),
        ]
    ),
]

if !useRealFFI {
    targets.insert(.target(name: "CDbxStub", dependencies: ["CDbxFFI"]), at: 1)
}

let package = Package(
    name: "dbx-macos",
    // v14, not v13: `NSViewController.loadViewIfNeeded()` is macOS 14+, and so
    // are the NavigationSplitView column-width modifiers the workbench uses.
    platforms: [.macOS(.v14)],
    products: [
        .executable(name: "dbx-app", targets: ["dbx-app"])
    ],
    targets: targets
)
