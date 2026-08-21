#!/usr/bin/env bash
# Automated half of the issue #47 desktop pass; the human half is notes/linux-desktop-checklist.md.
set -uo pipefail

REPO="${DG_REPO:-chud-lori/datagrep}"
WORKFLOW="linux-package.yml"
ARTIFACT="datagrep-linux-packages"
DEST="${DG_VERIFY_DIR:-$HOME/datagrep-verify}"
CFG="$DEST/config"
LOG="$DEST/datagrep-stderr.log"
WAIT_SECS="${DG_VERIFY_WAIT:-10}"

red() { printf "\033[31m%s\033[0m\n" "$*" >&2; }
green() { printf "\033[32m%s\033[0m\n" "$*"; }
blue() { printf "\033[34m%s\033[0m\n" "$*"; }
yellow() { printf "\033[33m%s\033[0m\n" "$*"; }
dim() { printf "\033[2m%s\033[0m\n" "$*"; }

die() {
  red "FATAL: $*"
  exit 1
}

RESULTS=()
FAILED=0
pass() { RESULTS+=("PASS  $*"); green "PASS  $*"; }
fail() { RESULTS+=("FAIL  $*"); red "FAIL  $*"; FAILED=1; }
skip() { RESULTS+=("SKIP  $*"); yellow "SKIP  $*"; }

[ "$(uname -s)" = "Linux" ] || die "this script runs on the Linux desktop under test, not $(uname -s). Copy the repo (or just this script) to the ThinkPad and run it there."
command -v gh >/dev/null || die "gh is required (and must be authenticated: gh auth login)"
command -v sha256sum >/dev/null || die "sha256sum is required"

blue "== Environment =="
HOST_GLIBC="$(getconf GNU_LIBC_VERSION 2>/dev/null | awk '{print $2}')"
dim "  distro:  $(. /etc/os-release 2>/dev/null && echo "${PRETTY_NAME:-unknown}")"
dim "  glibc:   ${HOST_GLIBC:-unknown}"
dim "  desktop: ${XDG_CURRENT_DESKTOP:-unset} (${XDG_SESSION_TYPE:-unknown} session)"
dim "  workdir: $DEST"

