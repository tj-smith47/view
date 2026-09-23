#!/bin/sh
# WHY: the moment README's "Remote editing" bullet promises -- view opening
# a file on another machine over SSH with the same editor, config and
# clipboard as at home. REMOTE names the machine and file, since this repo
# ships no host to demonstrate on; REMOTE_NVIM_BIN names the remote nvim
# when it is not on the login PATH ssh resolves (view's own --nvim-bin,
# forwarded to the far side).
#
# cap.sh's headless capture-pane produces a text snapshot, not the gif
# README's tape table names, so this drives its own tmux session the way
# cap.sh does and hands it to record_gif in lib.sh for the recording.
#
# Usage: REMOTE=host:path [REMOTE_NVIM_BIN=/path/to/nvim] \
#          scripts/dogfood/tapes/remote-editing.sh [outfile]
set -eu

HERE=$(cd -- "$(dirname -- "$0")" && pwd)
ROOT=$(cd -- "$HERE/../../.." && pwd)
: "${REMOTE:?set REMOTE=host:path to the machine and file this tape opens}"
OUT="${1:-$ROOT/assets/tapes/remote-editing.gif}"

BIN="${VIEW_BIN:-$ROOT/target/release/view}"
[ -x "$BIN" ] || BIN="$ROOT/target/debug/view"
[ -x "$BIN" ] || { echo "remote-editing.sh: no view binary; build one or set VIEW_BIN" >&2; exit 2; }
command -v tmux >/dev/null 2>&1 || { echo "remote-editing.sh: tmux is not on PATH" >&2; exit 2; }

SOCKET=view-cap-remote-$$
. "$HERE/../lib.sh"

cd -- "$ROOT"
if [ -n "${REMOTE_NVIM_BIN:-}" ]; then
  tmux -L "$SOCKET" new-session -d -s cap -x 220 -y 50 \
    "$BIN" --nvim-bin "$REMOTE_NVIM_BIN" --remote "$REMOTE"
else
  tmux -L "$SOCKET" new-session -d -s cap -x 220 -y 50 "$BIN" --remote "$REMOTE"
fi

record_gif "$SOCKET" "$OUT" 15

echo "remote-editing.sh: recorded $OUT" >&2
