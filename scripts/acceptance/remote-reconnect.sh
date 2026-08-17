#!/usr/bin/env bash
#
# Remote reconnect conformance: an ssh connection dropped out from under a
# live session, and what the person sitting at the editor sees between that
# drop and the session coming back.
#
# The headless suite already proves the schedule's arithmetic and the fold's
# banner, and the engine suite already proves that a killed client reads as a
# closed connection. None of them can reach the whole chain at once -- a real
# client process ending, view's own loop waiting out a backoff, a counted
# banner painted into a frame, an edit accepted by the engine that replaced
# the dead one -- and a recovery that never reaches the screen is
# indistinguishable from no recovery at all for the person waiting.
#
# The far side is the committed stand-in client rather than a real host: the
# behaviour under test is entirely view's (a connection ends, attempts are
# spaced, one of them works), and a run that needed a reachable sshd could
# not run in CI at all. The drop is induced by killing the client process,
# which is what a dropped connection is from this side of it.
#
# Every bound and every string this asserts is read out of the source that
# owns it, so a retuned backoff or a reworded banner fails here loudly
# instead of leaving the script asserting a claim the code stopped making.
set -euo pipefail

SCRIPT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
REPO_ROOT=$(cd -- "$SCRIPT_DIR/../.." && pwd)
VIEW_BIN=${VIEW_BIN:-$REPO_ROOT/target/release/view}
FIXTURE=$REPO_ROOT/compat/fixtures/minimal
FIXTURES=$REPO_ROOT/scripts/test-fixtures
SUPERVISION_RS=$REPO_ROOT/crates/view-core/src/native/supervision.rs
PROCESS_RS=$REPO_ROOT/crates/view-engine/src/process.rs

# The pane the legs read. Wide enough that no notice is truncated before the
# text an assertion greps for.
COLS=120
ROWS=40
# How often the screen is read. Charged in full to every measurement below,
# so it is small next to the tightest window any of them assert (one backoff
# base, one second).
POLL=0.25

# How many of the reconnect attempts are made to fail before the client
# starts working again. One, so the run proves both halves of the sequence
# it is about -- an attempt that fails does not end the session, and the
# attempt after it is spaced by the backoff rather than fired immediately --
# without spending the whole doubling ladder to do it.
REFUSED_ATTEMPTS=1

SESSION=""
ROOT=""
VIEW_PID=""
CLIENT_PID=""
CURRENT_LEG=startup
DUMP_DIR=$(mktemp -d "${TMPDIR:-/tmp}/view-reconnect-XXXXXX")

cleanup() {
    local code=$?
    tmux kill-session -t "$SESSION" 2>/dev/null || true
    # the client owns the far side's editor, and a run that ended between the
    # drop and the recovery would otherwise leave one behind
    pkill -f "view-reconnect-$$" 2>/dev/null || true
    [ -n "$ROOT" ] && rm -rf "$ROOT"
    if [ "$code" -eq 0 ]; then
        rm -rf "$DUMP_DIR"
    else
        printf '      pane dumps kept in %s\n' "$DUMP_DIR" >&2
    fi
    exit "$code"
}
trap cleanup EXIT INT TERM

now() { date +%s.%N; }
elapsed() { awk -v a="$1" -v b="$2" 'BEGIN { printf "%.2f", b - a }'; }
in_range() { awk -v v="$1" -v lo="$2" -v hi="$3" 'BEGIN { exit !(v >= lo && v <= hi) }'; }
plus() { awk -v a="$1" -v b="$2" 'BEGIN { printf "%.2f", a + b }'; }
minus() { awk -v a="$1" -v b="$2" 'BEGIN { printf "%.2f", a - b }'; }
times() { awk -v a="$1" -v b="$2" 'BEGIN { printf "%.2f", a * b }'; }

pane() { tmux capture-pane -t "$SESSION" -p 2>/dev/null || true; }

fail() {
    local dump="$DUMP_DIR/$CURRENT_LEG.pane"
    pane >"$dump" 2>/dev/null || true
    printf 'FAIL [%s]: %s\n' "$CURRENT_LEG" "$1" >&2
    printf '      pane dump: %s\n' "$dump" >&2
    return 1
}

# The value of a `Duration::from_secs` constant, read from the file that owns
# it. A backoff that moved and a script that did not would silently assert
# the wrong window, which is the one failure a timing acceptance cannot
# afford.
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

# The value of a plain integer constant, by the same rule.
const_int() {
    local file="$1" name="$2" value
    value=$(grep -oE "pub const $name: u32 = [0-9]+" "$file" | grep -oE '[0-9]+$')
    if [ -z "$value" ]; then
        printf 'FAIL: %s is not a u32 constant in %s any more\n' "$name" "$file" >&2
        return 1
    fi
    printf '%s' "$value"
}

