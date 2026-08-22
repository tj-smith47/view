#!/usr/bin/env bash
#
# Supervision conformance: every detection path driven through a real
# terminal session and observed where a user observes it, on the screen.
#
# The headless suite already proves the fold and the live suite already
# proves the wire facts against the pinned engine. Neither can reach the
# whole chain at once -- a key typed at a pty, view's own loop, a notice
# painted into a frame -- and a detection that never reaches the screen is
# indistinguishable from no detection at all for the person waiting at the
# editor. tmux is the instrument because it is the real terminal this tree
# already drives elsewhere.
#
# Every bound and every string this asserts is read out of the source that
# owns it, so a reworded notice or a retuned threshold fails here loudly
# instead of leaving the script asserting a claim the code stopped making.
set -euo pipefail

SCRIPT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
REPO_ROOT=$(cd -- "$SCRIPT_DIR/../.." && pwd)
# shellcheck source=scripts/acceptance/artifacts.sh
. "$SCRIPT_DIR/artifacts.sh"
VIEW_BIN=${VIEW_BIN:-$REPO_ROOT/target/release/view}
FIXTURE=$REPO_ROOT/compat/fixtures/minimal
SUPERVISION_RS=$REPO_ROOT/crates/view-core/src/native/supervision.rs
HEARTBEAT_RS=$REPO_ROOT/crates/view-engine/src/heartbeat.rs
STALL_RS=$REPO_ROOT/crates/view-engine/src/stall.rs

# The pane the legs read. Wide enough that no notice or modal row is
# truncated before the text an assertion greps for, tall enough that the
# banner and the modal are on screen together.
COLS=120
ROWS=40
# How often the screen is read. Charged in full to every measurement below,
# so it is small next to the tightest window any of them assert (2.5s).
POLL=0.25

# How long the wedge chunks are told to spin for. Never waited out: what
# ends each of them is the recovery under test, and the budget exists only
# so that a loop which simply finished can never be mistaken for one a
# recovery ended. Comfortably past the whole read-side observation window
# (detection, escalation, then the interrupt reaction window).
WEDGE_BUDGET_SECS=180

# One bracketed paste, sized past what a pipe will hold. This is the whole
# write-side induction: view sends a bracketed paste as a single
# `nvim_paste` message, so a peer that has stopped reading leaves exactly
# one message handed off and undelivered, which is what the write watch
# reports on. A flood of individual keys would fill the same pipe but only
# by giving the loop a large key backlog to work through first, which is a
# throughput question and not this one.
PASTE_BYTES=131072

SESSIONS=()
ROOTS=()
NVIM_PIDS=()
SESSION=""
ROOT=""
VIEW_PID=""
NVIM_PID=""
CURRENT_LEG=startup
DUMP_DIR=$(dump_dir view-acceptance)

cleanup() {
    local code=$?
    local pid session root
    for pid in ${NVIM_PIDS[@]+"${NVIM_PIDS[@]}"}; do
        # continued before it is killed: the write-side leg leaves its
        # engine stopped, and a stopped process reaped by nothing outlives
        # the run holding a full pipe and its own address space
        kill -CONT "$pid" 2>/dev/null || true
        kill -9 "$pid" 2>/dev/null || true
    done
    for session in ${SESSIONS[@]+"${SESSIONS[@]}"}; do
        tmux kill-session -t "$session" 2>/dev/null || true
    done
    for root in ${ROOTS[@]+"${ROOTS[@]}"}; do
        [ -n "$root" ] && rm -rf "$root"
    done
    # kept only when there is something in them: a run that refused before it
    # ever opened a session has no pane to dump, and pointing at an empty
    # directory reads as evidence that does not exist
    if [ "$code" -eq 0 ] || [ -z "$(ls -A "$DUMP_DIR" 2>/dev/null)" ]; then
        rm -rf "$DUMP_DIR"
    else
        printf '      pane dumps kept in %s\n' "$DUMP_DIR" >&2
    fi
    exit "$code"
}
trap cleanup EXIT INT TERM

