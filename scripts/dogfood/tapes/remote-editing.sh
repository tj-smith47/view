#!/bin/sh
# WHY: the moment README's "Remote editing" bullet promises -- view opening
# a file on another machine over SSH with the same editor, config and
# clipboard as at home. REMOTE names the machine and file, since this repo
# ships no host to demonstrate on.
#
# Usage: REMOTE=host:path scripts/dogfood/tapes/remote-editing.sh [outfile]
set -eu

HERE=$(cd -- "$(dirname -- "$0")" && pwd)
: "${REMOTE:?set REMOTE=host:path to the machine and file this tape opens}"
OUT="${1:-$HOME/.claude/tmp/dogfood-tapes/remote-editing.txt}"

exec "$HERE/../cap.sh" --size 263x88 --settle 5 "$OUT" -- --remote "$REMOTE"