# The banner a scheduled reconnect paints, on attempt `$1` of the cap: the
# format string is lifted out of `ReconnectProgress::notice` and filled the
# way the code fills it, so a reworded banner is a failure here rather than a
# script asserting text nothing renders any more.
banner_for() {
    printf '%s' "$RECONNECT_FMT" | sed -e "s/{}/$1/" -e "s/{}/$MAX_ATTEMPTS/"
}

# One arm of a `WedgeKind` method, so the text asserted on screen is the text
# the enum returns rather than a copy of it.
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
        fail "$what took ${measured}s, outside the [${lo}s, ${hi}s] the source's own constants bound it to"
        return 1
    fi
}

# The client process view is talking over, which is the thing this run kills:
# on a remote session it is the only child view spawns, and its death is what
# a dropped connection looks like from this side.
read_client_pid() {
    local start el
    start=$(now)
    while :; do
        CLIENT_PID=$(pgrep -P "$VIEW_PID" | head -1 || true)
        if [ -n "$CLIENT_PID" ]; then
            return 0
        fi
        el=$(elapsed "$start" "$(now)")
        if ! in_range "$el" 0 15; then
            fail "view (pid $VIEW_PID) has no client child after ${el}s"
            return 1
        fi
        sleep "$POLL"
    done
}

# Text typed into the buffer and read back off the screen: the whole
# assertion that an engine is not merely running but taking input.
assert_edit_accepted() {
    local token="$1"
    tmux send-keys -t "$SESSION" -l "o$token"
    tmux send-keys -t "$SESSION" Escape
    wait_for "$token" 20 "typed text ($token)" >/dev/null || return 1
}

# Text typed into the buffer and never written to the file, so a later
# reading of it off the screen can only have come back through the remote
# editor's own swap file.
type_unsaved() {
    local token="$1"
    assert_edit_accepted "$token" || return 1
    # `:preserve` rather than a wait on nvim's idle swap flush: the swap is
    # what a reconnect recovers from, and an acceptance that raced the flush
    # would fail for the clock rather than for the recovery
    tmux send-keys -t "$SESSION" -l ':preserve'
    tmux send-keys -t "$SESSION" Enter
}

for tool in tmux awk sed grep pgrep pkill python3; do
    command -v "$tool" >/dev/null 2>&1 || {
        printf 'FAIL: %s is not on PATH; this acceptance drives a real terminal\n' "$tool" >&2
        exit 1
    }
done
[ -x "$VIEW_BIN" ] || {
    printf 'FAIL: no view binary at %s (cargo build --release -p view, or set VIEW_BIN)\n' "$VIEW_BIN" >&2
    exit 1
}
[ -d "$FIXTURE" ] || {
    printf 'FAIL: the no-plugins fixture is missing at %s\n' "$FIXTURE" >&2
    exit 1
}

