#!/bin/sh
# WHY: the moment README's "Your work survives a crash" bullet promises --
# the hang banner, the interrupt/restart modal, and your buffer back after
# nvim wedges. cap.sh cannot pause the engine mid-session, so this script
# drives its own tmux session and SIGSTOPs the nvim child to stand in for a
# real wedge, deterministically, the way docs/performance.md's "The engine
# hangs" section measures it: a 10s wedge threshold plus one 2s probe.
#
# cap.sh's headless capture-pane produces a text snapshot, not the gif
# README's tape table names, so this hands the live session to record_gif
# in lib.sh, which attaches vhs to it for the whole sequence: launch,
# wedge, banner, <F5>, recovery.
#
# Usage: scripts/dogfood/tapes/engine-restart.sh [outfile]
set -eu

HERE=$(cd -- "$(dirname -- "$0")" && pwd)
ROOT=$(cd -- "$HERE/../../.." && pwd)
OUT="${1:-$ROOT/assets/tapes/engine-restart.gif}"

BIN="${VIEW_BIN:-$ROOT/target/release/view}"
[ -x "$BIN" ] || BIN="$ROOT/target/debug/view"
[ -x "$BIN" ] || { echo "engine-restart.sh: no view binary; build one or set VIEW_BIN" >&2; exit 2; }
command -v tmux >/dev/null 2>&1 || { echo "engine-restart.sh: tmux is not on PATH" >&2; exit 2; }

SOCKET=view-cap-restart-$$
. "$HERE/../lib.sh"

cd -- "$ROOT"
tmux -L "$SOCKET" new-session -d -s cap -x 220 -y 50 "$BIN" README.md
(
  sleep 3
  VIEW_PID=$(tmux -L "$SOCKET" list-panes -t cap -F '#{pane_pid}')
  NVIM_PID=$(pgrep -P "$VIEW_PID" -f nvim | head -1) || true
  if [ -n "$NVIM_PID" ]; then
    kill -STOP "$NVIM_PID"
    # vhs takes a few seconds of its own to launch and attach before its
    # recording clock starts, on top of the wedge's own threshold -- 16s
    # here is margin against both, confirmed against a live run logging
    # `crate::vlog`'s own supervision verdicts (VIEW_LOG=path): the banner
    # is up by 10s and stays until this CONT
    sleep 16
    tmux -L "$SOCKET" send-keys -t cap F5
    sleep 0.5
    kill -CONT "$NVIM_PID" 2>/dev/null || true
  fi
) &

record_gif "$SOCKET" "$OUT" 26

echo "engine-restart.sh: recorded $OUT" >&2
