#!/usr/bin/env bash
#
# ACP conformance: an agent session driven end to end through the shipped
# binary -- a real terminal, view's own loop, the pinned engine behind it --
# and observed where a user observes it, on the screen and in the files the
# session wrote.
#
# Every other test of this subsystem drives one seam: a transport test owns
# a session and reads events off a channel, an update test folds one message
# into a model, an oracle case compares a buffer against nvim's own answer.
# None of them run the composition. The loop that reads a key, routes a
# message, drains an effect, hands an RPC to the engine and paints the
# result is the piece with no unit test at all, and a subsystem whose every
# part works while the whole does not is exactly what a user meets first.
#
# Two agents run here, and the split is deliberate:
#
#   * the pinned adapter itself (the `[ai] agent` default, provisioned by
#     the same code path a first-run user gets) drives the session
#     lifecycle, a real streamed turn and a real tool call. It is the only
#     agent that can prove any of that against a real ACP implementation
#     rather than this repo's reading of one -- so what it asserts is that
#     the thing happened and in what order, never what the model said.
#   * the stub agent drives everything a real agent cannot be asked to do
#     on demand: stream a named sequence, hold a tool call non-terminal,
#     propose a specific diff, overlap two permission requests, die
#     mid-turn. It is a real subprocess speaking real JSON-RPC over real
#     pipes -- what it is not is a language model, and a scenario that
#     needs an agent to do one exact thing at one exact moment cannot be
#     obtained from one. That is what buys the exact assertions: the two
#     layers together are "it really happens" plus "it happens exactly so".
#
# Almost every string asserted below is read out of the source that owns it,
# so a reworded row fails here loudly rather than leaving an assertion
# quietly matching nothing. The exceptions are the handful built from the
# wire's own pinned vocabulary (`docs/acp-v1-wire-capture.md`) rather than
# from a `&str` constant, each guarded by a check that the template it slots
# into still exists.
#
# Needs `tmux`, `node` and `npm` for the agent leg, a network for the one
# cold provision, and credentials the pinned adapter can authenticate with
# for the real turn it drives. All three fail loudly rather than skipping:
# an acceptance leg that quietly opts out of the thing it exists to prove is
# worse than one that is not there.
set -euo pipefail

SCRIPT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
REPO_ROOT=$(cd -- "$SCRIPT_DIR/../.." && pwd)
# shellcheck source=scripts/acceptance/artifacts.sh
. "$SCRIPT_DIR/artifacts.sh"
VIEW_BIN=${VIEW_BIN:-$REPO_ROOT/target/release/view}
STUB_BIN=${STUB_BIN:-$REPO_ROOT/target/release/view-ai-stub-agent}
FIXTURE=$REPO_ROOT/compat/fixtures/minimal
PANEL_RS=$REPO_ROOT/crates/view-core/src/native/ai_panel/mod.rs
REVIEW_RS=$REPO_ROOT/crates/view-core/src/native/ai_panel/review.rs
PERMISSION_RS=$REPO_ROOT/crates/view-core/src/native/ai_panel/permission.rs
TRANSCRIPT_RS=$REPO_ROOT/crates/view-core/src/native/ai_panel/transcript.rs

# The panel is a fixed-width column beside the buffer, so widening the
# terminal does not widen it: these are chosen for the buffer and for having
# a review, a transcript and a banner on screen together, and the rows that
# still truncate are asserted on a leading prefix or read out of the log
# instead (each such assertion says so where it stands).
COLS=140
ROWS=44
# How often the screen is read. Charged in full to every measurement, so it
# is small next to the tightest window asserted (2.5s).
POLL=0.25
# How long any single observation is given. Generous: a loaded host must not
# flake, and nothing here is a latency measurement except the one leg that
# says so.
WAIT_SECS=30
# What provisioning is given, which is a download and a dependency install
# on a cold cache.
PROVISION_SECS=180
# The window a frame must render within after the agent process dies. A
# liveness bound, not a paint budget: it says the loop is not blocked on a
# dead agent's pipe, which is what this leg is about, and it is orders of
# magnitude looser than anything in the spec's own §3.1 table -- those are
# measured by `view-bench` against a quiet host, never through tmux.
FRAME_BUDGET_SECS=2.5

