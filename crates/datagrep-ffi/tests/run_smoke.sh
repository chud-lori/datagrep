#!/usr/bin/env bash
# Link tests/smoke.c against libdatagrep_ffi.a the way the Swift app does.
# PROFILE=debug for a debug build; MANIFEST overrides the cargo manifest.
set -euo pipefail

CRATE_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
PROFILE="${PROFILE:-release}"
export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-/Users/nurchudlori/Projects/dbx/target-ffi}"

CARGO_FLAGS=(-p datagrep-ffi)
[ "$PROFILE" = "release" ] && CARGO_FLAGS+=(--release)
[ -n "${MANIFEST:-}" ] && CARGO_FLAGS+=(--manifest-path "$MANIFEST")

echo "--- cargo build ${CARGO_FLAGS[*]}"
cargo build "${CARGO_FLAGS[@]}"

LIB_DIR="$CARGO_TARGET_DIR/$PROFILE"
STATIC="$LIB_DIR/libdatagrep_ffi.a"
[ -f "$STATIC" ] || { echo "no $STATIC"; exit 1; }

OUT="$LIB_DIR/datagrep_smoke"
# The system libraries a Swift app must also pass; SQLite is bundled inside the archive.
LINK_FLAGS=(-lc++ -framework Security -framework CoreFoundation -framework SystemConfiguration -lresolv -liconv)

echo "--- cc smoke.c"
set -x
cc -std=c11 -Wall -Wextra -Werror -O1 \
   -I"$CRATE_DIR/include" \
   "$CRATE_DIR/tests/smoke.c" "$STATIC" \
   "${LINK_FLAGS[@]}" \
   -o "$OUT"
set +x

TMPDIR_RUN="$(mktemp -d)"
trap 'rm -rf "$TMPDIR_RUN"' EXIT
echo "--- run (DATAGREP_SMOKE_DIR=$TMPDIR_RUN)"
DATAGREP_SMOKE_DIR="$TMPDIR_RUN" "$OUT"
