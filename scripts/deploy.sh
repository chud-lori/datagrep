#!/usr/bin/env bash
# Release-prep script for datagrep. Adapted from chud-lori/rusty-requester's
# scripts/deploy.sh, with one deliberate difference:
#
#   THIS SCRIPT NEVER PUSHES. It commits the version bump and creates the
#   annotated tag locally, then prints the exact `git push` commands for you
#   to run yourself. Pushing the tag is what triggers the release workflow,
#   so the push stays a human decision.
#
# Usage:
#   ./scripts/deploy.sh v0.2.0
#   ./scripts/deploy.sh -y v0.2.0     # skip the confirmation prompt
#
# Steps:
#   1. Validate the tag format (vX.Y.Z) and preflight (clean tree, on main,
#      tag doesn't already exist locally or on origin).
#   2. Bump the version everywhere it lives, together so nothing drifts:
#        Cargo.toml            [workspace.package] version
#        ui/macos/build-app.sh VERSION= (the app bundle's Info.plist version)
#        ui/macos/Sources/datagrep-app/UpdateCheck.swift  fallbackVersion
#        docs/latest.json      the static update manifest on GitHub Pages
#   3. Refresh Cargo.lock, check formatting, run the workspace tests.
#      (Run ./ci/gates.sh yourself for the full Tier-1 gate.)
#   4. Show the diff and ask for confirmation unless -y/--yes is passed.
#   5. Commit the bump and create an annotated tag — locally only.
#   6. Print the push commands. It does NOT run them. Ever.

set -euo pipefail

red() { printf "\033[31m%s\033[0m\n" "$*" >&2; }
green() { printf "\033[32m%s\033[0m\n" "$*"; }
blue() { printf "\033[34m%s\033[0m\n" "$*"; }
yellow() { printf "\033[33m%s\033[0m\n" "$*"; }
dim() { printf "\033[2m%s\033[0m\n" "$*"; }

die() {
  red "error: $*"
  exit 1
}

usage() {
  cat >&2 <<EOF
usage: $0 [-y|--yes] vX.Y.Z

Bumps the version in Cargo.toml, ui/macos/build-app.sh, UpdateCheck.swift and
docs/latest.json together, runs fmt + tests, then commits and tags LOCALLY.

This script never pushes — that is a hard rule, not a missing feature. It
prints the exact 'git push' commands at the end for you to run yourself;
pushing the tag is what triggers the release workflow.

Options:
  -y, --yes    Skip the final interactive confirmation prompt.
  -h, --help   Show this help.
EOF
}

# --- Parse args ----------------------------------------------------------
ASSUME_YES=0
while [ $# -gt 0 ]; do
  case "$1" in
    -y | --yes)
      ASSUME_YES=1
      shift
      ;;
    -h | --help)
      usage
      exit 0
      ;;
    --)
      shift
      break
      ;;
    -*)
      usage
      die "unknown option: $1"
      ;;
    *)
      break
      ;;
  esac
done

if [ $# -lt 1 ]; then
  usage
  die "missing release tag"
fi
if [ $# -gt 1 ]; then
  usage
  die "too many arguments"
fi
TAG="$1"
if ! [[ "$TAG" =~ ^v[0-9]+\.[0-9]+\.[0-9]+$ ]]; then
  die "tag must look like vX.Y.Z (got: $TAG)"
fi
VERSION="${TAG#v}" # strip the leading 'v' -> 0.2.0

# --- cd to repo root -----------------------------------------------------
REPO_ROOT=$(git rev-parse --show-toplevel 2>/dev/null) || die "not inside a git repo"
cd "$REPO_ROOT"

BUILD_APP_SH="ui/macos/build-app.sh"
UPDATE_CHECK_SWIFT="ui/macos/Sources/datagrep-app/UpdateCheck.swift"

# --- Preflight -----------------------------------------------------------
blue "-> Preflight checks"

BRANCH=$(git rev-parse --abbrev-ref HEAD)
if [ "$BRANCH" != "main" ]; then
  die "must be on main (currently on: $BRANCH)"
fi

if ! git diff-index --quiet HEAD --; then
  red "error: working tree has uncommitted changes:"
  git status --short >&2
  exit 1
fi

if git rev-parse "$TAG" >/dev/null 2>&1; then
  die "tag $TAG already exists locally"
fi

if git ls-remote --exit-code --tags origin "$TAG" >/dev/null 2>&1; then
  die "tag $TAG already exists on origin"
fi

dim "   branch=main - tree clean - $TAG is new"

# --- Bump versions, all four files together ------------------------------
blue "-> Bumping version to $VERSION"

# Cargo.toml: only the version line inside [workspace.package], nothing else
# (dependency version specs must not be touched). awk, not a greedy sed.
awk -v new="$VERSION" '
    /^\[workspace\.package\]/ { in_pkg=1 }
    /^\[/ && !/^\[workspace\.package\]/ { in_pkg=0 }
    in_pkg && /^version *= *"/ && !done {
        sub(/"[^"]*"/, "\"" new "\"")
        done=1
    }
    { print }
' Cargo.toml >Cargo.toml.new && mv Cargo.toml.new Cargo.toml
grep -q "^version = \"$VERSION\"" Cargo.toml || die "failed to bump Cargo.toml"

# build-app.sh writes CFBundleShortVersionString into Info.plist — if this
# drifts, the app compares the wrong "current version" and the update notice
# misfires (shows after updating, or never shows).
[ -f "$BUILD_APP_SH" ] || die "$BUILD_APP_SH not found"
awk -v new="$VERSION" '
    /^VERSION="[0-9]/ && !done {
        sub(/"[^"]*"/, "\"" new "\"")
        done=1
    }
    { print }
