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
# The colorscheme the local config sets travels with it: its name is read
# off the local nvim and the plugin that provides it is copied over, so
# the far side is themed as the person's own editor is. The local view
# side keeps the user's own config throughout.
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

BIN="${VIEW_BIN:-$(newest_build "$ROOT" view)}"
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

cleanup_remote_editing() {
  ssh "$REMOTE_HOST" "rm -rf -- '$REMOTE_DIR'" 2>/dev/null || true
  rm -f -- "$WRAPPER"
  cleanup
}
trap cleanup_remote_editing EXIT INT TERM

# the scheme's name and the file that defines it, read off the local
# config; empty when there is no local nvim to ask
theme=
theme_file=
if command -v nvim >/dev/null 2>&1; then
  answer=$(nvim --headless \
    -c 'lua local n = vim.g.colors_name or ""; local f = n ~= "" and vim.api.nvim_get_runtime_file("colors/" .. n .. ".*", false)[1] or ""; io.stdout:write(n .. "\n" .. f .. "\n")' \
    -c 'qa!' </dev/null 2>/dev/null) || answer=
  theme=$(printf '%s\n' "$answer" | sed -n 1p)
  theme_file=$(printf '%s\n' "$answer" | sed -n 2p)
fi

ssh "$REMOTE_HOST" "mkdir -p -- '$REMOTE_DIR/theme'"
scp -q -- "$HERE/remote-clean-init.lua" "$REMOTE_HOST:$REMOTE_DIR/init.lua"
theme_dir=
if [ -n "$theme_file" ]; then
  plugin_root=$(dirname -- "$(dirname -- "$theme_file")")
  # a scheme nvim ships is on the remote runtimepath already
  case "$plugin_root" in
    ("${VIMRUNTIME:-/nonexistent}"|*/share/nvim/runtime) ;;
    (*)
      tar -C "$plugin_root" --exclude=.git -cf - . |
        ssh "$REMOTE_HOST" "tar -C '$REMOTE_DIR/theme' -xf -"
      theme_dir=$REMOTE_DIR/theme
      ;;
  esac
fi

{
  printf '#!/bin/sh\n'
  printf "VIEW_DOGFOOD_THEME='%s' VIEW_DOGFOOD_THEME_DIR='%s'\n" "$theme" "$theme_dir"
  printf 'export VIEW_DOGFOOD_THEME VIEW_DOGFOOD_THEME_DIR\n'
  printf "exec '%s' -u '%s' \"\$@\"\n" "$REMOTE_REAL_NVIM" "$REMOTE_DIR/init.lua"
} >"$WRAPPER"
scp -q -- "$WRAPPER" "$REMOTE_HOST:$REMOTE_DIR/nvim.sh"
# macOS chmod takes no `--`, and the path is one this script chose
ssh "$REMOTE_HOST" "chmod +x '$REMOTE_DIR/nvim.sh'"

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

record_gif "$SOCKET" "$OUT" 11 220 50

echo "remote-editing.sh: recorded $OUT" >&2
