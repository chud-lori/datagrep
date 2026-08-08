#!/usr/bin/env bash
# ci/gates.sh — the Tier-1 gate.
#
# Tier-1 = every PR, hardware-independent, deterministic, <8 min. No
# wall-clock timing assertions live here — those need real hardware and
# belong to the Tier-2 nightly fleet (see ci/README.md).
#
# Runs identically locally and in CI (.github/workflows/ci.yml calls this
# file verbatim). Usage:
#
#   ./ci/gates.sh
#
# Deliberately does NOT hardcode which crates exist under crates/*: other
# crates may be mid-write at any given commit, so every cargo invocation
# uses --workspace and lets cargo tell us what's actually there. A build/
# test/clippy failure caused by a half-finished sibling crate is a REAL
# gate failure, not something this script papers over — see ci/README.md
# for how to read the output.

set -euo pipefail

# ---------------------------------------------------------------------------
# Setup
# ---------------------------------------------------------------------------

script_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" >/dev/null 2>&1 && pwd)"
repo_root="$(cd -- "${script_dir}/.." >/dev/null 2>&1 && pwd)"
cd "${repo_root}"

allowlist="${repo_root}/ci/grep-allowlist.txt"

overall_status=0
failed_gates=()

note() { printf '\n=== %s ===\n' "$1"; }

fail_gate() {
  overall_status=1
  failed_gates+=("$1")
  printf 'GATE FAILED: %s\n' "$1"
}

warn() {
  printf 'WARN: %s\n' "$1"
}

# ---------------------------------------------------------------------------
# 0. Build xtask (grep-gates / budget-check / count-crates). xtask opts out
#    of the root workspace on purpose (see xtask/Cargo.toml), so it is built
#    separately here rather than picked up by `cargo build --workspace`.
# ---------------------------------------------------------------------------

note "Building xtask"
if ! (cd "${repo_root}/xtask" && cargo build --release --quiet); then
  echo "gates.sh: xtask failed to build — the gate itself is broken, fix xtask/ first" >&2
  exit 2
fi

xtask_target_dir="${CARGO_TARGET_DIR:-${repo_root}/xtask/target}"
xtask_bin="${xtask_target_dir}/release/xtask"
if [[ ! -x "${xtask_bin}" ]]; then
  echo "gates.sh: xtask binary not found at ${xtask_bin} after a successful build" >&2
  exit 2
fi

# ---------------------------------------------------------------------------
# 1. cargo fmt --check
# ---------------------------------------------------------------------------

note "cargo fmt --check"
if cargo fmt --all --check; then
  echo "fmt: OK (workspace)"
else
  fail_gate "cargo fmt --check (workspace)"
fi

# xtask is a standalone package (its own [workspace] opt-out), so `--all`
# above never reaches it. Check it separately — it's still gated code.
if ! (cd "${repo_root}/xtask" && cargo fmt --check); then
  fail_gate "cargo fmt --check (xtask)"
fi

# ---------------------------------------------------------------------------
# 2. Supply chain: cargo-audit (RUSTSEC advisories) and cargo-deny
#    (advisories + licence policy + banned crates + allowed registries).
#    The policy — including the written reasoning behind every accepted
#    advisory — lives in deny.toml. Read that file before changing anything
#    in this section.
#
#    Runs ahead of clippy/test on purpose: both tools read Cargo.lock and
#    compile nothing, so they finish in seconds, and learning that a
#    dependency is vulnerable should not cost a five-minute build first.
#    Both do need network to fetch the RustSec advisory database.
#
#    Missing-tool policy: WARN locally, HARD FAIL in CI. `cargo install`-ing
#    either tool from source takes ~4 minutes, which would roughly double the
#    Tier-1 budget and be paid by every contributor on every clean checkout —
#    so a local `./ci/gates.sh` does not depend on having them. CI installs
#    both as prebuilt binaries in a couple of seconds (taiki-e/install-action,
#    see .github/workflows/ci.yml) before calling this script, so the gate
#    genuinely runs on every PR. `CI` is set by every CI provider, so a tool
#    missing *there* means the workflow is broken rather than that someone
#    skipped a local install, and passing quietly would leave a gate that
#    only looks enforced.
# ---------------------------------------------------------------------------

note "Supply chain (cargo-audit + cargo-deny)"

supply_chain_missing() {
  # $1 = tool name, $2 = install command to suggest
  if [[ -n "${CI:-}" ]]; then
    fail_gate "$1 is not installed (CI must install it — see .github/workflows/ci.yml)"
  else
    warn "$1 not installed, supply-chain gate skipped locally. Install with: $2"
  fi
}

# The accepted-advisory list is parsed out of deny.toml rather than repeated
# here. cargo-audit cannot read deny.toml — it wants its own .cargo/audit.toml
# in its own format — and two hand-maintained lists of advisory IDs would drift
# the first time someone accepted one in a hurry, leaving a gate that is green
# for the wrong reason. Matching only the `{ id = "RUSTSEC-...` form means a
# prose mention of an ID elsewhere in deny.toml cannot silently widen the
# exemption.
audit_ignore_args=()
while IFS= read -r adv_id; do
  [[ -n "${adv_id}" ]] || continue
  audit_ignore_args+=(--ignore "${adv_id}")
