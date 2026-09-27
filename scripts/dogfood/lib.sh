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
  editor_pid=$(tmux -L "$SOCKET" list-panes -a -F '#{pane_pid}' 2>/dev/null | head -1) || editor_pid=
  tmux -L "$SOCKET" kill-server 2>/dev/null || true
  if [ -n "${TAPE:-}" ]; then
    rm -f -- "$TAPE"
  fi
  if [ -n "${STATE_COPY:-}" ]; then
    # the editor the server just hung up on writes its theme cache into the
    # state copy as it exits, seconds later on a loaded host, so the copy
    # is removed once that process is gone, waiting ten seconds at most
    n=0
    while [ -n "$editor_pid" ] && kill -0 "$editor_pid" 2>/dev/null && [ "$n" -lt 100 ]; do
      sleep 0.1
      n=$((n + 1))
    done
    rm -rf -- "$STATE_COPY"
  fi
}

# WHY: every capture opens its session here, so none of them carries tmux's
# status line under the editor. The option is set on the server before the
# session exists, which gives the pane its full -y rows from the first
# frame; turning it off afterwards would resize the editor once as it
# starts. Usage: new_cap_session SOCKET COLS ROWS -- CMD [ARG...]
new_cap_session() {
  socket=$1
  cols=$2
  rows=$3
  [ "${4:-}" = -- ] || {
    echo "new_cap_session: usage: new_cap_session SOCKET COLS ROWS -- CMD..." >&2
    return 2
  }
  shift 4
  # tmux draws italics as standout when default-terminal names screen, which
  # a person's tmux.conf often does, and every italic comment then recorded
  # as a reversed block. The fallback is for a host whose terminfo has no
  # tmux entry.
  term=tmux-256color
  infocmp "$term" >/dev/null 2>&1 || term=xterm-256color
  tmux -L "$socket" set-option -g status off \; \
    set-option -g default-terminal "$term" \; \
    new-session -d -s cap -x "$cols" -y "$rows" "$@"
}

# WHY: shared by every tape script that turns a live tmux session into a
# gif. vhs needs a terminal attached for the whole recording, which
# cap.sh's headless capture-pane never has, so this drives vhs onto the
# same private socket the caller already attached its session to and
# leaves the resulting gif at $2. $TAPE is a script global, since
# cleanup() above reads it after this function has returned. $4 and $5
# are the tmux session's own columns and rows (its `-x`/`-y`). tmux
# resizes to whatever vhs attaches at (window-size=latest), so the pixel
# canvas is derived from them: a canvas guessed in pixels clipped
# engine-restart.sh's wedge banner at the right edge.
#
# The tape sets a Nerd Font because vhs's bundled font has no icon
# glyphs, and every tree icon, pill separator and statusline symbol under
# a person's config recorded as a box. RECORD_GIF_FONT names another
# family; the cell constants below hold for the default only. At
# FontSize 14 in that font, vhs 0.11.0's grid measures out to
# `cols = floor((Width - 145) / 9)` and `rows = floor((Height - 130) / 18)`:
# `tput cols`/`tput lines` read off plain-shell tapes put the column
# boundaries at Width 1198 and 2206 and the row boundaries at Height 1012,
# 1030, 1048 and 1066. new_cap_session turns tmux's status line off, so
# the pane is the whole session, and margin_cols/margin_rows cover
# hinting drift across hosts and vhs versions this recorder has not
# measured. A caller passes the `-x`/`-y` its session was created at,
# the way every tape script's `-x 220 -y 50` becomes
# `record_gif ... 220 50`.
# $6, when given, is the tape body played after the attach in place of
# `Sleep 500ms`, `Show` and one Sleep of $3 seconds. It starts hidden, so
# a body opens with the Sleep that covers the editor's start and the
# driver's setup, then its own `Show`: the first frame of the gif is the
# settled editor, whatever the attach and the setup took.
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
  font=${RECORD_GIF_FONT:-JetBrainsMono Nerd Font}
  command -v fc-list >/dev/null 2>&1 || {
    echo "record_gif: fc-list is not on PATH, so the tape font cannot be" \
      "checked (install fontconfig)" >&2
    return 2
  }
  # vhs draws in a headless browser that falls back to its bundled font
  # without a word, so a missing family is caught before the recording
  if [ -z "$(fc-list "$font" family 2>/dev/null)" ]; then
    echo "record_gif: font \"$font\" is not installed (fc-list lists no" \
      "such family). Install JetBrainsMono Nerd Font: brew install --cask" \
      "font-jetbrains-mono-nerd-font, or JetBrainsMono.zip from" \
      "github.com/ryanoasis/nerd-fonts/releases unpacked into" \
      "$HOME/.local/share/fonts and fc-cache -f" >&2
    return 2
  fi
  cell_w=9
  cell_h=18
  pad_w=145
  pad_h=130
  margin_cols=2
  margin_rows=1
  width=$(( (cols + margin_cols) * cell_w + pad_w ))
  height=$(( (rows + margin_rows) * cell_h + pad_h ))
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
    printf 'Set FontFamily "%s"\n' "$font"
    printf 'Set Framerate 10\n'
    # the attach is the recorder's own setup, so the gif opens on the editor
    printf 'Hide\n'
    printf 'Type "tmux -L %s attach -t cap"\n' "$socket"
    printf 'Enter\n'
    if [ -n "${6:-}" ]; then
      printf '%s\n' "$6"
    else
      printf 'Sleep 500ms\n'
      printf 'Show\n'
      printf 'Sleep %ss\n' "$seconds"
    fi
  } >"$TAPE"
  tmux -L "$socket" set-hook -g client-attached "wait-for -S $RECORDER_ATTACHED"
  vhs "$TAPE"
  rm -f -- "$TAPE"
  TAPE=
}

