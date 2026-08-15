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

if(NOT CMAKE_BUILD_TYPE AND NOT CMAKE_CONFIGURATION_TYPES)
    set(CMAKE_BUILD_TYPE Release CACHE STRING "" FORCE)
endif()

# ---------------------------------------------------------------------------
# Locate the workspace and the Rust FFI archive it builds.
# ---------------------------------------------------------------------------
get_filename_component(DATAGREP_REPO_ROOT "${CMAKE_CURRENT_SOURCE_DIR}/../.."
                       ABSOLUTE)
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

find_package(PkgConfig REQUIRED)
pkg_check_modules(DBUS REQUIRED dbus-1)

find_package(Qt6 REQUIRED COMPONENTS Core Gui Widgets)

# ---------------------------------------------------------------------------
# The application.
# ---------------------------------------------------------------------------
add_executable(datagrep
    src/main.cpp
    src/model/QueryStatus.cpp
    src/model/ResultModel.cpp
    src/ui/ConnectionDialog.cpp
    src/ui/MainWindow.cpp
    src/ui/ResultTableView.cpp
    src/ui/RowNumberHeader.cpp
    src/ui/SchemaTree.cpp
    src/ui/SqlEditor.cpp
    src/ui/SqlHighlighter.cpp
    src/ui/StatusBar.cpp
)

target_include_directories(datagrep PRIVATE
    "${CMAKE_CURRENT_SOURCE_DIR}/src"
    "${DATAGREP_FFI_INCLUDE}"
    ${DBUS_INCLUDE_DIRS}
)

if(DATAGREP_BUILD_RUST)
    add_dependencies(datagrep datagrep_ffi_build)
endif()

target_link_libraries(datagrep PRIVATE
    Qt6::Core
    Qt6::Gui
    Qt6::Widgets
    datagrep_ffi          # the imported libdatagrep_ffi.a
    Threads::Threads      # pthread
    ${CMAKE_DL_LIBS}      # dl
    m                     # libm
    ${DBUS_LIBRARIES}     # dbus-1 (keyring Secret Service)
)

target_link_directories(datagrep PRIVATE ${DBUS_LIBRARY_DIRS})
