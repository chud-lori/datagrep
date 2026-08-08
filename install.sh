#!/usr/bin/env bash
# datagrep installer — puts the `datagrep` CLI on your PATH (and, with --app,
# installs the macOS app). Downloads prebuilt binaries from GitHub Releases,
# verifies them against the release's SHA256SUMS, and never runs sudo for you.
#
#   curl -fsSL https://raw.githubusercontent.com/chud-lori/datagrep/main/install.sh | bash
#
# Options (pass after `bash -s --` when piping from curl):
#   --version VER    install this release tag instead of latest (e.g. v0.1.0)
#   --prefix DIR     install the CLI to DIR/bin
#   --user           install the CLI to ~/.local/bin
#   --app            also install the macOS app (macOS only)
#   --uninstall      remove the datagrep CLI (and the app if --app)
#   --help           show this help
#
# Building from source instead:  cargo install --path crates/datagrep-cli

set -euo pipefail

PROG=datagrep
REPO=chud-lori/datagrep

PREFIX=""
USER_INSTALL=0
WANT_APP=0
UNINSTALL=0
REL_VERSION="" # empty = latest

usage() {
  cat <<'EOF'
datagrep installer — puts the `datagrep` CLI on your PATH (and, with --app,
installs the macOS app). Downloads prebuilt binaries from GitHub Releases,
verifies them against the release's SHA256SUMS, and never runs sudo for you.

  curl -fsSL https://raw.githubusercontent.com/chud-lori/datagrep/main/install.sh | bash

Options (pass after `bash -s --` when piping from curl):
  --version VER    install this release tag instead of latest (e.g. v0.1.0)
  --prefix DIR     install the CLI to DIR/bin
  --user           install the CLI to ~/.local/bin
  --app            also install the macOS app (macOS only)
  --uninstall      remove the datagrep CLI (and the app if --app)
  --help           show this help

Building from source instead:  cargo install --path crates/datagrep-cli
EOF
}

die() {
  echo "datagrep: $*" >&2
  exit 1
}

# ---- parse args ----
while [ $# -gt 0 ]; do
  case "$1" in
    --user) USER_INSTALL=1 ;;
    --app) WANT_APP=1 ;;
    --uninstall) UNINSTALL=1 ;;
    --version)
      shift
      REL_VERSION="${1:-}"
      [ -n "$REL_VERSION" ] || die "--version needs a tag (e.g. --version v0.1.0)"
      ;;
    --version=*) REL_VERSION="${1#--version=}" ;;
    --prefix)
      shift
      PREFIX="${1:-}"
      [ -n "$PREFIX" ] || die "--prefix needs a directory"
      ;;
    --prefix=*) PREFIX="${1#--prefix=}" ;;
    -h | --help)
      usage
      exit 0
      ;;
    *) die "unknown option '$1' (try --help)" ;;
  esac
  shift
done

# ---- detect platform ----
os=$(uname -s)
arch=$(uname -m)
case "$os" in
  Darwin) os=macos ;;
  Linux) os=linux ;;
  *) die "no prebuilt binary for '$os' — build from source: cargo install --path crates/datagrep-cli" ;;
esac
case "$arch" in
  arm64 | aarch64) arch=arm64 ;;
  x86_64 | amd64) arch=x86_64 ;;
  *) die "no prebuilt binary for arch '$arch' — build from source: cargo install --path crates/datagrep-cli" ;;
esac

if [ "$WANT_APP" -eq 1 ] && [ "$os" != macos ]; then
  die "--app is macOS-only (the app is a native macOS bundle)"
fi

# ---- choose CLI install directory (never silently sudo) ----
if [ -n "$PREFIX" ]; then
  BINDIR="$PREFIX/bin"
elif [ "$USER_INSTALL" -eq 1 ]; then
  BINDIR="$HOME/.local/bin"
elif [ "$(id -u)" -eq 0 ] || [ -w /usr/local/bin ]; then
  BINDIR="/usr/local/bin"
else
  BINDIR="$HOME/.local/bin"
fi

check_writable() {
  # check_writable <dir> — the dir (or its creatable parent) must be writable.
  if [ -d "$1" ]; then
    [ -w "$1" ] || die "$1 is not writable. Re-run with sudo yourself, or use --user / --prefix DIR."
  else
    parent=$(dirname -- "$1")
    [ -d "$parent" ] && [ -w "$parent" ] || die "cannot create $1. Re-run with sudo yourself, or use --user / --prefix DIR."
  fi
}

# ---- app install directory ----
APPDIR=""
if [ "$WANT_APP" -eq 1 ]; then
  if [ -w /Applications ]; then
    APPDIR=/Applications
  else
    APPDIR="$HOME/Applications"
  fi
fi

# ---- uninstall ----
if [ "$UNINSTALL" -eq 1 ]; then
  if [ -e "$BINDIR/$PROG" ]; then
    echo "Removing $BINDIR/$PROG"
    rm -f "$BINDIR/$PROG"
  else
    echo "datagrep: CLI not found at $BINDIR/$PROG (nothing to remove)"
  fi
  if [ "$WANT_APP" -eq 1 ]; then
    for d in /Applications "$HOME/Applications"; do
      if [ -d "$d/$PROG.app" ]; then
        echo "Removing $d/$PROG.app"
        rm -rf "${d:?}/$PROG.app"
      fi
    done
  fi
  echo "Done."
  exit 0
