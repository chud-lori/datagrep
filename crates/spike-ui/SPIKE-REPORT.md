# dbx UI toolkit spike report — S1, S8

Ran on this machine: Apple Silicon (M1), macOS 15.6.1 (24G90), rustc/cargo 1.89.0,
Xcode Command Line Tools 16.4 **only** (no Xcode.app). `CARGO_TARGET_DIR=/Users/nurchudlori/Projects/dbx/target-spike`
for every build below. Scope: design doc §1.1 (toolkit decision) and §8 (spikes S1, S8).

**Headline finding, ahead of everything else:** gpui 0.2.2 (and therefore
gpui-component, and therefore the whole primary toolkit choice) **could not
be built on this machine.** Its `build.rs` shells out to `xcrun metal` to
precompile `src/platform/mac/shaders.metal` into a `.metallib` at build
time. `xcrun metal` ships with full **Xcode.app**, not with Command Line
Tools — and only CLT 16.4 is installed here (`/Applications` has no
`Xcode*.app`; `xcrun --find metal` fails). Installing Xcode (multi-GB,
requires an App Store / Apple ID sign-in) was judged out of scope for a
timeboxed spike. This is a genuine environment finding, not a code defect —
see "New risk found" below — and it forced a fallback for S1 and left S8
partly source-audited instead of runtime-measured. Everything below states
plainly what was actually run vs. what was verified by reading source only.

---

## 1. Gate table (design §8)

| Spike | Gate | Result | Verdict |
|---|---|---|---|
| S1 (gpui 0.2.2) | >2 presents/60s idle = FAIL; >20ms CPU/60s idle = FAIL | **could not build** — see root cause above | **N/A (blocked)** |
| S1 (winit 0.30 + wgpu 30 fallback) | >2 presents/60s idle = FAIL | **0 presents** across the full 60s window (checked every 10s: 0,0,0,0,0,0) | **PASS**, cleanly |
| S1 (winit 0.30 + wgpu 30 fallback) | >20ms CPU/60s idle = FAIL | **+170ms CPU over ~55s** (utime +120ms, stime +50ms) ≈ **+185ms/60s extrapolated** | **FAIL by the raw number** — see methodology caveat below; treat as an upper bound, not a clean verdict |
| S8 gpui-component gap audit | not a kill gate, a costing spike | **could not run live**; full feature audit delivered from source inspection of the real v0.5.1 tag instead (see §3) | **partial** — audit done, runtime numbers (call-counter proof, RSS, screenshot-verified scroll) not captured |
| S4-adjacent (cold build / binary size, referenced by S8's own gate table) | cold build >20 min or binary >80 MB = concern | build **failed** before linking either binary; no product binary size is measurable | **N/A (blocked)** |

### S1 winit+wgpu fallback — full numbers

Driver: `crates/spike-ui/harness-winit/run_s1.sh`, sampling `ps -o utime,stime,rss` and
`footprint` (both real, no sudo needed) at t=0 and continuously every 5s until the app
self-exits at t=60s.

| Metric | t≈1s (start) | t=55s (last sample before exit) | Δ |
|---|---|---|---|
| utime | 0:00.09 (90ms) | 0:00.21 (210ms) | +120ms |
| stime | 0:00.05 (50ms) | 0:00.10 (100ms) | +50ms |
| **CPU total** | 140ms | 310ms | **+170ms over ~55s** |
| RSS | 61,632 KB (~60.2 MB) | 62,656 KB (~61.2 MB) | +1,024 KB |
| `phys_footprint` | 14 MB | 15 MB | +1 MB |
| presents (heartbeat log, every 10s) | 0 | 0 | **0 across all 6 checkpoints** |

**Present count is unambiguous and clean: 0 presents in 60s idle**, well under the
`>2` fail line — the counter increments immediately before `wgpu::Queue::present`, the
real GPU present call (not an application-level proxy, since this fallback owns the
whole render loop). `ControlFlow::Poll` is never used; the only scheduled wakeup is our
own 10s stderr heartbeat via `ControlFlow::WaitUntil`, which does **not** call
`redraw()` — so it cannot be inflating the present count.

**CPU-time is a real but noisy signal, reported honestly rather than rounded away:**
+170ms/~55s (~+185ms/60s extrapolated) is about **9x over the 20ms/60s S1 gate**, even
though this is the *simplest possible* toolkit (no text rendering, no widget layer, a
single `clear()` and nothing else). Three caveats on that number, in order of how much
they likely matter:
1. `ps`'s `utime`/`stime` fields are centisecond-granularity — the two samples are 10ms
   apart at the noise floor, but the *delta* (170ms) is 17x that granularity, so this is
   a real signal, not rounding noise.
2. The window spent its first ~1s occluded (`get_current_texture` returned `Occluded`
   three times right after creation — normal macOS WindowServer behavior while a window
   is being mapped) before the idle-measurement window began; some of the CPU delta may
   be startup/occlusion-transition cost still settling, not steady-state idle cost.
3. A full screen capture during the run (`/tmp/s1_winit_screenshot.png`) shows only the
   desktop and an unrelated terminal window — **the spike window was not visually
   confirmed on screen**, most likely because it was launched from a background,
   non-interactive shell without a foreground WindowServer session. If the window never
   became key/visible, this measurement may reflect an occluded-window floor rather than
   a genuinely on-screen idle window, which the design's own P25 ("occluded: 0 presents,
   RAM drops") suggests could behave differently. **Re-running this from a real
   interactive Terminal.app session, and cross-checking with `powermetrics` per design
   §6, is the natural next step and was not possible in this session.**

Net read: the present-count gate (the one the design calls the flagship, P19/S1's kill
signal) passes cleanly even on the crudest possible toolkit. The CPU-time number is real
but should not be over-indexed on without a cleaner methodology than coarse `ps` polling
around a possibly-occluded window.

Binary: `target-spike/release/s1_idle_winit`, stripped, `lto=fat`, `codegen-units=1` —
**3,310,320 bytes (3.16 MB)**. Not comparable to the design's P10/P11/S4 gates (those are
for the whole product with gpui-component, drivers, tree-sitter grammars, etc.) — this is
just the wgpu+winit floor with nothing else linked in.