BACKOFF_BASE=$(const_secs "$PROCESS_RS" REMOTE_RECONNECT_BACKOFF_BASE)
MAX_ATTEMPTS=$(const_int "$PROCESS_RS" REMOTE_RECONNECT_MAX_ATTEMPTS)
RECONNECT_FMT=$(awk '
    index($0, "pub fn notice") { inside = 1 }
    inside && index($0, "{}") && index($0, "\"") { print; exit }
' "$SUPERVISION_RS" | sed -E 's/.*"(.*)".*/\1/')
[ -n "$RECONNECT_FMT" ] || {
    printf 'FAIL: ReconnectProgress::notice no longer formats a banner in %s\n' "$SUPERVISION_RS" >&2
    exit 1
}
DEAD_NOTICE=$(wedge_arm notice Dead)
# nvim's own wording for a swap file it replayed. The engine is pinned, so
# this is as fixed as the constants above, and a reconnect that recovered
# nothing prints something else.
SWAP_REPLAYED='Recovery completed'
FIRST_BANNER=$(banner_for 1)
SECOND_BANNER=$(banner_for 2)

# A dropped connection is resolved off the read side's own EOF rather than by
# waiting out a heartbeat threshold, so the banner is owed on the pass that
# saw the client go. The budget on top is this script's own: the poll
# granularity above, the paint that follows the fold, and the exec of a
# release binary's restart path on a loaded host.
DROP_MAX=3
# Each attempt is owed its full wait, and each measurement is timed from a
# banner observed up to one poll after it was painted -- so the lower bound
# carries exactly that poll and nothing else. It is not slack for an attempt
# that fired early: an attempt that does not wait is the client spin the
# backoff exists to prevent, and the unit suite holds that bound exactly
# against a clock this script does not have.
FIRST_WAIT=$BACKOFF_BASE
SECOND_WAIT=$(times "$BACKOFF_BASE" 2)
ATTEMPT_SLACK=3

printf 'view acceptance: remote reconnect (%s, %s, %sx%s)\n' \
    "${VIEW_BIN#"$REPO_ROOT/"}" "$(nvim --version | head -1)" "$COLS" "$ROWS"

CURRENT_LEG=remote-session
SESSION="view-reconnect-$$"
ROOT=$(mktemp -d "${TMPDIR:-/tmp}/view-reconnect-XXXXXX")
cp -R "$FIXTURE" "$ROOT/xdg_config_home"
mkdir -p "$ROOT/xdg_data_home" "$ROOT/xdg_state_home" "$ROOT/xdg_cache_home" "$ROOT/bin"
# the shipped default recovers a dropped connection without asking, and that
# recovery is the subject here: a run that switched it off would be asserting
# the modal instead
printf 'acceptance seed line\n' >"$ROOT/scratch.txt"
# the stand-in client, armed where view looks for the real one. `ssh` by
# name and on PATH rather than through a flag, because that is how a user's
# own client is found and the lookup is part of what the remote path does
printf '#!/bin/sh\nexec %s "$@"\n' "$FIXTURES/fake-ssh-flaky" >"$ROOT/bin/ssh"
chmod +x "$ROOT/bin/ssh"
# the client is refused this many times before it starts working; armed
# after the session below is up, so the connection it starts with succeeds
printf '0\n' >"$ROOT/refusals"

tmux kill-session -t "$SESSION" 2>/dev/null || true
tmux new-session -d -s "$SESSION" -x "$COLS" -y "$ROWS" \
    "env PATH=$ROOT/bin:$PATH \
         VIEW_STUB_REFUSALS=$ROOT/refusals \
         VIEW_STUB_INNER=$FIXTURES/delay-relay \
         VIEW_COMPAT_SOCK=$ROOT/compat.sock \
         XDG_CONFIG_HOME=$ROOT/xdg_config_home \
         XDG_DATA_HOME=$ROOT/xdg_data_home \
         XDG_STATE_HOME=$ROOT/xdg_state_home \
         XDG_CACHE_HOME=$ROOT/xdg_cache_home \
         TERM=xterm-256color COLORTERM=truecolor \
         $VIEW_BIN --remote view-test-host:$ROOT/scratch.txt"

wait_for 'acceptance seed line' 30 "the remote buffer" >/dev/null
VIEW_PID=$(tmux list-panes -t "$SESSION" -F '#{pane_pid}')
read_client_pid
assert_edit_accepted "ALIVE-$$"
UNSAVED="UNSAVED-$$"
type_unsaved "$UNSAVED"
printf '[1/3] %-34s ... %s  OK\n' 'remote session over the client' \
    "engine alive, edit accepted"

CURRENT_LEG=connection-dropped
printf '%s\n' "$REFUSED_ATTEMPTS" >"$ROOT/refusals"
drop_start=$(now)
kill -9 "$CLIENT_PID"
wait_for "$FIRST_BANNER" 20 "the first reconnect banner" >/dev/null
detected=$(elapsed "$drop_start" "$(now)")
assert_within "$detected" 0 "$DROP_MAX" "the dropped connection"
# the counted banner replaces the bare dead-connection notice rather than
# joining it: view has a recovery running, and saying only that the engine
# is gone would be less than it knows
if pane | grep -qF -- "$DEAD_NOTICE"; then
    fail "the bare dead-connection notice is on screen while a reconnect is running" || exit 1
fi
printf '[2/3] %-34s ... %s  OK\n' 'ssh process killed' \
    "Dead detected at ${detected}s, banner shows \"$FIRST_BANNER\""

CURRENT_LEG=reconnected
first_gap=$(wait_for "$SECOND_BANNER" 20 "the second reconnect banner")
assert_within "$first_gap" "$(minus "$FIRST_WAIT" "$POLL")" \
    "$(plus "$FIRST_WAIT" "$ATTEMPT_SLACK")" "the wait before the first attempt"
second_gap=$(wait_gone "$SECOND_BANNER" 30 "the reconnect banner")
assert_within "$second_gap" "$(minus "$SECOND_WAIT" "$POLL")" \
    "$(plus "$SECOND_WAIT" "$ATTEMPT_SLACK")" "the wait before the second attempt"
# the replacement engine's own report that it replayed the swap, and equally
# the first frame it paints: waiting on it is what keeps the redraw below
# from being typed at an engine still coming up
wait_for "$SWAP_REPLAYED" 30 "the swap-recovery report" >/dev/null
# that report is several lines of engine message and it covers the top of the
# buffer until something redraws over it, which is what the assertion below
# has to read
tmux send-keys -t "$SESSION" C-l
wait_gone "$SWAP_REPLAYED" 15 "the swap-recovery report" >/dev/null
wait_for "$UNSAVED" 30 "the unsaved text after the reconnect" >/dev/null
if grep -qF -- "$UNSAVED" "$ROOT/scratch.txt"; then
    fail "$UNSAVED is in the file on disk, so its return proves a re-read and not a swap recovery" || exit 1
fi
assert_edit_accepted "BACK-$$"
tmux kill-session -t "$SESSION" 2>/dev/null || true
printf '[3/3] %-34s ... %s  OK\n' 'reconnect after the backoff' \
    "reconnect succeeded on attempt $((REFUSED_ATTEMPTS + 1)) (backoff ${first_gap}s, ${second_gap}s), engine alive, unsaved work rehydrated, edit accepted"