fi

# ---- fetch helpers ----
fetch() {
  # fetch <url> <dest>
  if command -v curl >/dev/null 2>&1; then
    curl -fsSL "$1" -o "$2"
  elif command -v wget >/dev/null 2>&1; then
    wget -qO "$2" "$1"
  else
    die "need curl or wget to download."
  fi
}

release_url() {
  # release_url <asset>
  if [ -n "$REL_VERSION" ]; then
    echo "https://github.com/$REPO/releases/download/$REL_VERSION/$1"
  else
    echo "https://github.com/$REPO/releases/latest/download/$1"
  fi
}

sha256_of() {
  # sha256_of <file> — prints the hex digest
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum "$1" | awk '{print $1}'
  elif command -v shasum >/dev/null 2>&1; then
    shasum -a 256 "$1" | awk '{print $1}'
  else
    die "need sha256sum or shasum to verify downloads."
  fi
}

tmpdir=$(mktemp -d)
trap 'rm -rf "$tmpdir"' EXIT INT TERM

# ---- checksums first: everything downloaded is verified against SHA256SUMS ----
sums="$tmpdir/SHA256SUMS"
echo "Fetching SHA256SUMS${REL_VERSION:+ for $REL_VERSION}..."
fetch "$(release_url SHA256SUMS)" "$sums" ||
  die "could not download SHA256SUMS for ${REL_VERSION:-the latest release}.
       Wrong --version tag, or a release predating checksums? See
       https://github.com/$REPO/releases"

verify() {
  # verify <file> <asset-name> — compare against the SHA256SUMS entry
  expected=$(awk -v a="$2" '$2 == a {print $1}' "$sums")
  [ -n "$expected" ] || die "SHA256SUMS has no entry for $2 — refusing to install it."
  actual=$(sha256_of "$1")
  if [ "$actual" != "$expected" ]; then
    die "checksum mismatch for $2 (expected $expected, got $actual).
       Corrupted or tampered download — not installing."
  fi
  echo "  checksum OK: $2"
}

download_verified() {
  # download_verified <asset-name> -> file lands at $tmpdir/<asset-name>
  echo "Downloading $1..."
  fetch "$(release_url "$1")" "$tmpdir/$1" ||
    die "download failed: $(release_url "$1")
       No matching release asset? See https://github.com/$REPO/releases"
  verify "$tmpdir/$1" "$1"
}

# ---- install the CLI ----
asset="$PROG-$os-$arch"
download_verified "$asset"
check_writable "$BINDIR"
mkdir -p "$BINDIR"
install -m 0755 "$tmpdir/$asset" "$BINDIR/$PROG"
echo "Installed $BINDIR/$PROG"

# ---- optionally install the macOS app ----
if [ "$WANT_APP" -eq 1 ]; then
  app_zip="$PROG-macos.app.zip"
  download_verified "$app_zip"
  check_writable "$APPDIR"
  mkdir -p "$APPDIR"
  rm -rf "${APPDIR:?}/$PROG.app"
  # ditto preserves the bundle's symlinks and metadata (unzip can mangle .apps)
  ditto -x -k "$tmpdir/$app_zip" "$APPDIR"
  [ -d "$APPDIR/$PROG.app" ] || die "the zip did not contain $PROG.app (unexpected release layout)."
  echo "Installed $APPDIR/$PROG.app"
fi

# ---- PATH check ----
case ":$PATH:" in
  *":$BINDIR:"*)
    echo
    echo "Done. Run it from anywhere:"
    echo "    $PROG --help"
    ;;
  *)
    shell_name=$(basename -- "${SHELL:-sh}")
    case "$shell_name" in
      zsh) rc="$HOME/.zshrc" ;;
      bash) rc="$HOME/.bashrc" ;;
      *) rc="your shell profile" ;;
    esac
    echo
    echo "NOTE: $BINDIR is not on your PATH yet."
    echo "  Add it by appending this line to $rc, then restart your shell:"
    echo
    echo "    export PATH=\"$BINDIR:\$PATH\""
    echo
    echo "  (or run it with the full path for now: $BINDIR/$PROG --help)"
    ;;
esac

# ---- Gatekeeper note (the app is ad-hoc signed, not notarized) ----
if [ "$WANT_APP" -eq 1 ]; then
  cat <<EOF

macOS Gatekeeper note — please read:
  datagrep is NOT notarized (no Apple Developer account); the app is ad-hoc
  signed. If macOS says on first launch:

      "datagrep" cannot be opened because the developer cannot be verified.

  the app is NOT broken. Either right-click (Ctrl-click) $PROG.app and choose
  "Open" (then "Open" again — macOS remembers the choice), or clear the
  quarantine flag:

      xattr -dr com.apple.quarantine "$APPDIR/$PROG.app"

  Installing via this script usually avoids the prompt entirely (curl does not
  set the quarantine flag browsers do), but a browser-downloaded copy will
  always hit it once.
EOF
fi
