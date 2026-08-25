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
VIEW_BIN=${VIEW_BIN:-$TARGET_ROOT/release/view}
STUB_BIN=${STUB_BIN:-$TARGET_ROOT/release/view-ai-stub-agent}
FIXTURE=$REPO_ROOT/compat/fixtures/minimal
SUPERVISION_RS=$REPO_ROOT/crates/view-core/src/native/supervision.rs
HEARTBEAT_RS=$REPO_ROOT/crates/view-engine/src/heartbeat.rs
STALL_RS=$REPO_ROOT/crates/view-engine/src/stall.rs
PANEL_RS=$REPO_ROOT/crates/view-core/src/native/ai_panel/mod.rs
AI_UPDATE_RS=$REPO_ROOT/crates/view-core/src/update/ai.rs

# What the exit legs' buffer holds, and what their shell prints once it has
# the tty back. Distinct strings, because the assertion that view left is
# exactly "the second one is on screen and the first one's frame is not".
EXIT_SEED='exit-path seed line'
SHELL_BACK='SHELL-HAS-THE-TTY'

# The pane the legs read. Wide enough that no notice or modal row is
# truncated before the text an assertion greps for, tall enough that the
# banner and the modal are on screen together.
COLS=120
ROWS=40
# How often the screen is read. Charged in full to every measurement below,
# so it is small next to the tightest window any of them assert (2.5s).
POLL=0.25

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
    reap_views || code=1
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
    # the screen says what the user saw; the session's own log says why, and
    # the leg's root is removed on the way out, so it is copied out here or
    # it is gone
    if [ -n "${ROOT:-}" ] && [ -f "$ROOT/view.log" ]; then
        cp "$ROOT/view.log" "$DUMP_DIR/$CURRENT_LEG.log" 2>/dev/null || true
        printf '      view log:  %s\n' "$DUMP_DIR/$CURRENT_LEG.log" >&2
    fi
    # the byte stream the exit legs assert on, which the leg's root takes
    # with it on the way out: without it a reader of a failed restore-burst
    # match has the verdict and none of the bytes that produced it
    if [ -n "${ROOT:-}" ] && [ -f "$ROOT/pane.raw" ]; then
        cp "$ROOT/pane.raw" "$DUMP_DIR/$CURRENT_LEG.raw" 2>/dev/null || true
        printf '      pane bytes: %s\n' "$DUMP_DIR/$CURRENT_LEG.raw" >&2
    fi
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
             VIEW_LOG=$ROOT/view.log \
             TERM=xterm-256color COLORTERM=truecolor \
             $VIEW_BIN $ROOT/scratch.txt"

    wait_for "$seed" 30 "the seeded buffer" >/dev/null || return 1
    watch_view "$SESSION" || return 1
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
    # and the flush is waited for, not assumed: `type_line` returns once the
    # bytes are handed to tmux, so a caller that killed the engine next was
    # racing nvim's execution of the command its whole recovery depends on.
    # On a loaded host the kill wins, the swap holds nothing, and the leg
    # fails on the recovery notice instead of on the race
    wait_for "$PRESERVED" 15 "the swap flush" >/dev/null || return 1
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
    watch_view "$SESSION" || return 1
    read_engine_pid || return 1
}

