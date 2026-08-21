cmake_minimum_required(VERSION 3.19)

# datagrep — native Linux Qt6/C++ UI over the Rust core.
#
# This links the STATIC Rust FFI archive (libdatagrep_ffi.a, produced by
# `cargo build -p datagrep-ffi --release`) and includes the frozen C ABI header
# (crates/datagrep-ffi/include/datagrep.h). The engine is never built or changed
# from here beyond invoking cargo; this project is a pure CoreApi client.

project(datagrep-linux LANGUAGES CXX)

set(CMAKE_CXX_STANDARD 17)
set(CMAKE_CXX_STANDARD_REQUIRED ON)
set(CMAKE_AUTOMOC ON)   # Q_OBJECT classes across the UI need moc
set(CMAKE_AUTORCC ON)   # resources.qrc (engine brand SVGs) needs rcc

if(NOT CMAKE_BUILD_TYPE AND NOT CMAKE_CONFIGURATION_TYPES)
    set(CMAKE_BUILD_TYPE Release CACHE STRING "" FORCE)
endif()

# ---------------------------------------------------------------------------
# Locate the workspace and the Rust FFI archive it builds.
# ---------------------------------------------------------------------------
get_filename_component(DATAGREP_REPO_ROOT "${CMAKE_CURRENT_SOURCE_DIR}/../.."
                       ABSOLUTE)

# The app version, from the workspace manifest — the same source the packaging
# scripts stamp on the artifacts and the release script bumps alongside the
# update manifest. The update check compares against this.
file(STRINGS "${DATAGREP_REPO_ROOT}/Cargo.toml" _dg_version_lines
     REGEX "^version = \"")
if(NOT _dg_version_lines)
    message(FATAL_ERROR "could not read the workspace version from Cargo.toml")
endif()
list(GET _dg_version_lines 0 _dg_version_line)
string(REGEX REPLACE "^version = \"(.*)\"$" "\\1" DATAGREP_APP_VERSION
       "${_dg_version_line}")
set(DATAGREP_FFI_INCLUDE "${DATAGREP_REPO_ROOT}/crates/datagrep-ffi/include")
set(DATAGREP_FFI_LIB
    "${DATAGREP_REPO_ROOT}/target/release/libdatagrep_ffi.a"
    CACHE FILEPATH "Path to the prebuilt libdatagrep_ffi.a static archive")

# ---------------------------------------------------------------------------
# Optionally drive `cargo build` so `cmake --build` also (re)builds the engine.
# A Rust staticlib is NOT a CMake input, so a rebuilt engine is otherwise
# invisible to CMake — this custom target closes that gap. Turn it off
# (-DDATAGREP_BUILD_RUST=OFF) to link a .a you build yourself / ship prebuilt.
# ---------------------------------------------------------------------------
option(DATAGREP_BUILD_RUST
       "Run cargo build -p datagrep-ffi --release as part of the build" ON)

if(DATAGREP_BUILD_RUST)
    find_program(CARGO_EXECUTABLE cargo REQUIRED)
    add_custom_command(
        OUTPUT "${DATAGREP_FFI_LIB}"
        COMMAND "${CARGO_EXECUTABLE}" build -p datagrep-ffi --release
        WORKING_DIRECTORY "${DATAGREP_REPO_ROOT}"
        COMMENT "Building libdatagrep_ffi.a (cargo build -p datagrep-ffi --release)"
        VERBATIM)
    add_custom_target(datagrep_ffi_build DEPENDS "${DATAGREP_FFI_LIB}")
endif()

# The imported static archive. INTERFACE_INCLUDE_DIRECTORIES carries the C ABI
# header to every target that links it.
add_library(datagrep_ffi STATIC IMPORTED GLOBAL)
set_target_properties(datagrep_ffi PROPERTIES
    IMPORTED_LOCATION "${DATAGREP_FFI_LIB}"
    INTERFACE_INCLUDE_DIRECTORIES "${DATAGREP_FFI_INCLUDE}")

# ---------------------------------------------------------------------------
# Native system libraries the Rust staticlib depends on but does NOT contain.
#
#   A Rust `staticlib` bundles only Rust code; its native dependencies must be
#   named on the final link line. The authoritative list for THIS crate comes
#   from:
#       cargo rustc -p datagrep-ffi --release -- --print=native-static-libs
#   Re-run that whenever crate features change and reconcile the list below.
#
#   The always-present set on Linux:
#     * Threads::Threads / pthread — tokio's worker threads.
#     * ${CMAKE_DL_LIBS} (libdl)   — dynamic loading in std / drivers.
#     * m (libm)                   — the maths runtime std links against.
#     * dbus-1                     — datagrep-secrets uses the `keyring` crate's
#                                    Secret Service backend, which is D-Bus IPC.
#                                    Needs libdbus-1-dev at build, libdbus-1-3 at
#                                    runtime.
#   Feature-dependent extras that MAY appear (link errors will name them):
#     * z (libz)   — flate2, via the Mongo/Elasticsearch HTTP stacks.
# ---------------------------------------------------------------------------
find_package(Threads REQUIRED)
# The Rust archive pulls in flate2, whose miniz_oxide/zlib path references
# libz (inflateEnd, ...). A static archive does not carry that, so the final
# link needs -lz explicitly, or ld reports "DSO missing from command line".
find_package(ZLIB REQUIRED)

find_package(PkgConfig REQUIRED)
pkg_check_modules(DBUS REQUIRED dbus-1)