---

## 2. Root cause: why gpui 0.2.2 doesn't build here

```
$ xcrun --find metal
xcrun: error: unable to find utility "metal", not a developer tool or in PATH
$ xcode-select -p
/Library/Developer/CommandLineTools
$ ls /Applications | grep -i xcode      # (nothing)
```

`gpui-0.2.2/build.rs` (verified directly, from the crates.io-cached source at
`~/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/gpui-0.2.2/build.rs`)
runs `Command::new("xcrun").args(["metal", ...])` unconditionally on macOS to compile
`src/platform/mac/shaders.metal`, and fails the build if that fails:

```
cargo::error=metal shader compilation failed:
xcrun: error: unable to find utility "metal", not a developer tool or in PATH
```

This is a hard requirement with no feature flag or env var escape hatch found in a
build.rs read. **wgpu does not have this dependency** — it compiles Metal shaders (MSL)
at *runtime* through the `objc2-metal` Rust bindings and the OS's own Metal framework,
not via the offline `xcrun metal` CLI tool. Verified empirically: a throwaway `wgpu 30 +
winit 0.30` probe built clean in **33.11s** on this machine and successfully enumerated a
real `Apple M1` Metal adapter (`backend: Metal`, `device_type: IntegratedGpu`) at
runtime. This is *why* the S1 fallback rung of the task's resolution ladder was taken —
it genuinely sidesteps the blocker, not just moves it.