# A session whose pane keeps a shell alive after view has left, so the state
# view handed the terminal back in can be read where the user meets it: from
# the shell that gets the tty next.
#
# The three legs above never need this -- they observe view while it runs and
# kill the pane afterwards -- but the whole failure this shape exists for is
# invisible from inside view: a raw-mode alternate screen with nothing left
# running reads to a user as a frozen editor, and only the next occupant of
# the tty can say whether it was handed back.
#
# `stty -g` on both sides of the run rather than a reading of any single
# flag: it is the terminal's own restorable dump, so an exact comparison
# covers raw mode, echo, ISIG and everything else a session could leave
# altered.
start_exit_session() {
    local tag="$1"
    SESSION="view-acc-$$-$tag"
    ROOT=$(mktemp -d "${TMPDIR:-/tmp}/view-acc-$tag-XXXXXX")
    ROOTS+=("$ROOT")
    cp -R "$FIXTURE" "$ROOT/xdg_config_home"
    mkdir -p "$ROOT/xdg_data_home" "$ROOT/xdg_state_home" "$ROOT/xdg_cache_home"
    printf '\n[ai]\nagent = ["%s"]\n' "$STUB_BIN" >>"$ROOT/xdg_config_home/view/view.toml"
    printf '%s\n' "$EXIT_SEED" >"$ROOT/scratch.txt"

    cat >"$ROOT/run.sh" <<EOF
stty -g >"$ROOT/termios.before"
env VIEW_COMPAT_SOCK=$ROOT/compat.sock \
    XDG_CONFIG_HOME=$ROOT/xdg_config_home \
    XDG_DATA_HOME=$ROOT/xdg_data_home \
    XDG_STATE_HOME=$ROOT/xdg_state_home \
    XDG_CACHE_HOME=$ROOT/xdg_cache_home \
    VIEW_LOG=$ROOT/view.log \
    TERM=xterm-256color COLORTERM=truecolor \
    "$VIEW_BIN" "$ROOT/scratch.txt"
printf '%s' "\$?" >"$ROOT/exit.code"
stty -g >"$ROOT/termios.after"
printf '$SHELL_BACK\n'
exec sleep 600
EOF

    tmux kill-session -t "$SESSION" 2>/dev/null || true
    SESSIONS+=("$SESSION")
    tmux new-session -d -s "$SESSION" -x "$COLS" -y "$ROWS" -c "$ROOT" \
        "sh $ROOT/run.sh"
    # the raw byte stream the pane receives, which is where the teardown
    # escapes themselves are provable: tmux acts on them rather than echoing
    # them, so a screen capture can show the outcome but never the sequence
    tmux pipe-pane -o -t "$SESSION" "cat >>$ROOT/pane.raw"

    wait_for "$EXIT_SEED" 30 "the seeded buffer" >/dev/null || return 1
    # the pane runs the shell above, so view is its child rather than the
    # pane process itself
    watch_view "$SESSION" || return 1
    read_engine_pid || return 1
}

# Opens the agent panel and gets a stub agent genuinely running underneath
# it, so the exit under test is one with both children alive.
open_stub_panel() {
    local start el
    type_line ':View ai'
    wait_for "$TRUST_PROMPT" 30 "the project trust prompt" >/dev/null || return 1
    tmux send-keys -t "$SESSION" -l 'y'
    wait_for "$FOCUSED_TITLE" 30 "the entered agent panel" >/dev/null || return 1
    # a session starts on the first command, never on the panel opening
    tmux send-keys -t "$SESSION" -l 'hello'
    tmux send-keys -t "$SESSION" Enter
    start=$(now)
    while :; do
        STUB_PID=$(pgrep -P "$VIEW_PID" -f "$STUB_BIN" | head -1 || true)
        [ -n "$STUB_PID" ] && return 0
        el=$(elapsed "$start" "$(now)")
        if ! in_range "$el" 0 30; then
            fail "the stub agent never started under view (pid $VIEW_PID), so this leg would prove nothing about orphaned children"
            return 1
        fi
        sleep "$POLL"
    done
}

