#!/bin/sh
# WHY: the moment README's "Your work survives a crash" bullet promises --
# the hang banner, the interrupt/restart modal, and your buffer back after
# nvim wedges. cap.sh cannot pause the engine mid-session, so this script
# drives its own tmux session and SIGSTOPs the nvim child to stand in for a
# real wedge, deterministically, the way docs/performance.md's "The engine
# hangs" section measures it: a 10s wedge threshold plus one 2s probe.
#
# Usage: scripts/dogfood/tapes/engine-restart.sh [outfile]
# Writes <outfile>.banner (the wedge banner) and <outfile> (after <F5>
# restarts the engine).
set -eu

HERE=$(cd -- "$(dirname -- "$0")" && pwd)
ROOT=$(cd -- "$HERE/../../.." && pwd)
OUT="${1:-$HOME/.claude/tmp/dogfood-tapes/engine-restart.txt}"

BIN="${VIEW_BIN:-$ROOT/target/release/view}"
[ -x "$BIN" ] || BIN="$ROOT/target/debug/view"
[ -x "$BIN" ] || { echo "engine-restart.sh: no view binary; build one or set VIEW_BIN" >&2; exit 2; }
command -v tmux >/dev/null 2>&1 || { echo "engine-restart.sh: tmux is not on PATH" >&2; exit 2; }

SOCKET=view-cap-restart-$$
. "$HERE/../lib.sh"
trap cleanup EXIT INT TERM

mkdir -p -- "$(dirname -- "$OUT")"
tmux -L "$SOCKET" new-session -d -s cap -x 263 -y 88 "$BIN" README.md
sleep 3

VIEW_PID=$(tmux -L "$SOCKET" list-panes -t cap -F '#{pane_pid}')
NVIM_PID=$(pgrep -P "$VIEW_PID" -f nvim | head -1) || true
[ -n "$NVIM_PID" ] || { echo "engine-restart.sh: could not find the nvim child" >&2; exit 1; }

kill -STOP "$NVIM_PID"
sleep 13
tmux -L "$SOCKET" capture-pane -p -t cap >"$OUT.banner"

tmux -L "$SOCKET" send-keys -t cap F5
sleep 0.5
kill -CONT "$NVIM_PID" 2>/dev/null || true
sleep 3
tmux -L "$SOCKET" capture-pane -p -t cap >"$OUT"

echo "engine-restart.sh: wedge banner in $OUT.banner, recovered screen in $OUT" >&2
