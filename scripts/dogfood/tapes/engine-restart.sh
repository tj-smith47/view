#!/bin/sh
# WHY: the moment README's "Your work survives a crash" bullet promises: the
# hang banner, <F5> answering it, and README back in a fresh engine. cap.sh
# cannot pause the engine mid-session, so this script drives its own tmux
# session and SIGSTOPs the nvim child to stand in for a real wedge.
#
# The stopped engine is never resumed. The restart replaces it with a new
# process and ends the stopped one, so a CONT would show nothing a user
# ever sees.
#
# cap.sh's headless capture-pane produces a text snapshot, and README's tape
# table names a gif, so this hands the live session to record_gif in lib.sh,
# which attaches vhs to it for the whole sequence.
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
new_cap_session "$SOCKET" 220 50 -- "$BIN" README.md
(
  sleep 3
  tmux -L "$SOCKET" send-keys -t cap O 'A line typed before the engine hangs.' Escape
  # past the config's 'updatetime', so a config that keeps swap files has
  # flushed the line before the engine stops
  sleep 4
  VIEW_PID=$(tmux -L "$SOCKET" list-panes -t cap -F '#{pane_pid}')
  NVIM_PID=$(pgrep -P "$VIEW_PID" -f nvim | head -1) || true
  if [ -n "$NVIM_PID" ]; then
    kill -STOP "$NVIM_PID"
    # the banner rises after the 10s wedge threshold plus up to one 2s probe;
    # 15s leaves it on screen a few seconds before the key answers it
    sleep 15
    tmux -L "$SOCKET" send-keys -t cap F5
  fi
) &

record_gif "$SOCKET" "$OUT" 26 220 50

echo "engine-restart.sh: recorded $OUT" >&2
