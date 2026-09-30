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
  # a window smaller than its client is padded with dots by default, and a
  # tape that shrinks the window records them as a pattern over the vacated
  # region
  tmux -L "$socket" set-option -g status off \; \
    set-option -g default-terminal "$term" \; \
    set-option -g fill-character " " \; \
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
# family; the cell constants below hold for the default only. The tape
# sets Padding 0, so the gif is the terminal with no blank rim around it,
# and at FontSize 14 in that font vhs 0.11.0's grid then measures out to
# `cols = floor((Width - 25) / 9)` and `rows = floor((Height - 10) / 18)`:
# `tput cols`/`tput lines` read off plain-shell tapes put a column
# boundary at Width 2086 (228 columns at 2084) and row boundaries at
# Height 910 and 928. new_cap_session turns tmux's status line off, so
# the pane is the whole session, and margin_cols/margin_rows cover
# hinting drift across hosts and vhs versions this recorder has not
# measured. A caller passes the `-x`/`-y` its session was created at,
# the way every tape script's `-x 220 -y 50` becomes
# `record_gif ... 220 50`.
# The recording stays hidden until the driver's show_when_settled raises
# its mark, so the first frame is the settled editor however long the
# start took. $6, when given, is the tape body played from that moment in
# place of `Show` and one Sleep of $3 seconds; tape_body_opens_on_show
# says what it may open with. An empty $2 writes no gif, which is how
# record_still below takes a frame and nothing else.
record_gif() {
  socket=$1
  out=$2
  seconds=$3
  cols=${4:?record_gif: pass the tmux session -x columns}
  rows=${5:?record_gif: pass the tmux session -y rows}
  body=${6:-"Show
Sleep ${seconds}s"}
  tape_body_opens_on_show "$body" || {
    echo "record_gif: a tape body opens with its own Show, after Sleep" \
      "lines at most, and this one does not:" >&2
    printf '%s\n' "$body" >&2
    return 2
  }
  command -v vhs >/dev/null 2>&1 || {
    echo "record_gif: vhs is not on PATH (build it with" \
      "scripts/dogfood/vhs/build.sh)" >&2
    return 2
  }
  # stock vhs reads the cursor and text layers of a frame in two calls, and
  # a frame the terminal draws between them records the old cursor over the
  # new text
  vhs_found=$(vhs --version 2>/dev/null) || vhs_found=
  case "$vhs_found" in
    (*" $RECORD_GIF_VHS_VERSION") ;;
    (*)
      echo "record_gif: $(command -v vhs) reports \"$vhs_found\", and a tape" \
        "is recorded with vhs $RECORD_GIF_VHS_VERSION. Build it with" \
        "scripts/dogfood/vhs/build.sh and put its install directory ahead" \
        "of this one on PATH" >&2
      return 2
      ;;
  esac
  # tmux applies the synchronized-output bracket view writes from 3.7 on,
  # and an older one records half-drawn frames
  tmux_found=$(tmux -V 2>/dev/null) || tmux_found=
  if ! printf '%s\n' "$tmux_found" | awk '$1 == "tmux" {
      sub(/^next-/, "", $2); split($2, v, ".")
      if (v[1] ~ /^[0-9]+$/ && (v[1] > 3 || (v[1] == 3 && v[2] + 0 >= 7))) ok = 1
    } END { exit !ok }'; then
    echo "record_gif: $(command -v tmux) reports \"$tmux_found\", and a tape" \
      "needs tmux 3.7 or later" >&2
    return 2
  fi
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
  pad_w=25
  pad_h=10
  margin_cols=2
  margin_rows=1
  width=$(( (cols + margin_cols) * cell_w + pad_w ))
  height=$(( (rows + margin_rows) * cell_h + pad_h ))
  if [ -n "$out" ]; then
    mkdir -p -- "$(dirname -- "$out")"
  fi
  tapedir="${XDG_CACHE_HOME:-$HOME/.cache}/view-dogfood-tapes"
  mkdir -p -- "$tapedir"
  TAPE=$(mktemp "$tapedir/tape-XXXXXX.tape")
  {
    if [ -n "$out" ]; then
      printf 'Output "%s"\n' "$out"
    fi
    printf 'Require tmux\n'
    printf 'Set Width %s\n' "$width"
    printf 'Set Height %s\n' "$height"
    printf 'Set Padding 0\n'
    printf 'Set FontSize 14\n'
    printf 'Set FontFamily "%s"\n' "$font"
    printf 'Set Framerate 10\n'
    # the attach is the recorder's own setup, so the gif opens on the editor
    printf 'Hide\n'
    printf 'Type "tmux -L %s attach -t cap"\n' "$socket"
    printf 'Enter\n'
    # the recorder's ceiling sits past the driver's own, so a driver that
    # gives up has said why on stderr before the recording fails
    printf 'Wait+Screen@%ss /%s/\n' "$((SETTLE_CEILING_TENTHS / 10 + 15))" "$SETTLED_MARK"
    printf 'Sleep %sms\n' "$SETTLED_MARK_CLEARS_MS"
    printf '%s\n' "$body"
  } >"$TAPE"
  tmux -L "$socket" set-hook -g client-attached "wait-for -S $RECORDER_ATTACHED"
  vhs "$TAPE"
  rm -f -- "$TAPE"
  TAPE=
}

