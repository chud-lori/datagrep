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
# 2. cargo clippy --workspace --all-targets -- -D warnings
# ---------------------------------------------------------------------------

note "cargo clippy --workspace --all-targets -- -D warnings"
if cargo clippy --workspace --all-targets -- -D warnings; then
  echo "clippy: OK"
else
  fail_gate "cargo clippy --workspace --all-targets"
fi

# ---------------------------------------------------------------------------
# 3. cargo test --workspace
# ---------------------------------------------------------------------------

note "cargo test --workspace"
if cargo test --workspace; then
  echo "test: OK"
else
  fail_gate "cargo test --workspace"
fi

# ---------------------------------------------------------------------------
# 4. Anti-pattern greps, via xtask grep-gates.
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
# 5. Binary size vs budget.toml P10 (informational)/P11 (installed-on-disk).
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
# 6. Crate count vs P16d (limit 400) — WARN-only.
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
