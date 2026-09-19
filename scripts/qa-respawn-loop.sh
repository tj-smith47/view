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
# Three more shapes end the session from outside instead of from the command
# line, which is the other half of the question: a `:qa!` runs view's own
# teardown, and a signal is the ending that may not. Each takes `--signal`,
# so the pair TERM (view's own fatal-signal path) and KILL (no teardown at
# all, the parent-death tie alone) is measured at the same three points:
#
#   --shape signal-preattach  signalled as soon as the engine child exists,
#                             before the screen has settled
#   --shape signal-settled    signalled at the settled screen
#   --shape signal-editing    signalled with an `:e` in flight
#
# Usage: scripts/qa-respawn-loop.sh [--runs N] [--shape SHAPE] [--signal SIG]
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
SIGNAL=TERM
SEED='qa-respawn seed line'
DIRTY='one more line'
COLS=120
ROWS=40
SETTLE_BOUND=45
EXIT_BOUND=25
# What a signalled run asserts: the engine child is out of the process table
# this long after the process that owns it went away. An engine wedged past
# noticing its closed pipe never leaves on its own, so a bound here is the
# whole assertion rather than a convenience.
REAP_BOUND=5
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

# The view a pane is running, or nothing. The pane runs the wrapper shell,
# so view is its child rather than the pane process itself.
view_pid_of() {
    local run_pid
    run_pid=$(tmux list-panes -t "$1" -F '#{pane_pid}' 2>/dev/null | head -1 || true)
    [ -n "$run_pid" ] || return 0
    pgrep -P "$run_pid" -x view 2>/dev/null | head -1 || true
}

