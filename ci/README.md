# ci/

What runs when, and how to change it. Background: design doc §6
(measurement harness) and §11 point 3 ("write `budget.toml` and the Tier-1
CI gate before the first feature").

## What runs when

| | Trigger | Where | Script/workflow | Runtime |
|---|---|---|---|---|
| **Tier-1** | Every PR, every push to `main` | GitHub-hosted `ubuntu-latest` + `macos-latest` | `ci/gates.sh` via `.github/workflows/ci.yml` | Target <8 min (§6) |
| **Tier-2** | Nightly | Real hardware fleet | **not wired up yet — TODO, see below** | N/A |

### Tier-1 — `ci/gates.sh`

Hardware-independent and deterministic on purpose: no wall-clock frame-time
or latency assertions live here, because shared CI runners have 20–50%
timing variance and a gate that flakes gets disabled within a month (§6,
"Rules that keep the gate alive"). What Tier-1 actually checks, in order:

1. **`cargo fmt --all --check`** — workspace, plus `xtask` separately (it
   opts out of the root workspace on purpose, see `xtask/Cargo.toml`).
2. **`cargo clippy --workspace --all-targets -- -D warnings`**.
3. **`cargo test --workspace`**.
4. **The §5.2 anti-pattern greps**, via `xtask grep-gates` (source:
   `xtask/src/main.rs`):
   - `unbounded_channel` in `crates/dbx-core/src` — **hard fail**. Design:
     "any unbounded channel in the data path = re-implemented DBeaver."
     (Elsewhere in the tree it's a warning, not a fail — still worth
     justifying via the allowlist if intentional.)
   - `ControlFlow::Poll` outside `tests/`, `benches/`, `bench/`,
     `examples/`, or a `spike*` directory — **hard fail**. Costs
     1,800–9,000 ms CPU per 60 s idle (§5.0).
   - `tokio::time::interval` anywhere in gated source — **hard fail**.
     The only permitted timer is the armed-on-demand `DelayQueue`.
   - `.unwrap()` in non-test `src/` — **warning**, count reported, never
     blocks the build.
   - `format!` in a file whose path contains `render` or `paint` —
     **warning**. Should be `itoa`/`ryu` into a per-frame arena instead.

   Justified exceptions go in `ci/grep-allowlist.txt` (format documented in
   that file's header) — never silence a finding by editing the grep
   logic itself.
5. **Binary size vs `budget.toml` P10/P11** — **WARN-only for now**,
   because no crate produces the `dbx` application binary yet. Already
   wired via `xtask budget-check`; the moment a real binary exists, point
   `gates.sh`'s `app_bin` variable at it and this becomes a hard fail on a
   P11 (installed-on-disk, 90 MB) breach with no further changes.
6. **Crate count vs P16d (≤400 target, >600 fail)** — **WARN-only**, via
   `xtask count-crates`. `xtask` itself supports `--strict` to turn this
   into a hard fail later; `gates.sh` doesn't pass it yet.

`gates.sh` never hardcodes which crates exist under `crates/*` — every
`cargo` invocation uses `--workspace`, and if a sibling crate is mid-write
(no `src/lib.rs` yet, for instance), `cargo fmt`/`clippy`/`test` will
legitimately fail because `cargo` can't even load the workspace manifest.
**That is gates.sh correctly reporting a real problem, not a broken gate**
— read the actual `cargo` error before assuming the script is at fault.

### Tier-2 — nightly, real hardware (TODO, not implemented)

Per §6: P9 (scroll frame time), P12/P13 (idle CPU/wakeups), P19–P26 (present
count, GPU busy/power/memory, idle energy, compositor interactions) all need
real timing on real GPUs — lavapipe/WARP software rendering is 20–200×
slower per fragment with different pipeline-compile behavior, so it's
useless for frame-time gates even though it's fine for Tier-1's correctness
and CPU-side counters.

**Minimum fleet, none of it provisioned yet:**

- [ ] One Apple Silicon Mac mini
- [ ] One Windows box with a discrete NVIDIA/AMD GPU
- [ ] One Windows box with an Intel iGPU
- [ ] One Linux box on Mesa

Rules to build in when this lands (§6): fail on **absolute** budget breach
immediately; fail on **relative** regression only after 3 consecutive
nightlies (shared-runner-style variance doesn't apply to dedicated
hardware, but noise still exists). Store every run in a time series and
publish the graph. On failure, auto-upload the flamegraph, the `dhat`
file, and the Tracy trace. Also still open: baking `fixtures/postgres/`
into a versioned image/volume snapshot instead of reseeding 1M+ rows every
run (see `fixtures/README.md`) — Tier-2 is where that would actually get
used regularly enough to matter.

## How to add a budget line

1. Add the row to `budget.toml` in the repo root: a new `[Pxx]` table with
   `metric`, `target`, `fail` (all quoted strings — the parser in
   `xtask/src/main.rs` only understands `[Table]` + `key = "string"`, by
   design, so `xtask` stays dependency-free and builds offline on any
   runner).
2. Decide which tier it belongs in:
   - **Deterministic, hardware-independent** (a size, a count, a grep, an
     allocation assertion) → wire it into `ci/gates.sh` as a new numbered
     step, following the existing WARN vs hard-fail pattern. If it needs
     new logic beyond what `xtask` already does (`budget-check`,
     `count-crates`, `grep-gates`), add a new `xtask` subcommand — keep the
     parsing/measurement logic in Rust and unit-tested there, keep
     `gates.sh` itself as thin orchestration.
   - **Needs real timing, a GPU, or power measurement** → it belongs on the
     Tier-2 nightly fleet once that exists (see TODO above); don't try to
     approximate it on shared Tier-1 runners, that's how gates get muted.
3. If it's a hard fail, make sure the failure message names the budget
   line (`P<n>`) and points at the design doc section — someone hitting
   this in CI six months from now should not have to go spelunking for
   context.

## Design source

`../dbx-design.md` (repo root's parent directory — the design doc lives
one level above this repo) §5 (budget), §5.2 (banned anti-patterns — source
of the grep rules), §6 (measurement harness — source of the Tier-1/Tier-2
split and the fixture list), §11 point 3 (why this exists before the first
feature does).
