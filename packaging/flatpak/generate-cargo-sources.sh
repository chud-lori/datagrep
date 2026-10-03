#!/usr/bin/env bash
# Generates cargo-sources.json for an offline flatpak build; never committed. Needs python3 with aiohttp and tomlkit.
# ui/gtk4 has its own Cargo.lock, so both lockfiles' sources are merged with duplicates dropped.
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
