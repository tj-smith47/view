#!/bin/sh
# WHY: the moment README's "Remote editing" bullet promises -- view opening
# a file on another machine over SSH with the same editor, config and
# clipboard as at home. REMOTE names the machine and file, since this repo
# ships no host to demonstrate on; REMOTE_NVIM_BIN names the remote nvim
# when it is not on the login PATH ssh resolves (view's own --nvim-bin,
# forwarded to the far side).
#
# The tape's subject is SSH, clipboard and config parity, not the remote
# host's own plugin health, so this scp's remote-clean-init.lua (a small
# fixture this script ships beside itself) to the remote host and forwards
# a wrapper as --nvim-bin that runs the real remote nvim under `-u` against
# it. `--app-name` (NVIM_APPNAME) conflicts with `--remote`, and `-u` takes
# no room on --nvim-bin's own value (a bare executable path), so the
# wrapper is what carries it across. The local view side keeps the user's
# own config throughout; only the remote nvim's init changes.
#
# cap.sh's headless capture-pane produces a text snapshot, not the gif
# README's tape table names, so this drives its own tmux session the way
# cap.sh does and hands it to record_gif in lib.sh for the recording.
#
# Usage: REMOTE=host:path [REMOTE_NVIM_BIN=/path/to/nvim] \
#          scripts/dogfood/tapes/remote-editing.sh [outfile]
set -eu

HERE=$(cd -- "$(dirname -- "$0")" && pwd)
ROOT=$(cd -- "$HERE/../../.." && pwd)
: "${REMOTE:?set REMOTE=host:path to the machine and file this tape opens}"
OUT="${1:-$ROOT/assets/tapes/remote-editing.gif}"
REMOTE_HOST=${REMOTE%%:*}

BIN="${VIEW_BIN:-$ROOT/target/release/view}"
[ -x "$BIN" ] || BIN="$ROOT/target/debug/view"
[ -x "$BIN" ] || { echo "remote-editing.sh: no view binary; build one or set VIEW_BIN" >&2; exit 2; }
command -v tmux >/dev/null 2>&1 || { echo "remote-editing.sh: tmux is not on PATH" >&2; exit 2; }
command -v ssh >/dev/null 2>&1 || { echo "remote-editing.sh: ssh is not on PATH" >&2; exit 2; }
command -v scp >/dev/null 2>&1 || { echo "remote-editing.sh: scp is not on PATH" >&2; exit 2; }

SOCKET=view-cap-remote-$$
. "$HERE/../lib.sh"

REMOTE_REAL_NVIM="${REMOTE_NVIM_BIN:-nvim}"
REMOTE_INIT=/tmp/view-dogfood-clean-init.lua
REMOTE_WRAPPER=/tmp/view-dogfood-clean-nvim.sh

cleanup_remote_editing() {
  ssh "$REMOTE_HOST" "rm -f -- '$REMOTE_INIT' '$REMOTE_WRAPPER'" 2>/dev/null || true
  cleanup
}
trap cleanup_remote_editing EXIT INT TERM

scp -q -- "$HERE/remote-clean-init.lua" "$REMOTE_HOST:$REMOTE_INIT"
ssh "$REMOTE_HOST" "printf '#!/bin/sh\nexec \"%s\" -u \"%s\" \"\$@\"\n' \
  '$REMOTE_REAL_NVIM' '$REMOTE_INIT' >'$REMOTE_WRAPPER' && \
  chmod +x '$REMOTE_WRAPPER'"

cd -- "$ROOT"
tmux -L "$SOCKET" new-session -d -s cap -x 220 -y 50 \
  "$BIN" --nvim-bin "$REMOTE_WRAPPER" --remote "$REMOTE"

record_gif "$SOCKET" "$OUT" 15 220 50

echo "remote-editing.sh: recorded $OUT" >&2