# Everything view owes the terminal and the process table on its way out,
# asserted from the shell that got the tty back.
assert_exit_was_clean() {
    local want_code="$1" start el code
    wait_for "$SHELL_BACK" 30 "the shell after view left" >/dev/null || return 1

    code=$(cat "$ROOT/exit.code" 2>/dev/null || true)
    if [ "$code" != "$want_code" ]; then
        fail "view left with status ${code:-<none>}, expected $want_code"
        return 1
    fi

    if ! cmp -s "$ROOT/termios.before" "$ROOT/termios.after"; then
        fail "the terminal modes view was handed are not the ones it gave back: $(cat "$ROOT/termios.before") vs $(cat "$ROOT/termios.after")"
        return 1
    fi

    if [ "$(tmux display-message -p -t "$SESSION" '#{alternate_on}')" != 0 ]; then
        fail "the pane is still on the alternate screen, so the shell's own scrollback is behind view's last frame"
        return 1
    fi
    if [ "$(tmux display-message -p -t "$SESSION" '#{cursor_flag}')" != 1 ]; then
        fail "the caret is still hidden, which reads to a user as a terminal that stopped responding"
        return 1
    fi

    # the escapes themselves, off the raw stream: the two readings above are
    # tmux's verdict, and this is the byte sequence that produced it
    grep -qU $'\033\[?1049l' "$ROOT/pane.raw" || {
        fail "no leave-alternate-screen escape ever reached the pty"
        return 1
    }
    # the caret escapes are matched as one contiguous burst rather than
    # searched for individually: `?25h` alone is written by every frame that
    # has a caret to place, so finding one somewhere in a session's bytes
    # proves nothing about the teardown. Only `restore_bytes` closes a sync
    # bracket, resets the shape and shows the caret back to back, so this
    # match fails the moment any of the three leaves it.
    grep -qU $'\033\[<u' "$ROOT/pane.raw" || {
        fail "the kitty keyboard protocol was never popped, so a terminal that entered it keeps sending view's key encoding to the shell"
        return 1
    }
    grep -qU $'\033\[?2026l\033\[<u\033\[0 q\033\[?25h' "$ROOT/pane.raw" || {
        fail "the restore burst (sync-bracket close, kitty keyboard pop, caret shape reset, caret show) never reached the pty as one sequence"
        return 1
    }

    # nothing view spawned outlives it. Both children are signalled by
    # destructors `std::process::exit` would otherwise skip, so an orphan
    # here is a teardown that did not run rather than one that was slow.
    start=$(now)
    while :; do
        if ! kill -0 "$NVIM_PID" 2>/dev/null && ! kill -0 "$STUB_PID" 2>/dev/null; then
            return 0
        fi
        el=$(elapsed "$start" "$(now)")
        if ! in_range "$el" 0 10; then
            fail "view left children behind after ${el}s: nvim $NVIM_PID $(kill -0 "$NVIM_PID" 2>/dev/null && echo alive || echo gone), agent $STUB_PID $(kill -0 "$STUB_PID" 2>/dev/null && echo alive || echo gone)"
            return 1
        fi
        sleep "$POLL"
    done
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
ensure_artifact "$VIEW_BIN" "$TARGET_ROOT/release/view" \
    cargo build --release -p view || exit 1
ensure_artifact "$STUB_BIN" "$TARGET_ROOT/release/view-ai-stub-agent" \
    cargo build --release -p view-ai --features test-support --bin view-ai-stub-agent || exit 1
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
# crate-private rather than `pub`, so `const_str`'s pattern does not reach it.
# Only the head of it is ever asserted: the panel is a third of this pane
# wide, so its own framing truncates the title, and the part that survives is
# still the half that says "focused" -- the unfocused title is the bare
# `TITLE`, which carries no separator at all.
FOCUSED_TITLE=$(grep -oE 'const FOCUSED_TITLE: &str = "[^"]+"' "$PANEL_RS" |
    sed -E 's/.*"(.*)"/\1/' | cut -c1-16)
case "$FOCUSED_TITLE" in
*--*) ;;
*)
    printf 'FAIL: FOCUSED_TITLE in %s no longer reads as focused within its first 16 columns (%s), so a truncated title cannot be told from the unfocused one\n' \
        "$PANEL_RS" "${FOCUSED_TITLE:-nothing this can read}" >&2
    exit 1
    ;;
