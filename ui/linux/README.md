# datagrep — Linux UI (Qt6 / C++)

A native Linux front-end for datagrep, built with Qt6 Widgets and C++17. It is a
peer of the macOS app in `ui/macos`: both are thin CoreApi clients that link the
same Rust engine through the frozen C ABI in
`crates/datagrep-ffi/include/datagrep.h`. **No business logic lives in the UI** —
every query, schema walk and profile edit is a call into the engine, which runs
on its own tokio runtime thread.

## What's here

```
ui/linux/
  CMakeLists.txt              # links libdatagrep_ffi.a + Qt6 + native deps
  src/
    main.cpp
    ffi/
      datagrep_c_abi.h        # wraps the frozen datagrep.h in extern "C"
      DatagrepFfi.hpp         # RAII C++ wrappers: Core / Query / RowWindow
      RowPager.hpp            # bounded LRU over row windows (≤4 pages resident)
    model/
      QueryStatus.hpp/.cpp    # decoded datagrep_query_status_json
      ResultModel.hpp/.cpp    # QAbstractTableModel over the windowed row API  ← core
    ui/
      MainWindow.hpp/.cpp     # sidebar + editor + grid + honest status bar
      ResultTableView.hpp/.cpp# QTableView + copy-safe copy path
      RowNumberHeader.hpp/.cpp# the copy-safe row-number gutter (vertical header)
      SchemaTree.hpp/.cpp     # lazy schema tree, one level per expansion
      SqlEditor.hpp/.cpp      # QPlainTextEdit + statement-under-cursor
      SqlHighlighter.hpp/.cpp # placeholder SQL highlighter (upgrade seam below)
```

## Build dependencies

| Dependency        | Debian/Ubuntu package            | Fedora package                 |
|-------------------|----------------------------------|--------------------------------|
| C++17 toolchain   | `build-essential`                | `gcc-c++`                      |
| CMake ≥ 3.19      | `cmake`                          | `cmake`                        |
| Qt6 Widgets       | `qt6-base-dev`                   | `qt6-qtbase-devel`             |
| D-Bus (keyring)   | `libdbus-1-dev`                  | `dbus-devel`                   |
| pkg-config        | `pkg-config`                     | `pkgconf-pkg-config`           |
| Rust toolchain    | via [rustup](https://rustup.rs)  | via rustup                     |

`libdbus-1-dev` is required because `datagrep-secrets` stores connection
passwords through the `keyring` crate's **Secret Service** backend, which is
D-Bus IPC. At runtime you need `libdbus-1-3` and a Secret Service provider
(GNOME Keyring / KWallet) for saved secrets to resolve.

### Why the static archive needs extra `-l` flags

A Rust `staticlib` bundles only Rust code — its **native** dependencies are not
inside `libdatagrep_ffi.a` and must be named on the final link line. The
`CMakeLists.txt` links the always-present set (`pthread`, `dl`, `m`, `dbus-1`).
If a link error names another library (e.g. `-lz` from flate2 in the Mongo/ES
HTTP stacks), get the authoritative list with:

```sh
cargo rustc -p datagrep-ffi --release -- --print=native-static-libs
```

and add the named libraries to `target_link_libraries` in `CMakeLists.txt`.

## Build steps

From the repository root:

```sh
# 1. Build the Rust FFI static archive (CMake can also do this for you; see below)
cargo build -p datagrep-ffi --release
#    -> target/release/libdatagrep_ffi.a

# 2. Configure and build the Qt app
cmake -S ui/linux -B ui/linux/build -DCMAKE_BUILD_TYPE=Release
cmake --build ui/linux/build -j

# 3. Run
./ui/linux/build/datagrep
```

By default `-DDATAGREP_BUILD_RUST=ON` makes `cmake --build` run
`cargo build -p datagrep-ffi --release` itself, so step 1 is optional. Pass
`-DDATAGREP_BUILD_RUST=OFF` to link a prebuilt archive instead, and
`-DDATAGREP_FFI_LIB=/path/to/libdatagrep_ffi.a` to point at a non-default
location.

The profiles database (created by the engine) lives at
`$XDG_DATA_HOME/datagrep/profiles.db` (typically
`~/.local/share/datagrep/profiles.db`).

## How the results grid stays virtual

`ResultModel` is a `QAbstractTableModel` (a `QAbstractItemModel`) over the
windowed row API. It never materialises a whole result:

| Model method     | C ABI call(s)                                             |
|------------------|-----------------------------------------------------------|
| `rowCount()`     | rows exposed so far, grown toward `rows_loaded` from `datagrep_query_status_json` |
| `columnCount()`  | `columns` length from `datagrep_query_status_json`        |
| `data()`         | `datagrep_query_rows` (via a ≤4-page LRU) + `datagrep_rows_cell` / `datagrep_rows_cell_kind` for the addressed cell only |
| `headerData(H)`  | column name/type from `datagrep_query_status_json`        |
| `headerData(V)`  | the 1-based row number — `section + 1` (see copy-safety)  |
| `canFetchMore()` | `exposed < rows_loaded`, or the query is still streaming  |
| `fetchMore()`    | re-read `datagrep_query_status_json`, then `beginInsertRows`/`endInsertRows` |

The `RowPager` keeps at most 4 windows of 512 rows (2,048 rows) resident; an
evicted window's `datagrep_rows_free` runs immediately (RAII). A result of a
million rows therefore costs a few kilobytes of resident cells. The progress
callback (`datagrep_query_on_progress`) fires on a **background tokio thread**;
the model marshals it onto the GUI thread with a coalesced, queued signal before
touching any state — it never touches Qt objects from the foreign thread.

Cell text from `datagrep_rows_cell` is a **borrowed, non-NUL-terminated**
`const char*`; it is copied out by `(ptr, len)` and **never** freed. Every owned
`char*` (`*_json`, `err_out`, cancel outcome, cell detail) goes through
`datagrep_string_free` exactly once. See `DatagrepFfi.hpp` for the ownership
rules, mirrored 1:1 from the header.

## Copy-safety of the row-number gutter (guaranteed, not merely avoided)

The row-number gutter is Qt's **vertical header** (`RowNumberHeader`, a
`QHeaderView`). The row number is produced **only** by
`ResultModel::headerData(section, Qt::Vertical, Qt::DisplayRole) → section + 1`.