SESSIONS=()
ROOTS=()
SESSION=""
ROOT=""
# Assigned once the artifact checks below have passed; declared here so the
# exit trap can clear it even for a run that never got that far.
RESUME_FILE=""
CURRENT_LEG=startup
DUMP_DIR=$(mktemp -d "${TMPDIR:-/tmp}/view-ai-conformance-XXXXXX")

cleanup() {
    local code=$?
    local session root
    for session in ${SESSIONS[@]+"${SESSIONS[@]}"}; do
        tmux kill-session -t "$session" 2>/dev/null || true
    done
    # `tmux kill-session` returns before the pane's own process is gone, and
    # a removal that overtakes a still-live `view` walks past directories it
    # then re-creates -- which is how a "cleaned up" root survives the run
    # holding an empty state directory.
    for _ in 1 2 3 4 5 6 7 8 9 10; do
        pgrep -f "$VIEW_BIN" >/dev/null 2>&1 || break
        sleep 0.2
    done
    rm -f "$RESUME_FILE"
    for root in ${ROOTS[@]+"${ROOTS[@]}"}; do
        [ -n "$root" ] && rm -rf "$root"
    done
    if [ "$code" -eq 0 ] || [ -z "$(ls -A "$DUMP_DIR" 2>/dev/null)" ]; then
        rm -rf "$DUMP_DIR"
    else
        printf '      pane dumps kept in %s\n' "$DUMP_DIR" >&2
    fi
    exit "$code"
}
trap cleanup EXIT INT TERM

now() { date +%s.%N; }
elapsed() { awk -v a="$1" -v b="$2" 'BEGIN { printf "%.2f", b - a }'; }
under() { awk -v v="$1" -v hi="$2" 'BEGIN { exit !(v <= hi) }'; }

pane() { tmux capture-pane -t "$SESSION" -p 2>/dev/null || true; }

fail() {
    local dump="$DUMP_DIR/$CURRENT_LEG.pane"
    pane >"$dump" 2>/dev/null || true
    printf 'FAIL [%s]: %s\n' "$CURRENT_LEG" "$1" >&2
    printf '      pane dump: %s\n' "$dump" >&2
    if [ -n "$ROOT" ] && [ -f "$ROOT/view.log" ]; then
        cp "$ROOT/view.log" "$DUMP_DIR/$CURRENT_LEG.log" 2>/dev/null || true
        printf '      view log: %s\n' "$DUMP_DIR/$CURRENT_LEG.log" >&2
    fi
    return 1
}

pass() { printf 'ok   [%s] %s\n' "$CURRENT_LEG" "$1"; }

# The `&str` constant `name` holds, read from the file that owns it.
const_str() {
    local file="$1" name="$2" value
    value=$(grep -oE "const $name: &str = \"[^\"]+\"" "$file" | sed -E 's/.*"(.*)"/\1/')
    if [ -z "$value" ]; then
        printf 'FAIL: %s is not a &str constant in %s any more\n' "$name" "$file" >&2
        return 1
    fi
    printf '%s' "$value"
}

# The label one `ToolCallStatus` arm renders as, read from the match that
# owns it: the status vocabulary is the wire's, and the words on screen are
# this file's translation of it.
status_label() {
    local variant="$1" value
    value=$(grep -oE "ToolCallStatus::$variant => \"[a-z_]+\"" "$TRANSCRIPT_RS" |
        sed -E 's/.*"(.*)"/\1/')
    if [ -z "$value" ]; then
        printf 'FAIL: ToolCallStatus::%s has no rendered label in %s any more\n' \
            "$variant" "$TRANSCRIPT_RS" >&2
        return 1
    fi
    printf '%s' "$value"
}

# The literal `text` in `file`, as a check that a string this script builds
# from the wire's own vocabulary still has a template in the source to slot
# into. Prints nothing; fails loudly when the template is gone.
require_template() {
    local file="$1" text="$2"
    if ! grep -qF -- "$text" "$file"; then
        printf 'FAIL: %s no longer builds its rows from the template %s\n' "$file" "$text" >&2
        return 1
    fi
}

