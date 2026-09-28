#!/bin/sh
# WHY: the moment README's "Remote editing" bullet promises: view editing a
# file on another machine over SSH, where the characters you type appear
# as you press them. REMOTE names the machine and file, since this repo
# ships no host to demonstrate on; REMOTE_NVIM_BIN names the remote nvim
# when it is not on the PATH a non-login ssh resolves.
#
# The remote nvim runs under remote-clean-init.lua, a fixture shipped
# beside this script, through a wrapper forwarded as --nvim-bin that runs
# it under `-u`. `--app-name` conflicts with `--remote`, and --nvim-bin
# takes a bare executable path, so the wrapper carries the flag across.
# The colorscheme the local config sets travels with it, read off the
# local nvim by local-theme-probe.lua, so the far side is themed as the
# person's own editor is. A scheme under a plugin manager's root travels
# as its plugin directory, and one anywhere else as its one colors file,
# since the directory two levels above that file can be the whole config.
# A local Normal with no background is replayed on the far side. The local
# view side keeps the user's own config throughout.
#
# cap.sh's headless capture-pane produces a text snapshot, and README's
# tape table names a gif, so this drives its own tmux session and hands it
# to record_gif in lib.sh.
#
# Usage: REMOTE=host:path [REMOTE_NVIM_BIN=/path/to/nvim] \
#          scripts/dogfood/tapes/remote-editing.sh [outfile]
set -eu

HERE=$(cd -- "$(dirname -- "$0")" && pwd)
ROOT=$(cd -- "$HERE/../../.." && pwd)
: "${REMOTE:?set REMOTE=host:path to the machine and file this tape opens}"
OUT="${1:-$ROOT/assets/tapes/remote-editing.gif}"
REMOTE_HOST=${REMOTE%%:*}

SOCKET=view-cap-remote-$$
. "$HERE/../lib.sh"

BIN=$(absolute_path "${VIEW_BIN:-$(newest_build "$ROOT" view)}")
[ -n "$BIN" ] || { echo "remote-editing.sh: no view binary; build one or set VIEW_BIN" >&2; exit 2; }
command -v tmux >/dev/null 2>&1 || { echo "remote-editing.sh: tmux is not on PATH" >&2; exit 2; }
command -v ssh >/dev/null 2>&1 || { echo "remote-editing.sh: ssh is not on PATH" >&2; exit 2; }
command -v scp >/dev/null 2>&1 || { echo "remote-editing.sh: scp is not on PATH" >&2; exit 2; }

recording_state_home

REMOTE_REAL_NVIM="${REMOTE_NVIM_BIN:-nvim}"
REMOTE_DIR=/tmp/view-dogfood-remote-$$
cachedir="${XDG_CACHE_HOME:-$HOME/.cache}/view-dogfood-tapes"
mkdir -p -- "$cachedir"
WRAPPER="$cachedir/remote-nvim-$$.sh"
# a colorscheme plugin is a few hundred kilobytes, and a directory past
# this is something other than a scheme
THEME_COPY_MAX_BYTES=8388608

cleanup_remote_editing() {
  ssh "$REMOTE_HOST" "rm -rf -- '$REMOTE_DIR'" 2>/dev/null || true
  rm -f -- "$WRAPPER"
  cleanup
}
trap cleanup_remote_editing EXIT INT TERM

theme=
theme_file=
lazy_root=
background=opaque
if command -v nvim >/dev/null 2>&1; then
  answer=$(nvim --headless -c "luafile $HERE/local-theme-probe.lua" \
    </dev/null 2>/dev/null) || answer=
  theme=$(printf '%s\n' "$answer" | sed -n 1p)
  theme_file=$(printf '%s\n' "$answer" | sed -n 2p)
  lazy_root=$(printf '%s\n' "$answer" | sed -n 3p)
  background=$(printf '%s\n' "$answer" | sed -n 4p)
  if [ -z "$theme" ]; then
    echo "remote-editing.sh: the local config set no colorscheme by VimEnter" \
      "and VeryLazy, so the far side uses habamax" >&2
  elif [ -z "$theme_file" ]; then
    echo "remote-editing.sh: no colors/$theme file is on the local" \
      "runtimepath, so the far side uses habamax" >&2
  fi
else
  echo "remote-editing.sh: no local nvim to read the colorscheme from, so" \
    "the far side uses habamax" >&2
fi

