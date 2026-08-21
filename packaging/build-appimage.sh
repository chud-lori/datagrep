#!/usr/bin/env bash
# Build datagrep-<version>-x86_64.AppImage from the already-built Qt binary.
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BINARY="${1:-$REPO_ROOT/ui/linux/build/datagrep}"
OUT_DIR="${OUT_DIR:-$REPO_ROOT/dist}"
# Intermediates live under dist/ (gitignored) so packaging never litters the tree.
WORK_DIR="$OUT_DIR/.appimage-work"
TOOLS_DIR="$WORK_DIR/tools"
APPDIR="$WORK_DIR/AppDir"

if [[ ! -x "$BINARY" ]]; then
    echo "error: built binary not found at $BINARY" >&2
    echo "build ui/linux first (see header of this script), or pass the path" >&2
    exit 1
fi

ARCH="$(uname -m)"
if [[ "$ARCH" != "x86_64" ]]; then
    echo "error: only x86_64 is wired up (host is $ARCH)" >&2
    exit 1
fi

# Workspace version from the root Cargo.toml [workspace.package] block.
VERSION="${VERSION:-$(sed -n 's/^version = "\(.*\)"$/\1/p' "$REPO_ROOT/Cargo.toml" | head -n1)}"
if [[ -z "$VERSION" ]]; then
    echo "error: could not determine version (set VERSION=...)" >&2
    exit 1
fi
export VERSION   # linuxdeploy uses $VERSION for the output filename

if [[ -z "${QMAKE:-}" ]] && command -v qmake6 >/dev/null 2>&1; then
    export QMAKE="$(command -v qmake6)"
fi

# Let the AppImage tools run without FUSE (containers, CI runners).
export APPIMAGE_EXTRACT_AND_RUN=1

LINUXDEPLOY="$TOOLS_DIR/linuxdeploy-x86_64.AppImage"
PLUGIN_QT="$TOOLS_DIR/linuxdeploy-plugin-qt-x86_64.AppImage"
mkdir -p "$TOOLS_DIR" "$OUT_DIR"
fetch() {
    [[ -x "$2" ]] && return 0
    echo "downloading $(basename "$2")..."
    curl -fsSL --retry 3 -o "$2" "$1"
    chmod +x "$2"
}
fetch "https://github.com/linuxdeploy/linuxdeploy/releases/download/continuous/linuxdeploy-x86_64.AppImage" "$LINUXDEPLOY"
fetch "https://github.com/linuxdeploy/linuxdeploy-plugin-qt/releases/download/continuous/linuxdeploy-plugin-qt-x86_64.AppImage" "$PLUGIN_QT"

rm -rf "$APPDIR"

export DEPLOY_PLATFORM_THEMES="${DEPLOY_PLATFORM_THEMES:-1}"

export PATH="$TOOLS_DIR:$PATH"
cd "$OUT_DIR"
"$LINUXDEPLOY" \
    --appdir "$APPDIR" \
    --executable "$BINARY" \
    --desktop-file "$REPO_ROOT/packaging/datagrep.desktop" \
    --icon-file "$REPO_ROOT/packaging/icons/datagrep.png" \
    --plugin qt \
    --output appimage

if ! ls "$APPDIR"/usr/plugins/platformthemes/*.so >/dev/null 2>&1; then
    echo "error: no platform theme plugins in AppDir (install qt6-gtk-platformtheme)" >&2
    exit 1
fi

echo
echo "AppImage(s) in $OUT_DIR:"
ls -l "$OUT_DIR"/datagrep*.AppImage
