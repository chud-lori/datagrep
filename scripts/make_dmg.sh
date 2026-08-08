#!/usr/bin/env bash
#
# Build a drag-to-Applications DMG for the datagrep macOS app.
# Same recipe as chud-lori/rusty-requester's scripts/make_dmg.sh:
#
#   1. stage  : copy datagrep.app + an /Applications symlink (+ optional
#               .background/background.png if assets/dmg_background.png exists)
#   2. create : a temporary read-write DMG from the staging dir
#   3. mount  : that temp DMG so Finder can see it as a Volume
#   4. apply  : window bounds / icon size / icon positions via AppleScript so
#               the layout is baked into .DS_Store
#   5. detach : unmount the read-write DMG
#   6. convert: to a final UDZO-compressed read-only DMG
#
# Uses only macOS built-ins (hdiutil + osascript + ln + cp). No homebrew.
#
# Usage:
#   ./scripts/make_dmg.sh [path/to/datagrep.app]
#
# Defaults:
#   app path : ui/macos/datagrep.app   (built by ui/macos/build-app.sh)
#   output   : dist/datagrep-macos.dmg (override with DMG_PATH=...)
#
# Under CI / non-interactive shells the Finder AppleScript layout is skipped
# (no Finder to script there); the DMG still mounts fine with the app and the
# Applications symlink, just in Finder's default arrangement.
#
set -euo pipefail

REPO_ROOT="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." >/dev/null 2>&1 && pwd)"

APP_NAME="datagrep"
VOLNAME="datagrep"
APP_PATH="${1:-${REPO_ROOT}/ui/macos/${APP_NAME}.app}"
DIST_DIR="${DIST_DIR:-${REPO_ROOT}/dist}"
DMG_PATH="${DMG_PATH:-${DIST_DIR}/${APP_NAME}-macos.dmg}"
TEMP_DMG="${DIST_DIR}/${APP_NAME}-temp.dmg"
STAGE_DIR="${DIST_DIR}/dmg-staging"
BG_IMAGE="${REPO_ROOT}/assets/dmg_background.png" # optional — plain layout without it
BG_IMAGE_2X="${REPO_ROOT}/assets/dmg_background@2x.png"  # optional Retina half of the TIFF
# A Finder-built .DS_Store, baked once on a Mac and committed. CI runners have
# no Finder, so the AppleScript layout below is skipped there and the window
# would otherwise open with no background and default icon placement — i.e. the
# released DMG would never carry the layout a developer sees locally. Rebake
# after changing any geometry: DMG_BAKE_DS_STORE=1 ./scripts/make_dmg.sh
BAKED_DS_STORE="${REPO_ROOT}/assets/dmg_DS_Store"

# Window bounds / icon geometry. If a background image is ever added it must
# match WIN_W x WIN_H.
WIN_X=400
WIN_Y=120
WIN_W=600
WIN_H=400
ICON_SIZE=128
APP_X=175
APP_Y=200
LINK_X=425
LINK_Y=200

if [[ ! -d "$APP_PATH" ]]; then
  echo "error: $APP_PATH not found — build it first: cd ui/macos && ./build-app.sh" >&2
  exit 1
fi

# Force-unmount any stale "/Volumes/$VOLNAME" left behind by a previous
# interrupted run — otherwise a new mount returns the old cached volume and
# the AppleScript ends up editing the wrong .DS_Store.
if [ -d "/Volumes/${VOLNAME}" ]; then
  echo "-> unmounting stale /Volumes/${VOLNAME}"
  hdiutil detach "/Volumes/${VOLNAME}" -force >/dev/null 2>&1 || true
fi

echo "-> staging in $STAGE_DIR"
mkdir -p "$DIST_DIR"
rm -rf "$STAGE_DIR" "$TEMP_DMG" "$DMG_PATH"
mkdir -p "$STAGE_DIR"
cp -R "$APP_PATH" "$STAGE_DIR/${APP_NAME}.app"
ln -s /Applications "$STAGE_DIR/Applications"

HAVE_BG=0
if [[ -f "$BG_IMAGE" ]]; then
  mkdir -p "$STAGE_DIR/.background"
  # Ship a multi-resolution TIFF when the @2x art exists: a 600x400 PNG is
  # visibly soft on a Retina display, and tiffutil (a macOS built-in, so this
  # adds no dependency) packs both scales into one file that Finder picks from.
  if [[ -f "$BG_IMAGE_2X" ]] && command -v tiffutil >/dev/null 2>&1 &&
    tiffutil -cathidpicheck "$BG_IMAGE" "$BG_IMAGE_2X" \
      -out "$STAGE_DIR/.background/background.tiff" >/dev/null 2>&1; then
    BG_FILE="background.tiff"
  else
    cp "$BG_IMAGE" "$STAGE_DIR/.background/background.png"
    BG_FILE="background.png"
  fi
  HAVE_BG=1
fi

echo "-> creating temporary read-write DMG"
hdiutil create \
  -srcfolder "$STAGE_DIR" \
  -volname "$VOLNAME" \
  -fs HFS+ \
  -format UDRW \
  -ov \
  "$TEMP_DMG" \
  >/dev/null

echo "-> mounting temporary DMG"
DEVICE=$(hdiutil attach -readwrite -noverify -noautoopen "$TEMP_DMG" |
  grep -E '^/dev/' | head -n 1 | awk '{print $1}')
