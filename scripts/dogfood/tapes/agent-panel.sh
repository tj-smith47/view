#!/bin/sh
# WHY: the moment README's "Agents in the editor" bullet promises: the ACP
# agent panel opening beside your buffer and reviewing a proposed change,
# under the person's own config. `:View ai open` is the config-independent
# form of `<leader>ai`, whose leader is whatever the config sets
# (docs/keymaps.md). The agent is `view-ai-stub-agent`
# (crates/view-ai/tests/fixtures/stub_agent.rs, the fixture
# scripts/acceptance/ai-conformance.sh drives), so the turn ends in a real
# proposed change. `VIEW_AI_AGENT` names an adapter id and no command line
# (crates/view-ai/src/config.rs, AGENT_ENV), so the agent reaches view
# through `--config` against a copy of the user's own view.toml with one
# `[ai]` table appended. The user's file is left untouched.
#
# `propose` anchors its diff at the stub agent's cwd
# (named_inside_cwd(DIFF_FILE) in stub_agent.rs), so the tape runs from a
# scratch directory seeded with that file's expected old text, the seed
# ai-conformance.sh uses, and the review buffer has a real file to diff.
#
# cap.sh's headless capture-pane produces a text snapshot, and README's
# tape table names a gif, so this drives its own tmux session and hands it
# to record_gif in lib.sh.
#
# Usage: scripts/dogfood/tapes/agent-panel.sh [outfile]
set -eu

HERE=$(cd -- "$(dirname -- "$0")" && pwd)
ROOT=$(cd -- "$HERE/../../.." && pwd)
OUT="${1:-$ROOT/assets/tapes/agent-panel.gif}"

SOCKET=view-cap-agent-$$
. "$HERE/../lib.sh"

BIN="${VIEW_BIN:-$(newest_build "$ROOT" view)}"
[ -n "$BIN" ] || { echo "agent-panel.sh: no view binary; build one or set VIEW_BIN" >&2; exit 2; }
STUB_BIN="${VIEW_AI_STUB_BIN:-$(newest_build "$ROOT" view-ai-stub-agent)}"
[ -n "$STUB_BIN" ] || {
  echo "agent-panel.sh: no view-ai-stub-agent binary; build one with" \
    "cargo build --release -p view-ai --features test-support" \
    "--bin view-ai-stub-agent, or set VIEW_AI_STUB_BIN" >&2
  exit 2
}
command -v tmux >/dev/null 2>&1 || { echo "agent-panel.sh: tmux is not on PATH" >&2; exit 2; }

recording_state_home

cachedir="${XDG_CACHE_HOME:-$HOME/.cache}/view-dogfood-tapes"
mkdir -p -- "$cachedir"
# one path for every run: the first-run record is keyed on the config
# path, and a path that changed every run was a config told everything
# afresh on every recording
CFG="$cachedir/agent-panel-view.toml"
USER_CFG="${XDG_CONFIG_HOME:-$HOME/.config}/view/view.toml"
[ -f "$USER_CFG" ] && cat -- "$USER_CFG" >"$CFG" || : >"$CFG"
printf '\n[ai]\nagent = ["%s"]\n' "$STUB_BIN" >>"$CFG"

WORKDIR="$cachedir/agent-panel-work-$$"
mkdir -p -- "$WORKDIR"
printf 'alpha\nbeta\ngamma\n' >"$WORKDIR/view-ai-stub-diff.txt"

cleanup_agent_panel() {
  rm -rf -- "$WORKDIR"
  cleanup
}
trap cleanup_agent_panel EXIT INT TERM

cd -- "$WORKDIR"
new_cap_session "$SOCKET" 220 50 -- "$BIN" --config "$CFG" view-ai-stub-diff.txt
(
  wait_for_recorder "$SOCKET"
  start=$(date +%s)
  sleep 1.5
  dismiss_launch_notices "$SOCKET"
  sleep_until_elapsed "$start" 5
  tmux -L "$SOCKET" send-keys -t cap ':View ai open' Enter
  sleep 1
  # 'y' only where the trust prompt is up: a project trusted in an earlier
  # session opens straight on the focused panel, where a bare 'y' would sit
  # in the prompt box and reach the agent in front of 'propose'
  pane=$(tmux -L "$SOCKET" capture-pane -p -t cap)
  case "$pane" in
    (*'Trust '*)
      tmux -L "$SOCKET" send-keys -t cap 'y'
      sleep 0.5
      ;;
  esac
  tmux -L "$SOCKET" send-keys -t cap 'propose' Enter
) &

BODY='Sleep 3500ms
Show
Sleep 10s'
record_gif "$SOCKET" "$OUT" 10 220 50 "$BODY"

echo "agent-panel.sh: recorded $OUT" >&2
