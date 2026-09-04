#!/usr/bin/env bash
# Turns Cargo.lock into cargo-sources.json so flatpak-builder can pre-download
# every crate and `cargo --offline` resolves inside the network-less sandbox.
# Output is generated, never committed — CI regenerates it every run, so it
# cannot drift from the lockfile. Needs python3 with aiohttp and tomlkit.
#
# TWO lockfiles: ui/gtk4 is its own cargo workspace with its own Cargo.lock,
# and the generator takes one lockfile per run, so the two source lists are
# merged here. They overlap heavily — both resolve the engine's dependency
# graph — and an identical entry twice would have flatpak-builder unpack the
# same crate over itself.
set -euo pipefail
cd "$(dirname "$0")"

# Pinned commit: flatpak-builder-tools is an unversioned moving target.
REV=f03a673abe6ce189cea1c2857e2b44af2dd79d1f
URL="https://raw.githubusercontent.com/flatpak/flatpak-builder-tools/${REV}/cargo/flatpak-cargo-generator.py"

curl -fsSL "$URL" -o flatpak-cargo-generator.py
python3 flatpak-cargo-generator.py ../../Cargo.lock -o cargo-sources-engine.json
python3 flatpak-cargo-generator.py ../../ui/gtk4/Cargo.lock -o cargo-sources-gtk4.json

python3 - <<'PY'
import json

merged, seen = [], {}
for path in ("cargo-sources-engine.json", "cargo-sources-gtk4.json"):
    with open(path) as handle:
        for source in json.load(handle):
            key = (source.get("type"), source.get("dest"), source.get("dest-filename"))
            body = json.dumps(source, sort_keys=True)
            if key in seen:
                # Same destination, different content: picking one silently would
                # vendor a crate the other lockfile did not resolve.
                if seen[key] != body:
                    raise SystemExit(f"the two lockfiles disagree at {key}")
                continue
            seen[key] = body
            merged.append(source)

with open("cargo-sources.json", "w") as handle:
    json.dump(merged, handle, indent=4)
    handle.write("\n")
print(f"cargo-sources.json: {len(merged)} sources from two lockfiles")
PY

rm -f cargo-sources-engine.json cargo-sources-gtk4.json