# Waits for `text` to appear on screen, and reports how long it took.
wait_for() {
    local text="$1" budget="$2" what="$3" start el
    start=$(now)
    while :; do
        if pane | grep -qF -- "$text"; then
            elapsed "$start" "$(now)"
            return 0
        fi
        el=$(elapsed "$start" "$(now)")
        if ! under "$el" "$budget"; then
            fail "$what did not appear within ${budget}s (looked for '$text')"
            return 1
        fi
        sleep "$POLL"
    done
}

# The same, for a line in view's own diagnostic log: the events a message
# carries past the loop are not all things the screen shows, and a claim
# about one that reads only the screen is a claim about the renderer.
wait_for_log() {
    local pattern="$1" budget="$2" what="$3" start el
    start=$(now)
    while :; do
        if grep -qE -- "$pattern" "$ROOT/view.log" 2>/dev/null; then
            elapsed "$start" "$(now)"
            return 0
        fi
        el=$(elapsed "$start" "$(now)")
        if ! under "$el" "$budget"; then
            fail "$what was never logged within ${budget}s (looked for /$pattern/)"
            return 1
        fi
        sleep "$POLL"
    done
}

# The mirror of `wait_for`: waits for `text` to stop being on screen. What
# synchronises a keystroke against the state change it asks for, when the
# thing that proves the change is a row leaving rather than arriving.
until_gone() {
    local text="$1" budget="$2" what="$3" start el
    start=$(now)
    while :; do
        if ! pane | grep -qF -- "$text"; then
            elapsed "$start" "$(now)"
            return 0
        fi
        el=$(elapsed "$start" "$(now)")
        if ! under "$el" "$budget"; then
            fail "$what did not happen within ${budget}s ('$text' is still on screen)"
            return 1
        fi
        sleep "$POLL"
    done
}

refute() {
    local text="$1" what="$2"
    if pane | grep -qF -- "$text"; then
        fail "$what (found '$text' on screen)"
        return 1
    fi
}

send_text() { tmux send-keys -t "$SESSION" -l -- "$1"; }
send_key() { tmux send-keys -t "$SESSION" "$1"; }

# One session against `agent`, which is either the literal `default` (the
# `[ai]` table left out entirely, so the pinned adapter is provisioned the
# way a first-run user gets it) or a command line for the `[ai] agent` key.
#
# `cache` names the XDG cache directory, so the legs that provision nothing
# still get a private one while the leg that does can be handed a directory
# that survives between legs.
start_session() {
    local tag="$1" agent="$2" cache="$3"
    SESSION="view-ai-conf-$$-$tag"
    ROOT=$(mktemp -d "${TMPDIR:-/tmp}/view-ai-conf-$tag-XXXXXX")
    ROOTS+=("$ROOT")
    ROOTS+=("$cache")
    cp -R "$FIXTURE" "$ROOT/xdg_config_home"
    mkdir -p "$ROOT/xdg_data_home" "$ROOT/xdg_state_home" "$cache"
    if [ "$agent" != "default" ]; then
        printf '\n[ai]\nagent = %s\n' "$agent" >>"$ROOT/xdg_config_home/view/view.toml"
    fi
    # The file every proposal leg offers edits to, seeded with what the stub
    # agent's own `oldText` claims it holds.
    printf 'alpha\nbeta\ngamma\n' >"$ROOT/view-ai-stub-diff.txt"

    tmux kill-session -t "$SESSION" 2>/dev/null || true
    SESSIONS+=("$SESSION")
    # started in $ROOT: the working directory is what view takes as the
    # project root, which is both the directory it asks to trust and the one
    # the agent is spawned in
    tmux new-session -d -s "$SESSION" -x "$COLS" -y "$ROWS" -c "$ROOT" \
        "env VIEW_COMPAT_SOCK=$ROOT/compat.sock \
             XDG_CONFIG_HOME=$ROOT/xdg_config_home \
             XDG_DATA_HOME=$ROOT/xdg_data_home \
             XDG_STATE_HOME=$ROOT/xdg_state_home \
             XDG_CACHE_HOME=$cache \
             VIEW_LOG=$ROOT/view.log \
             TERM=xterm-256color COLORTERM=truecolor \
             $VIEW_BIN $ROOT/view-ai-stub-diff.txt"

    wait_for 'alpha' "$WAIT_SECS" "the seeded buffer" >/dev/null
}