echo "   mounted: $DEVICE"

# Give Finder time to register the volume before scripting it. On Sonoma+
# the first osascript call often fails if this is too short.
sleep 4

# Skip the Finder-layout AppleScript under CI / non-interactive shells:
# GitHub Actions runners have no Finder UI to render the layout, and invoking
# osascript->Finder there leaves the volume open, which makes hdiutil detach
# fail with "Resource busy". The DMG still works without the polish.
SKIP_LAYOUT="${SKIP_LAYOUT:-}"
if [ -n "${CI:-}" ] || [ -n "${GITHUB_ACTIONS:-}" ] || [ ! -t 0 ]; then
  SKIP_LAYOUT="1"
fi
# The tty test above is a proxy for "a Finder is around", and it is wrong when a
# Mac drives this from a pipe or an agent shell — which is exactly when the
# layout needs baking. This forces the real thing.
if [ -n "${DMG_FORCE_LAYOUT:-}" ]; then
  SKIP_LAYOUT=""
fi

if [ -n "$SKIP_LAYOUT" ]; then
  # No Finder here, but the layout can still ship: a .DS_Store baked on a Mac
  # carries the window bounds, icon size, icon positions and the background
  # reference, and Finder reads it as-is. Without this the released DMG looks
  # nothing like the one a developer builds locally.
  if [ -f "$BAKED_DS_STORE" ]; then
    echo "-> no Finder here; installing the baked .DS_Store layout"
    cp "$BAKED_DS_STORE" "/Volumes/${VOLNAME}/.DS_Store"
  else
    echo "-> skipping Finder window layout (no Finder, and no baked .DS_Store)"
    echo "   bake one on a Mac: DMG_BAKE_DS_STORE=1 ./scripts/make_dmg.sh"
  fi
else
  echo "-> applying Finder window layout via AppleScript"
  # Each fragile call is wrapped in `try` so a single failure (e.g. macOS
  # rejecting the background-picture write) doesn't abort the rest.
  osascript <<EOF || echo "   warning: AppleScript layout did not apply cleanly — DMG will still build."
tell application "Finder"
    tell disk "$VOLNAME"
        open
        set current view of container window to icon view
        set toolbar visible of container window to false
        set statusbar visible of container window to false
        try
            set sidebar width of container window to 0
        end try
        set the bounds of container window to {$WIN_X, $WIN_Y, $((WIN_X + WIN_W)), $((WIN_Y + WIN_H))}
        set viewOptions to the icon view options of container window
        set arrangement of viewOptions to not arranged
        set icon size of viewOptions to $ICON_SIZE
        try
            set text size of viewOptions to 13
        end try
        try
            set shows item info of viewOptions to false
        end try
        if $HAVE_BG is 1 then
            try
                set background picture of viewOptions to file ".background:${BG_FILE}"
            end try
        end if
        try
            set position of item "${APP_NAME}.app" of container window to {$APP_X, $APP_Y}
        end try
        try
            set position of item "Applications" of container window to {$LINK_X, $LINK_Y}
        end try
        update without registering applications
        delay 1
        close
    end tell
end tell
EOF
fi

# Make sure .DS_Store makes it to disk before unmount.
sync
sleep 1

# Capture the Finder-built layout so headless builds can reuse it. Deliberately
# opt-in: it is a binary asset, and overwriting it from a run whose AppleScript
# only half-applied would ship a broken layout everywhere.
if [ -n "${DMG_BAKE_DS_STORE:-}" ] && [ -z "$SKIP_LAYOUT" ]; then
  if [ -f "/Volumes/${VOLNAME}/.DS_Store" ]; then
    cp "/Volumes/${VOLNAME}/.DS_Store" "$BAKED_DS_STORE"
    echo "-> baked layout to $BAKED_DS_STORE ($(du -h "$BAKED_DS_STORE" | cut -f1))"
  else
    echo "   warning: no .DS_Store on the volume — nothing baked" >&2
  fi
fi

# Retry detach — even with -force, the first attempt can race a still-open
# file handle (Spotlight / fseventsd), especially on CI runners.
echo "-> detaching"
detach_ok=""
for attempt in 1 2 3 4 5; do
  if hdiutil detach "$DEVICE" -force >/dev/null 2>&1; then
    detach_ok="1"
    break
  fi
  echo "   detach attempt $attempt failed; sleeping 2s and retrying..."
  sleep 2
done
if [ -z "$detach_ok" ]; then
  # diskutil unmounts via different plumbing and sometimes succeeds where
  # hdiutil fails.
  echo "   hdiutil retries exhausted — trying diskutil unmount"
  diskutil unmount force "$DEVICE" >/dev/null 2>&1 || {
    echo "error: unable to unmount $DEVICE — aborting" >&2
    exit 16
  }
fi

echo "-> converting to compressed read-only DMG"
hdiutil convert "$TEMP_DMG" \
  -format UDZO \
  -imagekey zlib-level=9 \
  -o "$DMG_PATH" \
  >/dev/null
rm -f "$TEMP_DMG"
rm -rf "$STAGE_DIR"

SIZE=$(du -h "$DMG_PATH" | cut -f1)
echo ""
echo "DMG ready: $DMG_PATH (${SIZE})"
echo "  open it with: open $DMG_PATH"