# Svg: QSvgRenderer for the engine brand marks (vector, palette-aware).
# Network: the once-per-launch update manifest GET.
# Concurrent: the connection-test dial, which blocks for the connect timeout.
find_package(Qt6 REQUIRED COMPONENTS Core Gui Widgets Svg Network Concurrent)

# ---------------------------------------------------------------------------
# Optional: KSyntaxHighlighting (KF6, MIT) for real SQL syntax highlighting.
#
# When present it replaces the hand-rolled SqlHighlighter with the maintained
# KSyntaxHighlighting::SyntaxHighlighter + bundled "SQL" definition and themes
# (see src/ui/SqlEditor.cpp). It is deliberately OPTIONAL: if the package is not
# installed the build falls back to the built-in basic highlighter instead of
# failing. HAVE_KSYNTAXHIGHLIGHTING is defined on the target only when found, and
# SqlEditor.cpp keys the whole KSyntax path off that macro.
#
# Install on Debian/Ubuntu with: libkf6syntaxhighlighting-dev
# Turn the attempt off entirely with -DDATAGREP_USE_KSYNTAXHIGHLIGHTING=OFF.
# ---------------------------------------------------------------------------
option(DATAGREP_USE_KSYNTAXHIGHLIGHTING
       "Use KF6::SyntaxHighlighting for SQL highlighting when available" ON)

# CI knob. Because the package is optional by design, a build environment that
# lacks it silently compiles the fallback — which means the KSyntax code path
# could rot without any build ever noticing. The CI leg that exists to compile
# that path sets this ON so a missing package is a hard configure error there,
# while every other build keeps the graceful fallback.
option(DATAGREP_REQUIRE_KSYNTAXHIGHLIGHTING
       "Fail configuration when KF6SyntaxHighlighting cannot be found" OFF)

set(DATAGREP_HAVE_KSYNTAXHIGHLIGHTING OFF)
if(DATAGREP_USE_KSYNTAXHIGHLIGHTING)
    if(DATAGREP_REQUIRE_KSYNTAXHIGHLIGHTING)
        find_package(KF6SyntaxHighlighting REQUIRED)
    else()
        find_package(KF6SyntaxHighlighting QUIET)
    endif()
    if(KF6SyntaxHighlighting_FOUND)
        set(DATAGREP_HAVE_KSYNTAXHIGHLIGHTING ON)
        message(STATUS
            "KF6SyntaxHighlighting found — real SQL syntax highlighting enabled")
    else()
        message(STATUS
            "KF6SyntaxHighlighting not found — falling back to the built-in "
            "basic SQL highlighter")
    endif()
endif()

# ---------------------------------------------------------------------------
# The application.
# ---------------------------------------------------------------------------
add_executable(datagrep
    resources.qrc
    src/main.cpp
    src/model/ConnectionSafety.cpp
    src/model/GridEditing.cpp
    src/model/Mutation.cpp
    src/model/QueryHistory.cpp
    src/model/QueryStatus.cpp
    src/model/SavedQueries.cpp
    src/model/SupportDir.cpp
    src/model/ResultModel.cpp
    src/model/UpdateCheck.cpp
    src/ui/Appearance.cpp
    src/ui/ConflictResolution.cpp
    src/ui/ConnectionDialog.cpp
    src/ui/DetailPanel.cpp
    src/ui/EditingSurface.cpp
    src/ui/EditorTabs.cpp
    src/ui/EngineIcon.cpp
    src/ui/HistoryPanel.cpp
    src/ui/MainWindow.cpp
    src/ui/ResultTableView.cpp
    src/ui/RowNumberHeader.cpp
    src/ui/SchemaTree.cpp
    src/ui/SqlEditor.cpp
    src/ui/SqlHighlighter.cpp
    src/ui/StatusBar.cpp
    src/ui/Theme.cpp
    src/ui/UpdateNotice.cpp
)

target_compile_definitions(datagrep PRIVATE
    DATAGREP_APP_VERSION="${DATAGREP_APP_VERSION}")

target_include_directories(datagrep PRIVATE
    "${CMAKE_CURRENT_SOURCE_DIR}/src"
    "${DATAGREP_FFI_INCLUDE}"
    ${DBUS_INCLUDE_DIRS}
)

if(DATAGREP_BUILD_RUST)
    add_dependencies(datagrep datagrep_ffi_build)
endif()

# When KSyntaxHighlighting was located, link it and flip on the code path that
# uses it. Absent the package, neither the define nor the link exists and
# SqlEditor.cpp compiles its built-in fallback highlighter.
if(DATAGREP_HAVE_KSYNTAXHIGHLIGHTING)
    target_link_libraries(datagrep PRIVATE KF6::SyntaxHighlighting)
    target_compile_definitions(datagrep PRIVATE HAVE_KSYNTAXHIGHLIGHTING)
endif()

target_link_libraries(datagrep PRIVATE
    Qt6::Core
    Qt6::Gui
    Qt6::Widgets
    Qt6::Svg
    Qt6::Network         # the once-per-launch update manifest GET
    Qt6::Concurrent       # the connection-test dial, off the GUI thread
    datagrep_ffi          # the imported libdatagrep_ffi.a
    Threads::Threads      # pthread
    ${CMAKE_DL_LIBS}      # dl
    m                     # libm
    ZLIB::ZLIB            # libz (flate2 in the Rust archive)
    ${DBUS_LIBRARIES}     # dbus-1 (keyring Secret Service)
)

target_link_directories(datagrep PRIVATE ${DBUS_LIBRARY_DIRS})