# Opens the panel and answers the trust gate, leaving the panel entered --
# the state every prompt below is typed into.
open_panel() {
    local budget="$1"
    send_text ':View ai open'
    send_key Enter
    wait_for "$TRUST_PROMPT" "$WAIT_SECS" "the project trust prompt" >/dev/null
    send_text 'y'
    wait_for "$FOCUSED_TITLE" "$budget" "the entered agent panel" >/dev/null
}

submit() {
    send_text "$1"
    send_key Enter
}

# Leaves the panel, writes the buffer, and compares the file byte for byte
# with what was expected -- the diff review's contract is a byte-exact
# buffer mutation, and a screen that looks right is not that claim.
#
# Written and re-read until it settles rather than once: an accept reaches
# the buffer over RPC, and a `:w` dispatched in the same breath as the
# keystroke can win that race on a loaded host and write the file the accept
# had not landed in yet. A wrong answer that never settles still fails, at
# the budget, with the bytes it actually found.
assert_file_is() {
    local expected="$1" what="$2" actual start
    send_key Escape
    start=$(now)
    while :; do
        send_text ':w'
        send_key Enter
        sleep "$POLL"
        actual=$(cat "$ROOT/view-ai-stub-diff.txt")
        [ "$actual" = "$expected" ] && break
        if ! under "$(elapsed "$start" "$(now)")" "$WAIT_SECS"; then
            fail "$what -- the file holds $(printf '%q' "$actual"), expected $(printf '%q' "$expected")"
            return 1
        fi
    done
    send_text ':View ai focus'
    send_key Enter
    wait_for "$FOCUSED_TITLE" "$WAIT_SECS" "the panel re-entered after the write" >/dev/null
}

leg_session_lifecycle() {
    CURRENT_LEG=1-session-lifecycle
    local took chunks
    start_session lifecycle default "$ADAPTER_CACHE"
    open_panel "$WAIT_SECS"
    # A session starts on the first command, never on the panel opening, so
    # the lifecycle is driven the way a user drives it: by asking the agent
    # something. The question names a file that is not the open buffer, so
    # it cannot be answered out of the context view assembles and sends
    # along with every prompt (`view_ai::context::assemble`) -- answering it
    # takes a real tool call -- and it asks for prose rather than a word, so
    # the answer is long enough to arrive in more than one chunk.
    printf 'The mailbox key lives in the blue tin on the third shelf.\n' \
        >"$ROOT/notes.txt"
    submit 'Read notes.txt in this directory and tell me, in two full sentences, where the mailbox key is and what it is kept in.'
    # Before anything else: the wait itself is announced. A first run that
    # downloads and installs an agent while the panel sits silent is
    # indistinguishable from a feature that does not work.
    wait_for "$PROVISION_NOTICE" "$WAIT_SECS" "the first-run provisioning notice" >/dev/null
    # Provisioning (download, verify, install from the pinned lockfile)
    # happens on the way to the handshake, so this budget carries it.
    took=$(wait_for_log 'ai SessionReady' "$PROVISION_SECS" \
        "the pinned adapter's session")
    if ! find "$ADAPTER_CACHE" -maxdepth 8 -type d -name node_modules | grep -q .; then
        fail 'the adapter was launched without the dependencies its entry script imports'
        return 1
    fi
    if ! find "$ADAPTER_CACHE" -maxdepth 4 -type d -name "*$PINNED_VERSION*" | grep -q .; then
        fail "the provisioned adapter is not the pinned $PINNED_VERSION"
        return 1
    fi
    pass "the pinned adapter $PINNED_VERSION reached session/new in ${took}s"

    # Everything below asserts occurrences and their order, never content: a
    # model's words are its own, and a leg that pinned them would be a test
    # of the model. The exact rendering of each of these is leg 2's subject,
    # against an agent that can be told what to send.
    wait_for_log 'ai TurnEnded' "$PROVISION_SECS" "the real agent's turn ending" >/dev/null
    # Rendered incrementally means more than one chunk crossed the loop and
    # was folded into the transcript, each one repainting -- a turn
    # delivered whole at its end logs exactly one.
    chunks=$(grep -cE 'ai MessageChunk .*from_agent: true' "$ROOT/view.log" || true)
    if [ "${chunks:-0}" -lt 2 ]; then
        fail "the real agent's reply arrived in $chunks chunk(s); a streamed turn is more than one"
        return 1
    fi
    if ! wait_for "$AGENT_PREFIX" "$WAIT_SECS" "the real agent's reply in the panel" >/dev/null; then
        return 1
    fi
    pass "a real streamed reply reached the panel in $chunks chunks"

    assert_tool_call_went_non_terminal_then_terminal || return 1
    pass 'a real tool call was observed non-terminal, then terminal'
    tmux kill-session -t "$SESSION" 2>/dev/null || true
}

