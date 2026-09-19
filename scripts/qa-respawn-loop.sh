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
# Seven shapes, each widening the window from a different side:
#
#   --shape plain     the configuration at $VIEW_USER_CONFIG, nothing added
#   --shape modified  the same, quit out of a buffer with unsaved changes
#   --shape leavepre  the same configuration, copied, with a `VimLeavePre`
#                     that burns a second of Lua before the exit lands
#   --shape leaveslow the same, with the burn on `VimLeave` instead, which
#                     is the half of the exit view is already waiting out
#   --shape probe     the same, with four seconds burnt at `QuitPre`, so the
#                     liveness probe goes unanswered across the quit
#   --shape wedge     twelve seconds burnt at `QuitPre`, which is past the
#                     ten-second wedge threshold, so a verdict is due
#   --shape lingering the child closes the embed channel itself and then
#                     burns two seconds, so it outlives the channel by more
#                     than the half-second shutdown backstop and view has to
#                     kill a process whose leave nvim already announced
#
# Usage: scripts/qa-respawn-loop.sh [--runs N] [--shape SHAPE]
set -euo pipefail

SCRIPT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
REPO_ROOT=$(cd -- "$SCRIPT_DIR/.." && pwd)
VIEW_BIN=${VIEW_BIN:-$REPO_ROOT/target/release/view}
VIEW_USER_CONFIG=${VIEW_USER_CONFIG:-${XDG_CONFIG_HOME:-$HOME/.config}}
# never under /tmp: it is a small tmpfs shared with every other job on this
# host, and a run holds a copy of a plugin tree per invocation
SCRATCH=${CLAUDE_JOB_DIR:+$CLAUDE_JOB_DIR/tmp}
SCRATCH=${SCRATCH:-$HOME/.cache/view/qa-respawn}
EVIDENCE=${EVIDENCE:-$SCRATCH/evidence}
# KEEP=1 files every run's log, not only the ones a column flagged: a shape
# whose verdict never fires leaves nothing behind to read otherwise
KEEP=${KEEP:-0}

RUNS=30
SHAPE=plain
SEED='qa-respawn seed line'
DIRTY='one more line'
COLS=120
ROWS=40
SETTLE_BOUND=45
EXIT_BOUND=25
POLL=0.2
EXITED=0
RESPAWNED=0
STILL_RUNNING=0
STRAYS=0
NO_SETTLE=0
UNANNOUNCED=0
FORCED=0
WEDGED=0
ROOTS=''
SESSIONS=''

# The engine processes a view belongs to, or nothing at all when the caller
# holds no view pid. A default pid here reaches whatever the kernel gave
# that number: `pgrep -P 0` lists init and kthreadd, both of which survive
# the `kill -0` liveness filter, land in the strays column and are then sent
# a SIGKILL by a loop running as root.
engine_children() {
    [ -n "$1" ] || return 0
    pgrep -P "$1" 2>/dev/null | tr '\n' ' ' || true
}

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

