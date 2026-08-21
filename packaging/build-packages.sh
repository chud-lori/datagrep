#!/usr/bin/env bash
# Build datagrep .deb and .rpm packages from the already-built Qt binary.
#
# Uses fpm (https://fpm.readthedocs.io/) with a staged directory tree, because
# ui/linux/CMakeLists.txt intentionally has no install()/CPack wiring — we
# package the artifact the existing CMake build produces, not re-plumb the
# build. Build first:
#
#   cargo build -p datagrep-ffi --release
#   cmake -S ui/linux -B ui/linux/build -DCMAKE_BUILD_TYPE=Release -DDATAGREP_BUILD_RUST=OFF
#   cmake --build ui/linux/build
#
# Then:
#
#   packaging/build-packages.sh [path/to/datagrep-binary]
#
# Needs: fpm on PATH (gem install fpm), rpmbuild for the .rpm target
# (Debian/Ubuntu package `rpm`), and binutils' strip.
#
# Env overrides:
#   VERSION   package version   (default: workspace version from Cargo.toml)
#   OUT_DIR   output directory  (default: <repo>/dist)
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

# ---------------------------------------------------------------------------
# Stage the FHS tree both packages share.
# ---------------------------------------------------------------------------
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

# Shared metadata. Runtime dependencies are declared per target below:
# the Qt6 runtime, libdbus-1 (the keyring crate's Secret Service backend is
# D-Bus IPC), and zlib (flate2 inside the Rust static lib). glibc/libstdc++
# are omitted deliberately — they are essential on any system that can run
# apt/rpm. A Secret Service *provider* (GNOME Keyring / KWallet) is a
# Recommends, not a hard dep: the app runs without one, saved passwords don't.
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

# .deb — Debian/Ubuntu names. On Ubuntu 24.04 the time_t64-renamed packages
# (libqt6core6t64 etc.) Provide these unversioned names, so the deps resolve
# on both pre- and post-t64 distros.
fpm "${COMMON[@]}" -t deb \
    --depends libqt6core6 \
    --depends libqt6gui6 \
    --depends libqt6widgets6 \
    --depends libqt6network6 \
    --depends libdbus-1-3 \
    --depends zlib1g \
    --deb-recommends "gnome-keyring | kwalletd6" \
    usr

# .rpm — Fedora/RHEL names. qt6-qtbase-gui carries Qt6Gui/Widgets and pulls
# qt6-qtbase (Core); dbus-libs is libdbus-1.so.3. `zlib` is Provided by
# zlib-ng-compat on current Fedora.
fpm "${COMMON[@]}" -t rpm \
    --depends qt6-qtbase-gui \
    --depends dbus-libs \
    --depends zlib \
    usr

echo
echo "packages in $OUT_DIR:"
ls -l "$OUT_DIR"/datagrep*.deb "$OUT_DIR"/datagrep*.rpm
