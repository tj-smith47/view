#!/bin/sh
# WHY: the moment README's "Agents in the editor" bullet promises -- the
# ACP agent panel opening beside your buffer, captured under a real config
# rather than a fixture, since `:View ai toggle` is the config-independent
# form of `<leader>ai` (the leader key itself is whatever the user's own
# config sets, per docs/keymaps.md).
#
# cap.sh's headless capture-pane produces a text snapshot, not the gif
# README's tape table names, so this drives its own tmux session the way
# cap.sh does and hands it to record_gif in lib.sh for the recording.
#
# Usage: scripts/dogfood/tapes/agent-panel.sh [outfile]
set -eu

HERE=$(cd -- "$(dirname -- "$0")" && pwd)
ROOT=$(cd -- "$HERE/../../.." && pwd)
OUT="${1:-$ROOT/assets/tapes/agent-panel.gif}"

BIN="${VIEW_BIN:-$ROOT/target/release/view}"
[ -x "$BIN" ] || BIN="$ROOT/target/debug/view"
[ -x "$BIN" ] || { echo "agent-panel.sh: no view binary; build one or set VIEW_BIN" >&2; exit 2; }
command -v tmux >/dev/null 2>&1 || { echo "agent-panel.sh: tmux is not on PATH" >&2; exit 2; }

SOCKET=view-cap-agent-$$
. "$HERE/../lib.sh"

cd -- "$ROOT"
tmux -L "$SOCKET" new-session -d -s cap -x 220 -y 50 "$BIN" README.md
(
  sleep 3
  tmux -L "$SOCKET" send-keys -t cap ':View ai toggle' Enter
) &

record_gif "$SOCKET" "$OUT" 8

echo "agent-panel.sh: recorded $OUT" >&2
