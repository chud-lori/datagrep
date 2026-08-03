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
        // AppKit delegate protocols are not Swift 6 strict-concurrency clean;
        // v5 mode keeps the diagnostics honest instead of drowning in them.
        swiftSettings: [.swiftLanguageMode(.v5)],
        linkerSettings: useRealFFI
            ? [.unsafeFlags(["-L\(realLibDir)", "-ldbx_ffi"])]
            : []
    ),
    .executableTarget(
        name: "dbx-app",
        dependencies: ["DbxKit"],
        swiftSettings: [.swiftLanguageMode(.v5)],
        linkerSettings: [
            .linkedFramework("AppKit"),
            .linkedFramework("QuartzCore"),
        ]
    ),
]

if !useRealFFI {
    targets.insert(.target(name: "CDbxStub", dependencies: ["CDbxFFI"]), at: 1)
}

let package = Package(
    name: "dbx-macos",
    platforms: [.macOS(.v13)],
    products: [
        .executable(name: "dbx-app", targets: ["dbx-app"])
    ],
    targets: targets
)
