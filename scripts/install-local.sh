#!/usr/bin/env bash
# Build the app from the current checkout and install it as THE app, so what
# you are testing is the only datagrep on the machine.
#
# This exists because "build it and try it" kept producing two bundles — one in
# ui/macos and one in /Applications, at different versions — and it was never
# obvious which one was being launched. There is only ever one now.
#
# Usage:
#   ./scripts/install-local.sh
#
# Note the version it reports comes from VERSION= in ui/macos/build-app.sh, so a
# build made before a version bump carries the OLD number even though it has the
# NEW code. The commit is what matters; the number only becomes meaningful once
# scripts/deploy.sh has bumped it.
set -euo pipefail

REPO_ROOT="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." >/dev/null 2>&1 && pwd)"
APP="/Applications/datagrep.app"
BUILT="${REPO_ROOT}/ui/macos/datagrep.app"

blue() { printf "\033[34m%s\033[0m\n" "$*"; }
green() { printf "\033[32m%s\033[0m\n" "$*"; }

blue "-> building from $(git -C "$REPO_ROOT" rev-parse --short HEAD) ($(git -C "$REPO_ROOT" rev-parse --abbrev-ref HEAD))"
(cd "${REPO_ROOT}/ui/macos" && ./build-app.sh >/dev/null)

# A stale engine linked against fresh Swift is the worst failure mode here, so
# prove the bundle actually contains this checkout's Rust before installing it.
# `strings`, not `nm`: [profile.release] sets strip = "symbols", so the Rust
# symbol names are gone from the binary — only string literals survive.
# grep -c, NOT grep -q: `grep -q` exits on the first match, `strings` takes
# SIGPIPE, and under `set -o pipefail` the whole pipeline reports failure — so
# the check fired on a bundle that was perfectly fine.
if [ "$(strings "${BUILT}/Contents/MacOS/datagrep" 2>/dev/null | grep -c "could not read the profile store" || true)" -eq 0 ]; then
  echo "error: the built app has no datagrep engine symbols — refusing to install" >&2
  exit 1
fi

blue "-> replacing ${APP}"
pkill -f "datagrep.app/Contents/MacOS/datagrep" 2>/dev/null || true
sleep 1
rm -rf "${APP}"
# ditto, not cp: preserves the bundle's symlinks, permissions and signature.
ditto "${BUILT}" "${APP}"
# The build is local, so nothing quarantined it — but a previous DMG install may
# have left the flag on the path.
xattr -dr com.apple.quarantine "${APP}" 2>/dev/null || true

# Exactly one bundle, so there is nothing to confuse it with.
rm -rf "${BUILT}"

VERSION=$(defaults read "${APP}/Contents/Info.plist" CFBundleShortVersionString 2>/dev/null || echo "?")
green "installed ${APP} (reports ${VERSION}, built from $(git -C "$REPO_ROOT" rev-parse --short HEAD))"
echo
echo "  open ${APP}"
echo
echo "Happy with it? Then cut the release:"
echo "  ./scripts/deploy.sh v0.3.4     # bumps, tests, commits and tags LOCALLY"
echo "  git push origin main && git push origin v0.3.4"
