#!/usr/bin/env bash
# Build libdbx_ffi.a and link tests/smoke.c against it, exactly the way a
# Swift app links the same archive. Prints the real link line it used.
#
#   ./tests/run_smoke.sh              # release (what ships)
#   PROFILE=debug ./tests/run_smoke.sh
#
# `MANIFEST` may point at a workspace that resolves — the shared dbx workspace
# is written by several agents at once and can transiently fail to resolve,
# so the smoke test is verified against an isolated scratch workspace. Default
# is this crate's own manifest.
set -euo pipefail

CRATE_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
PROFILE="${PROFILE:-release}"
export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-/Users/nurchudlori/Projects/dbx/target-ffi}"

CARGO_FLAGS=(-p dbx-ffi)
[ "$PROFILE" = "release" ] && CARGO_FLAGS+=(--release)
[ -n "${MANIFEST:-}" ] && CARGO_FLAGS+=(--manifest-path "$MANIFEST")

echo "--- cargo build ${CARGO_FLAGS[*]}"
cargo build "${CARGO_FLAGS[@]}"

LIB_DIR="$CARGO_TARGET_DIR/$PROFILE"
STATIC="$LIB_DIR/libdbx_ffi.a"
[ -f "$STATIC" ] || { echo "no $STATIC"; exit 1; }

OUT="$LIB_DIR/dbx_smoke"
# The exact system libraries a Swift app must also pass. rusqlite is
# `bundled`, so SQLite itself is inside the archive; Security/CoreFoundation
# are the macOS keychain (dbx-secrets), and libresolv/libiconv come in via
# rustls/tokio.
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
echo "--- run (DBX_SMOKE_DIR=$TMPDIR_RUN)"
DBX_SMOKE_DIR="$TMPDIR_RUN" "$OUT"