# WHY: a still shows one scene the driver builds after the editor has
# settled, and how long that takes depends on the config and the agent, so
# the frame is taken when the driver raises the settled mark a second
# time. vhs writes a Screenshot only as it renders the frame after it,
# hence the Sleep behind it. $3 and $4 are the tmux
# session's own columns and rows, as for record_gif.
# Usage: record_still SOCKET OUT.png COLS ROWS
record_still() {
  mkdir -p -- "$(dirname -- "$2")"
  record_gif "$1" "" 0 "${3:?record_still: pass the tmux session -x columns}" \
    "${4:?record_still: pass the tmux session -y rows}" "Show
Wait+Screen@$((SETTLE_CEILING_TENTHS / 10 + 15))s /$SETTLED_MARK/
Sleep ${SETTLED_MARK_CLEARS_MS}ms
Screenshot \"$2\"
Sleep 100ms"
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

# WHY: the user's config opens nvim-tree on every start, a moment after
# its plugins load, and a tape shows the files it opened, or view's own
# tree, and never a sidebar its subject did not ask for. The config is
# left as it is; the tree is closed once it is on the pane, or after the
# wait ends with no tree shown, and either way before the tape shows.
# Usage: close_config_tree SOCKET
close_config_tree() {
  tenths=0
  while [ "$tenths" -lt 50 ]; do
    case "$(tmux -L "$1" capture-pane -p -t cap)" in
      (*NvimTree*) break ;;
    esac
    sleep 0.1
    tenths=$((tenths + 1))
  done
  tmux -L "$1" send-keys -t cap ':silent! NvimTreeClose' Enter
}

# WHY: the start of a tape takes as long as the editor, the config and an
# ssh connection take that day, so the recording shows from a condition
# and never from a clock. The pane is read at the recording's frame rate:
# a launch notice is dismissed as it appears (a first launch under a
# config leaves a "your config also draws ..." notice standing, and the
# next takes the top slot once the one ahead has cleared), and the editor
# is settled once the pane, colours included, has held still for a
# second with TEXT on it when TEXT is given. Then the mark record_gif's
# tape waits for is flashed on tmux's message line, and this returns once
# the mark has cleared, at the moment the tape shows. A pane that never
# settles fails here by name, and the recorder fails after it.
# Usage: show_when_settled SOCKET [TEXT]
show_when_settled() {
  last=
  still=0
  tenths=0
  while [ "$tenths" -lt "$SETTLE_CEILING_TENTHS" ]; do
    pane=$(tmux -L "$1" capture-pane -p -t cap) || pane=
    case "$pane" in
      (*'which view owns'*|*'gives it back'*|*'give them back'*)
        tmux -L "$1" send-keys -t cap ':View notifications dismiss' Enter
        sleep 0.8
        tenths=$((tenths + 8))
        still=0
        last=
        continue
        ;;
    esac
    styled=$(tmux -L "$1" capture-pane -p -e -t cap) || styled=
    case "$pane" in
      (*"${2:-}"*)
        if [ -n "$styled" ] && [ "$styled" = "$last" ]; then
          still=$((still + 1))
        else
          still=0
        fi
        ;;
      (*) still=0 ;;
    esac
    if [ "$still" -ge 10 ]; then
      tmux -L "$1" display-message -d "$SETTLED_MARK_SHOWN_MS" "$SETTLED_MARK"
      sleep "$(awk -v ms="$SETTLED_MARK_CLEARS_MS" 'BEGIN { print ms / 1000 }')"
      return 0
    fi
    last=$styled
    sleep 0.1
    tenths=$((tenths + 1))
  done
  echo "show_when_settled: the pane on socket $1 did not hold still" \
    "${2:+with \"$2\" on it }for a second within" \
    "$((SETTLE_CEILING_TENTHS / 10)) s, so the tape has no settled editor" \
    "to show" >&2
  return 1
}

# WHY: record_gif shows the recording at the settled mark, and a body that
# typed, hid or waited before its own Show would record that step on no
# frame at all, or open the gif on whatever the body did first. So a body
# opens with Show, after Sleep lines at most. Usage: tape_body_opens_on_show
# BODY (status 0 when it does)
tape_body_opens_on_show() {
  printf '%s\n' "$1" | {
    while IFS= read -r line; do
      case "$line" in
        (Show) exit 0 ;;
        ('Sleep '*|'') ;;
        (*) exit 1 ;;
      esac
    done
    exit 1
  }
}

# WHY: lazy.nvim's checker reports updates already fetched into the
# plugin clones on every start, from a list it keeps in memory, so no
# state a tape can stamp holds it off. A tape hands view this as
# `--cmd "$QUIET_LAZY"`, which view passes to the editor it starts and to
# any it restarts, and the report stays off that recording while the
# person's config is left as it is. The tape scripts that source this file
# read it.
# shellcheck disable=SC2034
QUIET_LAZY='lua vim.api.nvim_create_autocmd("User", { pattern = "LazyDone", once = true, callback = function() require("lazy.core.config").options.checker.notify = false end })'

# WHY: lazy.nvim fetches into the person's plugin clones whenever its last
# check is older than its frequency. Each tape runs against a copy of the
# state directory with that check stamped to now, so no clone is fetched
# into and the person's own state is never written. The copy leaves nvim's
# swap files behind, so a crash in some earlier session raises no swap
# prompt in the recording. Usage: recording_state_home (exports
# XDG_STATE_HOME; cleanup() removes the copy).
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

# WHY: a file opened under a person's config starts rust-analyzer, whose
# cargo check makes and removes temp directories under the target dir, and
# a file tree watching the recorded repo raises a notice naming one that
# vanished. The editor inherits this from the tmux server the capture
# starts, so its checks build in a cache outside the tree, warm from one
# run to the next.
CARGO_TARGET_DIR="${XDG_CACHE_HOME:-$HOME/.cache}/view-dogfood-tapes/target"
export CARGO_TARGET_DIR

# WHY: rust-analyzer checks the workspace as a Rust file opens, and a check
# that has to build lands its highlighting and diagnostics seconds into the
# capture, so the cache above is brought up to date first. Cold it builds
# the whole workspace, which is why it runs niced and says so first. A tree
# that does not check would put its errors on screen, so the capture stops.
# Usage: warm_cargo_target REPO_ROOT CALLER_NAME
warm_cargo_target() {
  command -v cargo >/dev/null 2>&1 || return 0
  echo "$2: warming the cargo target dir $CARGO_TARGET_DIR" >&2
  if ! (cd -- "$1" && nice -n 15 cargo check --workspace --all-targets); then
    echo "$2: cargo check failed, so nothing is recorded" >&2
    exit 2
  fi
}

# WHY: a tape changes directory before it starts the editor, so a binary the
# caller named relative to its own directory is resolved first. Prints an
# empty path as it is. Usage: absolute_path PATH
absolute_path() {
  case "$1" in
    (/*|'') printf '%s' "$1" ;;
    (*) printf '%s/%s' "$PWD" "$1" ;;
  esac
}

RECORDER_ATTACHED=view-recorder-attached
# the version scripts/dogfood/vhs/build.sh stamps on the vhs it builds
RECORD_GIF_VHS_VERSION=v0.11.0-view1
SETTLED_MARK=view-tape-settled
# the mark stands long enough for the recorder's screen poll to see it, and
# the tape shows once it has gone
SETTLED_MARK_SHOWN_MS=400
SETTLED_MARK_CLEARS_MS=600
SETTLE_CEILING_TENTHS=300

trap cleanup EXIT INT TERM
