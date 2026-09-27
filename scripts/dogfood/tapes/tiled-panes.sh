#!/bin/sh
# WHY: the moment README's "Every window in a frame" bullet promises, in
# the shape of a working day: a Rust file beside the README, each in a
# frame of its own, reflowing as the terminal shrinks and grows back, then
# `:View window fit` sizing the Rust tile to its code. cap.sh writes a
# text capture and no gif, so this script drives its own tmux session the
# same way cap.sh does, and hands it to record_gif in lib.sh.
#
# A first launch under a config leaves a "your config also draws ..."
# notice standing over a tile. The dismiss verb is sent while the gif is
# still hidden, repeated against the live pane, because the next notice
# takes the top slot only once the one ahead of it has cleared.
#
# Usage: scripts/dogfood/tapes/tiled-panes.sh [outfile]
set -eu

HERE=$(cd -- "$(dirname -- "$0")" && pwd)
ROOT=$(cd -- "$HERE/../../.." && pwd)
OUT="${1:-$ROOT/assets/tapes/tiled-panes.gif}"

SOCKET=view-cap-tiles-$$
. "$HERE/../lib.sh"

BIN="${VIEW_BIN:-$(newest_build "$ROOT" view)}"
[ -n "$BIN" ] || { echo "tiled-panes.sh: no view binary; build one or set VIEW_BIN" >&2; exit 2; }
command -v tmux >/dev/null 2>&1 || { echo "tiled-panes.sh: tmux is not on PATH" >&2; exit 2; }

recording_state_home

cd -- "$ROOT"
# --panes tiles: under a tiling desktop "auto" answers nvim, and the tape
# shows the frames whichever desktop records it
new_cap_session "$SOCKET" 220 50 -- "$BIN" --panes tiles README.md
(
  wait_for_recorder "$SOCKET"
  start=$(date +%s)
  sleep 2
  tmux -L "$SOCKET" send-keys -t cap ':vsplit crates/view-core/src/update/look.rs' Enter
  sleep 1.5
  n=0
  while [ "$n" -lt 8 ]; do
    pane=$(tmux -L "$SOCKET" capture-pane -p -t cap)
    case "$pane" in
      (*'which view owns'*|*'gives it back'*|*'give them back'*)
        tmux -L "$SOCKET" send-keys -t cap ':View notifications dismiss' Enter
        sleep 0.8
        ;;
      (*)
        break
        ;;
    esac
    n=$((n + 1))
  done
  sleep_until_elapsed "$start" 10
  sleep 1.5
  tmux -L "$SOCKET" resize-window -t cap -x 150 -y 38
  sleep 2.5
  # back to the recorder's own size, which the window took at the attach;
  # an explicit 220x50 leaves a strip of tmux's fill along two edges
  tmux -L "$SOCKET" resize-window -t cap -A
  sleep 2
  tmux -L "$SOCKET" send-keys -t cap ':View window fit'
  sleep 1
  tmux -L "$SOCKET" send-keys -t cap Enter
) &

BODY='Sleep 9500ms
Show
Sleep 12s'
record_gif "$SOCKET" "$OUT" 12 220 50 "$BODY"

echo "tiled-panes.sh: recorded $OUT" >&2