ssh "$REMOTE_HOST" "mkdir -p -- '$REMOTE_DIR/theme/colors'"
scp -q -- "$HERE/remote-clean-init.lua" "$REMOTE_HOST:$REMOTE_DIR/init.lua"
theme_dir=
if [ -n "$theme_file" ]; then
  plugin_root=$(dirname -- "$(dirname -- "$theme_file")")
  manager_dir=$(dirname -- "$plugin_root")
  managed=
  case "$manager_dir" in
    ("$lazy_root") managed=1 ;;
    (*/start|*/opt)
      if [ "$(basename -- "$(dirname -- "$(dirname -- "$manager_dir")")")" = pack ]; then
        managed=1
      fi
      ;;
  esac
  case "$plugin_root" in
    # a scheme nvim ships is on the remote runtimepath already
    ("${VIMRUNTIME:-/nonexistent}"|*/share/nvim/runtime) ;;
    (*)
      if [ -n "$managed" ]; then
        bytes=$(tar -C "$plugin_root" --exclude=.git -cf - . | wc -c | tr -d ' ')
        if [ "$bytes" -gt "$THEME_COPY_MAX_BYTES" ]; then
          echo "remote-editing.sh: $plugin_root holds $bytes bytes, past the" \
            "$THEME_COPY_MAX_BYTES a colorscheme plugin is copied at, so" \
            "nothing is copied to $REMOTE_HOST" >&2
          exit 2
        fi
        tar -C "$plugin_root" --exclude=.git -cf - . |
          ssh "$REMOTE_HOST" "tar -C '$REMOTE_DIR/theme' -xf -"
      else
        echo "remote-editing.sh: $theme_file sits under no plugin manager's" \
          "root, so that one file is copied and a scheme needing more" \
          "falls back to habamax" >&2
        scp -q -- "$theme_file" \
          "$REMOTE_HOST:$REMOTE_DIR/theme/colors/$(basename -- "$theme_file")"
      fi
      theme_dir=$REMOTE_DIR/theme
      ;;
  esac
fi

transparent=
if [ "$background" = transparent ]; then
  transparent=1
fi
{
  printf '#!/bin/sh\n'
  printf "VIEW_DOGFOOD_THEME='%s' VIEW_DOGFOOD_THEME_DIR='%s'\n" "$theme" "$theme_dir"
  printf "VIEW_DOGFOOD_TRANSPARENT='%s'\n" "$transparent"
  printf 'export VIEW_DOGFOOD_THEME VIEW_DOGFOOD_THEME_DIR VIEW_DOGFOOD_TRANSPARENT\n'
  printf "exec '%s' -u '%s' \"\$@\"\n" "$REMOTE_REAL_NVIM" "$REMOTE_DIR/init.lua"
} >"$WRAPPER"
scp -q -- "$WRAPPER" "$REMOTE_HOST:$REMOTE_DIR/nvim.sh"
# macOS chmod takes no `--`, and the path is one this script chose
ssh "$REMOTE_HOST" "chmod +x '$REMOTE_DIR/nvim.sh'"

# the far side's init falls back to habamax in silence, so the scheme it
# loaded is read back before the recording
if [ -n "$theme" ]; then
  loaded=$(ssh "$REMOTE_HOST" "'$REMOTE_DIR/nvim.sh' --headless -c 'lua io.stdout:write(vim.g.colors_name or \"\")' -c 'qa!' </dev/null 2>/dev/null") || loaded=
  if [ "$loaded" != "$theme" ]; then
    echo "remote-editing.sh: $REMOTE_HOST did not load $theme from what was" \
      "copied (it loaded ${loaded:-nothing}), so the far side uses habamax" >&2
  fi
fi

cd -- "$ROOT"
new_cap_session "$SOCKET" 220 50 -- \
  "$BIN" --nvim-bin "$REMOTE_DIR/nvim.sh" --remote "$REMOTE"

SENTENCE='Every key you press shows up at once, on a file on another machine.'
(
  wait_for_recorder "$SOCKET"
  show_when_settled "$SOCKET" "$(basename -- "${REMOTE#*:}")"
  sleep 1
  tmux -L "$SOCKET" send-keys -t cap O
  sleep 0.4
  # one key at a time at a typing pace, so each character is seen landing
  printf '%s\n' "$SENTENCE" | fold -w1 | while IFS= read -r ch; do
    tmux -L "$SOCKET" send-keys -t cap -l -- "$ch"
    sleep 0.07
  done
  sleep 1
  tmux -L "$SOCKET" send-keys -t cap Escape
) &

record_gif "$SOCKET" "$OUT" 9 220 50

echo "remote-editing.sh: recorded $OUT" >&2
