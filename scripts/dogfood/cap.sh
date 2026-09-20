#!/bin/sh
# Capture harness for dogfood evidence: runs the built `view` inside a
# detached tmux session of a named size, under the user's own config, and
# writes one text capture of what the pane holds.
#
# Usage:
#   cap.sh [--size WIDTHxHEIGHT] [--keys NOTATION] [--settle SECONDS] \
#          <outfile> [-- <view argument>...]
#
# Examples:
#   cap.sh caps/tiles-263x88.txt
#   cap.sh --size 120x40 --keys ':vsplit' caps/vsplit.txt
#   cap.sh caps/file.txt -- README.md
#
# One capture per invocation, because a capture is evidence for one claim and
# a file holding two of them has to say which is which.
set -eu

HERE=$(cd -- "$(dirname -- "$0")" && pwd)
ROOT=$(cd -- "$HERE/../.." && pwd)

SIZE=263x88
KEYS=
SETTLE=3
OUT=

while [ $# -gt 0 ]; do
  case "$1" in
    (--size) SIZE="${2:?--size takes WIDTHxHEIGHT}"; shift 2 ;;
    (--keys) KEYS="${2:?--keys takes tmux send-keys notation}"; shift 2 ;;
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

BIN="${VIEW_BIN:-}"
if [ -z "$BIN" ]; then
  for candidate in "$ROOT/target/release/view" "$ROOT/target/debug/view"; do
    if [ -x "$candidate" ]; then BIN="$candidate"; break; fi
  done
fi
[ -n "$BIN" ] || { echo "cap.sh: no view binary; build one or set VIEW_BIN" >&2; exit 2; }

command -v tmux >/dev/null 2>&1 || { echo "cap.sh: tmux is not on PATH" >&2; exit 2; }

# a socket of its own, so a capture never attaches to, resizes or kills the
# server the user is working in
SOCKET=view-cap-$$
cleanup() { tmux -L "$SOCKET" kill-server 2>/dev/null || true; }
trap cleanup EXIT INT TERM

mkdir -p -- "$(dirname -- "$OUT")"

tmux -L "$SOCKET" new-session -d -s cap -x "$COLS" -y "$ROWS" "$BIN" "$@"
sleep "$SETTLE"
if [ -n "$KEYS" ]; then
  tmux -L "$SOCKET" send-keys -t cap "$KEYS"
  sleep "$SETTLE"
fi
tmux -L "$SOCKET" capture-pane -p -t cap >"$OUT"

echo "cap.sh: ${COLS}x${ROWS} capture of $BIN -> $OUT" >&2