esac
TRUST_PROMPT=$(grep -oE '"Trust \{\}' "$AI_UPDATE_RS" | sed -E 's/"(.*)\{\}/\1/')
[ -n "$TRUST_PROMPT" ] || {
    printf 'FAIL: the AI trust prompt is not built from a literal in %s any more\n' "$AI_UPDATE_RS" >&2
    exit 1
}
STUB_PID=""

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

# nvim's own acknowledgement of `:preserve`, and the cue that the swap file
# a restart recovers from actually holds the unsaved line.
PRESERVED='File preserved'

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

# Both wedges spin unconditionally, with no duration bound of their own.
#
# A bound is what a bounded loop has to read a clock to enforce, and a clock
# read from inside a wedged engine is not a measurement this harness can
# trust: on dev-macos the same `vim.uv.hrtime()` budget that holds for its
# whole 180 s on dev-linux is observed crossed after 18-50 s of wall (the
# leg then fails on the modal, which is still 30 s away), and a 120 s budget
# in the same shape has equally been observed crossed only after 411 s. An
# unbounded loop reads no clock, so neither reading can end it.
#
# Nothing is lost by dropping the bound. What ends each wedge is the recovery
# under test, and the property the bound was there for -- that a loop which
# simply finished can never be mistaken for one a recovery ended -- is
# strictly stronger here, since a loop with no exit condition cannot finish.
# A leg that fails before its recovery leaves the engine spinning, which the
# script's own cleanup kills along with every other engine it started.
#
# A Lua `while` pumps nothing: an engine inside one answers neither the
# liveness probe nor the interrupt, which is the wedge class the restart
# exists for.
LUA_WEDGE=':lua while true do end'
# A Vimscript `while`, whose break check pumps the event loop: it stops
# answering the probe just the same, and it is the wedge class `<C-c>`
# reaches, so it is what proves the modal's first choice recovers anything.
VIM_WEDGE=':while 1 | endwhile'

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
printf '[1/6] %-33s ... %s  OK\n' 'read-side wedge (blocked Lua)' \
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
printf '[2/6] %-33s ... %s  OK\n' 'dead connection (SIGKILL)' \
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
printf '[3/6] %-33s ... %s  OK\n' 'write-side wedge (existing)' \
    "banner at ${write_notice}s (regression check, unchanged)"

# The two legs below are about the exit rather than the wedge: an engine
# that stops is only half the contract, and the half a user meets is what
# the terminal looks like afterwards. Both are run with the panel open and a
# real agent child underneath it, because the exit that stranded a terminal
# in the field was one taken from that state.
CURRENT_LEG=clean-exit
start_exit_session quit
open_stub_panel
tmux send-keys -t "$SESSION" Escape
type_line ':q'
assert_exit_was_clean 0
end_session
printf '[4/6] %-33s ... %s  OK\n' 'user quit under a live panel' \
    'exits 0, termios identical, alternate screen left, caret shown, no children behind'

# the deaths view does not choose. SIGHUP is the one the field incident was
# diagnosed from -- the link drops, sshd HUPs the session, and the foreground
# job is signalled where it stands -- and SIGTERM is what every supervisor and
# `kill` sends. Handled, each takes the same teardown `:q` does; unhandled,
# each ends the process on a raw-mode alternate screen the user has to repair
# from another shell. Both are run because they arrive through different
# registrations and a handler installed for one proves nothing about the other.
leg=4
for signal in HUP TERM; do
    leg=$((leg + 1))
    CURRENT_LEG="fatal-signal-$signal"
    start_exit_session "signal-$signal"
    open_stub_panel
    tmux send-keys -t "$SESSION" Escape
    kill "-$signal" "$VIEW_PID"
    want=$((128 + $(kill -l "$signal")))
    assert_exit_was_clean "$want"
    end_session
    printf '[%d/6] %-33s ... %s  OK\n' "$leg" "SIG$signal under a live panel" \
        "exits $want, termios identical, alternate screen left, caret shown, no children behind"
done
