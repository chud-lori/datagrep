// swift-tools-version: 6.1
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
        resources: [.process("Resources")],
        swiftSettings: [.swiftLanguageMode(.v5)],
        linkerSettings: useRealFFI
            ? [
                .unsafeFlags([
                    "\(realLibDir)/libdatagrep_ffi.a", "-lc++", "-lresolv", "-liconv",
                    "-lz",
                ]
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
    platforms: [.macOS(.v14)],
    products: [
        .executable(name: "datagrep-app", targets: ["datagrep-app"])
    ],
    targets: targets
)
