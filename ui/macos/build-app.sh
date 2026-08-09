#!/usr/bin/env bash
# Builds ui/macos and assembles datagrep.app by hand.
#
# There is no Xcode on this machine — Command Line Tools 16.4 only — so there is
# no xcodebuild and no .xcodeproj. `swift build` produces the executable and this
# script writes the bundle layout (Info.plist + Contents/MacOS/) around it.
set -euo pipefail

cd "$(dirname "$0")"

CONFIG="${CONFIG:-release}"
APP_NAME="datagrep"
BUNDLE_ID="com.lori.datagrep"
VERSION="0.3.1"

# Default to the REAL engine. The stub links a synthetic in-memory dataset and
# cannot connect to any database, so a stub bundle looks fine and does nothing —
# not what anyone following the README wants. Opt into it with DATAGREP_FFI=stub.
if [ "${DATAGREP_FFI:-real}" != "stub" ]; then
    export DATAGREP_FFI=real
    REPO_ROOT="$(cd ../.. && pwd)"
    export DATAGREP_FFI_LIB_DIR="${DATAGREP_FFI_LIB_DIR:-${REPO_ROOT}/target/release}"
    if [ ! -f "${DATAGREP_FFI_LIB_DIR}/libdatagrep_ffi.a" ]; then
        echo "==> building the engine (cargo build --release -p datagrep-ffi)"
        (cd "${REPO_ROOT}" && cargo build --release -p datagrep-ffi)
    fi
    [ -f "${DATAGREP_FFI_LIB_DIR}/libdatagrep_ffi.a" ] || {
        echo "no libdatagrep_ffi.a in ${DATAGREP_FFI_LIB_DIR}" >&2
        echo "build it with: cargo build --release -p datagrep-ffi" >&2
        exit 1
    }
fi

echo "==> swift build -c ${CONFIG}  (DATAGREP_FFI=${DATAGREP_FFI:-stub})"
swift build -c "${CONFIG}"

BIN_DIR="$(swift build -c "${CONFIG}" --show-bin-path)"
BIN="${BIN_DIR}/datagrep-app"
[ -x "${BIN}" ] || { echo "build produced no executable at ${BIN}" >&2; exit 1; }

APP="${PWD}/${APP_NAME}.app"
rm -rf "${APP}"
mkdir -p "${APP}/Contents/MacOS" "${APP}/Contents/Resources"

cp "${BIN}" "${APP}/Contents/MacOS/${APP_NAME}"

# App icon. CFBundleIconFile below must match this basename WITHOUT extension
# ("datagrep", not "datagrep.icns") — Finder/Dock resolve it themselves, and a
# wrong value fails silently (app launches fine, just keeps the generic
# document icon). macOS also caches Dock/Finder icon lookups aggressively per
# bundle path, so a stale icon after rebuilding usually means the cache, not
# this script — `touch` the .app and/or restart Dock/Finder to force it.
ICNS_SRC="${PWD}/../../assets/datagrep.icns"
if [ -f "${ICNS_SRC}" ]; then
  cp "${ICNS_SRC}" "${APP}/Contents/Resources/${APP_NAME}.icns"
  echo "==> bundled ${APP_NAME}.icns"
else
  echo "==> WARNING: ${ICNS_SRC} missing — app will show the generic document icon" >&2
fi

# SwiftPM resource bundles (engine brand marks). `Bundle.module` resolves these
# from Bundle.main.resourceURL, so they MUST be inside Contents/Resources — if
# this loop copies nothing, every engine icon silently falls back to an SF
# Symbol and `Bundle.module` traps on a target that has no other resource.
shopt -s nullglob
BUNDLES=("${BIN_DIR}"/*.bundle)
if [ ${#BUNDLES[@]} -eq 0 ]; then
  echo "==> WARNING: no .bundle in ${BIN_DIR} — engine icons will fall back to SF Symbols" >&2
else
  for b in "${BUNDLES[@]}"; do
    cp -R "${b}" "${APP}/Contents/Resources/"
    echo "==> bundled $(basename "${b}")"
  done
fi
shopt -u nullglob

cat > "${APP}/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleName</key>              <string>${APP_NAME}</string>
  <key>CFBundleDisplayName</key>       <string>${APP_NAME}</string>
  <key>CFBundleExecutable</key>        <string>${APP_NAME}</string>
  <key>CFBundleIconFile</key>          <string>${APP_NAME}</string>
  <key>CFBundleIdentifier</key>        <string>${BUNDLE_ID}</string>
  <key>CFBundleVersion</key>           <string>${VERSION}</string>
  <key>CFBundleShortVersionString</key><string>${VERSION}</string>
  <key>CFBundlePackageType</key>       <string>APPL</string>
  <key>CFBundleInfoDictionaryVersion</key><string>6.0</string>
  <key>LSMinimumSystemVersion</key>    <string>14.0</string>
  <key>NSApplicationSupportsSecureRestorableState</key><true/>
  <key>NSHighResolutionCapable</key>   <true/>
  <key>NSSupportsAutomaticGraphicsSwitching</key><true/>
  <key>NSPrincipalClass</key>          <string>NSApplication</string>
  <key>LSApplicationCategoryType</key> <string>public.app-category.developer-tools</string>
</dict>
</plist>
PLIST

printf 'APPL????' > "${APP}/Contents/PkgInfo"

# Ad-hoc signature. codesign ships with the Command Line Tools; without this the
# app still launches but macOS re-validates it on every start.
if command -v codesign >/dev/null 2>&1; then
  codesign --force --sign - --timestamp=none "${APP}" >/dev/null 2>&1 \
    && echo "==> ad-hoc signed" || echo "==> codesign failed (app will still run)"
fi

SIZE=$(du -sh "${APP}" | cut -f1)
echo "==> built ${APP} (${SIZE})"
echo "    open ${APP}"
