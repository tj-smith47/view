#!/bin/sh
# WHY: the moment README's "Agents in the editor" bullet promises -- the
# ACP agent panel opening beside your buffer, captured under a real config
# rather than a fixture, since `:View ai toggle` is the config-independent
# form of `<leader>ai` (the leader key itself is whatever the user's own
# config sets, per docs/keymaps.md).
#
# Usage: scripts/dogfood/tapes/agent-panel.sh [outfile]
set -eu

HERE=$(cd -- "$(dirname -- "$0")" && pwd)
OUT="${1:-$HOME/.claude/tmp/dogfood-tapes/agent-panel.txt}"

exec "$HERE/../cap.sh" --size 263x88 --keys ':View ai toggle' --submit \
  --settle 3 "$OUT" -- README.md