done < <(grep -E '^[[:space:]]*\{[[:space:]]*id[[:space:]]*=[[:space:]]*"RUSTSEC-' \
           "${repo_root}/deny.toml" | grep -oE 'RUSTSEC-[0-9]{4}-[0-9]{4}' || true)

# `--deny warnings` promotes unmaintained/unsound/yanked findings to failures,
# so cargo-audit matches deny.toml, which has no warn tier for advisories
# either. The `${arr[@]+...}` dance is not decoration: under `set -u`, bash 3.2
# (what macOS ships as /bin/bash) treats an empty "${arr[@]}" as unbound and
# aborts the script, which would silently happen the day the ignore list is
# emptied — i.e. exactly when the gate is finally clean.
if cargo audit --version >/dev/null 2>&1; then
  if cargo audit --deny warnings ${audit_ignore_args[@]+"${audit_ignore_args[@]}"}; then
    echo "cargo-audit: OK"
  else
    fail_gate "cargo audit (RUSTSEC advisory)"
  fi
else
  supply_chain_missing "cargo-audit" "cargo install cargo-audit --locked"
fi

# cargo-deny output is captured and printed only on failure. `multiple-versions`
# is deliberately a warn tier (see deny.toml) and currently reports 27 duplicated
# crates across ~300 lines; dumping that into every green run is how people learn
# to stop reading gate output altogether. On failure the whole thing is printed
# verbatim, which is the case where you want it.
if cargo deny --version >/dev/null 2>&1; then
  if deny_out="$(cargo deny check --hide-inclusion-graph 2>&1)"; then
    deny_warns="$(printf '%s\n' "${deny_out}" | grep -c 'warning\[' || true)"
    echo "cargo-deny: OK (${deny_warns} warning(s); run 'cargo deny check bans' to read them)"
  else
    printf '%s\n' "${deny_out}"
    fail_gate "cargo deny check (advisories/licences/bans/sources)"
  fi
else
  supply_chain_missing "cargo-deny" "cargo install cargo-deny --locked"
fi

# ---------------------------------------------------------------------------
# 3. cargo clippy --workspace --all-targets -- -D warnings
# ---------------------------------------------------------------------------

note "cargo clippy --workspace --all-targets -- -D warnings"
if cargo clippy --workspace --all-targets -- -D warnings; then
  echo "clippy: OK"
else
  fail_gate "cargo clippy --workspace --all-targets"
fi

# ---------------------------------------------------------------------------
# 4. cargo test --workspace
# ---------------------------------------------------------------------------

note "cargo test --workspace"
if cargo test --workspace; then
  echo "test: OK"
else
  fail_gate "cargo test --workspace"
fi

# ---------------------------------------------------------------------------
# 5. Anti-pattern greps, via xtask grep-gates.
#    HARD FAIL: unbounded_channel in crates/datagrep-core/src, ControlFlow::Poll
#    outside spike/bench/test dirs, tokio::time::interval anywhere in gated
#    source. WARN (count only): .unwrap() in non-test src/. WARN: format!
#    in *render*/*paint* files. ci/grep-allowlist.txt waives justified
#    non-data-path cases; see that file's header for the format.
# ---------------------------------------------------------------------------

note "Anti-pattern greps"
if "${xtask_bin}" grep-gates "${repo_root}" --allowlist "${allowlist}"; then
  echo "grep-gates: OK"
else
  fail_gate "grep-gates (banned anti-patterns)"
fi

# ---------------------------------------------------------------------------
# 6. Binary size vs budget.toml P10 (informational)/P11 (installed-on-disk).
#    WARN-only for now: no application binary target exists yet (M0 is still
#    landing crates/*). Wired via `xtask budget-check` so that the moment a
#    `datagrep` binary exists, pointing this step at it turns P11 into a hard
#    fail with zero further changes to this script.
# ---------------------------------------------------------------------------

note "Binary size vs budget.toml (P10/P11) — WARN-only, no bin target yet"
app_target_dir="${CARGO_TARGET_DIR:-${repo_root}/target}"
app_bin="${app_target_dir}/release/datagrep"
if [[ -f "${app_bin}" ]]; then
  if "${xtask_bin}" budget-check "${app_bin}" --budget "${repo_root}/budget.toml"; then
    echo "budget-check: OK"
  else
    fail_gate "budget-check (P11 fail threshold breached)"
  fi
else
  warn "no '${app_bin}' found — P10/P11 size gate is wired (xtask budget-check) but not yet enforced. Once a crate produces the datagrep application binary, point app_bin above at it and this becomes a hard fail on P11 breach."
fi

# ---------------------------------------------------------------------------
# 7. Crate count vs P16d (limit 400) — WARN-only.
# ---------------------------------------------------------------------------

note "Crate count vs P16d (limit 400) — WARN-only"
if ! "${xtask_bin}" count-crates "${repo_root}" --budget "${repo_root}/budget.toml"; then
  warn "count-crates could not run (cargo tree failed — likely a mid-write crate elsewhere in crates/*); not gating on it"
fi

# ---------------------------------------------------------------------------
# Summary
# ---------------------------------------------------------------------------

note "Summary"
if [[ "${overall_status}" -eq 0 ]]; then
  echo "gates.sh: ALL GATES PASSED"
else
  echo "gates.sh: ${#failed_gates[@]} gate(s) FAILED:"
  for g in "${failed_gates[@]}"; do
    echo "  - ${g}"
  done
fi

exit "${overall_status}"