# Whether the session has an engine child yet. The pre-attach shape signals
# on this rather than on the settled screen: the child exists within
# milliseconds of launch and the screen settles seconds later, so this is
# the point where view owns a process it has not finished attaching to.
engine_appeared() {
    local view_pid
    view_pid=$(view_pid_of "$1")
    [ -n "$view_pid" ] || return 1
    [ -n "$(engine_children "$view_pid")" ]
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

# Waits for every pid named to leave the process table, or for `bound`
# seconds to pass; 1 on the bound, with the survivors on stdout. This is the
# signal shapes' assertion: the tie is what the bound is measuring.
await_reaped() {
    local bound="$1" start=$SECONDS left
    shift
    while :; do
        left=$(still_running "$@")
        if [ -z "$left" ]; then
            return 0
        fi
        if [ $((SECONDS - start)) -ge "$bound" ]; then
            printf '%s' "$left"
            return 1
        fi
        sleep "$POLL"
    done
}

# The case runner needs the functions above and none of the session below.
if [ "${QA_RESPAWN_SOURCED:-0}" = 1 ]; then
    return 0
fi

while [ $# -gt 0 ]; do
    case "$1" in
        (--runs) RUNS=$2; shift 2 ;;
        (--shape) SHAPE=$2; shift 2 ;;
        (--signal) SIGNAL=$2; shift 2 ;;
        (*) printf 'unknown argument: %s\n' "$1" >&2; exit 2 ;;
    esac
done

# A shape nobody implements used to fall through to the plain one, so a
# typo produced a full tally of the wrong measurement under the right name.
LEAVE_EVENT=''
BURN_NS=0
PRE_BURN=''
# Which ending a run takes, and where. `quit` is the command line; `signal`
# is the ending view does not choose, and the columns a quit reads off the
# log say nothing about it.
ENDING=quit
KILL_AT=''
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
    (signal-preattach) ENDING=signal; KILL_AT=preattach ;;
    (signal-settled) ENDING=signal; KILL_AT=settled ;;
    (signal-editing) ENDING=signal; KILL_AT=editing ;;
    (*) printf 'unknown shape: %s\n' "$SHAPE" >&2; exit 2 ;;
esac
# A signal nobody sends used to be accepted and then refused by `kill` once
# per run, which reads as thirty failures of the loop rather than one bad
# argument.
case "$SIGNAL" in
    (TERM|KILL) ;;
    (*) printf 'unknown signal: %s\n' "$SIGNAL" >&2; exit 2 ;;
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
    local idx="$1" root session ready view_pid engine_pids after_pids left verdict
    session="view-qa-$$-$idx"
    root=$(mktemp -d "$SCRATCH/view-qa-XXXXXX")
    ROOTS="$ROOTS $root"
    SESSIONS="$SESSIONS $session"
    printf '%s\n' "$SEED" >"$root/scratch.txt"
    printf '%s\n' "$SEED" >"$root/other.txt"

    cat >"$root/run.sh" <<EOF
env VIEW_LOG=$root/view.log XDG_CONFIG_HOME=$CONFIG_HOME \\
    TERM=xterm-256color COLORTERM=truecolor \\
    "$VIEW_BIN" "$root/scratch.txt"
printf '%s' "\$?" >"$root/exit.code"
exec sleep 600
EOF

    tmux kill-session -t "$session" 2>/dev/null || true
    tmux new-session -d -s "$session" -x "$COLS" -y "$ROWS" -c "$root" "sh $root/run.sh"

    # the pre-attach shape ends the run before there is a screen to read, so
    # what it waits for is the child itself
    ready=pane_settled
    [ "$KILL_AT" = preattach ] && ready=engine_appeared
    if ! await "$ready" "$session" "$SETTLE_BOUND"; then
        tmux capture-pane -p -t "$session" >"$EVIDENCE/no-settle-$SHAPE-$idx.screen" 2>/dev/null || true
        tmux kill-session -t "$session" 2>/dev/null || true
        NO_SETTLE=$((NO_SETTLE + 1))
        printf 'run %-3s no-settle\n' "$idx"
        return 0
    fi

    view_pid=$(view_pid_of "$session")
    if [ -z "$view_pid" ]; then
        tmux kill-session -t "$session" 2>/dev/null || true
        NO_SETTLE=$((NO_SETTLE + 1))
        printf 'run %-3s no-view\n' "$idx"
        return 0
    fi
    engine_pids=$(engine_children "$view_pid")

    if [ "$SHAPE" = modified ]; then
        tmux send-keys -t "$session" -l "i$DIRTY"
        tmux send-keys -t "$session" Escape
        await pane_dirty "$session" 10 || true
    fi

    case "$ENDING" in
        (quit)
            tmux send-keys -t "$session" -l ':qa!'
            tmux send-keys -t "$session" Enter
            ;;
        (signal)
            # sent and not awaited: the point of this shape is an ending
            # that lands while view is answering something else
            if [ "$KILL_AT" = editing ]; then
                tmux send-keys -t "$session" -l ":e $root/other.txt"
                tmux send-keys -t "$session" Enter
            fi
            kill -"$SIGNAL" "$view_pid" 2>/dev/null || true
            ;;
    esac

    await exit_recorded "$root/exit.code" "$EXIT_BOUND" || true

    if [ "$ENDING" = signal ]; then
        if [ -f "$root/exit.code" ]; then
            verdict=exited
        else
            verdict=still-running
        fi
    elif grep -q 'restarted pid=' "$root/view.log" 2>/dev/null; then
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

    # The three columns below read a quit out of the log, and a signalled
    # run writes none of them: an engine whose owner was killed outright
    # announces no leave and nothing in view is left to force a shutdown or
    # to reach a verdict. Counted there, every signalled run would report an
    # unannounced quit it never attempted.
    if [ "$ENDING" = quit ]; then
        # whether supervision reached a verdict at all: the shape that burns
        # past the wedge threshold is only measuring what it claims to when
        # this column moves
        if grep -q 'supervision verdict' "$root/view.log" 2>/dev/null; then
            WEDGED=$((WEDGED + 1))
        fi

        # whether view had to kill the child: the shape that makes an engine
        # outlive its own channel is only measuring what it claims to when
        # this column moves, and every other shape should leave it at zero
        if grep -q 'shutdown forced' "$root/view.log" 2>/dev/null; then
            FORCED=$((FORCED + 1))
            cp "$root/view.log" "$EVIDENCE/forced-$SHAPE-$idx.log" 2>/dev/null || true
        fi

        # the announcement the whole predicate rests on, as its own column:
        # a run that exits without one exits for a reason nothing here
        # recorded
        if ! grep -q 'announced: nvim is leaving' "$root/view.log" 2>/dev/null; then
            UNANNOUNCED=$((UNANNOUNCED + 1))
            cp "$root/view.log" "$EVIDENCE/unannounced-$SHAPE-$idx.log" 2>/dev/null || true
        fi
    fi

    # sampled again here, not only before the quit: a respawn's engine is a
    # child view started after the `:qa!`, so the pre-quit sample names
    # every process this run is answerable for except the one a respawn
    # leaves behind -- which is the process the column exists to find
    after_pids=$(engine_children "$view_pid")
    # A signalled run is given the reaping bound before its survivors are
    # counted, because that bound is what the shape asserts; a quit is read
    # at the instant its exit was recorded, which is what it has always
    # asserted.
    if [ "$ENDING" = signal ]; then
        # shellcheck disable=SC2086
        left=$(await_reaped "$REAP_BOUND" "$view_pid" $engine_pids $after_pids) || true
    else
        # shellcheck disable=SC2086
        left=$(still_running "$view_pid" $engine_pids $after_pids)
    fi
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

printf 'shape=%s signal=%s runs=%s config=%s bin=%s\n' \
    "$SHAPE" "$SIGNAL" "$RUNS" "$CONFIG_HOME" "$VIEW_BIN"
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
