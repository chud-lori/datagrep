#!/usr/bin/env bash
# Turns Cargo.lock into cargo-sources.json so flatpak-builder can pre-download
# every crate and `cargo --offline` resolves inside the network-less sandbox.
# Output is generated, never committed — CI regenerates it every run, so it
# cannot drift from the lockfile. Needs python3 with aiohttp and tomlkit.
set -euo pipefail
cd "$(dirname "$0")"

# Pinned commit: flatpak-builder-tools is an unversioned moving target.
REV=f03a673abe6ce189cea1c2857e2b44af2dd79d1f
URL="https://raw.githubusercontent.com/flatpak/flatpak-builder-tools/${REV}/cargo/flatpak-cargo-generator.py"

curl -fsSL "$URL" -o flatpak-cargo-generator.py
python3 flatpak-cargo-generator.py ../../Cargo.lock -o cargo-sources.json