' "$BUILD_APP_SH" >"$BUILD_APP_SH.new" && mv "$BUILD_APP_SH.new" "$BUILD_APP_SH"
chmod +x "$BUILD_APP_SH"
grep -q "^VERSION=\"$VERSION\"" "$BUILD_APP_SH" || die "failed to bump $BUILD_APP_SH"

# The Swift-side fallback (used when the binary runs outside a bundle).
[ -f "$UPDATE_CHECK_SWIFT" ] || die "$UPDATE_CHECK_SWIFT not found"
awk -v new="$VERSION" '
    /fallbackVersion = "/ && !done {
        sub(/"[^"]*"/, "\"" new "\"")
        done=1
    }
    { print }
' "$UPDATE_CHECK_SWIFT" >"$UPDATE_CHECK_SWIFT.new" && mv "$UPDATE_CHECK_SWIFT.new" "$UPDATE_CHECK_SWIFT"
grep -q "fallbackVersion = \"$VERSION\"" "$UPDATE_CHECK_SWIFT" || die "failed to bump $UPDATE_CHECK_SWIFT"

# The static update manifest GitHub Pages serves (docs/ on main). The release
# workflow re-asserts this file after the release publishes, so even a manual
# tag can't leave it stale.
cat >docs/latest.json <<EOF
{
  "version": "$VERSION",
  "tag": "$TAG",
  "release_url": "https://github.com/chud-lori/datagrep/releases/tag/$TAG",
  "release_notes_url": "https://github.com/chud-lori/datagrep/releases/tag/$TAG",
  "install_url": "https://raw.githubusercontent.com/chud-lori/datagrep/main/install.sh"
}
EOF

dim "   Cargo.toml + build-app.sh + UpdateCheck.swift + docs/latest.json bumped"

# --- Refresh Cargo.lock ---------------------------------------------------
# Workspace members inherit [workspace.package] version, so the lockfile
# records the new number. --workspace only re-pins the local crates; no
# dependency versions move.
blue "-> Refreshing Cargo.lock"
cargo update --workspace --quiet

# --- Format check ---------------------------------------------------------
# Mirrors ci/gates.sh gate 1; cheap, fail fast.
blue "-> Checking formatting (cargo fmt --all --check)"
if ! cargo fmt --all --check >/dev/null 2>&1; then
  red "error: code is not rustfmt-clean. Run 'cargo fmt --all' and re-run deploy."
  cargo fmt --all --check 2>&1 | head -20
  exit 1
fi

# --- Tests ----------------------------------------------------------------
blue "-> Running tests (cargo test --workspace)"
cargo test --workspace --quiet
dim "   (clippy + anti-pattern greps live in ./ci/gates.sh — run it for the full gate)"

# --- Confirm --------------------------------------------------------------
green "All checks passed. Diff to be committed:"
echo
git --no-pager diff --stat
echo
yellow "About to (locally only — nothing is pushed):"
yellow "  - commit: \"Release $TAG\""
yellow "  - tag:    $TAG (annotated)"
echo
if [ "$ASSUME_YES" -eq 1 ]; then
  yellow "Auto-confirm enabled (-y); proceeding."
else
  read -rp "Proceed? [y/N] " CONFIRM
  case "$CONFIRM" in
    y | Y | yes | YES) ;;
    *)
      red "aborted."
      exit 1
      ;;
  esac
fi

# --- Commit + tag (LOCAL ONLY) -------------------------------------------
blue "-> Committing"
git add Cargo.toml Cargo.lock docs/latest.json "$BUILD_APP_SH" "$UPDATE_CHECK_SWIFT"
git commit -m "Release $TAG"

blue "-> Tagging"
git tag -a "$TAG" -m "Release $TAG"

green "Done — committed and tagged locally. Nothing has been pushed."
echo
yellow "To publish (this triggers the release workflow, which builds the"
yellow "zip + DMG, uploads them, and re-asserts docs/latest.json):"
echo
echo "    git push origin main && git push origin $TAG"
echo
dim "To undo instead:  git tag -d $TAG && git reset --hard HEAD~1"
