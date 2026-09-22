#!/bin/sh
# WHY: the moment README's "Every window in a frame" bullet promises -- two
# windows open at once, each with its own frame and gap, right after
# `:View ui panes tiles` turns tiling on. cap.sh's single --keys argument
# cannot carry two Ex commands each ended by its own Enter, so this script
# drives its own tmux session the same way cap.sh does.
#
# Usage: scripts/dogfood/tapes/tiled-panes.sh [outfile]
set -eu

HERE=$(cd -- "$(dirname -- "$0")" && pwd)
ROOT=$(cd -- "$HERE/../../.." && pwd)
OUT="${1:-$HOME/.claude/tmp/dogfood-tapes/tiled-panes.txt}"

BIN="${VIEW_BIN:-$ROOT/target/release/view}"
[ -x "$BIN" ] || BIN="$ROOT/target/debug/view"
[ -x "$BIN" ] || { echo "tiled-panes.sh: no view binary; build one or set VIEW_BIN" >&2; exit 2; }
command -v tmux >/dev/null 2>&1 || { echo "tiled-panes.sh: tmux is not on PATH" >&2; exit 2; }

SOCKET=view-cap-tiles-$$
. "$HERE/../lib.sh"
trap cleanup EXIT INT TERM

mkdir -p -- "$(dirname -- "$OUT")"
tmux -L "$SOCKET" new-session -d -s cap -x 263 -y 88 "$BIN" README.md
sleep 3
tmux -L "$SOCKET" send-keys -t cap ':View ui panes tiles' Enter
sleep 1
tmux -L "$SOCKET" send-keys -t cap ':vsplit docs/tiled-ui.md' Enter
sleep 2
tmux -L "$SOCKET" capture-pane -p -t cap >"$OUT"
tmux -L "$SOCKET" display-message -p -t cap '#{cursor_x},#{cursor_y}' >"$OUT.cursor"

echo "tiled-panes.sh: capture in $OUT (caret in $OUT.cursor)" >&2
