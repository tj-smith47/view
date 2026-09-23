#!/bin/sh
# WHY: shared by every dogfood tmux capture script (cap.sh and the scripts
# under tapes/), so the private-socket teardown is defined once. Two
# scripts carrying the same function body drift apart on the next edit to
# either one, with nothing to say the other exists
# (view-oracle's shell_guards.rs, no_two_scripts_define_the_same_function_body).
#
# Sourced, never executed. The caller defines SOCKET before sourcing this
# file; the trap armed at the bottom is what pairs record_gif's mktemp
# with its removal (check-style.sh's check_temp_traps reads both from the
# same file), so a caller never arms its own.
cleanup() {
  tmux -L "$SOCKET" kill-server 2>/dev/null || true
  if [ -n "${TAPE:-}" ]; then
    rm -f -- "$TAPE"
  fi
}

# WHY: shared by every tape script that turns a live tmux session into a
# gif. vhs needs a terminal attached for the whole recording, which
# cap.sh's headless capture-pane never has, so this drives vhs onto the
# same private socket the caller already attached its session to and
# leaves the resulting gif at $2. $TAPE is a script global, not a local:
# cleanup() above reads it after this function has returned. $4 and $5
# are vhs's own pixel Width/Height, not the tmux session's columns and
# rows: tmux resizes to whatever vhs attaches at (window-size=latest),
# so a caller whose session was created at a given -x/-y needs a canvas
# that maps to at least that many columns/rows or vhs's own recording
# crops the far side of the pane -- not just a tile running off the edge
# but, confirmed against engine-restart.sh's wedge banner, a right-anchored
# floating notice that nvim still carries in its grid model but vhs never
# captures a pixel of. Every tape script here opens its session at
# `-x 220 -y 50`, so the default is sized for that (roughly 8.6px/col,
# 15px/row at FontSize 14) with margin, and a caller opening a bigger
# session passes its own width/height the same way tiled-panes.sh's two
# tiles do.
record_gif() {
  socket=$1
  out=$2
  seconds=$3
  width=${4:-2000}
  height=${5:-820}
  command -v vhs >/dev/null 2>&1 || {
    echo "record_gif: vhs is not on PATH (go install" \
      "github.com/charmbracelet/vhs@latest)" >&2
    return 2
  }
  mkdir -p -- "$(dirname -- "$out")"
  tapedir="${XDG_CACHE_HOME:-$HOME/.cache}/view-dogfood-tapes"
  mkdir -p -- "$tapedir"
  TAPE=$(mktemp "$tapedir/tape-XXXXXX.tape")
  {
    printf 'Output "%s"\n' "$out"
    printf 'Require tmux\n'
    printf 'Set Width %s\n' "$width"
    printf 'Set Height %s\n' "$height"
    printf 'Set FontSize 14\n'
    printf 'Type "tmux -L %s attach -t cap"\n' "$socket"
    printf 'Enter\n'
    printf 'Sleep %ss\n' "$seconds"
  } >"$TAPE"
  vhs "$TAPE"
  rm -f -- "$TAPE"
  TAPE=
}

trap cleanup EXIT INT TERM
