#!/bin/sh
# Capture harness for dogfood evidence: runs the built `view` inside a
# detached tmux session of a named size, under the user's own config, and
# writes one text capture of what the pane holds.
#
# Usage:
#   cap.sh [--size WIDTHxHEIGHT] [--keys NOTATION] [--submit]
#          [--settle SECONDS] [--ansi] <outfile> [-- <view argument>...]
#
# Examples:
#   cap.sh caps/tiles-263x88.txt
#   cap.sh --size 120x40 --keys ':vsplit' --submit caps/vsplit.txt
#   cap.sh caps/file.txt -- README.md
#   cap.sh --size 220x50 --keys ':View ai open' --submit \
#          --keys 'propose' --submit caps/agent-review.txt
#
# Each --keys is one step: its notation reaches `tmux send-keys` as one
# argument, then --submit (when it follows that --keys) sends the Enter a
# command line waits for, then the capture waits --settle seconds before the
# next step. Two steps are two writes with a settle between them, so a
# surface the first step opens has taken the keyboard before the second
# step types into it.
#
# --ansi keeps the SGR escapes in the capture, because a colour defect (a
# frame drawn in a shade the background swallows) is invisible in text.
#
# The caret is written beside the capture, as `<outfile>.cursor` holding
# `column,row`. capture-pane prints no caret of its own, and a caret drawn on
# the wrong cell is invisible in the text.
#
# One capture per invocation, because a capture is evidence for one claim and
# a file holding two of them has to say which is which.
set -eu

HERE=$(cd -- "$(dirname -- "$0")" && pwd)
ROOT=$(cd -- "$HERE/../.." && pwd)

SIZE=263x88
# one step per line: `k` then the notation, or `s` for the Enter that
# submits the step before it
STEPS=
NL='
'
ANSI=
SETTLE=3
OUT=

while [ $# -gt 0 ]; do
  case "$1" in
    (--size) SIZE="${2:?--size takes WIDTHxHEIGHT}"; shift 2 ;;
    (--keys)
      keys="${2:?--keys takes tmux send-keys notation}"
      case "$keys" in
        (*"$NL"*) echo "cap.sh: --keys takes one line of notation" >&2; exit 2 ;;
      esac
      STEPS="$STEPS${STEPS:+$NL}k$keys"; shift 2 ;;
    (--submit)
      case "$STEPS" in
        ('') echo "cap.sh: --submit follows the --keys it submits" >&2; exit 2 ;;
      esac
      STEPS="$STEPS${NL}s"; shift ;;
    (--ansi) ANSI=-e; shift ;;
    (--settle) SETTLE="${2:?--settle takes whole seconds}"; shift 2 ;;
    (--) shift; break ;;
    (-*) echo "cap.sh: unknown option $1" >&2; exit 2 ;;
    (*)
      if [ -n "$OUT" ]; then
        echo "cap.sh: one capture per invocation, and $OUT is already named" >&2
        exit 2
      fi
      OUT="$1"; shift ;;
  esac
done

[ -n "$OUT" ] || { echo "usage: cap.sh [options] <outfile> [-- <view argument>...]" >&2; exit 2; }

COLS=$(echo "$SIZE" | sed 's/x.*//')
ROWS=$(echo "$SIZE" | sed 's/.*x//')
case "$COLS$ROWS" in
  (*[!0-9]*|'') echo "cap.sh: --size wants WIDTHxHEIGHT, got $SIZE" >&2; exit 2 ;;
esac

# a socket of its own, so a capture never attaches to, resizes or kills the
# server the user is working in
SOCKET=view-cap-$$
. "$HERE/lib.sh"

BIN="${VIEW_BIN:-$(newest_build "$ROOT" view)}"
[ -n "$BIN" ] || { echo "cap.sh: no view binary; build one or set VIEW_BIN" >&2; exit 2; }

command -v tmux >/dev/null 2>&1 || { echo "cap.sh: tmux is not on PATH" >&2; exit 2; }

mkdir -p -- "$(dirname -- "$OUT")"
# rust-analyzer attaches to a Rust file and to nothing else, so any other
# capture has no check to wait for
for arg in "$@"; do
  case "$arg" in
    (*.rs) warm_cargo_target "$ROOT" cap.sh; break ;;
  esac
done

new_cap_session "$SOCKET" "$COLS" "$ROWS" -- "$BIN" "$@"
sleep "$SETTLE"
if [ -n "$STEPS" ]; then
  # the settle closes a step, so it runs before the next `k` and after the
  # last one, whether or not that step was submitted
  first=1
  old_ifs=$IFS
  IFS=$NL
  # a notation holding `*` is keys to send, never a pattern to expand
  set -f
  for step in $STEPS; do
    case "$step" in
      (k*)
        [ "$first" = 1 ] || sleep "$SETTLE"
        first=0
        tmux -L "$SOCKET" send-keys -t cap "${step#k}" ;;
      (s) tmux -L "$SOCKET" send-keys -t cap Enter ;;
    esac
  done
  set +f
  IFS=$old_ifs
  sleep "$SETTLE"
fi
tmux -L "$SOCKET" capture-pane -p $ANSI -t cap >"$OUT"
tmux -L "$SOCKET" display-message -p -t cap '#{cursor_x},#{cursor_y}' >"$OUT.cursor"

echo "cap.sh: ${COLS}x${ROWS} capture of $BIN -> $OUT (caret in $OUT.cursor)" >&2
