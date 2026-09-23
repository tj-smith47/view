#!/bin/sh
# WHY: the moment README's "Agents in the editor" bullet promises -- the
# ACP agent panel opening beside your buffer, reviewing a proposed change,
# captured under a real config rather than a fixture. `:View ai open` is
# the config-independent form of `<leader>ai` (the leader key itself is
# whatever the user's own config sets, per docs/keymaps.md); this script
# drives it all the way through a turn rather than stopping at an empty
# panel, using `view-ai-stub-agent` (`crates/view-ai/tests/fixtures/
# stub_agent.rs`, the same fixture `scripts/acceptance/ai-conformance.sh`
# drives) as the `[ai] agent`, so the alt text's "reviewing a proposed
# change" is something the gif actually shows rather than a promise it
# makes. `VIEW_AI_AGENT` only ever names an adapter id
# (`crates/view-ai/src/config.rs`'s `AGENT_ENV`), never a command line, so
# the override has to go through `--config` against a copy of the user's
# own `view.toml` (theme, native surfaces and all) with one `[ai]` table
# appended -- never the real file itself.
#
# Startup leaves three "statusline/winbar/vim.notify was drawing ..."
# notices standing (`crates/view-core/src/update/surface_conflict.rs`:
# `record_native_notice_sticky_once` -- sticky by design, taken down by
# `:View notifications dismiss` one at a time or by
# `Messages::dismiss_read_sticky` once each has stood its reading window
# with no input arriving first). This tape sends the dismiss verb before
# the panel content settles so none of them sit over it.
#
# cap.sh's headless capture-pane produces a text snapshot, not the gif
# README's tape table names, so this drives its own tmux session the way
# cap.sh does and hands it to record_gif in lib.sh for the recording.
#
# `propose` anchors its diff at the stub agent's own cwd
# (`named_inside_cwd(DIFF_FILE)` in stub_agent.rs), so opening README.md
# from the repo root left the review buffer empty: no
# view-ai-stub-diff.txt exists there. This runs from a scratch directory
# seeded with that file's expected old text instead (the same seed
# scripts/acceptance/ai-conformance.sh uses), so propose has a real file
# to diff and the panel shows an actual reviewed change.
#
# Usage: scripts/dogfood/tapes/agent-panel.sh [outfile]
set -eu

HERE=$(cd -- "$(dirname -- "$0")" && pwd)
ROOT=$(cd -- "$HERE/../../.." && pwd)
OUT="${1:-$ROOT/assets/tapes/agent-panel.gif}"

BIN="${VIEW_BIN:-$ROOT/target/release/view}"
[ -x "$BIN" ] || BIN="$ROOT/target/debug/view"
[ -x "$BIN" ] || { echo "agent-panel.sh: no view binary; build one or set VIEW_BIN" >&2; exit 2; }
STUB_BIN="${VIEW_AI_STUB_BIN:-$ROOT/target/release/view-ai-stub-agent}"
[ -x "$STUB_BIN" ] || STUB_BIN="$ROOT/target/debug/view-ai-stub-agent"
[ -x "$STUB_BIN" ] || {
  echo "agent-panel.sh: no view-ai-stub-agent binary; build one with" \
    "cargo build --release -p view-ai --features test-support" \
    "--bin view-ai-stub-agent, or set VIEW_AI_STUB_BIN" >&2
  exit 2
}
command -v tmux >/dev/null 2>&1 || { echo "agent-panel.sh: tmux is not on PATH" >&2; exit 2; }

SOCKET=view-cap-agent-$$
. "$HERE/../lib.sh"

cachedir="${XDG_CACHE_HOME:-$HOME/.cache}/view-dogfood-tapes"
mkdir -p -- "$cachedir"
CFG="$cachedir/agent-panel-view-$$.toml"
USER_CFG="${XDG_CONFIG_HOME:-$HOME/.config}/view/view.toml"
[ -f "$USER_CFG" ] && cat -- "$USER_CFG" >"$CFG" || : >"$CFG"
printf '\n[ai]\nagent = ["%s"]\n' "$STUB_BIN" >>"$CFG"

WORKDIR="$cachedir/agent-panel-work-$$"
mkdir -p -- "$WORKDIR"
printf 'alpha\nbeta\ngamma\n' >"$WORKDIR/view-ai-stub-diff.txt"

cleanup_agent_panel() {
  rm -f -- "$CFG"
  rm -rf -- "$WORKDIR"
  cleanup
}
trap cleanup_agent_panel EXIT INT TERM

cd -- "$WORKDIR"
tmux -L "$SOCKET" new-session -d -s cap -x 220 -y 50 "$BIN" --config "$CFG" view-ai-stub-diff.txt
(
  sleep 3
  tmux -L "$SOCKET" send-keys -t cap ':View ai open' Enter
  sleep 1
  # 'y' only where the trust prompt is actually up, read off the live
  # pane: a project already trusted from an earlier session skips
  # straight to the focused panel, and a bare 'y' typed into that focused
  # panel would sit in the prompt box until Enter, which the propose line
  # below sends -- the same text it means to submit, doubled.
  pane=$(tmux -L "$SOCKET" capture-pane -p -t cap)
  case "$pane" in
    (*'Trust '*)
      tmux -L "$SOCKET" send-keys -t cap 'y'
      sleep 0.5
      ;;
  esac
  tmux -L "$SOCKET" send-keys -t cap 'propose' Enter
  sleep 8
  # the AI panel still has focus after submitting the turn (docs/keymaps.md:
  # "AI Agent -- focused, Esc returns"), and ':' typed there goes into its
  # own input box rather than opening the command line -- Esc first, so the
  # dismiss verb below actually reaches `:View notifications dismiss`.
  tmux -L "$SOCKET" send-keys -t cap Escape
  sleep 0.5
  # the next sticky notice only takes the top slot (and its own dismissal
  # timer) once the one ahead of it has cleared, so firing the verb three
  # times back to back outran that handoff -- this reads the live pane and
  # keeps sending the verb until none of the native-override notices
  # (view-native/src/report.rs's "... still loads. Turn it off with ...")
  # remain, capped so a real stuck notice cannot hang the tape.
  n=0
  while [ "$n" -lt 8 ]; do
    pane=$(tmux -L "$SOCKET" capture-pane -p -t cap)
    case "$pane" in
      (*'still loads'*|*'which view owns'*)
        tmux -L "$SOCKET" send-keys -t cap ':View notifications dismiss' Enter
        sleep 0.8
        ;;
      (*)
        break
        ;;
    esac
    n=$((n + 1))
  done
) &

record_gif "$SOCKET" "$OUT" 26 220 50

echo "agent-panel.sh: recorded $OUT" >&2
