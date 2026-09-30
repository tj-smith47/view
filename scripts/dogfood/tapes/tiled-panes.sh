#!/bin/sh
# WHY: the moment README's "Every window is a frame" bullet promises, in
# the shape of a working day: a Rust file beside the README, each in a
# frame of its own, resize mode (`<C-w>m`, docs/keymaps.md) stepping the
# Rust tile narrower and back with the status line reading RESIZE, then
# `:View window fit` sizing that tile to its code. cap.sh writes a text
# capture and no gif, so this script drives its own tmux session the same
# way cap.sh does, and hands it to record_gif in lib.sh.
#
# The two files open side by side from the command line, and the launch
# notices are dismissed while the gif is still hidden, so the first frame
# is the settled layout with nothing standing over a tile.
#
# Usage: scripts/dogfood/tapes/tiled-panes.sh [outfile]
set -eu

HERE=$(cd -- "$(dirname -- "$0")" && pwd)
ROOT=$(cd -- "$HERE/../../.." && pwd)
OUT="${1:-$ROOT/assets/tapes/tiled-panes.gif}"

SOCKET=view-cap-tiles-$$
. "$HERE/../lib.sh"

BIN=$(absolute_path "${VIEW_BIN:-$(newest_build "$ROOT" view)}")
[ -n "$BIN" ] || { echo "tiled-panes.sh: no view binary; build one or set VIEW_BIN" >&2; exit 2; }
command -v tmux >/dev/null 2>&1 || { echo "tiled-panes.sh: tmux is not on PATH" >&2; exit 2; }

recording_state_home
warm_cargo_target "$ROOT" tiled-panes.sh

cd -- "$ROOT"
# --panes tiles: under a tiling desktop "auto" answers nvim, and the tape
# shows the frames whichever desktop records it
new_cap_session "$SOCKET" 220 50 -- "$BIN" --panes tiles \
  crates/view-core/src/update/look.rs README.md -O --cmd "$QUIET_LAZY"
(
  wait_for_recorder "$SOCKET"
  close_config_tree "$SOCKET"
  show_when_settled "$SOCKET" README.md
  sleep 1.5
  # focus is on the Rust tile, the first file on the command line
  tmux -L "$SOCKET" send-keys -t cap C-w m
  sleep 1
  for key in h h h l; do
    tmux -L "$SOCKET" send-keys -t cap "$key"
    sleep 0.7
  done
  sleep 0.5
  tmux -L "$SOCKET" send-keys -t cap Enter
  sleep 1
  tmux -L "$SOCKET" send-keys -t cap ':View window fit'
  sleep 1
  tmux -L "$SOCKET" send-keys -t cap Enter
) &

BODY='Show
Sleep 9700ms'
record_gif "$SOCKET" "$OUT" 10 220 50 "$BODY"

echo "tiled-panes.sh: recorded $OUT" >&2
