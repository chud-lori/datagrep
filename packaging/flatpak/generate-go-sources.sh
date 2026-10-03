#!/usr/bin/env bash
# Turns the sidecar modules' dependency graph into go-sources.json, a file:// GOPROXY flatpak-builder pre-downloads.
set -euo pipefail
cd "$(dirname "$0")"

SIDECAR_ROOT="$(cd ../.. && pwd)/sidecar"
if [[ ! -d "$SIDECAR_ROOT" ]]; then
    # flatpak-builder rejects an empty sources file, which silently drops every source of the module.
    echo '[{"type": "inline", "contents": "", "dest-filename": ".go-sources-none"}]' > go-sources.json
    echo "go-sources.json: no sidecar/ tree, 0 sources"
    exit 0
fi

WORK="$(mktemp -d)"
# Go writes its module cache read-only.
trap 'chmod -R u+w "$WORK" && rm -rf "$WORK"' EXIT

find "$SIDECAR_ROOT" -name go.mod -not -path '*/vendor/*' | while read -r gomod; do
    for arch in amd64 arm64; do
        (cd "$(dirname "$gomod")" && GOOS=linux GOARCH="$arch" GOMODCACHE="$WORK" \
            GOFLAGS="-mod=readonly -buildvcs=false" GOTOOLCHAIN=local go list -deps ./... >/dev/null)
    done
done

python3 - "$WORK/cache/download" <<'PY'
import hashlib, json, os, sys

root = sys.argv[1]
sources, versions = [], set()
for dirpath, _, files in sorted(os.walk(root)):
    rel = os.path.relpath(dirpath, root)
    if not rel.endswith("/@v") or rel.startswith("sumdb"):
        continue
    for name in sorted(files):
        if not name.endswith((".mod", ".zip")):
            continue
        with open(os.path.join(dirpath, name), "rb") as handle:
            digest = hashlib.sha256(handle.read()).hexdigest()
        sources.append({
            "type": "file",
            "url": f"https://proxy.golang.org/{rel}/{name}",
            "sha256": digest,
            "dest": f"goproxy/{rel}",
            "dest-filename": name,
        })
        versions.add((rel, name.rsplit(".", 1)[0]))
for rel, version in sorted(versions):
    sources.append({
        "type": "inline",
        "contents": json.dumps({"Version": version}),
        "dest": f"goproxy/{rel}",
        "dest-filename": f"{version}.info",
    })

if not sources:
    sources.append({"type": "inline", "contents": "", "dest-filename": ".go-sources-none"})
with open("go-sources.json", "w") as handle:
    json.dump(sources, handle, indent=4)
    handle.write("\n")
print(f"go-sources.json: {len(sources)} sources for {len(versions)} module versions")
PY