# Which of these pids are alive, each named once. The duplicates are the
# point of the dedupe: the same engine is sampled before the quit and again
# after it, and a list naming it twice reports one process as two.
still_running() {
    local pid live=''
    for pid in "$@"; do
        [ -n "$pid" ] || continue
        case " $live " in
            (*" $pid "*) continue ;;
        esac
        if kill -0 "$pid" 2>/dev/null; then
            live="$live $pid"
        fi
    done
    printf '%s' "$live"
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

# The case runner needs the functions above and none of the session below.
if [ "${QA_RESPAWN_SOURCED:-0}" = 1 ]; then
    return 0
fi

while [ $# -gt 0 ]; do
    case "$1" in
        (--runs) RUNS=$2; shift 2 ;;
        (--shape) SHAPE=$2; shift 2 ;;
        (*) printf 'unknown argument: %s\n' "$1" >&2; exit 2 ;;
    esac
done

# A shape nobody implements used to fall through to the plain one, so a
# typo produced a full tally of the wrong measurement under the right name.
LEAVE_EVENT=''
BURN_NS=0
PRE_BURN=''
case "$SHAPE" in
    (plain|modified) ;;
    (leavepre) LEAVE_EVENT=VimLeavePre; BURN_NS=1000000000 ;;
    (leaveslow) LEAVE_EVENT=VimLeave; BURN_NS=1000000000 ;;
    (probe) LEAVE_EVENT=QuitPre; BURN_NS=4000000000 ;;
    (wedge) LEAVE_EVENT=QuitPre; BURN_NS=12000000000 ;;
    (lingering)
        LEAVE_EVENT=VimLeave
        BURN_NS=2000000000
        PRE_BURN='pcall(vim.fn.chanclose, 1)'
        ;;
    (*) printf 'unknown shape: %s\n' "$SHAPE" >&2; exit 2 ;;
esac
EXIT_BOUND=$((EXIT_BOUND + BURN_NS / 1000000000))

[ -x "$VIEW_BIN" ] || { printf 'no view binary at %s\n' "$VIEW_BIN" >&2; exit 2; }
command -v tmux >/dev/null || { printf 'tmux is the terminal this drives\n' >&2; exit 2; }
[ -d "$VIEW_USER_CONFIG/nvim" ] || {
    printf 'no nvim configuration under %s\n' "$VIEW_USER_CONFIG" >&2
    exit 2
}

mkdir -p "$SCRATCH" "$EVIDENCE"

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
# for the rest. Copied once rather than per run, because lazy.nvim resolves
# its plugin tree against it and a fresh copy per run would measure that
# resolution instead of the quit.
CONFIG_HOME=$VIEW_USER_CONFIG
if [ -n "$LEAVE_EVENT" ]; then
    CONFIG_HOME=$(mktemp -d "$SCRATCH/view-qa-cfg-XXXXXX")
    ROOTS="$ROOTS $CONFIG_HOME"
    cp -R "$VIEW_USER_CONFIG/nvim" "$CONFIG_HOME/nvim"
    [ -d "$VIEW_USER_CONFIG/view" ] && cp -R "$VIEW_USER_CONFIG/view" "$CONFIG_HOME/view"
    mkdir -p "$CONFIG_HOME/nvim/plugin"
    cat >"$CONFIG_HOME/nvim/plugin/qa-respawn-slow-leave.lua" <<LUA
-- Lua burnt on the way out, so nothing view sent is answered across the
-- window the quit itself opens. The lingering shape closes the embed
-- channel before the burn, which is the one way from inside nvim to make
-- the process outlive its own connection.
vim.api.nvim_create_autocmd('$LEAVE_EVENT', {
  group = vim.api.nvim_create_augroup('qa_respawn_slow_leave', { clear = true }),
  callback = function()
    $PRE_BURN
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

one_run() {
    local idx="$1" root session run_pid view_pid engine_pids after_pids left verdict
    session="view-qa-$$-$idx"
    root=$(mktemp -d "$SCRATCH/view-qa-XXXXXX")
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
    engine_pids=$(engine_children "$view_pid")

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

    if [ "$KEEP" = 1 ]; then
        cp "$root/view.log" "$EVIDENCE/run-$SHAPE-$idx.log" 2>/dev/null || true
    fi

    # whether supervision reached a verdict at all: the shape that burns
    # past the wedge threshold is only measuring what it claims to when
    # this column moves
    if grep -q 'supervision verdict' "$root/view.log" 2>/dev/null; then
        WEDGED=$((WEDGED + 1))
    fi

    # whether view had to kill the child: the shape that makes an engine
    # outlive its own channel is only measuring what it claims to when this
    # column moves, and every other shape should leave it at zero
    if grep -q 'shutdown forced' "$root/view.log" 2>/dev/null; then
        FORCED=$((FORCED + 1))
        cp "$root/view.log" "$EVIDENCE/forced-$SHAPE-$idx.log" 2>/dev/null || true
    fi

    # the announcement the whole predicate rests on, as its own column: a
    # run that exits without one exits for a reason nothing here recorded
    if ! grep -q 'announced: nvim is leaving' "$root/view.log" 2>/dev/null; then
        UNANNOUNCED=$((UNANNOUNCED + 1))
        cp "$root/view.log" "$EVIDENCE/unannounced-$SHAPE-$idx.log" 2>/dev/null || true
    fi

    # sampled again here, not only before the quit: a respawn's engine is a
    # child view started after the `:qa!`, so the pre-quit sample names
    # every process this run is answerable for except the one a respawn
    # leaves behind -- which is the process the column exists to find
    after_pids=$(engine_children "$view_pid")
    # shellcheck disable=SC2086
    left=$(still_running "$view_pid" $engine_pids $after_pids)
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

printf '\n%s: exited %s  respawned %s  still-running %s  no-settle %s  strays %s  unannounced %s  forced %s  wedged %s\n' \
    "$SHAPE" "$EXITED" "$RESPAWNED" "$STILL_RUNNING" "$NO_SETTLE" "$STRAYS" "$UNANNOUNCED" \
    "$FORCED" "$WEDGED"
if [ "$RESPAWNED" -eq 0 ] && [ "$STILL_RUNNING" -eq 0 ] &&
    [ "$NO_SETTLE" -eq 0 ] && [ "$STRAYS" -eq 0 ]; then
    exit 0
fi
exit 1