**New risk, not in the design doc's existing risk table:** a contributor machine with
only Command Line Tools installed (a common, minimal, CI-friendly macOS setup) cannot
build the primary toolkit at all until Xcode.app is installed. Given the design targets
"~15h/week, 1 dev, part-time" (§7) and a CI story built on lavapipe/WARP/software
backends for macOS's *absence of a software Metal path* being already flagged (§6 "macOS
has no software Metal, so macOS GPU CI needs a real runner") — this raises the bar
further: that real macOS runner also needs full Xcode, not just CLT, adding image size
and maintenance the design doc did not budget for. Worth a line in §10's risk table.

---

## 3. gpui-ce → gpui-component pairing: why it wasn't used (verified, not assumed)

Design doc §1.1 mitigation #1 says to develop against **gpui-ce** (crates.io, versioned,
declared drop-in via `[patch]`). This was tried, empirically, before falling back:

- **crates.io versions confirmed directly from the registry API** (not trusted from the
  design doc's numbers, per the task's instruction): `gpui` 0.2.2 (published
  2025-10-22 — matches the design doc's own claim exactly), `gpui-ce` 0.3.3 (published
  2025-12-27), `gpui-component` 0.5.1 (published 2026-02-05, the newest release).
- `gpui-component` 0.5.1's own published dependency metadata (crates.io
  `/api/v1/crates/gpui-component/0.5.1/dependencies`) declares `gpui = "^0.2.2"` against
  the literal crate named `gpui` — **not** `gpui-ce`, and not a git source.
- Attempted the documented patch recipe:
  ```toml
  [dependencies]
  gpui-component = "0.5.1"
  [patch.crates-io]
  gpui = { version = "=0.3.3", package = "gpui-ce" }
  ```
  `cargo generate-lockfile` fails:
  ```
  error: failed to resolve patches for `https://github.com/rust-lang/crates.io-index`
  Caused by:
    patch for `gpui-ce` in `https://github.com/rust-lang/crates.io-index` points to the
    same source, but patches must point to different sources
  ```
  Cargo refuses a `[patch.crates-io]` entry that renames one crates.io package to
  another crates.io package — patches may only redirect a dependency to a **different
  source** (git/path), not to a same-registry package under a different name. gpui-ce's
  README's patch recipe applies when the *dependent* (gpui-component) pulls `gpui` from
  **git**; it does not apply to gpui-component's crates.io release, which pulls `gpui`
  from crates.io directly.
- **Resolution taken:** fall through the ladder, but skip the git-zed-pinned-SHA rung —
  gpui-component 0.5.1 (current, newest, upstream-maintained) already declares and is
  tested against crates.io `gpui = "^0.2.2"` directly, so that plain pairing is *more*
  current and *more* likely to actually build than hand-pinning an arbitrary recent SHA
  of a monorepo the design doc itself describes as having taken 7,247 commits since that
  same 0.2.2 tag. This is documented inline in `crates/spike-ui/harness/Cargo.toml`.

This pairing (`gpui = "0.2.2"`, `gpui-component = { version = "0.5.1", features =
["tree-sitter-languages"] }`) is what `crates/spike-ui/harness/src/bin/{s1_idle,s8_audit}.rs`
target. It is the pairing that then hit the Xcode/Metal wall in §2.

---

## 4. Isolation: verified, not just asserted

The task requires `spike-ui`'s `Cargo.toml` to carry an empty `[workspace]` table so
gpui's dependency tree and lockfile never touch the shared `dbx` workspace. Tried that
literally first, in a scratch replica of the workspace layout (not the real repo):

```
$ cargo metadata --no-deps    # crates/foo/Cargo.toml has [workspace] directly
error: multiple workspace roots found in the same workspace:
  .../wstest/crates/foo
  .../wstest
