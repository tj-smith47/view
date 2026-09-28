#!/bin/sh
# WHY: the README's hero still, the frame a reader sees before any word:
# view's frames under the person's own config, the file tree, a Rust file
# of this repo, and the agent panel beside it with a proposed change under
# review. The agent is `view-ai-stub-agent`, handed to view through a copy
# of the user's view.toml the way agent-panel.sh hands it, and its
# `propose` turn diffs `view-ai-stub-diff.txt` in the agent's cwd. That
# cwd is view's, the repo root here so the tree shows this repo, so the
# seed is written there for the recording and removed by the exit trap.
#
# Usage: scripts/dogfood/tapes/hero.sh [outfile.png]
set -eu

HERE=$(cd -- "$(dirname -- "$0")" && pwd)
ROOT=$(cd -- "$HERE/../../.." && pwd)

SOCKET=view-cap-hero-$$
. "$HERE/../lib.sh"

OUT=$(absolute_path "${1:-$ROOT/assets/view-screenshot.png}")
BIN=$(absolute_path "${VIEW_BIN:-$(newest_build "$ROOT" view)}")
[ -n "$BIN" ] || { echo "hero.sh: no view binary; build one or set VIEW_BIN" >&2; exit 2; }
STUB_BIN=$(absolute_path "${VIEW_AI_STUB_BIN:-$(newest_build "$ROOT" view-ai-stub-agent)}")
[ -n "$STUB_BIN" ] || {
  echo "hero.sh: no view-ai-stub-agent binary; build one with" \
    "cargo build --release -p view-ai --features test-support" \
    "--bin view-ai-stub-agent, or set VIEW_AI_STUB_BIN" >&2
  exit 2
}
command -v tmux >/dev/null 2>&1 || { echo "hero.sh: tmux is not on PATH" >&2; exit 2; }

SEED="$ROOT/view-ai-stub-diff.txt"
if [ -e "$SEED" ]; then
  echo "hero.sh: $SEED already exists, and the recording would overwrite it" >&2
  exit 2
fi

recording_state_home
warm_cargo_target "$ROOT" hero.sh

cachedir="${XDG_CACHE_HOME:-$HOME/.cache}/view-dogfood-tapes"
mkdir -p -- "$cachedir"
# one path for every run, since the first-run record is keyed on the
# config path
CFG="$cachedir/hero-view.toml"
USER_CFG="${XDG_CONFIG_HOME:-$HOME/.config}/view/view.toml"
[ -f "$USER_CFG" ] && cat -- "$USER_CFG" >"$CFG" || : >"$CFG"
printf '\n[ai]\nagent = ["%s"]\n' "$STUB_BIN" >>"$CFG"

cleanup_hero() {
  rm -f -- "$SEED"
  cleanup
}
trap cleanup_hero EXIT INT TERM
printf 'alpha\nbeta\ngamma\n' >"$SEED"

cd -- "$ROOT"
# the review lands in the tile already showing the seed, so the Rust file
# keeps its own tile above it
new_cap_session "$SOCKET" 220 50 -- "$BIN" --config "$CFG" --panes tiles \
  crates/view-ai/src/context.rs view-ai-stub-diff.txt -o --cmd "$QUIET_LAZY"
(
  wait_for_recorder "$SOCKET"
  show_when_settled "$SOCKET" context.rs
  # the tiles open level, and the diff under review needs few rows
  tmux -L "$SOCKET" send-keys -t cap ':resize 35' Enter
  sleep 0.5
  tmux -L "$SOCKET" send-keys -t cap ':View ai open' Enter
  sleep 1
  # 'y' only where the trust prompt is up: a project trusted in an earlier
  # session opens straight on the focused panel
  pane=$(tmux -L "$SOCKET" capture-pane -p -t cap)
  case "$pane" in
    (*'Trust '*)
      tmux -L "$SOCKET" send-keys -t cap 'y'
      sleep 0.5
      ;;
  esac
  tmux -L "$SOCKET" send-keys -t cap 'propose' Enter
  show_when_settled "$SOCKET" 'hunk 1/1'
) &

record_still "$SOCKET" "$OUT" 220 50

echo "hero.sh: recorded $OUT" >&2