# two decimals, not one: a recovery this fast rounds to a flat zero at one,
# and "recovers in 0.0s" reads as a measurement that did not happen
elapsed() { awk -v a="$1" -v b="$2" 'BEGIN { printf "%.2f", b - a }'; }
in_range() { awk -v v="$1" -v lo="$2" -v hi="$3" 'BEGIN { exit !(v >= lo && v <= hi) }'; }
plus() { awk -v a="$1" -v b="$2" 'BEGIN { printf "%.2f", a + b }'; }

pane() { tmux capture-pane -t "$SESSION" -p 2>/dev/null || true; }

fail() {
    local dump="$DUMP_DIR/$CURRENT_LEG.pane"
    pane >"$dump" 2>/dev/null || true
    printf 'FAIL [%s]: %s\n' "$CURRENT_LEG" "$1" >&2
    printf '      pane dump: %s\n' "$dump" >&2
    return 1
}

# The value of a `Duration::from_secs` constant, read from the file that
# owns it. A threshold that moved and a script that did not would silently
# assert the wrong window, which is the one failure a timing acceptance
# cannot afford.
const_secs() {
    local file="$1" name="$2" value
    value=$(grep -oE "pub const $name: Duration = Duration::from_secs\([0-9]+\)" "$file" |
        grep -oE '[0-9]+' | tail -1)
    if [ -z "$value" ]; then
        printf 'FAIL: %s is not a from_secs constant in %s any more\n' "$name" "$file" >&2
        return 1
    fi
    printf '%s' "$value"
}

# The `&str` constant `name` holds, read from the file that owns it: the
# keys the modal binds are typed here exactly as the source names them.
const_str() {
    local file="$1" name="$2" value
    value=$(grep -oE "pub const $name: &str = \"[^\"]+\"" "$file" | sed -E 's/.*"(.*)"/\1/')
    if [ -z "$value" ]; then
        printf 'FAIL: %s is not a &str constant in %s any more\n' "$name" "$file" >&2
        return 1
    fi
    printf '%s' "$value"
}

