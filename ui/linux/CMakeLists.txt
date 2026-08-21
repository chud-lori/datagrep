cmake_minimum_required(VERSION 3.19)

# datagrep — native Linux Qt6/C++ UI over the Rust core.

project(datagrep-linux LANGUAGES CXX)

set(CMAKE_CXX_STANDARD 17)
set(CMAKE_CXX_STANDARD_REQUIRED ON)
set(CMAKE_AUTOMOC ON)   # Q_OBJECT classes across the UI need moc
set(CMAKE_AUTORCC ON)   # resources.qrc (engine brand SVGs) needs rcc

if(NOT CMAKE_BUILD_TYPE AND NOT CMAKE_CONFIGURATION_TYPES)
    set(CMAKE_BUILD_TYPE Release CACHE STRING "" FORCE)
endif()

get_filename_component(DATAGREP_REPO_ROOT "${CMAKE_CURRENT_SOURCE_DIR}/../.."
                       ABSOLUTE)

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

add_library(datagrep_ffi STATIC IMPORTED GLOBAL)
set_target_properties(datagrep_ffi PROPERTIES
    IMPORTED_LOCATION "${DATAGREP_FFI_LIB}"
    INTERFACE_INCLUDE_DIRECTORIES "${DATAGREP_FFI_INCLUDE}")

find_package(Threads REQUIRED)
find_package(ZLIB REQUIRED)

find_package(PkgConfig REQUIRED)
pkg_check_modules(DBUS REQUIRED dbus-1)

# Svg: QSvgRenderer for the engine brand marks (vector, palette-aware).
find_package(Qt6 REQUIRED COMPONENTS Core Gui Widgets Svg Network Concurrent)

option(DATAGREP_USE_KSYNTAXHIGHLIGHTING
       "Use KF6::SyntaxHighlighting for SQL highlighting when available" ON)

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