# The status sequence one real tool call was seen in. Read from the log
# rather than the screen because it is the order that is being asserted, and
# the log's own line order is arrival order -- a screen poll could only ever
# say which statuses were reachable, never which came first. The vocabulary
# is the wire's (`docs/acp-v1-wire-capture.md` pins
# `pending`/`in_progress`/`completed`/`failed`); which call it belongs to and
# what it was called are the agent's business, not this leg's.
assert_tool_call_went_non_terminal_then_terminal() {
    local statuses first_terminal first_non_terminal
    statuses=$(grep -oE 'ai ToolCallUpdate .*status: [A-Za-z]+' "$ROOT/view.log" |
        grep -oE '[A-Za-z]+$' || true)
    if [ -z "$statuses" ]; then
        fail 'the real agent made no tool call at all'
        return 1
    fi
    first_non_terminal=$(printf '%s\n' "$statuses" | grep -nE '^(Pending|InProgress)$' |
        head -1 | cut -d: -f1)
    first_terminal=$(printf '%s\n' "$statuses" | grep -nE '^(Completed|Failed)$' |
        head -1 | cut -d: -f1)
    if [ -z "$first_non_terminal" ] || [ -z "$first_terminal" ]; then
        fail "a real tool call was never seen both non-terminal and terminal (saw: $(printf '%s' "$statuses" | tr '\n' ' '))"
        return 1
    fi
    if [ "$first_non_terminal" -ge "$first_terminal" ]; then
        fail "the tool call reached a terminal status before a non-terminal one (saw: $(printf '%s' "$statuses" | tr '\n' ' '))"
        return 1
    fi
}

leg_streaming_and_tool_status() {
    CURRENT_LEG=2-streaming-and-tool-status
    local resume
    start_session stream "$STUB_ARGV" "$(mktemp -d)"
    resume=$RESUME_FILE
    rm -f "$resume"
    open_panel "$WAIT_SECS"

    submit 'tool-call'
    # Mid-turn: the message is half written and the call is not finished.
    # Both are rendered before anything ends, which is what "streams" means
    # for a panel -- a turn rendered only at its end shows neither.
    wait_for "${AGENT_PREFIX}streaming" "$WAIT_SECS" \
        "the first half of the streamed message" >/dev/null
    wait_for "$RUNNING_LABEL: Probe the file" "$WAIT_SECS" \
        "the tool call's non-terminal status" >/dev/null
    refute "${AGENT_PREFIX}streaming and done" \
        "the turn's second half rendered before it was sent"

    touch "$resume"
    wait_for "${AGENT_PREFIX}streaming and done" "$WAIT_SECS" \
        "the streamed message growing in place" >/dev/null
    wait_for "$DONE_LABEL: Probe the file" "$WAIT_SECS" \
        "the tool call's terminal status" >/dev/null
    pass 'a turn rendered incrementally, its tool call non-terminal then terminal'

    # The wire's remaining update kinds, in one turn, through the same
    # loop. Asserted on the rows they render as rather than on message
    # text: chunks sharing a message id fold into one growing entry (the
    # wire's own "a change in messageId indicates a new message"), so the
    # later ones are inside a row the panel's width has already truncated.
    submit 'stream'
    wait_for "$DONE_LABEL: Read a.rs" "$WAIT_SECS" "the streamed tool call" >/dev/null
    wait_for "$TERMINAL_CONTENT" "$WAIT_SECS" "the streamed terminal content" >/dev/null
    wait_for "$PLAN_PREFIX" "$WAIT_SECS" "the streamed plan" >/dev/null
    pass 'every streamed update kind reached the panel'
    tmux kill-session -t "$SESSION" 2>/dev/null || true
}

