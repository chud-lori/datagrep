# datagrep — Linux UI (GTK4 / libadwaita)

A native GNOME front-end, peer of `ui/macos` (Swift/AppKit) and `ui/linux`
(Qt6/C++). All three are thin clients over the same Rust engine and **hold no
business logic**: every query, schema walk and profile edit is a call into the
core, which runs on its own tokio runtime thread.

`ui/linux` is not retired by this. It stays until this reaches parity (issue
#36; the Qt UI is #4).

## What's here today

```
ui/gtk4/
  Cargo.toml                # its own workspace — see "Build integration"
  src/
    main.rs                 # the binary; ui::run() is the whole of it
    sql.rs                  # Derived: base statement + the ORDER BY / WHERE headers added
    ffi/mod.rs              # safe wrappers: Core / Query / RowWindow
    model/
      pager.rs              # bounded LRU over row windows (<= 4 pages resident)
      status.rs             # decoded datagrep_query_status_json
      profile.rs            # decoded datagrep_profiles_list_json
      catalog.rs            # decoded datagrep_catalog_children_json, enumeration included
      detail.rs             # decoded datagrep_catalog_describe_json: columns, indexes, stats
      format.rs             # counts, bytes, durations, day titles — one spelling each
      history.rs            # the query-history store: JSONL day files, retention, dedupe
      row.rs                # the GListModel item: an index, nothing else
      result.rs             # ResultModel: GListModel over the windowed row API  <- core
    ui/
      mod.rs                # AdwApplication, style sheet, config-directory paths
      window.rs             # the shell: toolbar view, split views, breakpoints, run path
      sidebar.rs            # connections list + its own flat header bar
      schema.rs             # the lazy catalog tree (GtkTreeListModel)
      grid.rs               # GtkColumnView + the row-number gutter
      status_bar.rs         # honest row counts, elapsed, cancel
      utility.rs            # the utility pane: AdwViewSwitcher over Inspector and History
      inspector.rs          # schema detail for the selected object, cell detail for the cell
      history.rs            # the history panel: filters, day sections, rerun, retention
  examples/preview.rs       # snapshots the realised window to PNG (see "Building")
  tests/streaming.rs        # the model against the real engine, headless
  tests/catalog.rs          # the sidebar's data path against the real engine, headless
  tests/history.rs          # the history store against the format the other two read
```

The model came first because `GtkColumnView` is virtualised over a `GListModel`,
so nothing above it could be right until it was.

The editor and the inspector mount into `Window::editor_slot()` and
`Window::utility_slot()`. Both are `AdwBin`s that take no space until something
is put in them, so a slot nobody has filled yet is invisible rather than an
empty rectangle. Every statement — typed, replayed, or re-issued by a header
click — goes through `Window::run`, which is what keeps the derived clauses from
being bypassed by where the SQL came from.

## The three decisions

### 1. Rust (`gtk4-rs`), calling the engine through the C ABI

Language was never the fork — the seam was. This crate depends on `datagrep-ffi`
and calls its `#[no_mangle] pub extern "C"` entry points as ordinary Rust
functions. Same ABI, same semantics as Swift and Qt see; what disappears is the
*transcription* of it. There is no header to keep in step, no `bindgen`, and no
stub to reimplement (`ui/macos/Sources/CDatagrepStub/DatagrepStub.c` is 823
lines of one). Signatures are checked by the compiler, so the drift that already
happened once — `ui/macos/Sources/CDatagrepFFI/include/datagrep.h` is still
missing `datagrep_profiles_add_json`, `datagrep_profiles_update` and
`datagrep_profiles_get_json` — cannot happen here.

Linking `datagrep-core` directly instead was rejected. The ABI is what keeps the
engine frontend-agnostic and independently testable, and its cost is smaller
than it looks: the hot path (`datagrep_query_rows` + `datagrep_rows_cell`) is
already raw pointers into an arena, not JSON. JSON is paid on status ticks,
catalog expansions and profile edits — cold paths — so bypassing the ABI would
buy no measurable speed while making this the one front-end that can reach into
engine internals.

Rust also buys something C could not: `RowWindow::cell` returns a `&'a str`
borrowed from `&'a self`. The header's "borrowed, NOT null-terminated, never
free it, never hold it past `datagrep_rows_free`" becomes a borrow the compiler
enforces — and a genuinely zero-copy read, where the C++ wrapper has to return a
`std::string` copy because it cannot express the lifetime.

### 2. Build integration: its own Cargo workspace, no CMake and no meson

`ui/gtk4/Cargo.toml` declares an empty `[workspace]`, the same opt-out `xtask/`
uses. It is not a member of the root workspace on purpose: `ci/gates.sh` runs
`cargo clippy/test --workspace`, contributors run it on macOS, and there is no
GTK4 there — a member crate would make the Tier-1 gate unrunnable on the primary
development machine.

The cost of a second workspace is a second lockfile outside the Tier-1
supply-chain gate. `.github/workflows/linux-gtk4.yml` closes that: it runs
`cargo deny` against the **root** `deny.toml`, so there is one licence
allowlist, one banned-crate list and one accepted-advisory list for the whole
product.

CMake and meson both lose here. Cargo already builds the engine, resolves
`gtk4-sys` through `pkg-config`, and is the only tool that has to run — adding a
second build system to drive a Rust binary would buy nothing.

### 3. Packaging: Flatpak, not AppImage

Decided before any widget exists because it fixes the library floor.

Ubuntu 22.04 — our glibc floor, forced by a real shipping bug — ships GTK 4.6
and libadwaita **1.1**. On that floor `AdwToolbarView`, `AdwNavigationSplitView`,
`AdwOverlaySplitView`, `AdwBreakpoint`, `AdwSwitchRow` and `AdwSpinRow` (all
1.4), `AdwDialog`/`AdwAlertDialog`/`AdwPreferencesDialog` (1.5) and
`GtkInscription` (GTK 4.8, the widget that exists specifically for column-view
cells at scale) are all missing. What is left is `AdwLeaflet` and `AdwFlap` —
both deprecated in 1.4. Pinning to the system floor means writing 2022's
superseded idiom for a front-end whose entire reason to exist is looking native.

Bundling a newer libadwaita in an AppImage drags a newer GTK4 with it (1.4+
requires GTK > 4.6), plus `gsettings` schemas, an icon theme and the input
modules — the awkwardness issue #36 already flags. A Flatpak on
`org.gnome.Platform//47` (libadwaita 1.6 / GTK 4.16) or `//48` (1.7 / 4.18)
supplies all of it, compiles schemas into `/app/share/glib-2.0/schemas` as a
normal part of the build, and brings the portals dark-mode and file dialogs want.

It also disposes of the glibc class of bug outright: the runtime carries its own
glibc, so what the build host happens to ship stops being the oldest desktop
that can run the result.

Sandbox permissions this will need, each explicit rather than inherited:
`--talk-name=org.freedesktop.secrets` (the engine stores passwords in the
Secret Service), `--share=network`, and `--filesystem=~/.ssh:ro` for tunnels.

The API floor is therefore **libadwaita 1.4 / GTK 4.12** — high enough for the
window composition above, low enough to compile on a stock `ubuntu-24.04`
runner, which is what keeps CI feedback fast. The CI job asserts it with
`pkg-config --atleast-version`, so the floor is a build failure rather than a
paragraph. Anything from 1.5/1.6 is used only behind a runtime version check.

## The utility pane

GNOME has no docking, and inventing some would be the least native thing this
front-end could do. The platform's answer is a collapsible pane at the end of
the window, so the inspector and the history live in one `AdwOverlaySplitView`
sidebar with an `AdwViewSwitcher` over two `AdwViewStack` pages. It mounts into
`Window::utility_slot()`, an `AdwBin` that takes no space until it is filled,
and it starts closed: the header toggle opens it, and only one click ever opens
it unasked — a `{n fields}` chip, which is an unambiguous request to see inside.

**Inspector.** Schema detail comes from `datagrep_catalog_describe_json`, called
by the tree on *selection* and cached per node — a failure is cached too, so
re-selecting a broken object cannot loop on the engine. `columns: null` and
`columns: []` are drawn as two different sentences ("not reported" and "none"),
because the engine means two different things by them. Cell detail is
`datagrep_rows_cell_detail_json` plus the row envelope, and the legend under it
spells out the four kinds the grid distinguishes; NULL, empty, ABSENT and
nested are the distinction this product is ahead of the field on, and a pane
that blurred them would give it back.

**History, and why the store is here at all.** The engine keeps a
`query_history` table, and **no `datagrep_history_*` entry point exists in the C
ABI** — verified against `crates/datagrep-ffi`. Every front-end therefore keeps
its own log, so the only thing that can make them one history is the file
format. This store writes what `ui/linux/src/model/QueryHistory.cpp` and
`ui/macos/Sources/DatagrepKit/QueryHistory.swift` write, byte for byte: one
`YYYY-MM-DD.jsonl` per local day, oldest line first, compact JSON with keys in
alphabetical order (Qt's `QJsonObject` sorts them; Swift asks for
`.sortedKeys`), `retention.json` beside them, and the same FNV-1a `textHash`
over the same whitespace normalisation. `tests/history.rs` asserts a Qt-written
line loads and re-encodes to the identical bytes, which is the part that would
silently rot otherwise.

Four behaviours are contracts, not preferences:

- **History is not scoped to the current connection.** Connection is a filter
  the user may apply, never one applied for them.
- **Retention is stated and editable** — "keeping the last 10,000 queries, up to
  180 days" is on screen, and the dialog says where the files are.
- **Failures are kept, with their error.** The query you want back is usually
  the one that broke. A run that never got a query handle is recorded too:
  the entry is opened before the engine is asked.
- **The engine id is stored on every entry**, so a deleted connection still
  reads.

**Rerunning goes through `Window::run`**, the same path typed SQL takes. The
panel emits `rerun-requested`; the pane selects the connection the entry names
(and says so if it is gone), then calls `run` — it never builds a query itself,
and it holds no `Core` handle to build one with. The confirm-writes prompt the
Qt window puts in `executeStatement()` has no GTK counterpart yet; when it
lands in `run`, a replay is behind it by construction rather than by anyone
remembering to guard the second entry point. `run-started` is emitted after
every guard and before the engine is asked, so a statement that gets refused is
never recorded as one that ran.

## Streaming, and what the model exposes

`GtkColumnView` has no `canFetchMore`/`fetchMore` pull — the scrollbar is sized
straight from `n_items` — so every row the engine has loaded is exposed as soon
as it lands, rather than revealed in batches the way the Qt model does. Batching
here would only make the scrollbar lie about how much result there is.

Rows arrive through the engine's own progress callback, which fires on a tokio
worker thread. A `bounded(1)` async channel is both the hop to the main context
and the coalescing latch: while a tick is queued the channel is full, so
`try_send` drops the extra exactly as Qt's `tickQueued_` atomic does.

A schema delta appends columns on the right — it never moves or renames one, or
the grid would shuffle mid-scroll. Every resident window is dropped when it
happens: the ABI indexes `row * cols + col` into a flat array, so asking a window
fetched under the old column count for a new column returns *another row's cell*
rather than an error. The model then raises `columns-changed`, which is what the
grid rebuilds its header from — no diffing `column_count` on every status tick.

Cell text is read with `with_cell(row, col, |kind, text| …)`: the `&str` is
borrowed from the window's arena for the duration of the closure, so a redraw
allocates nothing. `with_status` is the same shape for the status bar, which
would otherwise clone the whole column list every tick.

Everything above emits while holding no borrow: GTK re-enters through
`get_item` while `items_changed` runs, so a `RefCell` held across an emission is
a panic waiting for a scroll position.

## The row-number gutter

Both other front-ends guarantee by construction that row numbers cannot reach
the clipboard — macOS with an `NSRulerView`, Qt with a vertical `QHeaderView`
whose values are not `QModelIndex`es. The GTK4 equivalent, and why it holds:

The gutter is a **separate `GtkListView`, outside the horizontally-scrolling
`GtkScrolledWindow`**, sharing only the grid's vertical `GtkAdjustment`. Its
factory renders `GtkListItem::position() + 1` and reads no result data at all.

1. **Excluded from copy by construction.** Nothing in `ResultModel` can produce
   a row number. The only row-addressed text it returns comes from
   `with_cell(row, col)`, and `col` indexes the result's own columns — there is
   no column index that yields the gutter. A copy path serialises selected
   cells through that one call, so the number is not filtered out of the
   clipboard; it never had a route to it.
2. **Pinned by construction.** It lives outside the horizontal scroller, so
   horizontal scrolling cannot move it. `GtkColumnView` has no frozen columns,
   so a leading *column* could not have this property.
3. **Cannot break virtualisation.** `position() + 1` is a pure function of the
   row index. Painting the gutter never touches the pager, so scrolling it costs
   zero row fetches.

The gutter shares this same `ResultModel` rather than a parallel model of
numbers, so its row count tracks the grid's through the same `items_changed` and
cannot drift. That is why `ResultRow` is cached by index: both views ask for the
same positions, so one flyweight serves both.

## Sorting

Header clicks emit SQL — a derived `ORDER BY` against the engine — and do not
sort loaded rows. **No `GtkSorter` is ever attached to this model.** Each column does carry one —
`GtkColumnView` will not make a header clickable without it — but it is a
`GtkCustomSorter` that answers `Equal` for every pair, and nothing consumes it:
there is no `GtkSortListModel` anywhere. The click is intercepted through
`GtkColumnViewSorter::changed`, turned into an `ORDER BY` in `sql.rs` and
re-issued; the sorter's only remaining job is the arrow in the header.
Sorting the
2,048 rows that happen to be resident out of a 500,000-row result and calling
the result sorted is precisely the lie the macOS grid refuses to tell.

## Building

```
sudo apt-get install libgtk-4-dev libadwaita-1-dev libdbus-1-dev pkg-config
cargo test --manifest-path ui/gtk4/Cargo.toml
```

**CI is not the only compiler.** `brew install gtk4 libadwaita` puts GTK 4.22 and
libadwaita 1.9 on the Mac the rest of the project is developed on, and `gtk4-rs`
builds against them through `pkg-config` with nothing else configured — the whole
front-end compiles, runs and renders there. That is a development convenience and
not a supported target: the shipping route is the Flatpak, and the floor
`linux-gtk4.yml` asserts is what a change has to hold to.

The window is hard to look at through a screen-sharing session, so
`examples/preview.rs` seeds a SQLite profile, runs a statement and renders the
realised window straight to a PNG with `GtkWidgetPaintable` — no visible window
needed, and the row-number alignment is a thing you can look at rather than
reason about:

```
PREVIEW_DIR=/tmp/dg PREVIEW_PNG=/tmp/dg/window.png cargo run --example preview
```

It runs three statements a second apart (one DDL, one that fails, one that
streams 5,000 rows), clicks a cell, describes a table, and writes three PNGs:
`window.png` with the inspector open, `window-history.png` with the history
page and its detail strip, and `window-rerun.png` after driving `history.rerun`
— where the entry reads `×2`, which is the replay having gone through the run
path rather than around it.
