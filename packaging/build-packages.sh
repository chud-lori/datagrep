#!/usr/bin/env bash
# Build datagrep .deb and .rpm packages from the already-built Qt binary.
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BINARY="${1:-$REPO_ROOT/ui/linux/build/datagrep}"
OUT_DIR="${OUT_DIR:-$REPO_ROOT/dist}"
# Staged under dist/ (gitignored) so packaging never litters the tree.
STAGE="$OUT_DIR/.pkgroot"

if [[ ! -x "$BINARY" ]]; then
    echo "error: built binary not found at $BINARY" >&2
    echo "build ui/linux first (see header of this script), or pass the path" >&2
    exit 1
fi
command -v fpm >/dev/null 2>&1 || { echo "error: fpm not on PATH (gem install fpm)" >&2; exit 1; }

VERSION="${VERSION:-$(sed -n 's/^version = "\(.*\)"$/\1/p' "$REPO_ROOT/Cargo.toml" | head -n1)}"
if [[ -z "$VERSION" ]]; then
    echo "error: could not determine version (set VERSION=...)" >&2
    exit 1
fi

rm -rf "$STAGE"
install -Dm755 "$BINARY" "$STAGE/usr/bin/datagrep"
install -Dm644 "$REPO_ROOT/packaging/datagrep.desktop" \
               "$STAGE/usr/share/applications/datagrep.desktop"
install -Dm644 "$REPO_ROOT/packaging/icons/datagrep.png" \
               "$STAGE/usr/share/icons/hicolor/256x256/apps/datagrep.png"
install -Dm644 "$REPO_ROOT/packaging/icons/datagrep.svg" \
               "$STAGE/usr/share/icons/hicolor/scalable/apps/datagrep.svg"

# Strip the staged copy only — the original build output stays intact.
if command -v strip >/dev/null 2>&1; then
    strip --strip-unneeded "$STAGE/usr/bin/datagrep" || true
fi

mkdir -p "$OUT_DIR"

COMMON=(
    -s dir
    --name datagrep
    --version "$VERSION"
    --architecture native
    --license "Apache-2.0"
    --url "https://github.com/chud-lori/datagrep"
    --maintainer "Lori <imlori000@gmail.com>"
    --description "Native Qt6 UI for datagrep: browse schemas and run queries against your databases."
    --chdir "$STAGE"
    --package "$OUT_DIR"
    --force
)

fpm "${COMMON[@]}" -t deb \
    --depends libqt6core6 \
    --depends libqt6gui6 \
    --depends libqt6widgets6 \
    --depends libqt6network6 \
    --depends libdbus-1-3 \
    --depends zlib1g \
  # Recommends, not hard deps: the app runs without them, saved passwords do not.
    --deb-recommends "gnome-keyring | kwalletd6" \
    --deb-recommends qt6-gtk-platformtheme \
    --deb-recommends qt6-xdgdesktopportal-platformtheme \
    usr

fpm "${COMMON[@]}" -t rpm \
    --depends qt6-qtbase-gui \
    --depends dbus-libs \
    --depends zlib \
    usr

echo
echo "packages in $OUT_DIR:"
ls -l "$OUT_DIR"/datagrep*.deb "$OUT_DIR"/datagrep*.rpm