leg_diff_accept_and_reject() {
    CURRENT_LEG=3-diff-accept-and-reject
    start_session diff "$STUB_ARGV" "$(mktemp -d)"
    open_panel "$WAIT_SECS"

    submit 'propose'
    wait_for "$REVIEW_KEY_HINT" "$WAIT_SECS" "the diff review's keys" >/dev/null
    wait_for '+BETA' "$WAIT_SECS" "the proposed hunk" >/dev/null
    send_text 'a'
    # The review closing on its last open hunk, not the `+BETA` row the line
    # above already proved is on screen -- that string is what the proposal
    # renders as either way, so waiting on it would return on its first poll
    # whether or not the accept did anything, and would leave the write
    # below racing the RPC that carries the accept into the buffer.
    until_gone "$REVIEW_KEY_HINT" "$WAIT_SECS" "the review closing on the accept" >/dev/null
    assert_file_is 'alpha
BETA
gamma' 'the accepted hunk was not written byte for byte'
    pass 'a proposal accepted through the review reached the buffer byte-exactly'

    submit 'propose2'
    wait_for "$REVIEW_KEY_HINT" "$WAIT_SECS" "the second diff review's keys" >/dev/null
    wait_for '+GAMMA' "$WAIT_SECS" "the second proposed hunk" >/dev/null
    send_text 'x'
    until_gone "$REVIEW_KEY_HINT" "$WAIT_SECS" "the review closing on the reject" >/dev/null
    assert_file_is 'alpha
BETA
gamma' 'a rejected hunk changed the buffer'
    refute 'GAMMA' 'a rejected hunk reached the buffer'
    pass 'a rejected proposal left the buffer untouched'
    tmux kill-session -t "$SESSION" 2>/dev/null || true
}

leg_cancel_mid_turn() {
    CURRENT_LEG=4-cancel-mid-turn
    start_session cancel "$STUB_ARGV" "$(mktemp -d)"
    open_panel "$WAIT_SECS"

    submit 'ask'
    wait_for "$PERMISSION_PROMPT" "$WAIT_SECS" "the agent's permission request" >/dev/null
    send_key C-c
    # Half one, read back from the agent itself: what it was handed for the
    # request it was holding. The wire's own word, not an option it offered.
    wait_for "${AGENT_PREFIX}chose cancelled" "$WAIT_SECS" \
        "the pending permission settled as cancelled" >/dev/null
    # Half two: the original prompt's own promise. Nothing on screen carries
    # a stop reason, so this is read where the loop recorded it.
    wait_for_log 'ai TurnEnded \{ stop_reason: Cancelled \}' "$WAIT_SECS" \
        "the cancelled turn" >/dev/null
    # ... and the user-visible consequence of both: the question the agent
    # is no longer waiting on an answer to is off the screen. A regression
    # that settled the wire correctly and left a dead prompt pinned there
    # forever would satisfy every assertion above this one.
    refute "$PERMISSION_PROMPT" 'the cancelled permission prompt stayed on screen'
    pass 'both halves of the cancellation contract, driven from the keyboard'
    tmux kill-session -t "$SESSION" 2>/dev/null || true
}

leg_agent_crash() {
    CURRENT_LEG=5-agent-crash
    local start took
    start_session crash "$STUB_ARGV" "$(mktemp -d)"
    open_panel "$WAIT_SECS"

    submit 'die'
    # In the panel and nowhere else: the notice is a row of the panel the
    # session belongs to, not a modal over the buffer and not a toast that
    # ages out of a long-running session unseen.
    wait_for_log 'ai SessionCrashed' "$WAIT_SECS" "the agent's death" >/dev/null
    wait_for "$CRASH_PREFIX" "$WAIT_SECS" "the panel-local crash notice" >/dev/null

    # Immediately after the crash, with no dismissal and no recovery in
    # between: a keystroke reaches the buffer and its frame renders. A loop
    # blocked on a dead agent's pipe fails here instead of hanging the run.
    send_key Escape
    send_text 'Gopaint-after-crash'
    send_key Escape
    start=$(now)
    took=$(wait_for 'paint-after-crash' "$FRAME_BUDGET_SECS" \
        "a frame after the agent died")
    if ! under "$took" "$FRAME_BUDGET_SECS"; then
        fail "the first frame after the crash took ${took}s, over ${FRAME_BUDGET_SECS}s"
        return 1
    fi
    pass "a frame rendered ${took}s after the crash, banner still up"

    # Which surface the notice is on, checked by acting on it rather than by
    # noting the absence of some other overlay: the panel's own banner is
    # what `<C-d>` inside the panel clears, and nothing modal answers that
    # key at all.
    send_text ':View ai focus'
    send_key Enter
    wait_for "$FOCUSED_TITLE" "$WAIT_SECS" "the panel re-entered over the banner" >/dev/null
    send_key C-d
    sleep "$POLL"
    refute "$CRASH_PREFIX" 'the crash notice did not answer the panel-local dismiss key'
    pass 'the crash notice was the panel-local banner, dismissed from inside the panel'
    tmux kill-session -t "$SESSION" 2>/dev/null || true
}

