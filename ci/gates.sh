#!/usr/bin/env bash
# The Tier-1 gate.

set -euo pipefail

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

note "cargo fmt --check"
if cargo fmt --all --check; then
  echo "fmt: OK (workspace)"
else
  fail_gate "cargo fmt --check (workspace)"
fi

if ! (cd "${repo_root}/xtask" && cargo fmt --check); then
  fail_gate "cargo fmt --check (xtask)"
fi

note "Supply chain (cargo-audit + cargo-deny)"

supply_chain_missing() {
  # $1 = tool name, $2 = install command to suggest
  if [[ -n "${CI:-}" ]]; then
    fail_gate "$1 is not installed (CI must install it — see .github/workflows/ci.yml)"
  else
    warn "$1 not installed, supply-chain gate skipped locally. Install with: $2"
  fi
}

# cargo-audit cannot read deny.toml; ignored advisories are passed as flags instead.
audit_ignore_args=()
while IFS= read -r adv_id; do
  [[ -n "${adv_id}" ]] || continue
  audit_ignore_args+=(--ignore "${adv_id}")
done < <(grep -E '^[[:space:]]*\{[[:space:]]*id[[:space:]]*=[[:space:]]*"RUSTSEC-' \
           "${repo_root}/deny.toml" | grep -oE 'RUSTSEC-[0-9]{4}-[0-9]{4}' || true)

if cargo audit --version >/dev/null 2>&1; then
  # The ${arr[@]+...} expansion is required: bash 3.2 under `set -u` errors on empty arrays.
  if cargo audit --deny warnings ${audit_ignore_args[@]+"${audit_ignore_args[@]}"}; then
    echo "cargo-audit: OK"
  else
    fail_gate "cargo audit (RUSTSEC advisory)"
  fi
else
  supply_chain_missing "cargo-audit" "cargo install cargo-audit --locked"
fi

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

note "Go sidecar (gofmt, vet, test -race, mod verify, govulncheck, go-licenses)"

sidecar_dir="${repo_root}/sidecar"
govulncheck_pkg="golang.org/x/vuln/cmd/govulncheck@v1.7.0"
go_licenses_pkg="github.com/google/go-licenses/v2@v2.0.1"
# Same spirit as deny.toml [licenses]: anything unlisted fails, GPL/AGPL/LGPL/SSPL included.
go_licenses_allowed="0BSD,Apache-2.0,BSD-2-Clause,BSD-3-Clause,ISC,MIT,MPL-2.0"

if [[ ! -f "${sidecar_dir}/go.mod" ]]; then
  echo "go: no sidecar/go.mod, Go gates skipped"
elif ! go version >/dev/null 2>&1; then
  supply_chain_missing "go" "https://go.dev/dl/ (the version in sidecar/go.mod)"
else
  export GOFLAGS=-mod=readonly

  if grep -Eq '^(go|toolchain go)[[:space:]]*[0-9]+\.[0-9]+\.[0-9]+[[:space:]]*$' "${sidecar_dir}/go.mod"; then
    echo "go toolchain pin: OK"
  else
    fail_gate "sidecar/go.mod must pin a patch-level toolchain (go 1.N.P or toolchain go1.N.P)"
  fi

  gofmt_out="$(gofmt -l "${sidecar_dir}")"
  if [[ -z "${gofmt_out}" ]]; then
    echo "gofmt: OK"
  else
    printf '%s\n' "${gofmt_out}"
    fail_gate "gofmt -l (sidecar)"
  fi

  if go -C "${sidecar_dir}" mod verify; then
    echo "go mod verify: OK"
  else
    fail_gate "go mod verify (sidecar)"
  fi

  if go -C "${sidecar_dir}" vet ./...; then
    echo "go vet: OK"
  else
    fail_gate "go vet (sidecar)"
  fi

  if go -C "${sidecar_dir}" test -race ./...; then
    echo "go test -race: OK"
  else
    fail_gate "go test -race (sidecar)"
  fi

  if go -C "${sidecar_dir}" run "${govulncheck_pkg}" ./...; then
    echo "govulncheck: OK"
  else
    fail_gate "govulncheck (sidecar)"
  fi

  # Our own packages are Apache-2.0 via the repo-root LICENSE, which go-licenses cannot see from sidecar/.
  if go -C "${sidecar_dir}" run "${go_licenses_pkg}" check ./... \
       --allowed_licenses="${go_licenses_allowed}" --ignore "$(go -C "${sidecar_dir}" list -m)"; then
    echo "go-licenses: OK"
  else
    fail_gate "go-licenses check (sidecar dependency licence not allowed)"
  fi
fi

note "cargo clippy --workspace --all-targets -- -D warnings"
if cargo clippy --workspace --all-targets -- -D warnings; then
  echo "clippy: OK"
else
  fail_gate "cargo clippy --workspace --all-targets"
fi

note "cargo test --workspace"
if cargo test --workspace; then
  echo "test: OK"
else
  fail_gate "cargo test --workspace"
fi

note "Anti-pattern greps"
if "${xtask_bin}" grep-gates "${repo_root}" --allowlist "${allowlist}"; then
  echo "grep-gates: OK"
else
  fail_gate "grep-gates (banned anti-patterns)"
fi

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

note "Crate count vs P16d (limit 400) — WARN-only"
if ! "${xtask_bin}" count-crates "${repo_root}" --budget "${repo_root}/budget.toml"; then
  warn "count-crates could not run (cargo tree failed — likely a mid-write crate elsewhere in crates/*); not gating on it"
fi

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