# One arm of a `WedgeKind` method, so the text asserted on screen is the
# text the enum returns rather than a copy of it.
wedge_arm() {
    local method="$1" variant="$2" value
    value=$(awk -v method="pub const fn $method" -v arm="Self::$variant" '
        index($0, method) { inside = 1 }
        inside && index($0, arm) && index($0, "=> \"") { print; exit }
    ' "$SUPERVISION_RS" | sed -E 's/.*=> "(.*)",?[[:space:]]*$/\1/')
    if [ -z "$value" ]; then
        printf 'FAIL: WedgeKind::%s has no %s arm in %s any more\n' "$method" "$variant" "$SUPERVISION_RS" >&2
        return 1
    fi
    printf '%s' "$value"
}

# nvim's own key notation as tmux spells it. The two agree on everything
# this modal binds once the brackets nvim wraps a named key in are off.
tmux_key() {
    local notation="$1"
    notation=${notation#<}
    printf '%s' "${notation%>}"
}

# Waits for `pattern` to be on screen, answering how long that took.
wait_for() {
    local pattern="$1" budget="$2" what="$3" start el
    start=$(now)
    while :; do
        if pane | grep -qF -- "$pattern"; then
            elapsed "$start" "$(now)"
            return 0
        fi
        if ! tmux has-session -t "$SESSION" 2>/dev/null; then
            fail "the view session exited while waiting for $what"
            return 1
        fi
        el=$(elapsed "$start" "$(now)")
        if ! in_range "$el" 0 "$budget"; then
            fail "$what never appeared: no '$pattern' on screen after ${budget}s"
            return 1
        fi
        sleep "$POLL"
    done
}

# Waits for `pattern` to leave the screen, answering how long that took.
wait_gone() {
    local pattern="$1" budget="$2" what="$3" start el
    start=$(now)
    while :; do
        if ! pane | grep -qF -- "$pattern"; then
            elapsed "$start" "$(now)"
            return 0
        fi
        el=$(elapsed "$start" "$(now)")
        if ! in_range "$el" 0 "$budget"; then
            fail "$what is still on screen after ${budget}s: '$pattern'"
            return 1
        fi
        sleep "$POLL"
    done
}

assert_within() {
    local measured="$1" lo="$2" hi="$3" what="$4"
    if ! in_range "$measured" "$lo" "$hi"; then
        fail "$what took ${measured}s, outside the [${lo}s, ${hi}s] the source's own thresholds bound it to"
        return 1
    fi
}

# A session of view's own, isolated from whatever this machine's user has
# configured: a personal init.lua floats errors of its own onto the screen
# and a personal view.toml can switch off the very affordance under test.
start_session() {
    local tag="$1" seed="$2"
    SESSION="view-acc-$$-$tag"
    ROOT=$(mktemp -d "${TMPDIR:-/tmp}/view-acc-$tag-XXXXXX")
    ROOTS+=("$ROOT")
    cp -R "$FIXTURE" "$ROOT/xdg_config_home"
    mkdir -p "$ROOT/xdg_data_home" "$ROOT/xdg_state_home" "$ROOT/xdg_cache_home"
    # the shipped default recovers a dead engine without asking, which is
    # the right default and the wrong one for an acceptance run that has to
    # observe the modal a user is offered
    printf '\n[supervision]\nauto_restart = false\n' >>"$ROOT/xdg_config_home/view/view.toml"
    printf '%s\n' "$seed" >"$ROOT/scratch.txt"

    tmux kill-session -t "$SESSION" 2>/dev/null || true
    SESSIONS+=("$SESSION")
    tmux new-session -d -s "$SESSION" -x "$COLS" -y "$ROWS" \
        "env VIEW_COMPAT_SOCK=$ROOT/compat.sock \
             XDG_CONFIG_HOME=$ROOT/xdg_config_home \
             XDG_DATA_HOME=$ROOT/xdg_data_home \
             XDG_STATE_HOME=$ROOT/xdg_state_home \
             XDG_CACHE_HOME=$ROOT/xdg_cache_home \
             TERM=xterm-256color COLORTERM=truecolor \
             $VIEW_BIN $ROOT/scratch.txt"

    wait_for "$seed" 30 "the seeded buffer" >/dev/null || return 1
    VIEW_PID=$(tmux list-panes -t "$SESSION" -F '#{pane_pid}')
    read_engine_pid || return 1
}

# The engine behind the current session. Re-read after every restart: the
# recovery under test replaces the process, and a cleanup holding only the
# pid it started with would leave the replacement running.
read_engine_pid() {
    local start el
    start=$(now)
    while :; do
        NVIM_PID=$(pgrep -P "$VIEW_PID" -x nvim | head -1 || true)
        if [ -n "$NVIM_PID" ]; then
            NVIM_PIDS+=("$NVIM_PID")
            return 0
        fi
        el=$(elapsed "$start" "$(now)")
        if ! in_range "$el" 0 15; then
            fail "view (pid $VIEW_PID) has no nvim child after ${el}s"
            return 1
        fi
        sleep "$POLL"
    done
}

type_line() {
    tmux send-keys -t "$SESSION" -l "$1"
    tmux send-keys -t "$SESSION" Enter
}

# Text typed into the buffer and never written to the file, so a later
# reading of it off the screen can only have come back through nvim's own
# swap file.
type_unsaved() {
    local token="$1"
    tmux send-keys -t "$SESSION" -l "o$token"
    tmux send-keys -t "$SESSION" Escape
    wait_for "$token" 15 "the unsaved line" >/dev/null || return 1
    # `:preserve` rather than a wait on nvim's idle swap flush: the swap is
    # what a restart recovers from, and an acceptance that raced the flush
    # would fail for the clock rather than for the recovery
    type_line ':preserve'
}

# The recovery a restart owes, from either wedge: a fresh engine, the
# unsaved text back off the swap, and an input path that works again.
assert_restart_recovered() {
    local token="$1" live="$2" modal_title="$3"
    wait_gone "$modal_title" 30 "the modal" >/dev/null || return 1
    # the session's own account of the recovery, and equally the first frame
    # the replacement paints: waiting on it is what keeps the assertions
    # below from reading a screen an engine is still coming up behind
    wait_for "$RECOVERY_NOTICE" 30 "the swap-recovery notice" >/dev/null || return 1
    # and nvim's own multi-line report, which covers the top of the buffer
    # the assertions below have to read, goes with no keypress at all: the
    # redraw that retires it rides the same fold that raised the notice
    wait_gone "$SWAP_REPLAYED" 15 "the swap-recovery report" >/dev/null || return 1
    wait_for "$token" 30 "the unsaved text after the restart" >/dev/null || return 1
    if grep -qF -- "$token" "$ROOT/scratch.txt"; then
        fail "$token is in the file on disk, so its return proves a re-read and not a swap recovery"
        return 1
    fi
    tmux send-keys -t "$SESSION" -l "o$live"
    tmux send-keys -t "$SESSION" Escape
    wait_for "$live" 15 "typed text after the restart" >/dev/null || return 1
    VIEW_PID=$(tmux list-panes -t "$SESSION" -F '#{pane_pid}')
    read_engine_pid || return 1
}

end_session() {
    tmux kill-session -t "$SESSION" 2>/dev/null || true
}

for tool in tmux awk sed grep pgrep; do
    command -v "$tool" >/dev/null 2>&1 || {
        printf 'FAIL: %s is not on PATH; this acceptance drives a real terminal\n' "$tool" >&2
        exit 1
    }
done
ensure_artifact "$VIEW_BIN" "$REPO_ROOT/target/release/view" \
    cargo build --release -p view || exit 1
[ -d "$FIXTURE" ] || {
    printf 'FAIL: the no-plugins fixture is missing at %s\n' "$FIXTURE" >&2
    exit 1
}

READ_NOTICE=$(wedge_arm notice ReadSide)
WRITE_NOTICE=$(wedge_arm notice WriteSide)
DEAD_NOTICE=$(wedge_arm notice Dead)
BUSY_TITLE=$(wedge_arm title ReadSide)
GONE_TITLE=$(wedge_arm title Dead)
INTERRUPT_KEY=$(tmux_key "$(const_str "$SUPERVISION_RS" INTERRUPT_NOTATION)")
RESTART_KEY=$(tmux_key "$(const_str "$SUPERVISION_RS" RESTART_NOTATION)")

WEDGE_THRESHOLD=$(const_secs "$HEARTBEAT_RS" HEARTBEAT_WEDGE_THRESHOLD)
PROBE_INTERVAL=$(const_secs "$HEARTBEAT_RS" HEARTBEAT_PROBE_INTERVAL)
MODAL_THRESHOLD=$(const_secs "$SUPERVISION_RS" ENGINE_BUSY_MODAL_THRESHOLD)
INTERRUPT_WINDOW=$(const_secs "$SUPERVISION_RS" INTERRUPT_REACTION_WINDOW)
WRITER_THRESHOLD=$(const_secs "$STALL_RS" WRITER_STALL_THRESHOLD)

# A verdict cannot be reached before its own threshold, and is owed no
# sooner than the next reading after it: one probe interval for the read
# side, whose cadence is what looks again, and one observation for the
# write side, whose deadline is exact. The slack on top is this script's
# own -- the poll granularity above, plus the paint that follows the fold.
# The lower bounds carry half a second of the same slack in the other
# direction: each is timed from a keystroke sent before the loop anchors
# the condition, so a measurement can only run long, and the tolerance
# exists for the rounding, never to forgive a verdict reached early.
OBSERVATION_SLACK=0.5
BANNER_MIN=$(awk -v t="$WEDGE_THRESHOLD" -v s="$OBSERVATION_SLACK" 'BEGIN { printf "%.1f", t - s }')
BANNER_MAX=$(plus "$((WEDGE_THRESHOLD + PROBE_INTERVAL))" "$OBSERVATION_SLACK")
WRITE_MIN=$(awk -v t="$WRITER_THRESHOLD" -v s="$OBSERVATION_SLACK" 'BEGIN { printf "%.1f", t - s }')
WRITE_MAX=$(plus "$WRITER_THRESHOLD" 3)
# measured from the banner, which is itself observed up to one poll after
# the fold anchored the episode the modal's own threshold runs from
MODAL_MIN=$(awk -v t="$MODAL_THRESHOLD" 'BEGIN { printf "%.1f", t - 1 }')
MODAL_MAX=$(plus "$MODAL_THRESHOLD" 2.5)
INTERRUPT_MIN=$(awk -v w="$INTERRUPT_WINDOW" 'BEGIN { printf "%.1f", w - 0.5 }')
INTERRUPT_MAX=$(plus "$INTERRUPT_WINDOW" 3)
# a closed connection spends no patience at all, so both annunciators are
# owed on the same observation that saw it close
DEAD_MAX=2.5

# nvim's own wording for a swap file it replayed. The engine is pinned, so
# this is as fixed as the constants above, and a restart that recovered
# nothing prints something else.
SWAP_REPLAYED='Recovery completed'

# The line view itself raises once its replacement engine has replayed a swap
# file, and the whole reason the report above comes down without a keypress
# (see `swap_recovery_notice`). Held to the source's own wording by the same
# rule as everything else asserted here.
RECOVERY_NOTICE='unsaved changes recovered from the swap file'
grep -qF -- "$RECOVERY_NOTICE" "$SUPERVISION_RS" || {
    printf 'FAIL: a recovery no longer reports "%s" in %s\n' "$RECOVERY_NOTICE" "$SUPERVISION_RS" >&2
    exit 1
}

# The clause the modal adds once an interrupt has gone unanswered for longer
# than a reply could still be in flight. Held to the source's own wording by
# the same rule as everything else asserted here.
INTERRUPT_UNANSWERED='interrupt sent'
grep -qF -- "$INTERRUPT_UNANSWERED" "$SUPERVISION_RS" || {
    printf 'FAIL: the modal no longer reports "%s" in %s\n' "$INTERRUPT_UNANSWERED" "$SUPERVISION_RS" >&2
    exit 1
}

# A Lua `while`, which pumps nothing: an engine inside one answers neither
# the liveness probe nor the interrupt, which is the wedge class the
# restart exists for. `vim.uv.hrtime` because libuv's loop-cached clock
# never advances inside a loop that reaches no loop iteration.
LUA_WEDGE=":lua local t=vim.uv.hrtime() while $((WEDGE_BUDGET_SECS * 1000000000)) - (vim.uv.hrtime()-t) > 0 do end"
# A Vimscript `while`, whose break check pumps the event loop: it stops
# answering the probe just the same, and it is the wedge class `<C-c>`
# reaches, so it is what proves the modal's first choice recovers anything.
VIM_WEDGE=":let g:t=reltime() | while $WEDGE_BUDGET_SECS - reltimefloat(reltime(g:t)) > 0 | endwhile"

printf 'view acceptance: supervision (%s, %s, %sx%s)\n' \
    "${VIEW_BIN#"$REPO_ROOT/"}" "$(nvim --version | head -1)" "$COLS" "$ROWS"

CURRENT_LEG=read-side-lua
start_session lua 'acceptance seed line'
UNSAVED="UNSAVED-LUA-$$"
type_unsaved "$UNSAVED"
type_line "$LUA_WEDGE"
banner=$(wait_for "$READ_NOTICE" 20 "the read-side banner")
assert_within "$banner" "$BANNER_MIN" "$BANNER_MAX" "the read-side banner"
modal=$(wait_for "$BUSY_TITLE" 40 "the busy modal")
assert_within "$modal" "$MODAL_MIN" "$MODAL_MAX" "the busy modal"
tmux send-keys -t "$SESSION" "$INTERRUPT_KEY"
# what the modal owes a wedge the interrupt cannot reach: not silence, but
# the fact that nothing answered it
unanswered=$(wait_for "$INTERRUPT_UNANSWERED" 20 "the unanswered-interrupt line")
assert_within "$unanswered" "$INTERRUPT_MIN" "$INTERRUPT_MAX" "the unanswered-interrupt line"
tmux send-keys -t "$SESSION" "$RESTART_KEY"
assert_restart_recovered "$UNSAVED" "LIVE-LUA-$$" "$BUSY_TITLE"
end_session
printf '[1/3] %-33s ... %s  OK\n' 'read-side wedge (blocked Lua)' \
    "banner at ${banner}s, modal at ${modal}s after it, interrupt unanswered at ${unanswered}s, restart recovers (swap rehydrated)"

CURRENT_LEG=read-side-vimscript
start_session vim 'acceptance seed line'
type_line "$VIM_WEDGE"
vim_banner=$(wait_for "$READ_NOTICE" 20 "the read-side banner")
assert_within "$vim_banner" "$BANNER_MIN" "$BANNER_MAX" "the read-side banner"
vim_modal=$(wait_for "$BUSY_TITLE" 40 "the busy modal")
assert_within "$vim_modal" "$MODAL_MIN" "$MODAL_MAX" "the busy modal"
interrupt_start=$(now)
tmux send-keys -t "$SESSION" "$INTERRUPT_KEY"
wait_gone "$BUSY_TITLE" 20 "the busy modal" >/dev/null
wait_gone "$READ_NOTICE" 20 "the read-side banner" >/dev/null
LIVE=LIVE-VIM-$$
tmux send-keys -t "$SESSION" -l "o$LIVE"
tmux send-keys -t "$SESSION" Escape
wait_for "$LIVE" 15 "typed text after the interrupt" >/dev/null
recovered=$(elapsed "$interrupt_start" "$(now)")
end_session
printf '      %-33s ... %s  OK\n' 'read-side wedge (Vimscript)' \
    "banner at ${vim_banner}s, modal at ${vim_modal}s after it, interrupt recovers in ${recovered}s"

CURRENT_LEG=dead-connection
start_session dead 'acceptance seed line'
UNSAVED="UNSAVED-DEAD-$$"
type_unsaved "$UNSAVED"
kill -9 "$NVIM_PID"
dead_notice=$(wait_for "$DEAD_NOTICE" 15 "the dead-connection banner")
dead_modal=$(wait_for "$GONE_TITLE" 15 "the dead-connection modal")
assert_within "$dead_notice" 0 "$DEAD_MAX" "the dead-connection banner"
assert_within "$(plus "$dead_notice" "$dead_modal")" 0 "$DEAD_MAX" "the dead-connection modal"
tmux send-keys -t "$SESSION" "$RESTART_KEY"
assert_restart_recovered "$UNSAVED" "LIVE-DEAD-$$" "$GONE_TITLE"
end_session
printf '[2/3] %-33s ... %s  OK\n' 'dead connection (SIGKILL)' \
    "banner+modal at $(plus "$dead_notice" "$dead_modal")s (Dead skips the grace period), restart recovers, swap rehydrated"

CURRENT_LEG=write-side
start_session write 'acceptance seed line'
head -c "$PASTE_BYTES" /dev/zero | tr '\0' 'x' >"$ROOT/paste.txt"
# stopped rather than killed: the connection stays open and the peer stops
# reading it, which is the only shape the write watch reports on -- a
# closed connection is the leg above
kill -STOP "$NVIM_PID"
write_start=$(now)
tmux load-buffer -b "view-acc-$$" "$ROOT/paste.txt"
tmux paste-buffer -p -b "view-acc-$$" -t "$SESSION"
# timed from the stop rather than from the paste that follows it, so the
# measurement contains the whole window the loop could have anchored its
# stall in and can only ever read long
wait_for "$WRITE_NOTICE" 25 "the write-side banner" >/dev/null
write_notice=$(elapsed "$write_start" "$(now)")
assert_within "$write_notice" "$WRITE_MIN" "$WRITE_MAX" "the write-side banner"
# the write side outranks the read side while both are true, because a
# writer that stopped delivering is enough on its own to silence the probes
# the read side is waiting on
if pane | grep -qF -- "$READ_NOTICE"; then
    fail "the read-side notice is on screen instead of the write side's, which reports the cause" || exit 1
fi
kill -CONT "$NVIM_PID"
end_session
printf '[3/3] %-33s ... %s  OK\n' 'write-side wedge (existing)' \
    "banner at ${write_notice}s (regression check, unchanged)"