leg_permission_overlap() {
    CURRENT_LEG=6-permission-overlap
    start_session overlap "$STUB_ARGV" "$(mktemp -d)"
    open_panel "$WAIT_SECS"

    submit 'ask-twice'
    wait_for "$PERMISSION_PROMPT call_001" "$WAIT_SECS" \
        "the first permission request" >/dev/null
    # The second request is answered rather than left hanging, and answered
    # with an outcome rather than an error -- the agent reports the word it
    # was handed, so a reply shape it could not read would name a code here.
    wait_for "${AGENT_PREFIX}overlap cancelled" "$WAIT_SECS" \
        "the overlapping request's reply" >/dev/null
    # ... and the first request is still the one on screen, unanswered.
    wait_for "$PERMISSION_PROMPT call_001" "$WAIT_SECS" \
        "the first request after the overlap" >/dev/null
    refute "$PERMISSION_PROMPT call_002" 'the overlapping request displaced the first'

    # The session survives the degrade: answering the first still ends the
    # turn, which is what "tolerates the reply shape" has to mean.
    send_text 'y'
    # Read from the log, not the screen: chunks sharing a message id fold
    # into one transcript entry, so this answer lands inside the row the
    # overlap report already opened rather than starting one of its own.
    wait_for_log 'ai MessageChunk .* text: "chose allow-once"' "$WAIT_SECS" \
        "the first request's own answer" >/dev/null
    wait_for_log 'ai TurnEnded' "$WAIT_SECS" "the turn ending after the overlap" >/dev/null
    pass 'an overlapping permission request was answered without disturbing the first'
    tmux kill-session -t "$SESSION" 2>/dev/null || true
}

command -v tmux >/dev/null || {
    printf 'FAIL: tmux is required (this drives a real terminal session)\n' >&2
    exit 1
}
ensure_artifact "$VIEW_BIN" "$REPO_ROOT/target/release/view" \
    cargo build --release -p view || exit 1
ensure_artifact "$STUB_BIN" "$REPO_ROOT/target/release/view-ai-stub-agent" \
    cargo build --release -p view-ai --features test-support --bin view-ai-stub-agent || exit 1
[ -d "$FIXTURE" ] || {
    printf 'FAIL: the no-plugins fixture is missing at %s\n' "$FIXTURE" >&2
    exit 1
}

FOCUSED_TITLE=$(const_str "$PANEL_RS" FOCUSED_TITLE)
# Truncated deliberately: the panel is a column beside the buffer and the
# hint row is wider than it, so the full constant is never on screen. The
# leading run of it still fails loudly if the keys are reworded.
REVIEW_KEY_HINT=$(const_str "$REVIEW_RS" KEY_HINT)
REVIEW_KEY_HINT=${REVIEW_KEY_HINT:0:20}
RUNNING_LABEL=$(status_label InProgress)
DONE_LABEL=$(status_label Completed)
PERMISSION_PROMPT=$(grep -oE 'format!\("Permission requested for' "$PERMISSION_RS" |
    sed -E 's/.*"(.*)/\1/')
[ -n "$PERMISSION_PROMPT" ] || {
    printf 'FAIL: the permission prompt is not built from a literal in %s any more\n' \
        "$PERMISSION_RS" >&2
    exit 1
}
# How the transcript labels the agent's own side of the conversation, and
# the separator it joins to -- one row's whole prefix, from the two places
# that own its halves.
AGENT_PREFIX=$(grep -oE 'TranscriptRole::Agent => "[A-Za-z]+"' "$TRANSCRIPT_RS" |
    sed -E 's/.*"(.*)"/\1/')