# WHY: a tape body that cuts a wait with Hide and Show is timed from the
# recorder's attach, and vhs takes a varying while to start, so a driver
# that times its keys from this return shares the body's clock. tmux
# keeps the signal record_gif's hook raises, so a driver that asks after
# the attach returns at once. The wait ends with the server when the
# recorder never attaches. Usage: wait_for_recorder SOCKET
wait_for_recorder() {
  tmux -L "$1" wait-for "$RECORDER_ATTACHED"
}

# WHY: a first launch under a config leaves a "your config also draws ..."
# notice standing (surface_conflict.rs, record_native_notice_sticky_once),
# and the next one takes the top slot only once the one ahead of it has
# cleared, so the dismiss verb is repeated against the live pane. The cap
# keeps a notice that never clears from hanging the tape.
# Usage: dismiss_launch_notices SOCKET
dismiss_launch_notices() {
  n=0
  while [ "$n" -lt 8 ]; do
    pane=$(tmux -L "$1" capture-pane -p -t cap)
    case "$pane" in
      (*'which view owns'*|*'gives it back'*|*'give them back'*)
        tmux -L "$1" send-keys -t cap ':View notifications dismiss' Enter
        sleep 0.8
        ;;
      (*)
        break
        ;;
    esac
    n=$((n + 1))
  done
}

# WHY: a driver whose setup takes a varying while (notices to dismiss, an
# ssh connection to open) has to start its visible part at the moment the
# tape body shows it, so it waits out the rest of a deadline counted from
# the attach. date +%s is the clock POSIX sh has, and the start it reads
# drops its fraction, so the wait can end up to one second early: a body's
# hidden lead ends a second before the deadline it is paired with.
# Usage: start=$(date +%s); ...; sleep_until_elapsed "$start" SECONDS
sleep_until_elapsed() {
  while [ $(( $(date +%s) - $1 )) -lt "$2" ]; do
    sleep 0.2
  done
}

# WHY: lazy.nvim opens its update report over the editor whenever its last
# check is older than its frequency, and a tape records whatever is on
# screen. Each tape runs against a copy of the state directory with that
# check stamped to now, so the person's own state is never written. The
# copy leaves nvim's swap files behind, so a crash in some earlier session
# raises no swap prompt in the recording. Usage: recording_state_home
# (exports XDG_STATE_HOME; cleanup() removes the copy).
recording_state_home() {
  real="${XDG_STATE_HOME:-$HOME/.local/state}"
  statedir="${XDG_CACHE_HOME:-$HOME/.cache}/view-dogfood-tapes"
  mkdir -p -- "$statedir"
  STATE_COPY=$(mktemp -d "$statedir/state-XXXXXX")
  if [ -d "$real" ]; then
    cp -R -- "$real/." "$STATE_COPY/"
  fi
  rm -rf -- "$STATE_COPY/nvim/swap"
  mkdir -p -- "$STATE_COPY/nvim/lazy"
  lazy_state="$STATE_COPY/nvim/lazy/state.json"
  now=$(date +%s)
  if [ -f "$lazy_state" ] && grep -q '"last_check"' "$lazy_state"; then
    sed "s/\"last_check\":[0-9]*/\"last_check\":$now/" "$lazy_state" >"$lazy_state.new"
    mv -- "$lazy_state.new" "$lazy_state"
  else
    printf '{"checker":{"last_check":%s}}\n' "$now" >"$lazy_state"
  fi
  XDG_STATE_HOME=$STATE_COPY
  export XDG_STATE_HOME
}

# WHY: a capture or a tape records whichever binary it is handed, and a
# release build left over from an earlier day is a recording of code without
# the change under review. So the newer of the two builds is taken, never
# the first one found. Prints nothing when neither exists.
# Usage: newest_build REPO_ROOT BINARY_NAME
newest_build() {
  found=
  for candidate in "$1/target/release/$2" "$1/target/debug/$2"; do
    if [ -x "$candidate" ] && { [ -z "$found" ] || [ "$candidate" -nt "$found" ]; }; then
      found=$candidate
    fi
  done
  printf '%s' "$found"
}

RECORDER_ATTACHED=view-recorder-attached

trap cleanup EXIT INT TERM
