#!/usr/bin/env bash
# Build static dist/.sidecars/<goos>-<goarch>/datagrep-sidecar-<engine> for each engine and GOOS/GOARCH argument.
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
OUT_DIR="${OUT_DIR:-$REPO_ROOT/dist}"
# A dot-dir, so the `sha256sum *` and `dist/*` globs in CI never pick it up.
SIDECAR_OUT="$OUT_DIR/.sidecars"
ENGINES_DIR="$REPO_ROOT/sidecar/engines"

if [[ $# -gt 0 ]]; then
    TARGETS=("$@")
else
    TARGETS=(darwin/arm64 darwin/amd64 linux/amd64 linux/arm64)
fi

for target in "${TARGETS[@]}"; do
    if [[ ! "$target" =~ ^[a-z0-9]+/[a-z0-9]+$ ]]; then
        echo "error: target must be GOOS/GOARCH, got '$target'" >&2
        exit 1
    fi
    rm -rf "$SIDECAR_OUT/${target/\//-}"
    mkdir -p "$SIDECAR_OUT/${target/\//-}"
done

shopt -s nullglob
ENGINES=("$ENGINES_DIR"/*/)
shopt -u nullglob
if [[ ${#ENGINES[@]} -eq 0 ]]; then
    echo "no engines under sidecar/engines/; nothing to build"
    exit 0
fi
command -v go >/dev/null 2>&1 || { echo "error: go not on PATH" >&2; exit 1; }

export CGO_ENABLED=0
export GOFLAGS="${GOFLAGS:--mod=readonly}"

for engine_dir in "${ENGINES[@]}"; do
    engine="$(basename "$engine_dir")"
    # An engine may be its own module; otherwise it belongs to the sidecar/ module.
    if [[ -f "$engine_dir/go.mod" ]]; then
        module_dir="$engine_dir" pkg="."
    else
        module_dir="$REPO_ROOT/sidecar" pkg="./engines/$engine"
    fi
    for target in "${TARGETS[@]}"; do
        out="$SIDECAR_OUT/${target/\//-}/datagrep-sidecar-$engine"
        echo "==> $engine ($target)"
        (cd "$module_dir" && GOOS="${target%/*}" GOARCH="${target#*/}" \
            go build -trimpath -buildvcs=false -ldflags "-s -w -buildid=" -o "$out" "$pkg")
    done
done

echo
for target in "${TARGETS[@]}"; do
    ls -l "$SIDECAR_OUT/${target/\//-}"/datagrep-sidecar-*
done