[ -n "$AGENT_PREFIX" ] || {
    printf 'FAIL: TranscriptRole::Agent has no rendered label in %s any more\n' \
        "$TRANSCRIPT_RS" >&2
    exit 1
}
require_template "$TRANSCRIPT_RS" '"{prefix}: {}"' || exit 1
AGENT_PREFIX="$AGENT_PREFIX: "
# A plan row's own opening, and a placeholder for a content kind the panel
# shows a label for rather than the content itself. Both are built from the
# wire's pinned vocabulary (`docs/acp-v1-wire-capture.md`'s `Plan` and
# `Terminal` pins) slotted into a template that lives in the source, so the
# template is what is checked and the wire word is what is substituted.
require_template "$TRANSCRIPT_RS" '"plan [{status}, {priority}]: {}"' || exit 1
PLAN_PREFIX='plan ['
require_template "$REPO_ROOT/crates/view-ai/src/acp/driver.rs" '"[{kind} content]"' || exit 1
TERMINAL_CONTENT='[terminal content]'
# The crash banner's own opening, and the trust prompt's -- both `format!`
# literals rather than `&str` constants, so both are read to their first
# substitution.
CRASH_PREFIX=$(grep -oE '"Error: \{message\}' "$PANEL_RS" | sed -E 's/"(.*)\{message\}/\1/')
[ -n "$CRASH_PREFIX" ] || {
    printf 'FAIL: the crash banner is not built from a literal in %s any more\n' "$PANEL_RS" >&2
    exit 1
}
TRUST_PROMPT=$(grep -oE '"Trust \{\}' "$REPO_ROOT/crates/view-core/src/update/ai.rs" |
    sed -E 's/"(.*)\{\}/\1/')
[ -n "$TRUST_PROMPT" ] || {
    printf 'FAIL: the AI trust prompt is not built from a literal any more\n' >&2
    exit 1
}
# How the first-run provisioning wait announces itself.
PROVISION_NOTICE=$(const_str "$REPO_ROOT/crates/view/src/ai_worker.rs" PROVISION_NOTICE_PREFIX)

# The stub agent's first argument is the file whose appearance releases a
# held turn; one path serves every leg because no two sessions overlap.
PINNED_VERSION=$(grep -A 2 'id: "claude-code"' "$REPO_ROOT/crates/view-ai/src/provision.rs" |
    grep -oE 'version: "[^"]+"' | sed -E 's/.*"(.*)"/\1/')
[ -n "$PINNED_VERSION" ] || {
    printf 'FAIL: the claude-code row has no pinned version in provision.rs any more\n' >&2
    exit 1
}

RESUME_FILE=$(mktemp -u "${TMPDIR:-/tmp}/view-ai-conf-resume-$$-XXXXXX")
STUB_ARGV="[\"$STUB_BIN\", \"$RESUME_FILE\"]"
# Shared across the run so the pinned adapter is downloaded and installed
# once, not once per leg -- the only leg that provisions anything is the
# first, and a private cache per leg would only slow a re-run down.
ADAPTER_CACHE=$(mktemp -d "${TMPDIR:-/tmp}/view-ai-conf-cache-XXXXXX")
ROOTS+=("$ADAPTER_CACHE")

# `ai-conformance.sh 3 5` runs those legs alone. Every leg builds its own
# session from nothing, so any subset is a run in its own right -- which is
# what makes reverting one task and re-running the one leg that covers it a
# practical way to check that the leg is really the thing being asserted.
LEGS=(leg_session_lifecycle leg_streaming_and_tool_status leg_diff_accept_and_reject
    leg_cancel_mid_turn leg_agent_crash leg_permission_overlap)
if [ "$#" -eq 0 ]; then
    selected=("${LEGS[@]}")
else
    selected=()
    for want in "$@"; do
        leg=${LEGS[$((want - 1))]:-}
        [ -n "$leg" ] || {
            printf 'FAIL: there is no leg %s (1..%s)\n' "$want" "${#LEGS[@]}" >&2
            exit 1
        }
        selected+=("$leg")
    done
fi
for leg in "${selected[@]}"; do "$leg"; done

printf 'ai conformance: %s of %s legs green\n' "${#selected[@]}" "${#LEGS[@]}"