blue "== Fetch newest successful $WORKFLOW artifact =="
RUN_JSON="$(gh run list --repo "$REPO" --workflow="$WORKFLOW" --status success --limit 1 --json databaseId,headSha,createdAt -q '.[0]')"
[ -n "$RUN_JSON" ] && [ "$RUN_JSON" != "null" ] || die "no successful $WORKFLOW run found. Dispatch one first: gh workflow run $WORKFLOW --repo $REPO --ref main"
RUN_ID="$(echo "$RUN_JSON" | grep -o '"databaseId":[0-9]*' | cut -d: -f2)"
HEAD_SHA="$(echo "$RUN_JSON" | grep -o '"headSha":"[^"]*"' | cut -d'"' -f4)"
CREATED="$(echo "$RUN_JSON" | grep -o '"createdAt":"[^"]*"' | cut -d'"' -f4)"
dim "  run $RUN_ID, built from ${HEAD_SHA:0:9} on $CREATED"
AGE_DAYS=$(( ( $(date +%s) - $(date -d "$CREATED" +%s 2>/dev/null || echo 0) ) / 86400 ))
[ "$AGE_DAYS" -gt 7 ] && yellow "  note: this build is ${AGE_DAYS} days old — a fresh one is: gh workflow run $WORKFLOW --repo $REPO --ref main"

rm -rf "$DEST/artifact"
mkdir -p "$DEST/artifact"
gh run download "$RUN_ID" --repo "$REPO" -n "$ARTIFACT" -D "$DEST/artifact" || die "artifact download failed (run $RUN_ID, name $ARTIFACT)"
cd "$DEST/artifact" || die "cannot enter $DEST/artifact"

blue "== Checksums and contents =="
if sha256sum -c SHA256SUMS >/dev/null 2>&1; then
  pass "SHA256SUMS verifies for every file in the artifact"
else
  sha256sum -c SHA256SUMS 2>&1 | grep -v ': OK$' >&2
  die "checksum mismatch — the download is corrupt or incomplete; delete $DEST/artifact and rerun"
fi
for kind in "*.AppImage" "*.deb" "*.rpm"; do
  if compgen -G "$kind" >/dev/null; then pass "artifact contains $kind"; else fail "artifact is missing $kind"; fi
done
APP="$(compgen -G "*.AppImage" | head -1)"
[ -n "$APP" ] || die "no AppImage to test — nothing further can run"
chmod +x "$APP"

blue "== glibc floor (the 21 Aug blocker: built on 2.39, dead on 2.35) =="
rm -rf squashfs-root
"./$APP" --appimage-extract >/dev/null 2>&1 || die "could not extract the AppImage ($APP --appimage-extract failed) — the file itself is broken"
NEED_GLIBC="$(find squashfs-root -type f \( -name '*.so*' -o -path '*/usr/bin/*' \) -exec strings -a {} + 2>/dev/null | grep -o 'GLIBC_[0-9][0-9.]*' | sed 's/GLIBC_//' | sort -uV | tail -1)"
if [ -z "$NEED_GLIBC" ]; then
  skip "glibc requirement scan found no versioned symbols (unexpected — check manually with objdump -T)"
elif [ -z "$HOST_GLIBC" ]; then
  skip "could not determine host glibc version"
elif [ "$(printf '%s\n%s\n' "$NEED_GLIBC" "$HOST_GLIBC" | sort -V | tail -1)" = "$HOST_GLIBC" ]; then
  pass "AppImage needs glibc <= $NEED_GLIBC, host has $HOST_GLIBC"
else
  fail "AppImage requires glibc $NEED_GLIBC but this machine has $HOST_GLIBC — it will not start; the packaging build base regressed to something too new"
fi

blue "== Launch (isolated DATAGREP_CONFIG_DIR, stderr -> $LOG) =="
rm -rf "$CFG"
mkdir -p "$CFG"
: > "$LOG"
LAUNCH_CMD="./$APP"
DATAGREP_CONFIG_DIR="$CFG" "./$APP" >>"$LOG" 2>&1 &
PID=$!
sleep 3
if ! kill -0 "$PID" 2>/dev/null && grep -qi 'fuse\|squashfs' "$LOG"; then
  yellow "  direct AppImage launch died with a FUSE-style error (fix: sudo apt install libfuse2); retrying via extracted AppRun"
  LAUNCH_CMD="./squashfs-root/AppRun"
  : > "$LOG"
  DATAGREP_CONFIG_DIR="$CFG" ./squashfs-root/AppRun >>"$LOG" 2>&1 &
  PID=$!
  sleep 3
fi

ALIVE=1
for _ in $(seq 3 "$WAIT_SECS"); do
  kill -0 "$PID" 2>/dev/null || { ALIVE=0; break; }
  sleep 1
done
if [ "$ALIVE" = 1 ]; then
  pass "process still running after ${WAIT_SECS}s"
else
  wait "$PID"
  RC=$?
  fail "process exited (code $RC) within ${WAIT_SECS}s of launch — last stderr lines follow"
  tail -20 "$LOG" >&2
fi

if [ "$ALIVE" = 1 ]; then
  if [ "${XDG_SESSION_TYPE:-}" = "x11" ] && command -v xdotool >/dev/null; then
    if xdotool search --onlyvisible --name '^datagrep' >/dev/null 2>&1; then
      pass "a visible window titled 'datagrep' exists"
    else
      fail "the process is alive but no visible 'datagrep' window appeared — the window never appeared"
    fi
  elif command -v wmctrl >/dev/null && wmctrl -l >/dev/null 2>&1; then
    if wmctrl -l | grep -qi datagrep; then
      pass "a window titled 'datagrep' exists (via wmctrl)"
    else
      fail "the process is alive but no 'datagrep' window is listed — the window never appeared"
    fi
  else
    skip "window-appeared check needs xdotool (X11) or wmctrl — confirm by eye that a window is on screen"
  fi

  if [ -s "$CFG/profiles.sqlite" ]; then
    pass "engine started: profiles.sqlite created in the isolated config dir"
  else
    fail "profiles.sqlite never appeared in $CFG — the engine did not start (expect a 'Could not open the engine' dialog)"
  fi
fi

if grep -qE 'undefined symbol|error while loading shared libraries|Could not open the engine' "$LOG"; then
  fail "stderr contains a loader/engine failure — see $LOG"
  grep -E 'undefined symbol|error while loading shared libraries|Could not open the engine' "$LOG" | head -5 >&2
elif [ -s "$LOG" ]; then
  dim "  stderr is non-empty ($(wc -l < "$LOG") lines) — usually harmless Qt noise, kept at $LOG"
fi

if [ "$ALIVE" = 1 ]; then
  blue "== Shutdown and cold relaunch against the same config dir =="
  kill "$PID" 2>/dev/null
  TERMED=0
  for _ in 1 2 3 4 5; do
    kill -0 "$PID" 2>/dev/null || { TERMED=1; break; }
    sleep 1
  done
  if [ "$TERMED" = 1 ]; then
    pass "process terminated cleanly on SIGTERM"
  else
    kill -9 "$PID" 2>/dev/null
    fail "process ignored SIGTERM for 5s and was killed -9"
  fi
  DATAGREP_CONFIG_DIR="$CFG" $LAUNCH_CMD >>"$LOG" 2>&1 &
  PID2=$!
  sleep 5
  if kill -0 "$PID2" 2>/dev/null; then
    pass "second launch over the existing profiles.sqlite survives 5s"
    kill "$PID2" 2>/dev/null
    sleep 2
    kill -9 "$PID2" 2>/dev/null
  else
    wait "$PID2"
    fail "second launch died (code $?) — the app cannot reopen its own config dir"
  fi
fi

echo
blue "== Summary =="
for r in "${RESULTS[@]}"; do echo "  $r"; done
echo
yellow "NOT verified by this script (it launches, it does not use):"
dim "  - everything on the human checklist: notes/linux-desktop-checklist.md"
dim "  - the Qt build has no --diag flag, so engine health beyond 'profiles.sqlite appeared' is unproven"
dim "  - history/ and tabs/ stores are only written on use; their persistence is a checklist item"
dim "  - a green run here means: fetchable, checksummed, glibc-compatible, starts, opens its engine, restarts. Nothing more."
echo
blue "Manual session (reuses this sandbox so saves persist into the restart test):"
echo "  cd $DEST/artifact && DATAGREP_CONFIG_DIR=$CFG $LAUNCH_CMD 2>&1 | tee -a $LOG"
echo
if [ "$FAILED" = 0 ]; then green "automated pass: GREEN"; else red "automated pass: RED"; fi
exit "$FAILED"