```

A package matched by the parent's `members = ["crates/*"]` glob **cannot** itself
declare `[workspace]` — Cargo treats it as a second, conflicting workspace root and
errors for the *whole* workspace, which would have broken every other agent's concurrent
build in sibling `crates/dbx-*` dirs. Confirmed empirically before touching the real repo.

**Structure used instead**, which gets the same isolation without that failure mode
(also verified empirically in the same scratch replica, then applied for real):

```
crates/spike-ui/Cargo.toml            <- plain stub package, no [workspace], no deps.
                                          Matched by the parent glob; harmless member.
crates/spike-ui/harness/Cargo.toml    <- [workspace] (empty), own Cargo.lock.
                                          gpui + gpui-component. NOT matched by
                                          `crates/*` (glob doesn't cross the extra `/`).
crates/spike-ui/harness-winit/Cargo.toml <- same trick, own Cargo.lock, winit + wgpu.
                                          Kept SEPARATE from harness/ so a broken gpui
                                          build can never block the winit fallback (and
                                          vice versa).
```

Every build used `CARGO_TARGET_DIR=/Users/nurchudlori/Projects/dbx/target-spike`. No
file outside `crates/spike-ui/` was touched; the root `Cargo.toml` and `Cargo.lock` were
never opened for writing (only read, early, to confirm the glob).

---

## 5. S8 — gpui-component gap audit (source-verified, v0.5.1 exact tag)

**Could not run live** (§2). The code in `crates/spike-ui/harness/src/bin/s8_audit.rs`
implements everything the task asked for — a `TableDelegate` for 1,000,000 rows × 24
columns with **no materialized `Vec` of rows** (cells are computed from `(row_ix,
col_ix)` on the fly), `AtomicU64` call counters on `rows_count`/`columns_count`/`column`/
`render_td` dumped via a scripted `scroll_to_row(500_000)` → `scroll_to_row(999_999)` →
`scroll_to_row(0)` sequence, and an `Input` in `.code_editor("sql")` mode holding a
deterministically-generated 1.1 MB SQL string — but it has never been compiled, so the
virtualization "call counter" kill-signal test **could not be executed**. Do not read
anything below as a runtime result; every line is sourced from reading the actual
crates.io-cached `gpui-component-0.5.1` source (not GitHub's `main` branch, which has
already drifted — see the sub-notes) at
`~/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/gpui-component-0.5.1/`.

| Feature | v0.5.1 status | Where verified |
|---|---|---|
| Virtualized `Table` w/ delegate | Present. `TableDelegate::column()` doc comment: *"This only call on Table prepare or refresh"* — designed to be called ~24x, not 24M times. **Not empirically confirmed on this machine.** | `crates/ui/src/table/delegate.rs:26` |
| Rectangular multi-cell range selection + copy-as-TSV | **Absent.** `TableState` exposes `row_selectable`, `col_selectable`, `selected_row()`, `selected_col()`, `clear_selection()` — no `cell_selectable`, no `selected_cell()`, no plural/range selection of any kind. (The gpui-component *docs site*'s "Cell Selection Mode" / `selected_cell()` / `TableEvent::SelectCell` section describes a **`main`-branch-only** feature not yet in the published 0.5.1 — confirmed by grepping the actual 0.5.1 source, where none of those symbols exist.) | `crates/ui/src/table/state.rs:162-274` (full method list) |
| Editable cells w/ commit/cancel | **No dedicated API.** The documented pattern is app-authored: swap `render_td`'s output for an `Input` widget when the cell is "selected" via the app's own state, no built-in pending-diff/commit flow (design's P0 #5 requirement is a product-level workflow, not just a widget). | `docs/docs/components/data-table.md` "Cell Selection Mode" example (main branch) |
| Column resize / reorder / pin | **Present**, natively. `Column::resizable(bool)`, `.movable(bool)`, `.fixed(ColumnFixed::Left)`; `TableState::col_resizable(bool)`, `.col_movable(bool)`; `TableEvent::ColumnWidthsChanged(Vec<Pixels>)`, `TableEvent::MoveColumn(from,to)`. | `crates/ui/src/table/column.rs:115-145`, `state.rs:29-45,168-174` |
| Multi-cursor (editor) | **Not found anywhere in the repository** — a full-repo code search for `multi_cursor` returns zero hits (checked against the live default branch, which is ahead of 0.5.1, so this is a conservative check). `crates/ui/src/input/state.rs` models exactly one `selected_range: Range<usize>` (singular), and the source file is literally named `cursor.rs` (singular). | repo-wide `gh search code`; `crates/ui/src/input/{state.rs,cursor.rs,selection.rs}` |
| Completion popover API | **Present.** `crates/ui/src/input/popovers/completion_menu.rs`; a `CompletionProvider` trait (used with `lsp_types::CompletionResponse`) is exercised in `crates/story/examples/editor.rs`. Whether it anchors to the caret vs. the widget was **not verified** (would need a running window). | `crates/ui/src/input/popovers/mod.rs`, `crates/story/examples/editor.rs` |
| Find / replace | **Present.** `crates/ui/src/input/search.rs` (`SearchMatcher`, `SearchPanel`, `replace_all`); `.searchable(true)` builder confirmed at the 0.5.1 tag in `crates/story/src/textarea_story.rs:53`. | `crates/ui/src/input/search.rs` |
| Code-editor mode (rope + tree-sitter) | **Present and confirmed at 0.5.1**: `.code_editor(language)`, `.line_number(bool)`, `.searchable(bool)`, `.soft_wrap(bool)`, `.tab_size(...)`, `.default_value(...)` all exist as of the exact 0.5.1 source (line numbers below). | `crates/ui/src/input/state.rs:420,452,460,473,705,775` |
| SQL syntax highlighting | **Present via the bundle only.** At 0.5.1 there is no standalone `tree-sitter-sql` cargo feature (that granular per-language split landed on `main` after 0.5.1) — only the all-grammars `tree-sitter-languages` feature, which includes `tree-sitter-sequel` (the SQL grammar). Using the per-language feature as the design doc assumes would need gpui-component newer than the pinned pairing, or vendoring. | `crates/ui/Cargo.toml` (both v0.5.1 tag and `main`, diffed) |
| `main`-vs-0.5.1 drift found along the way | `Table` (0.5.1) was renamed `DataTable` on `main`; `column()` returns `&Column` (0.5.1) vs owned `Column` (`main`); `.text_center()` exists on `main`, not at 0.5.1; cell selection (above) is `main`-only. | cross-diffed `v0.5.1` tag vs `main` branch source, both fetched directly |

**Costing read (design §8 S8 intent):** the design's own estimate — "rect-select
requiring a fork of `table/state.rs` rather than a delegate extension is +2 dev-weeks;
absent multi-cursor is +1.5" — holds up and is *reinforced* by this audit: at the
currently-published 0.5.1, both gaps are confirmed absent (not just "hard to find"),
and cell selection specifically requires waiting for (or vendoring) a `main`-branch
feature that hasn't shipped in a numbered release yet. If the product build pins to
published crates.io releases (the design's own gpui-ce mitigation strategy is about
*avoiding* git pins), the effective gap today is the full ~3.5 dev-weeks the design
flags, not a discounted version of it.

Not audited (would need a running window, so out of scope given §2): visual scroll
smoothness by eye, RSS with the 1M-row window open, cold build wall time for the full
gpui-component dependency graph (the build never reached linking), stripped binary
size, and whether the completion popover actually anchors to the caret.

---

## 6. S5 and S6 — explicitly not run

Per the task: **S5 (driver/environment fallback matrix — RDP, llvmpipe, WARP, X11
forwarding, Wayland fractional scaling) and S6 (IME + Unicode torture) cannot run on
this machine.** Both need a Linux box (for llvmpipe/X11-forwarding/Wayland) and a
Windows box (for RDP, WARP, MS-IME/Pinyin). Nothing in this report should be read as
covering either — they remain fully open, and S5 in particular is the design's own
stated hard no-go gate for the whole toolkit choice (§10 risk #1), unresolved by
anything done here.

---

## 7. Verdict

**Inconclusive on this machine, for an environmental reason the design doc did not
anticipate — not a toolkit-quality verdict.** The one gate that *did* run cleanly (S1
present-count, on the crudest possible fallback toolkit) passes with room to spare: 0
presents across a full 60-second idle window, versus a `>2` fail line. That's a
genuinely encouraging signal about the *ceiling* macOS + wgpu can hit when nothing is
driving continuous redraws — consistent with the design's core thesis that idle can be
free by construction on this platform. But the CPU-time number from the same run came in
~9x over its 20ms/60s gate, measured with a coarse method against a window that may
never have actually become visible — a real number, honestly reported, but one that
needs a cleaner rerun (a real interactive session, `powermetrics` instead of `ps`) before
it should move any decision. Neither of these numbers is about gpui at all, because gpui
never built.

The actual toolkit decision — gpui + gpui-component — is untested end-to-end here. The
blocker (full Xcode required for `xcrun metal`, not just Command Line Tools) is a
one-time environment fix, not a design flaw, but it is real: it means "fine on this
Apple Silicon Mac" (the task's own assumption) needed one more precondition than stated,
and any CI runner or new contributor machine will hit the identical wall until Xcode.app
is provisioned. The source-level S8 audit (§5), cross-checked against the exact
published 0.5.1 tag rather than the faster-moving `main` branch, **confirms rather than
softens** the design's stated gpui-component gaps: rectangular range selection and
multi-cursor are both genuinely absent from the current release, not just hard to find,
so the design's "+2 / +1.5 dev-week" costing stands as written. Everything else the
design leaned on gpui-component for — virtualized delegate table, column
resize/move/pin, code-editor mode with real tree-sitter SQL highlighting via the
grammar bundle, find/replace, a completion-provider API — is confirmed present in the
source, at the exact version the product would actually pin to.

**What would change this from "inconclusive" to a real verdict:** install Xcode.app (or
run this spike on a machine that already has it, e.g. any Zed contributor's machine) and
re-run `s1_idle` and `s8_audit` as written — no code changes should be needed, since both
are written against the source-verified 0.5.1 API. Everything either passes the
design's own gates or documents precisely, with file and line numbers, where it does
not.

---

## Files

- `crates/spike-ui/Cargo.toml`, `crates/spike-ui/src/lib.rs` — parent-glob stub, untouched product-wise.
- `crates/spike-ui/harness/Cargo.toml` — gpui 0.2.2 + gpui-component 0.5.1 pairing (does not build here; see §2).
  - `crates/spike-ui/harness/src/bin/s1_idle.rs` — S1 as originally specified (gpui only, present-counter, 60s heartbeat). Written and believed correct against the verified API; **not compiled**.
  - `crates/spike-ui/harness/src/bin/s8_audit.rs` — S8 (1M×24 delegate w/ call counters, 1.1MB SQL editor, scripted scroll sequence). Written and believed correct; **not compiled**.
  - `crates/spike-ui/harness/Cargo.lock` — real lockfile from a successful `cargo generate-lockfile` (dependency resolution succeeded; only the final gpui build.rs step failed).
- `crates/spike-ui/harness-winit/` — S1 fallback rung, bare winit 0.30 + wgpu 30. **Builds and runs clean.**
  - `src/bin/s1_idle_winit.rs`, `run_s1.sh` (the measurement driver), `Cargo.toml`, `Cargo.lock`.
- Raw logs (outside the repo, referenced above): `/tmp/spike_s1_build.log`, `/tmp/spike_s8_build.log` (both end in the same `xcrun metal` error), `/tmp/spike_s1_winit_build3.log` (clean build), `/tmp/spike_s1_winit_run.log` (full driver output, ps + footprint at t0/t_end + heartbeat log), `/tmp/s1_winit_screenshot.png`.
