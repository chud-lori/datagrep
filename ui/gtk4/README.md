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
    ffi/mod.rs              # safe wrappers: Core / Query / RowWindow
    model/
      pager.rs              # bounded LRU over row windows (<= 4 pages resident)
      status.rs             # decoded datagrep_query_status_json
      row.rs                # the GListModel item: an index, nothing else
      result.rs             # ResultModel: GListModel over the windowed row API  <- core
  tests/streaming.rs        # the model against the real engine, headless
```

Widgets land on top of this. The model is first because `GtkColumnView` is
virtualised over a `GListModel`, so nothing above it can be right until this is.

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
sort loaded rows. **No `GtkSorter` is ever attached to this model.** Sorting the
2,048 rows that happen to be resident out of a 500,000-row result and calling
the result sorted is precisely the lie the macOS grid refuses to tell.

## Building

```
sudo apt-get install libgtk-4-dev libadwaita-1-dev libdbus-1-dev pkg-config
cargo test --manifest-path ui/gtk4/Cargo.toml
```

The results model is the exception to "CI is the compiler": it is gio, not GTK,
so `brew install glib` is enough to build and run the whole of it — engine
included — on the Mac the rest of the project is developed on. Everything above
this layer does need GTK4, and `.github/workflows/linux-gtk4.yml` is the only
place that exists.