The guarantee is structural, not a matter of formatting the copy string
carefully:

1. Qt ships **no** built-in Ctrl+C for item views. Copy is implemented once, in
   `ResultTableView::copySelection()`, and it serialises
   `selectionModel()->selectedIndexes()` — nothing else.
2. `selectedIndexes()` contains only `QModelIndex` values, i.e. **model cells**.
   A vertical-header value is not a `QModelIndex` and can never be a member of
   that list.
3. Therefore a row number is **structurally incapable** of appearing in copied
   output. There is no code path that could include it, the same way the macOS
   grid's row numbers live in an `NSRulerView` while every copy path enumerates
   `tableColumns` only.

The row number is also never injected as a model column 0, which would defeat
the guarantee. It is chrome, derived purely from the row index, so painting it
never touches the pager (zero fetches while scrolling the gutter).

## SQL editor and the upgrade seam

The editor is a `QPlainTextEdit` with a small placeholder `QSyntaxHighlighter`
(`SqlHighlighter`). "Run the statement under the cursor" splits the buffer on
`;` boundaries that are outside strings/identifiers/comments and sends exactly
that substring to `datagrep_query_run`.

The editor talks to its highlighter only through the `QSyntaxHighlighter` base,
so upgrading is localised:

- **Preferred: KSyntaxHighlighting** (KF6, **MIT**). Ships a
  `QSyntaxHighlighter` subclass plus a maintained SQL definition and themes.
  Add `find_package(KF6SyntaxHighlighting)` and construct it against the
  editor's document — a near drop-in for `SqlHighlighter`.
- **QScintilla** gives a full editor widget (folding, autocomplete) but is
  **GPLv3 / commercial only**; adopting it would impose GPLv3 on the whole UI
  binary, so it is intentionally not wired in. If the project accepts that
  licensing, replace the `QPlainTextEdit` with a `QsciScintilla`.

## Packaging (plan — not yet implemented)

Packaging is deliberately left as a stub for a follow-up. The intended shape:

- **AppImage** (primary, distro-agnostic): build with CMake in Release, run
  `linuxdeploy` + `linuxdeploy-plugin-qt` to pull in the Qt6 runtime and
  platform plugins, and bundle `libdbus-1` transitively. Ship a `.desktop` file
  and an icon (reuse `assets/`). Produces one portable `datagrep-x86_64.AppImage`.
- **.deb** (Debian/Ubuntu): CPack `DEB` generator, or `dpkg-buildpackage` with a
  `debian/` dir. `Depends:` should list `libqt6widgets6`, `libdbus-1-3`, and a
  Secret Service provider recommendation.
- **.rpm** (Fedora/RHEL): CPack `RPM` generator or a `.spec`, `Requires:`
  `qt6-qtbase-gui`, `dbus-libs`.

A `CPack` block will be added to `CMakeLists.txt` with `install(TARGETS …)` and
a `.desktop`/icon install once the layout is agreed. None of this changes the
engine or the ABI.

## Status of this scaffold

This is the **start** of the Linux UI (issue #4). The code is written to build on
Linux with Qt6 present; it was authored and statically verified against
`crates/datagrep-ffi/include/datagrep.h` on a machine without Qt6/CMake, so it
has **not** been compiled here. The FFI signatures, ownership/free rules and the
model↔ABI mapping are verified against the header; a Linux build with Qt6 is the
remaining step to confirm it compiles and links clean.
