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
# are the tmux session's own columns and rows (its `-x`/`-y`), not vhs's
# pixel Width/Height: tmux resizes to whatever vhs attaches at
# (window-size=latest), so a canvas guessed in pixels drifts from the
# session's actual shape -- confirmed against engine-restart.sh's wedge
# banner, clipped at the right edge because the old fixed 2000x820
# default carried no padding budget at all. The pixel canvas here is
# derived from cols/rows instead: at `Set FontSize 14` (the only size
# every tape script requests), vhs's own terminal grid measures out to
# `cols = floor((Width - 147) / 9)` and `rows = floor((Height - 136) / 16)`
# (probed with `tput cols`/`tput lines` against three Width/Height pairs
# on this recorder's build of vhs 0.11.0 -- 1200x600 -> 117x29,
# 2200x1000 -> 228x54, 3000x1400 -> 317x79 -- all three solve the same
# 9px/16px cell and 147px/136px pad exactly). tmux's own status line
# takes one more row than the pane itself, and MARGIN_COLS/MARGIN_ROWS
# cover font-hinting drift across hosts and vhs versions this recorder
# has not measured; a caller whose session was created at a given
# `-x`/`-y` passes that same shape here, the way every tape script's
# `-x 220 -y 50` becomes `record_gif ... 220 50`.
record_gif() {
  socket=$1
  out=$2
  seconds=$3
  cols=${4:?record_gif: pass the tmux session -x columns}
  rows=${5:?record_gif: pass the tmux session -y rows}
  command -v vhs >/dev/null 2>&1 || {
    echo "record_gif: vhs is not on PATH (go install" \
      "github.com/charmbracelet/vhs@latest)" >&2
    return 2
  }
  cell_w=9
  cell_h=16
  pad_w=147
  pad_h=136
  status_rows=1
  margin_cols=2
  margin_rows=1
  width=$(( (cols + margin_cols) * cell_w + pad_w ))
  height=$(( (rows + status_rows + margin_rows) * cell_h + pad_h ))
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
