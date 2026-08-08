#!/bin/bash
# Drives s1_idle_winit for the S1 measurement: launches it, samples CPU-time
# and RSS (ps -o utime,stime / ps -o rss) and phys_footprint
# (via the `footprint` tool) every 5s for ~65s, keeping the LAST sample taken
# before the process exits as the "t_end" reading (the app self-quits after
# its 6th 10s heartbeat, so we poll instead of racing a fixed sleep against
# its exit). Captures the binary's own stderr heartbeat log too.
set -uo pipefail

BIN=/Users/nurchudlori/Projects/dbx/target-spike/release/s1_idle_winit
LOG=/tmp/spike_s1_winit_run.log
: > "$LOG"

echo "=== S1 (winit+wgpu) driver starting $(date) ===" | tee -a "$LOG"

"$BIN" >> "$LOG" 2>&1 &
PID=$!
echo "pid=$PID" | tee -a "$LOG"

sleep 1
echo "--- ps at t0 ---" | tee -a "$LOG"
ps -o pid,utime,stime,rss,vsz -p "$PID" | tee -a "$LOG"
echo "--- footprint at t0 ---" | tee -a "$LOG"
footprint "$PID" 2>&1 | tee -a "$LOG"

LAST_PS=""
LAST_FOOTPRINT=""
LAST_T=0
for i in $(seq 1 15); do
  sleep 5
  T=$((i * 5))
  if ! kill -0 "$PID" 2>/dev/null; then
    echo "process exited between t=$LAST_T s and t=$T s; using t=$LAST_T s sample as t_end" | tee -a "$LOG"
    break
  fi
  LAST_PS=$(ps -o pid,utime,stime,rss,vsz -p "$PID" 2>/dev/null)
  LAST_FOOTPRINT=$(footprint "$PID" 2>&1)
  LAST_T=$T
  echo "[driver] t=${T}s sampled ok" | tee -a "$LOG"
done

echo "--- ps at t_end (t=${LAST_T}s, last successful sample before exit/cap) ---" | tee -a "$LOG"
echo "$LAST_PS" | tee -a "$LOG"
echo "--- footprint at t_end (t=${LAST_T}s) ---" | tee -a "$LOG"
echo "$LAST_FOOTPRINT" | tee -a "$LOG"

if kill -0 "$PID" 2>/dev/null; then
  echo "process still alive after cap, killing" | tee -a "$LOG"
  kill "$PID" 2>/dev/null
fi
wait "$PID" 2>/dev/null

echo "=== S1 (winit+wgpu) driver done $(date) ===" | tee -a "$LOG"
