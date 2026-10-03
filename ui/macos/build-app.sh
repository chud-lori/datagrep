#!/usr/bin/env bash
# Builds ui/macos and assembles datagrep.app by hand.
# Needs only the Command Line Tools: no xcodebuild, no .xcodeproj.
set -euo pipefail

cd "$(dirname "$0")"

CONFIG="${CONFIG:-release}"
APP_NAME="datagrep"
BUNDLE_ID="com.lori.datagrep"
VERSION="0.4.0"

# Default to the real engine; the stub cannot connect to any database. Opt in with DATAGREP_FFI=stub.
if [ "${DATAGREP_FFI:-real}" != "stub" ]; then
    export DATAGREP_FFI=real
    REPO_ROOT="$(cd ../.. && pwd)"
    export DATAGREP_FFI_LIB_DIR="${DATAGREP_FFI_LIB_DIR:-${REPO_ROOT}/target/release}"
    # Always rebuild: a stale .a links silently against fresh Swift and fails far from here.
    if [ -z "${DATAGREP_SKIP_FFI_BUILD:-}" ]; then
        echo "==> building the engine (cargo build --release -p datagrep-ffi)"
        (cd "${REPO_ROOT}" && cargo build --release -p datagrep-ffi) || {
            echo "engine build failed" >&2
            exit 1
        }
    fi
    [ -f "${DATAGREP_FFI_LIB_DIR}/libdatagrep_ffi.a" ] || {
        echo "no libdatagrep_ffi.a in ${DATAGREP_FFI_LIB_DIR}" >&2
        echo "build it with: cargo build --release -p datagrep-ffi" >&2
        exit 1
    }
fi

# SwiftPM does not track libdatagrep_ffi.a, so a newer engine needs the product deleted to relink.
PRELINK_BIN="$(swift build -c "${CONFIG}" --show-bin-path)/datagrep-app"
if [ -f "${PRELINK_BIN}" ] && [ "${DATAGREP_FFI_LIB_DIR:-}/libdatagrep_ffi.a" -nt "${PRELINK_BIN}" ]; then
    echo "==> engine is newer than the last link; forcing a relink"
    rm -f "${PRELINK_BIN}"
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

GOARCH="$(uname -m | sed 's/x86_64/amd64/')"
../../packaging/build-sidecars.sh "darwin/${GOARCH}"
shopt -s nullglob
SIDECARS=("${PWD}/../../dist/.sidecars/darwin-${GOARCH}"/datagrep-sidecar-*)
shopt -u nullglob
if [ ${#SIDECARS[@]} -gt 0 ]; then
  mkdir -p "${APP}/Contents/Helpers"
  cp "${SIDECARS[@]}" "${APP}/Contents/Helpers/"
  echo "==> bundled ${#SIDECARS[@]} engine sidecar(s) into Contents/Helpers"
fi

# CFBundleIconFile must be this basename without extension; a wrong value fails silently.
# A stale icon after rebuilding is the Dock/Finder cache: touch the .app or restart Dock.
ICNS_SRC="${PWD}/../../assets/datagrep.icns"
if [ -f "${ICNS_SRC}" ]; then
  cp "${ICNS_SRC}" "${APP}/Contents/Resources/${APP_NAME}.icns"
  echo "==> bundled ${APP_NAME}.icns"
else
  echo "==> WARNING: ${ICNS_SRC} missing; app will show the generic document icon" >&2
fi

# Bundle.module resolves from Contents/Resources; without these it traps or falls back to SF Symbols.
shopt -s nullglob
BUNDLES=("${BIN_DIR}"/*.bundle)
if [ ${#BUNDLES[@]} -eq 0 ]; then
  echo "==> WARNING: no .bundle in ${BIN_DIR}; engine icons will fall back to SF Symbols" >&2
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

# Ad-hoc signing keys keychain ACLs on the cdhash, so every rebuild re-prompts; prefer a stable identity.
# Make one in Keychain Access (Self Signed Root, Code Signing) named datagrep-dev, or set DATAGREP_SIGN_IDENTITY.
SIGN_IDENTITY="${DATAGREP_SIGN_IDENTITY:-datagrep-dev}"
if command -v codesign >/dev/null 2>&1; then
  SIGN_AS="-"
  if security find-identity -v -p codesigning 2>/dev/null | grep -qF "${SIGN_IDENTITY}"; then
    SIGN_AS="${SIGN_IDENTITY}"
  fi
  # The bundle is signed without --deep, so nested helpers must be signed first.
  shopt -s nullglob
  for helper in "${APP}/Contents/Helpers"/*; do
    codesign --force --sign "${SIGN_AS}" --timestamp=none "${helper}" >/dev/null 2>&1 \
      || echo "==> codesign of $(basename "${helper}") failed"
  done
  shopt -u nullglob
  if [ "${SIGN_AS}" != "-" ]; then
    codesign --force --sign "${SIGN_IDENTITY}" --timestamp=none "${APP}" >/dev/null 2>&1 \
      && echo "==> signed as ${SIGN_IDENTITY}" \
      || echo "==> codesign with ${SIGN_IDENTITY} failed (app will still run)"
  else
    codesign --force --sign - --timestamp=none "${APP}" >/dev/null 2>&1 \
      && echo "==> ad-hoc signed (no '${SIGN_IDENTITY}' identity; the keychain will re-prompt after every build; see the comment above)" \
      || echo "==> codesign failed (app will still run)"
  fi
fi

SIZE=$(du -sh "${APP}" | cut -f1)
echo "==> built ${APP} (${SIZE})"
echo "    open ${APP}"
