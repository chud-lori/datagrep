#!/usr/bin/env bash
# Build datagrep-<version>-x86_64.AppImage from the already-built Qt binary.
#
# This script does NOT build anything — it packages the output of the existing
# CMake build (see ui/linux/README.md). Build first:
#
#   cargo build -p datagrep-ffi --release
#   cmake -S ui/linux -B ui/linux/build -DCMAKE_BUILD_TYPE=Release -DDATAGREP_BUILD_RUST=OFF
#   cmake --build ui/linux/build
#
# Then:
#
#   packaging/build-appimage.sh [path/to/datagrep-binary]
#
# Tooling: linuxdeploy + linuxdeploy-plugin-qt (downloaded on first run into
# dist/.appimage-work/tools). The qt plugin walks the binary's Qt dependencies and
# bundles the Qt runtime + platform plugins into the AppDir; linuxdeploy then
# emits a single portable AppImage. References:
#   https://docs.appimage.org/packaging-guide/from-source/native-binaries.html
#   https://github.com/linuxdeploy/linuxdeploy-plugin-qt
#
# Env overrides:
#   VERSION   package version   (default: workspace version from Cargo.toml)
#   OUT_DIR   output directory  (default: <repo>/dist)
#   QMAKE     qmake binary the qt plugin should query (default: qmake6 if found)
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

# The qt plugin locates Qt by interrogating qmake. On Debian/Ubuntu the Qt6
# qmake is `qmake6` (bare `qmake` may be Qt5 or absent), so point the plugin
# at it explicitly unless the caller already did.
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

# linuxdeploy writes the AppImage into the current directory; the plugin is
# discovered because it sits next to (or on PATH relative to) the invocation —
# pass the AppDir + inputs explicitly and run from OUT_DIR.
export PATH="$TOOLS_DIR:$PATH"
cd "$OUT_DIR"
"$LINUXDEPLOY" \
    --appdir "$APPDIR" \
    --executable "$BINARY" \
    --desktop-file "$REPO_ROOT/packaging/datagrep.desktop" \
    --icon-file "$REPO_ROOT/packaging/icons/datagrep.png" \
    --plugin qt \
    --output appimage

echo
echo "AppImage(s) in $OUT_DIR:"
ls -l "$OUT_DIR"/datagrep*.AppImage
