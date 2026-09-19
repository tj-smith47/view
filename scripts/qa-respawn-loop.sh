#!/usr/bin/env bash
#
# Repro loop for the quit that comes back: `:qa!` at a real terminal, under
# the configuration a person actually runs, tallied per run as exited,
# respawned or still running.
#
# The defect it exists to observe is a stop resolved before the reader
# thread published what nvim said on its way out, so the only instrument
# that can see it is a whole session -- a pty, view's own loop, the engine's
# own `VimLeavePre` -- and the tally it prints is the evidence a fix is
# judged against, before and after.
#
# Four shapes, each widening the window from a different side:
#
#   --shape plain     the configuration at $VIEW_USER_CONFIG, nothing added
#   --shape modified  the same, quit out of a buffer with unsaved changes
#   --shape leavepre  the same configuration, copied, with a `VimLeavePre`
#                     that burns a second of Lua before the exit lands
#   --shape leaveslow the same, with the burn on `VimLeave` instead, which
#                     is the half of the exit view is already waiting out
#   --shape probe     the same, with four seconds burnt at `QuitPre`, so the
#                     liveness probe goes unanswered across the quit
#
# Usage: scripts/qa-respawn-loop.sh [--runs N] [--shape SHAPE]
set -euo pipefail

SCRIPT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
REPO_ROOT=$(cd -- "$SCRIPT_DIR/.." && pwd)
VIEW_BIN=${VIEW_BIN:-$REPO_ROOT/target/release/view}
VIEW_USER_CONFIG=${VIEW_USER_CONFIG:-${XDG_CONFIG_HOME:-$HOME/.config}}
EVIDENCE=${EVIDENCE:-${TMPDIR:-/tmp}/qa-respawn-evidence}

RUNS=30
SHAPE=plain
SEED='qa-respawn seed line'
DIRTY='one more line'
COLS=120
ROWS=40
SETTLE_BOUND=45
EXIT_BOUND=25
POLL=0.2

while [ $# -gt 0 ]; do
    case "$1" in
        (--runs) RUNS=$2; shift 2 ;;
        (--shape) SHAPE=$2; shift 2 ;;
        (*) printf 'unknown argument: %s\n' "$1" >&2; exit 2 ;;
    esac
done

[ -x "$VIEW_BIN" ] || { printf 'no view binary at %s\n' "$VIEW_BIN" >&2; exit 2; }
command -v tmux >/dev/null || { printf 'tmux is the terminal this drives\n' >&2; exit 2; }
[ -d "$VIEW_USER_CONFIG/nvim" ] || {
    printf 'no nvim configuration under %s\n' "$VIEW_USER_CONFIG" >&2
    exit 2
}

EXITED=0
RESPAWNED=0
STILL_RUNNING=0
STRAYS=0
NO_SETTLE=0
UNANNOUNCED=0
ROOTS=''
SESSIONS=''
mkdir -p "$EVIDENCE"

# Everything this loop made, removed on the way out however the way out was
# reached: a run interrupted mid-tally otherwise leaves a scratch tree and a
# detached tmux session behind for every iteration it had got through.
cleanup() {
    local root session
    for session in $SESSIONS; do
        tmux kill-session -t "$session" 2>/dev/null || true
    done
    for root in $ROOTS; do
        [ -n "$root" ] && rm -rf "$root"
    done
}
trap cleanup EXIT

# The configuration every run of this invocation spends: the user's own for
# the plain and modified shapes, and a copy of it carrying the slow leave
# for the third. Copied once rather than per run, because lazy.nvim resolves
# its plugin tree against it and a fresh copy per run would measure that
# resolution instead of the quit.
CONFIG_HOME=$VIEW_USER_CONFIG
LEAVE_EVENT=''
BURN_NS=1000000000
[ "$SHAPE" = leavepre ] && LEAVE_EVENT=VimLeavePre
[ "$SHAPE" = leaveslow ] && LEAVE_EVENT=VimLeave
if [ "$SHAPE" = probe ]; then
    LEAVE_EVENT=QuitPre
    BURN_NS=4000000000
fi
if [ -n "$LEAVE_EVENT" ]; then
    CONFIG_HOME=$(mktemp -d "${TMPDIR:-/tmp}/view-qa-cfg-XXXXXX")
    ROOTS="$ROOTS $CONFIG_HOME"
    cp -R "$VIEW_USER_CONFIG/nvim" "$CONFIG_HOME/nvim"
    [ -d "$VIEW_USER_CONFIG/view" ] && cp -R "$VIEW_USER_CONFIG/view" "$CONFIG_HOME/view"
    mkdir -p "$CONFIG_HOME/nvim/plugin"
    cat >"$CONFIG_HOME/nvim/plugin/qa-respawn-slow-leave.lua" <<LUA
-- Lua burnt on the way out, so nothing view sent is answered across the
-- window the quit itself opens.
vim.api.nvim_create_autocmd('$LEAVE_EVENT', {
  group = vim.api.nvim_create_augroup('qa_respawn_slow_leave', { clear = true }),
  callback = function()
    local deadline = vim.uv.hrtime() + $BURN_NS
    local sink = 0
    while vim.uv.hrtime() < deadline do
      for _ = 1, 10000 do
        sink = sink + 1
      end
    end
    return sink ~= nil and false or false
  end,
})
LUA
fi

# The pids this loop is allowed to end: the ones it watched start. Nothing
# here reaches for a name -- peer sessions on this host run nvim children of
# their own, and a sweep by name takes theirs with it.
end_watched() {
    local pid
    for pid in "$@"; do
        [ -n "$pid" ] || continue
        kill -0 "$pid" 2>/dev/null || continue
        kill -KILL "$pid" 2>/dev/null || true
    done
}

pane_holds() {
    local text
    text=$(tmux capture-pane -p -t "$1" 2>/dev/null || true)
    grep -Fq -- "$2" <<<"$text"
}

pane_settled() {
    pane_holds "$1" "$SEED"
}

pane_dirty() {
    pane_holds "$1" "$DIRTY"
}

exit_recorded() {
    [ -f "$1" ]
}

# Waits for `cond arg` to hold, or for `bound` seconds to pass; 1 on the
# bound. The cadence is the instrument's own: a screen a person reads
# settles on nobody's schedule, so there is nothing to wait on here but the
# screen itself.
await() {
    local cond="$1" arg="$2" bound="$3" start=$SECONDS
    until "$cond" "$arg"; do
        if [ $((SECONDS - start)) -ge "$bound" ]; then
            return 1
        fi
        sleep "$POLL"
    done
    return 0
}

one_run() {
    local idx="$1" root session run_pid view_pid engine_pids left verdict pid
    session="view-qa-$$-$idx"
    root=$(mktemp -d "${TMPDIR:-/tmp}/view-qa-XXXXXX")
    ROOTS="$ROOTS $root"
    SESSIONS="$SESSIONS $session"
    printf '%s\n' "$SEED" >"$root/scratch.txt"

    cat >"$root/run.sh" <<EOF
env VIEW_LOG=$root/view.log XDG_CONFIG_HOME=$CONFIG_HOME \\
    TERM=xterm-256color COLORTERM=truecolor \\
    "$VIEW_BIN" "$root/scratch.txt"
printf '%s' "\$?" >"$root/exit.code"
exec sleep 600
EOF

    tmux kill-session -t "$session" 2>/dev/null || true
    tmux new-session -d -s "$session" -x "$COLS" -y "$ROWS" -c "$root" "sh $root/run.sh"

    if ! await pane_settled "$session" "$SETTLE_BOUND"; then
        tmux capture-pane -p -t "$session" >"$EVIDENCE/no-settle-$SHAPE-$idx.screen" 2>/dev/null || true
        tmux kill-session -t "$session" 2>/dev/null || true
        NO_SETTLE=$((NO_SETTLE + 1))
        printf 'run %-3s no-settle\n' "$idx"
        return 0
    fi

    # the pane runs the shell above, so view is its child rather than the
    # pane process itself
    run_pid=$(tmux list-panes -t "$session" -F '#{pane_pid}' | head -1 || true)
    view_pid=$(pgrep -P "$run_pid" -x view | head -1 || true)
    engine_pids=$(pgrep -P "${view_pid:-0}" 2>/dev/null | tr '\n' ' ' || true)

    if [ "$SHAPE" = modified ]; then
        tmux send-keys -t "$session" -l "i$DIRTY"
        tmux send-keys -t "$session" Escape
        await pane_dirty "$session" 10 || true
    fi

    tmux send-keys -t "$session" -l ':qa!'
    tmux send-keys -t "$session" Enter

    await exit_recorded "$root/exit.code" "$EXIT_BOUND" || true

    if grep -q 'restarted pid=' "$root/view.log" 2>/dev/null; then
        verdict=respawned
    elif [ -f "$root/exit.code" ]; then
        verdict=exited
    else
        verdict=still-running
    fi

    case "$verdict" in
        (exited)
            EXITED=$((EXITED + 1))
            ;;
        (respawned)
            RESPAWNED=$((RESPAWNED + 1))
            cp "$root/view.log" "$EVIDENCE/respawn-$SHAPE-$idx.log" 2>/dev/null || true
            tmux capture-pane -p -t "$session" >"$EVIDENCE/respawn-$SHAPE-$idx.screen" 2>/dev/null || true
            ;;
        (still-running)
            STILL_RUNNING=$((STILL_RUNNING + 1))
            cp "$root/view.log" "$EVIDENCE/stuck-$SHAPE-$idx.log" 2>/dev/null || true
            tmux capture-pane -p -t "$session" >"$EVIDENCE/stuck-$SHAPE-$idx.screen" 2>/dev/null || true
            ;;
    esac

    # the announcement the whole predicate rests on, as its own column: a
    # run that exits without one exits for a reason nothing here recorded
    if ! grep -q 'announced: nvim is leaving' "$root/view.log" 2>/dev/null; then
        UNANNOUNCED=$((UNANNOUNCED + 1))
        cp "$root/view.log" "$EVIDENCE/unannounced-$SHAPE-$idx.log" 2>/dev/null || true
    fi

    left=''
    for pid in $view_pid $engine_pids; do
        if kill -0 "$pid" 2>/dev/null; then
            left="$left $pid"
        fi
    done
    if [ -n "${left# }" ]; then
        STRAYS=$((STRAYS + 1))
        printf 'run %-3s %-13s strays:%s\n' "$idx" "$verdict" "$left"
    else
        printf 'run %-3s %s\n' "$idx" "$verdict"
    fi

    # shellcheck disable=SC2086
    end_watched $left
    tmux kill-session -t "$session" 2>/dev/null || true
    rm -rf "$root"
}

printf 'shape=%s runs=%s config=%s bin=%s\n' "$SHAPE" "$RUNS" "$CONFIG_HOME" "$VIEW_BIN"
i=1
while [ "$i" -le "$RUNS" ]; do
    one_run "$i"
    i=$((i + 1))
done

printf '\n%s: exited %s  respawned %s  still-running %s  no-settle %s  strays %s  unannounced %s\n' \
    "$SHAPE" "$EXITED" "$RESPAWNED" "$STILL_RUNNING" "$NO_SETTLE" "$STRAYS" "$UNANNOUNCED"
[ "$RESPAWNED" -eq 0 ] && [ "$STILL_RUNNING" -eq 0 ] && [ "$NO_SETTLE" -eq 0 ] && [ "$STRAYS" -eq 0 ]
