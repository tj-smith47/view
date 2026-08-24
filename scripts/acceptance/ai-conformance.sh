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
#     propose a specific diff, overlap two permission requests, ask for a
#     file it may not have, die mid-turn. It is a real subprocess speaking
#     real JSON-RPC over real
#     pipes -- what it is not is a language model, and a scenario that
#     needs an agent to do one exact thing at one exact moment cannot be
#     obtained from one. That is what buys the exact assertions: the two
#     layers together are "it really happens" plus "it happens exactly so".
#
# Almost every string asserted below is read out of the source that owns it,
# so a reworded row fails here loudly rather than leaving an assertion
# quietly matching nothing. Two kinds of exception remain: strings built
# from the wire's own pinned vocabulary (`docs/acp-v1-wire-capture.md`),
# each guarded by a check that the template it slots into still exists;
# and the `ai TurnEnded` and `ai UsageUpdated` log lines, which render
# through vlog's derive catch-all and so have no template to guard -- a
# rename there fails these waits loudly at their own timeout, which is the
# check they get.
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
STUB_RS=$REPO_ROOT/crates/view-ai/tests/fixtures/stub_agent.rs
MAPPINGS_RS=$REPO_ROOT/crates/view-core/src/native/mappings.rs
NVIM_API_RS=$REPO_ROOT/crates/view-engine/src/nvim_api.rs

# The panel is a fixed-width column beside the buffer, so widening the
# terminal does not widen it: these are chosen for the buffer and for having
# a review, a transcript and a banner on screen together, and the rows that
# still truncate are asserted on a leading prefix or read out of the log
# instead (each such assertion says so where it stands).
COLS=140
ROWS=44
# The width the review's own way out has to survive: nvim's grid keeps its
# full width under the panel, so the panel covers the right 30% of every
# header row, and this is the narrowest terminal the header is asserted
# readable end to end in.
NARROW_COLS=120
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
DUMP_DIR=$(dump_dir view-ai-conformance)

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

elapsed() { awk -v a="$1" -v b="$2" 'BEGIN { printf "%.2f", b - a }'; }
under() { awk -v v="$1" -v hi="$2" 'BEGIN { exit !(v <= hi) }'; }

pane() { tmux capture-pane -t "$SESSION" -p 2>/dev/null || true; }

# The pane with the panel column cut away: on every row, everything left of
# the first vertical border.
#
# A review is drawn by nvim as extmarks in the buffer it proposes edits to,
# so its rows have to be found where the file is and nowhere else. Searching
# the whole pane would also be answered by a panel that rendered its own copy
# of the diff -- which is exactly the surface this subsystem deleted, and
# exactly what a regression would bring back.
#
# `|| true` for the same reason `pane` has one: a reader feeding
# `grep -q` is killed by the pipe the moment the match is found, and under
# `pipefail` that death would be reported as "no match" by every caller.
buffer_region() { pane | sed 's/│.*//' || true; }

# Fails unless a framed panel really is on screen, so that `buffer_region`
# is cutting something off rather than passing the whole pane through and
# leaving every in-buffer assertion answered from anywhere.
assert_panel_is_framed() {
    if ! pane | grep -q '│'; then
        fail 'no panel border is on screen, so the buffer region is the whole pane and an in-buffer assertion would prove nothing about where the review is drawn'
        return 1
    fi
}

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

# The glyph one transcript marker renders as, read from the `const` that
# owns it and decoded out of Rust's `\u{...}` escape: the panel says who
# spoke and how a call went in a marker rather than a word, so the marker is
# what this script has to look for on screen.
mark_str() {
    local name="$1" value
    value=$(grep -oE "^const $name: &str = \"[^\"]+\"" "$TRANSCRIPT_RS" |
        sed -E 's/.*"(.*)"/\1/')
    if [ -z "$value" ]; then
        printf 'FAIL: %s is not a marker constant in %s any more\n' \
            "$name" "$TRANSCRIPT_RS" >&2
        return 1
    fi
    printf '%b' "$(printf '%s' "$value" | sed -E 's/\\u\{([0-9a-fA-F]+)\}/\\u\1/g')"
}

# The marker one status arm renders as, joined from the match arm that
# names a constant and the constant that holds the glyph -- so a reworded
# arm or a changed glyph both fail here rather than quietly matching
# nothing.
arm_mark() {
    local arm="$1" name
    name=$(grep -oE "$arm => \(?[A-Z_]+" "$TRANSCRIPT_RS" |
        sed -E 's/.*[( ]([A-Z_]+)$/\1/')
    if [ -z "$name" ]; then
        printf 'FAIL: %s renders no marker in %s any more\n' \
            "$arm" "$TRANSCRIPT_RS" >&2
        return 1
    fi
    mark_str "$name"
}

status_mark() {
    arm_mark "ToolCallStatus::$1"
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

# Waits for `text` to appear in what `reader` prints, and reports how long
# it took. `where` names the region for the failure message.
wait_in() {
    local reader="$1" where="$2" text="$3" budget="$4" what="$5" start el
    start=$(now)
    while :; do
        if "$reader" | grep -qF -- "$text"; then
            elapsed "$start" "$(now)"
            return 0
        fi
        el=$(elapsed "$start" "$(now)")
        if ! under "$el" "$budget"; then
            fail "$what did not appear $where within ${budget}s (looked for '$text')"
            return 1
        fi
        sleep "$POLL"
    done
}

# Waits for `text` to appear on screen, and reports how long it took.
wait_for() { wait_in pane 'on screen' "$@"; }

# The same, restricted to the file's own region of the screen.
wait_in_buffer() { wait_in buffer_region 'in the buffer region' "$@"; }

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

# `wait_for` against a pattern rather than a literal, for the one row whose
# text depends on when it was read: an unresolved tool call's marker is
# whichever spinner frame the tick has reached.
wait_for_re() {
    local pattern="$1" budget="$2" what="$3" start el
    start=$(now)
    while :; do
        if pane | grep -qE -- "$pattern"; then
            elapsed "$start" "$(now)"
            return 0
        fi
        el=$(elapsed "$start" "$(now)")
        if ! under "$el" "$budget"; then
            fail "$what did not appear within ${budget}s (looked for /$pattern/)"
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

# One review verb through its `:View` form -- the way in that exists
# whatever happened to the keys, and the one a user reaches for when
# `<leader>h` is already theirs. A separate assertion from the keys below,
# never a substitute for them: the two reach the same dispatch by different
# routes, and only the keys prove the mappings the decoration installs are
# on the buffer and answer.
review_verb() {
    send_text ":View review $1"
    send_key Enter
}

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
    local took chunks prompt echoed tail_word
    start_session lifecycle default "$ADAPTER_CACHE"
    open_panel "$WAIT_SECS"
    # A session starts on the first command, never on the panel opening, so
    # the lifecycle is driven the way a user drives it: by asking the agent
    # something. The question names a file that is not the open buffer, so
    # it cannot be answered out of the context view assembles and sends
    # along with every prompt (`view_ai::context::assemble`) -- answering it
    # takes a real tool call -- and it asks for a long answer rather than a
    # short one: the adapter forwards whatever its stream has buffered when
    # it flushes, so a reply the model finishes inside one flush arrives
    # whole (observed: a two-sentence answer once arrived in a single
    # chunk), while one that takes seconds to generate cannot.
    printf 'The mailbox key lives in the blue tin on the third shelf.\n' \
        >"$ROOT/notes.txt"
    prompt='Read notes.txt in this directory, then write me a detailed answer of at least eight full sentences that says where the mailbox key is, what container it is kept in, which shelf that container sits on, and how someone unfamiliar with the house would go about finding it.'
    submit "$prompt"
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
    # delivered whole at its end logs exactly one. The line shape this reads
    # is checked against `vlog.rs`'s own template at startup.
    chunks=$(grep -cE 'ai MessageChunk .*from_agent: true' "$ROOT/view.log" || true)
    if [ "${chunks:-0}" -lt 2 ]; then
        fail "the real agent's reply arrived in $chunks chunk(s); a streamed turn is more than one"
        return 1
    fi
    # A word of the agent's own last chunks, read out of the log rather
    # than chosen here -- a model's words are its own, and this leg pins
    # none of them; it only asks that whatever arrived is what is drawn.
    # Words the prompt already used are dropped, so the echo of the
    # question cannot answer for the reply.
    #
    # The marker alone cannot: the panel follows the tail, and a reply
    # hundreds of rows long has scrolled its own opening row -- marker and
    # all -- off the top by the time it finishes. That is the panel doing
    # exactly what a transcript should. So the needle is any row of the
    # entry, and the marker is one of them, for the short reply that still
    # fits whole.
    echoed=$(printf '%s' "$prompt" | grep -oE '[A-Za-z]{5,}' | sort -u)
    tail_word=$(grep -E 'ai MessageChunk .* from_agent: true' "$ROOT/view.log" |
        tail -2 | sed -E 's/.*text: "(.*)".*/\1/' | grep -oE '[A-Za-z]{5,}' |
        grep -vxF "$echoed" | tail -1)
    if [ -z "$tail_word" ]; then
        fail 'the real agent ended its reply on no word of its own to look for'
        return 1
    fi
    if ! wait_for_re "$AGENT_PREFIX|$tail_word" "$WAIT_SECS" \
        "the real agent's reply in the panel" >/dev/null; then
        return 1
    fi
    pass "a real streamed reply reached the panel in $chunks chunks, ending in '$tail_word'"

    assert_tool_call_went_non_terminal_then_terminal || return 1
    pass 'a real tool call was observed non-terminal, then terminal'
    tmux kill-session -t "$SESSION" 2>/dev/null || true
}

# The status sequence one real tool call was seen in -- the first call the
# agent reported, followed by its own `tool_call_id` so this asserts one
# call transitioning rather than two calls coinciding. Read from the log
# rather than the screen because it is the order that is being asserted, and
# the log's own line order is arrival order -- a screen poll could only ever
# say which statuses were reachable, never which came first. The vocabulary
# is the wire's (`docs/acp-v1-wire-capture.md` pins
# `pending`/`in_progress`/`completed`/`failed`); what the call was called is
# the agent's business, not this leg's.
assert_tool_call_went_non_terminal_then_terminal() {
    local id statuses first_terminal first_non_terminal
    id=$(grep -oE 'ai ToolCallUpdate \{ tool_call_id: "[^"]+"' "$ROOT/view.log" |
        head -1 | sed -E 's/.*"(.*)"/\1/')
    if [ -z "$id" ]; then
        fail 'the real agent made no tool call at all'
        return 1
    fi
    # the greedy `.*` takes the last `status: ` on a line, so a title that
    # happens to carry the word cannot stand in for the real field
    statuses=$(grep -F "ai ToolCallUpdate { tool_call_id: \"$id\"" "$ROOT/view.log" |
        grep -oE '.*status: [A-Za-z]+' | grep -oE '[A-Za-z]+$' || true)
    first_non_terminal=$(printf '%s\n' "$statuses" | grep -nE "^($NON_TERMINAL_STATUSES)$" |
        head -1 | cut -d: -f1)
    first_terminal=$(printf '%s\n' "$statuses" | grep -nE "^($TERMINAL_STATUSES)$" |
        head -1 | cut -d: -f1)
    if [ -z "$first_non_terminal" ] || [ -z "$first_terminal" ]; then
        fail "tool call $id was never seen both non-terminal and terminal (saw: $(printf '%s' "$statuses" | tr '\n' ' '))"
        return 1
    fi
    if [ "$first_non_terminal" -ge "$first_terminal" ]; then
        fail "tool call $id reached a terminal status before a non-terminal one (saw: $(printf '%s' "$statuses" | tr '\n' ' '))"
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
    wait_for_re "($SPINNER_ALTERNATION) Probe the file" "$WAIT_SECS" \
        "the tool call's non-terminal status" >/dev/null
    # The marker moves with nothing typed and nothing arriving from the
    # agent: the turn is held here, so a second frame on screen is the
    # editor's own loop deadline coming due and nothing else.
    held=$(pane | grep -oE "($SPINNER_ALTERNATION) Probe the file" | head -1 | awk '{print $1}')
    [ -n "$held" ] || fail "the spinner frame left the screen before it could be read"
    others=$(printf '%s' "$SPINNER_ALTERNATION" |
        awk -v skip="$held" 'BEGIN { RS = "|" } $0 != skip { printf "%s%s", (n++ ? "|" : ""), $0 }')
    wait_for_re "($others) Probe the file" "$WAIT_SECS" \
        "the spinner advancing while the turn is held" >/dev/null
    refute "${AGENT_PREFIX}streaming and done" \
        "the turn's second half rendered before it was sent"

    touch "$resume"
    wait_for "${AGENT_PREFIX}streaming and done" "$WAIT_SECS" \
        "the streamed message growing in place" >/dev/null
    wait_for "${DONE_MARK}Probe the file" "$WAIT_SECS" \
        "the tool call's terminal status" >/dev/null
    pass 'a turn rendered incrementally, its tool call non-terminal then terminal'

    # The wire's remaining update kinds, in one turn, through the same
    # loop. Asserted on the rows they render as rather than on message
    # text: chunks sharing a message id fold into one growing entry (the
    # wire's own "a change in messageId indicates a new message"), so the
    # later ones are inside a row the panel's width has already truncated.
    submit 'stream'
    wait_for "${DONE_MARK}Read a.rs" "$WAIT_SECS" "the streamed tool call" >/dev/null
    wait_for "$TERMINAL_CONTENT" "$WAIT_SECS" "the streamed terminal content" >/dev/null
    wait_for "${PLAN_ACTIVE_MARK}Read the file" "$WAIT_SECS" \
        "the streamed plan" >/dev/null
    # Reasoning reaches the loop, reaches the screen, and reaches it in its
    # own voice. The refute is the actual contract: the wire carries
    # reasoning apart from the answer precisely so that no consumer renders
    # one as the other, and a fold that collapsed the two would leave the
    # agent apparently claiming what it was only considering.
    wait_for_log "ai ThoughtChunk .* text: \"$STREAM_THOUGHT\"" "$WAIT_SECS" \
        "the streamed reasoning" >/dev/null
    wait_for "${THOUGHT_PREFIX}${STREAM_THOUGHT}" "$WAIT_SECS" \
        "the streamed reasoning in its own voice" >/dev/null
    refute "${AGENT_PREFIX}${STREAM_THOUGHT}" "reasoning was rendered as the agent's own answer"
    # Usage is asserted on its decoded numbers, not on its arrival: `used`
    # and `size` are what any context-window readout is built from, and an
    # update that arrived with them swapped, defaulted or dropped would
    # satisfy every check that only looked for the event's name.
    wait_for_log "ai UsageUpdated \{ used: $STREAM_USED, size: $STREAM_SIZE" "$WAIT_SECS" \
        "the streamed usage accounting" >/dev/null
    wait_for "$USAGE_ROW" "$WAIT_SECS" "the accounting on the panel itself" >/dev/null
    pass 'every streamed update kind reached the panel'
    tmux kill-session -t "$SESSION" 2>/dev/null || true
}

leg_diff_accept_and_reject() {
    local key
    CURRENT_LEG=3-diff-accept-and-reject
    start_session diff "$STUB_ARGV" "$(mktemp -d)"
    open_panel "$WAIT_SECS"
    # before the first in-buffer read, not after it: `buffer_region` finds
    # the buffer by cutting at the panel's own left border, so a border this
    # script could not find would leave every wait below reading the whole
    # pane and passing on text drawn in the panel
    assert_panel_is_framed || return 1

    # Abandoned first, and restated afterwards. The session deduplicates a
    # diff it has already raised, so the second review below can only open
    # if closing the first one told the agent side to forget it -- which is
    # the whole of what abandoning means: the user dismissed the proposal
    # unread, and an agent restating it must reach them again rather than be
    # deduplicated against a review nobody looked at.
    #
    # Left through the `:View` form, which is the only decision here that
    # is: everything the user is expected to press is pressed below.
    submit 'propose'
    wait_in_buffer "$PROPOSED_BETA" "$WAIT_SECS" \
        "the abandoned proposal's own line, drawn in the file" >/dev/null
    review_verb leave
    until_gone "$REVIEW_KEY_HINT" "$WAIT_SECS" "the review closing unanswered" >/dev/null
    refute "$PROPOSED_BETA" 'an abandoned review left its proposal drawn in the file'
    wait_for "${REVIEW_MARK}discarded the proposal" "$WAIT_SECS" \
        "the abandoned review's own account of itself" >/dev/null
    assert_file_is 'alpha
beta
gamma' 'an abandoned review changed the buffer'
    pass 'a proposal left through :View review leave took its drawing with it'

    # The review as a user meets it: the line the agent proposes, drawn in
    # the file at the row it would go to, and the keys that decide it named
    # on the hunk itself rather than only in the panel column. Both are read
    # out of the buffer region, so a panel that grew its own copy of the
    # diff back could not answer either of them.
    submit 'propose'
    wait_in_buffer "$PROPOSED_BETA" "$WAIT_SECS" \
        "the agent's proposed line, drawn in the file" >/dev/null
    wait_in_buffer "$REVIEW_KEY_HINT" "$WAIT_SECS" \
        "the current hunk's own header, at the hunk rather than in the panel" >/dev/null
    # Narrowed, because the header's width is the whole risk: the panel
    # covers the right of every row it is drawn on, and a header on one
    # line loses its tail there -- the way out first, which is the one key
    # a reader who wants no part of the proposal needs.
    tmux resize-window -t "$SESSION" -x "$NARROW_COLS" -y "$ROWS"
    wait_in_buffer "$REVIEW_LEAVE_HINT" "$WAIT_SECS" \
        "the review's way out, still readable at $NARROW_COLS columns" >/dev/null
    tmux resize-window -t "$SESSION" -x "$COLS" -y "$ROWS"
    wait_in_buffer "$REVIEW_KEY_HINT" "$WAIT_SECS" \
        "the header back at $COLS columns" >/dev/null
    # The buffer's own text is still the buffer's own text: the proposal is
    # decoration until it is accepted, and a review that had already written
    # itself into the file would show `BETA` here with nothing left to
    # decide.
    wait_in_buffer 'beta' "$WAIT_SECS" "the row the proposal would replace" >/dev/null
    pass 'a proposal abandoned unread was raised again, drawn in the file it edits'

    # Pressed, not commanded. The maps are buffer-local and are installed by
    # the same call that draws the marks, so a key typed at the buffer is
    # the only assertion that both halves of the decoration arrived: what is
    # visible, and what answers.
    key=$(review_key accept) || return 1
    send_text "$key"
    # The review closing on its last open hunk, and then its own count of
    # what it decided: the file assertion below proves the bytes, and this
    # proves they were written by an accept rather than by anything else
    # that could have touched the buffer.
    until_gone "$REVIEW_KEY_HINT" "$WAIT_SECS" "the review closing on the accept" >/dev/null
    # The namespace went with it. A decoration that outlived its review
    # would leave the user reading a proposal that is already the file's own
    # text, with no keys left to answer it.
    refute "$PROPOSED_BETA" 'the accepted proposal is still drawn over the text it became'
    wait_for "${REVIEW_MARK}accepted 1 and rejected 0 hunks" "$WAIT_SECS" \
        "the accepted review's own account of itself" >/dev/null
    assert_file_is 'alpha
BETA
gamma' 'the accepted hunk was not written byte for byte'
    pass 'a proposal accepted with <leader>ha in the buffer reached it byte-exactly'

    submit 'propose2'
    wait_in_buffer "$PROPOSED_GAMMA" "$WAIT_SECS" \
        "the second proposal's own line, drawn in the file" >/dev/null
    key=$(review_key reject) || return 1
    send_text "$key"
    until_gone "$REVIEW_KEY_HINT" "$WAIT_SECS" "the review closing on the reject" >/dev/null
    wait_for "${REVIEW_MARK}accepted 0 and rejected 1 hunks" "$WAIT_SECS" \
        "the rejected review's own account of itself" >/dev/null
    assert_file_is 'alpha
BETA
gamma' 'a rejected hunk changed the buffer'
    refute 'GAMMA' 'a rejected hunk reached the buffer, or is still drawn over it'
    pass 'a proposal rejected with <leader>hx left the buffer untouched'
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
    # `1` is the first option the stub offers (allow-once), the digit the
    # prompt paints against that row.
    send_text '1'
    # Read from the log, not the screen: chunks sharing a message id fold
    # into one transcript entry, so this answer lands inside the row the
    # overlap report already opened rather than starting one of its own.
    wait_for_log 'ai MessageChunk .* text: "chose allow-once"' "$WAIT_SECS" \
        "the first request's own answer" >/dev/null
    wait_for_log 'ai TurnEnded' "$WAIT_SECS" "the turn ending after the overlap" >/dev/null
    pass 'an overlapping permission request was answered without disturbing the first'
    tmux kill-session -t "$SESSION" 2>/dev/null || true
}

# The prompt the user actually has to answer: its keys on screen, the key
# they press, and what a standing grant does with the next request. The
# live defect this covers shipped because no leg ever looked at a
# permission prompt -- every leg above answers one and asserts what came
# back, none of them read what the rows offered.
leg_permission_keys_and_grant() {
    CURRENT_LEG=8-permission-keys-and-grant
    start_session grant "$STUB_ARGV" "$(mktemp -d)"
    open_panel "$WAIT_SECS"

    submit 'ask-always'
    wait_for "$PERMISSION_PROMPT call_101" "$WAIT_SECS" "the permission request" >/dev/null
    # Every option, with the digit that answers it and the wire kind it
    # answers with. A prompt that painted names alone -- which is what the
    # dogfood session met -- fails here.
    local row
    for row in "$PERMISSION_ROW_DENY" "$PERMISSION_ROW_ONCE" "$PERMISSION_ROW_ALWAYS"; do
        wait_for "$row" "$WAIT_SECS" "the option row '$row'" >/dev/null
    done
    wait_for "$PERMISSION_KEY_HINT" "$WAIT_SECS" "the prompt's own key hint" >/dev/null
    pass 'the prompt paints every option with the key that answers it'

    # The letter that used to mean always-allow: the prompt's vocabulary is
    # the digits it paints, so a letter must answer nothing at all now.
    send_text 'a'
    sleep "$POLL"
    wait_for "$PERMISSION_PROMPT call_101" "$WAIT_SECS" \
        "the prompt after an unmapped letter" >/dev/null
    pass 'a letter answers no permission prompt'

    send_text '3'
    # The outbound breadcrumb, which is what the frozen-session forensics
    # had no way to read: which option id was actually sent back.
    wait_for_log 'ai AnswerPermission .* option_id: "allow-always"' "$WAIT_SECS" \
        "the answer view sent back" >/dev/null
    wait_for "${AGENT_PREFIX}first allow-always" "$WAIT_SECS" \
        "the agent's report of the first answer" >/dev/null

    # The second request, of the same tool kind, is answered by the grant
    # rather than by the user -- visibly, on the transcript, and with the
    # same option id.
    wait_for "$AUTO_ALLOW_LINE" "$WAIT_SECS" "the standing grant's own row" >/dev/null
    # Read from the log: this chunk shares a message id with the one above,
    # so it folds into that same transcript row and the row is wider than
    # the panel.
    wait_for_log 'ai MessageChunk .* text: "second allow-always"' "$WAIT_SECS" \
        "the agent's report of the auto-answer" >/dev/null
    refute "$PERMISSION_PROMPT call_102" 'a granted kind asked the user again'
    wait_for_log 'ai TurnEnded' "$WAIT_SECS" "the turn ending after the grant" >/dev/null
    pass 'an always-allow answered the next request of that kind without asking'

    # The refusing half of the same promise, on a kind the grant above says
    # nothing about: an "always" that only held one way would leave this
    # second request asking again.
    submit 'refuse-always'
    wait_for "$PERMISSION_PROMPT call_201" "$WAIT_SECS" "the refusal request" >/dev/null
    wait_for "$PERMISSION_ROW_NEVER" "$WAIT_SECS" \
        "the always-reject row '$PERMISSION_ROW_NEVER'" >/dev/null

    send_text '3'
    wait_for_log 'ai AnswerPermission .* option_id: "reject-always"' "$WAIT_SECS" \
        "the refusal view sent back" >/dev/null
    # From the log for the same reason the auto-answer below is: every
    # chunk this stub sends shares one message id, so they fold into a
    # single transcript row wider than the panel.
    wait_for_log 'ai MessageChunk .* text: "first reject-always"' "$WAIT_SECS" \
        "the agent's report of the first refusal" >/dev/null

    wait_for "$AUTO_REFUSE_LINE" "$WAIT_SECS" "the standing refusal's own row" >/dev/null
    wait_for_log 'ai MessageChunk .* text: "second reject-always"' "$WAIT_SECS" \
        "the agent's report of the auto-refusal" >/dev/null
    refute "$PERMISSION_PROMPT call_202" 'a refused kind asked the user again'
    pass 'an always-reject refused the next request of that kind without asking'
    tmux kill-session -t "$SESSION" 2>/dev/null || true
}

leg_filesystem_round_trip() {
    CURRENT_LEG=7-filesystem-round-trip
    local settled
    start_session fs "$STUB_ARGV" "$(mktemp -d)"
    # Two lines and a final newline, because all three are things the answer
    # can get wrong: nvim's line list carries no record of a terminator at
    # all, so the join and the trailing newline are reconstructed on the way
    # back out and a one-line file would exercise neither.
    printf 'first line\nsecond line\n' >"$ROOT/$STUB_FS_FILE"
    open_panel "$WAIT_SECS"

    # Read from the log rather than the screen: what is being asserted is
    # the exact bytes handed back to the agent, and the panel is a column
    # too narrow to hold them -- a screen assertion here could only ever
    # check a prefix of the answer it exists to pin.
    submit 'read'
    wait_for_log "ai MessageChunk .* text: \"read first line\\\\nsecond line\\\\n\"" \
        "$WAIT_SECS" "the file's exact content, back at the agent" >/dev/null
    pass 'a read crossed to nvim and came back byte for byte'

    # The path is absolute, well-formed and nowhere near the session
    # directory. The code it is refused with is the assertion: an
    # unresolvable path inside the boundary and a resolvable one outside it
    # answer identically, so that an agent cannot learn what exists out
    # there by watching the code change.
    submit 'read-outside'
    wait_for_log "ai MessageChunk .* text: \"read refused $INVALID_PARAMS\"" \
        "$WAIT_SECS" "the refusal of a path outside the session directory" >/dev/null
    if grep -qE "ai MessageChunk .* text: \"read refused $RESOURCE_NOT_FOUND\"" \
        "$ROOT/view.log"; then
        fail 'a path outside the session directory was refused with a code that reports whether it exists'
        return 1
    fi
    pass 'a read outside the session directory was refused without reporting what is there'

    # The write's own content ends without a newline, which nvim adds back
    # on save unless it is told not to -- so comparing bytes rather than
    # lines is the only comparison that can fail when it should.
    printf '%s' "$STUB_FS_WRITE_CONTENT" >"$ROOT/expected-fs-write"
    submit 'write'
    wait_for_log 'ai MessageChunk .* text: "wrote"' "$WAIT_SECS" \
        "the write's acceptance, back at the agent" >/dev/null
    # The reply crosses back the moment nvim reports the save, and the
    # comparison below reads the file the save produced; re-read until it
    # settles for the reason `assert_file_is` does, and fail at the budget
    # with the bytes actually found.
    settled=$(now)
    while ! cmp -s "$ROOT/expected-fs-write" "$ROOT/$STUB_FS_FILE"; do
        if ! under "$(elapsed "$settled" "$(now)")" "$WAIT_SECS"; then
            fail "the agent's write did not land byte for byte -- the file holds $(printf '%q' "$(cat "$ROOT/$STUB_FS_FILE")")"
            return 1
        fi
        sleep "$POLL"
    done
    pass "an agent's write reached disk through nvim's buffer, byte for byte"
    tmux kill-session -t "$SESSION" 2>/dev/null || true
}

# The stretch before the agent's first event, which on a real thinking model
# is seconds of a panel holding perfectly still -- the third dogfood report
# could not tell it apart from a prompt that never left. Its own leg rather
# than a block inside leg 2, because the stub gives every chunk one message
# id and a turn added ahead of that leg's own folds into the entry its
# assertions read.
leg_prompt_awaiting_its_answer() {
    CURRENT_LEG=9-prompt-awaiting-its-answer
    local resume held others
    start_session think "$STUB_ARGV" "$(mktemp -d)"
    resume=$RESUME_FILE
    rm -f "$resume"
    open_panel "$WAIT_SECS"

    # Held before the stub has sent anything at all, which is the one window
    # where nothing else on screen could be the thing moving: the prompt is
    # the only entry there is.
    submit 'think'
    wait_for_re "($SPINNER_ALTERNATION) think" "$WAIT_SECS" \
        "the submitted prompt's own spinner" >/dev/null
    held=$(pane | grep -oE "($SPINNER_ALTERNATION) think" | head -1 | awk '{print $1}')
    [ -n "$held" ] || fail "the prompt's spinner frame left the screen before it could be read"
    others=$(printf '%s' "$SPINNER_ALTERNATION" |
        awk -v skip="$held" 'BEGIN { RS = "|" } $0 != skip { printf "%s%s", (n++ ? "|" : ""), $0 }')
    # A second frame with nothing typed and nothing arriving: the same loop
    # deadline the tool call's marker rides, reached from submit instead of
    # from a call in flight.
    wait_for_re "($others) think" "$WAIT_SECS" \
        "the prompt's marker advancing before the agent has sent anything" >/dev/null

    # ... and it stands back down on the agent's first word rather than
    # spinning behind the answer it was waiting for. The stub holds the turn
    # open again after that word, which is what makes the stand-down
    # attributable: a marker back at its own glyph while no turn has ended
    # cannot have been settled by the turn ending.
    touch "$resume"
    wait_for "${AGENT_PREFIX}thought about it" "$WAIT_SECS" \
        "the agent's first word" >/dev/null
    wait_for "${USER_PREFIX}think" "$WAIT_SECS" \
        "the prompt's marker standing back down" >/dev/null
    if grep -qE 'ai TurnEnded' "$ROOT/view.log" 2>/dev/null; then
        fail "the turn ended before the stand-down could be told apart from end_turn's own"
        return 1
    fi
    pass "a submitted prompt animated from submit until the agent's first word, and no further"

    # Released, so the leg leaves a finished turn behind rather than a
    # session the stub is still holding.
    rm -f "$resume"
    wait_for_log 'ai TurnEnded' "$WAIT_SECS" "the held turn ending" >/dev/null
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
REVIEW_KEY_HINT=$(const_str "$REVIEW_RS" KEY_HINT) || exit 1
REVIEW_KEY_HINT=${REVIEW_KEY_HINT:0:20}
# An empty needle is `grep -F ''`, which matches every screen there is:
# every review assertion below would pass vacuously.
[ -n "$REVIEW_KEY_HINT" ] || {
    printf 'FAIL: the review key hint read empty from %s\n' "$REVIEW_RS" >&2
    exit 1
}
# The one key that has to be readable whatever the terminal's width: a
# review whose way out is under the panel is a state the user cannot leave
# without knowing a key nothing on screen names.
REVIEW_LEAVE_HINT=$(const_str "$REVIEW_RS" LEAVE_HINT) || exit 1
# What a proposed line looks like where the review draws it: the stub's own
# `newText` behind the prefix the decoration chunk puts in front of every
# virtual line. Both halves are guarded rather than derived -- the templates
# below are what a rewording of either would fail on, and the words
# themselves are the same ones `assert_file_is` already spells out of the
# same fixture.
require_template "$STUB_RS" '("alpha\nbeta\ngamma\n", "alpha\nBETA\ngamma\n")' || exit 1
require_template "$STUB_RS" '("alpha\nBETA\ngamma\n", "alpha\nBETA\nGAMMA\n")' || exit 1
require_template "$NVIM_API_RS" "virt[#virt + 1] = { { '+' .. line, 'DiffAdd' } }" || exit 1
PROPOSED_BETA='+BETA'
PROPOSED_GAMMA='+GAMMA'
# The keys the review installs on the buffer, read and shape-checked out of
# the table the maps are generated from, so a reworded key fails here rather
# than being typed at a buffer that no longer answers it.
# shellcheck disable=SC2034 # read by `review_key`, which lives in artifacts.sh
REVIEW_KEYS=$(review_keys_of "$MAPPINGS_RS") || exit 1
# The marker the review's own transcript lines carry. Every review ends
# with one, and what it says is how the review ended -- which is the one
# reading of a decision that comes from the review itself rather than from
# the screen it happened to be drawn on.
REVIEW_MARK=$(mark_str REVIEW_MARK) || exit 1
DONE_MARK=$(status_mark Completed) || exit 1
PERMISSION_PROMPT=$(grep -oE 'format!\("Permission requested for' "$PERMISSION_RS" |
    sed -E 's/.*"(.*)/\1/')
[ -n "$PERMISSION_PROMPT" ] || {
    printf 'FAIL: the permission prompt is not built from a literal in %s any more\n' \
        "$PERMISSION_RS" >&2
    exit 1
}
# The marker the transcript opens the agent's own side of the conversation
# with, and the one reasoning wears instead. Read from the two arms
# separately rather than one derived from the other, since the whole
# assertion downstream is that they are not the same glyph.
AGENT_PREFIX=$(grep -oE 'TranscriptRole::Agent => \(?[A-Z_]+' "$TRANSCRIPT_RS" |
    sed -E 's/.*[( ]([A-Z_]+)$/\1/')
[ -n "$AGENT_PREFIX" ] || {
    printf 'FAIL: TranscriptRole::Agent renders no marker in %s any more\n' \
        "$TRANSCRIPT_RS" >&2
    exit 1
}
AGENT_PREFIX=$(mark_str "$AGENT_PREFIX") || exit 1
THOUGHT_PREFIX=$(grep -oE 'TranscriptRole::Thought => \(?[A-Z_]+' "$TRANSCRIPT_RS" |
    sed -E 's/.*[( ]([A-Z_]+)$/\1/')
[ -n "$THOUGHT_PREFIX" ] || {
    printf 'FAIL: TranscriptRole::Thought renders no marker in %s any more\n' \
        "$TRANSCRIPT_RS" >&2
    exit 1
}
THOUGHT_PREFIX=$(mark_str "$THOUGHT_PREFIX") || exit 1
# The marker the user's own side wears once nothing is pending on it -- the
# glyph the submitted prompt's spinner stands back down to.
USER_PREFIX=$(arm_mark "TranscriptRole::User") || exit 1
# Every frame an unresolved call's marker cycles through, as one alternation:
# the frame on screen depends on when the screen was read, so the assertion
# that a call is running has to accept any of them.
SPINNER_ALTERNATION=$(awk '
    /^const SPINNER_FRAMES/ { inside = 1; next }
    inside && /^\];/ { exit }
    inside && match($0, /"[^"]+"/) {
        frame = substr($0, RSTART + 1, RLENGTH - 2)
        sub(/ $/, "", frame)
        printf "%s%s", (n++ ? "|" : ""), frame
    }
' "$TRANSCRIPT_RS")
[ -n "$SPINNER_ALTERNATION" ] || {
    printf 'FAIL: SPINNER_FRAMES no longer lists the running marker frames in %s\n' \
        "$TRANSCRIPT_RS" >&2
    exit 1
}
SPINNER_ALTERNATION=$(printf '%b' "$(printf '%s' "$SPINNER_ALTERNATION" |
    sed -E 's/\\u\{([0-9a-fA-F]+)\}/\\u\1/g')")
# The marker the plan's current task opens with -- the task the stub's own
# plan update sends as `in_progress` (`docs/acp-v1-wire-capture.md`'s `Plan`
# pin), read from the arm that renders that state rather than from a word
# the row no longer carries.
PLAN_ACTIVE_MARK=$(arm_mark "PlanEntryStatus::InProgress") || exit 1
# A placeholder for a content kind the panel shows a label for rather than
# the content itself, built from the wire's pinned vocabulary
# (`docs/acp-v1-wire-capture.md`'s `Terminal` pin) slotted into a template
# that lives in the source, so the template is what is checked and the wire
# word is what is substituted.
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
# The prompt's own rows, rendered from the template that paints them rather
# than copied out of it: a hand-copied row is a second definition of the
# same string, and the two drifted apart once already -- the template gained
# its indent as a `{}` placeholder, every leg of this script died in setup,
# and nothing failed until someone ran it by hand. Derived, a reworded row
# changes these assertions with it, and a template this cannot fill fails
# loudly below.
permission_row() {
    local key="$1" name="$2" note="$3" fmt
    # The first `{key}` template in production code -- `awk` stops at the
    # test module rather than `head -1` trusting source order.
    fmt=$(awk '
        /#\[cfg\(test\)\]/ { exit }
        match($0, /"[^"]*\{key\}[^"]*"/) { print substr($0, RSTART + 1, RLENGTH - 2); exit }
    ' "$PERMISSION_RS")
    if [ -z "$fmt" ]; then
        printf 'FAIL: %s builds no option row from a {key} template any more\n' \
            "$PERMISSION_RS" >&2
        return 1
    fi
    # The leading `{}` is the indent; every assertion here reads past it,
    # matching the row as a substring that starts at the digit.
    fmt=${fmt#'{}'}
    fmt=${fmt/'{key}'/$key}
    fmt=${fmt/'{}'/$name}
    fmt=${fmt/'{}'/$note}
    # A placeholder left over is a template that grew a field this script
    # does not know how to fill: fail rather than assert a row with a `{}`
    # in it that no screen will ever show.
    case "$fmt" in
        *'{'*)
            printf 'FAIL: %s option row template %s has a field this script cannot fill\n' \
                "$PERMISSION_RS" "$fmt" >&2
            return 1
            ;;
    esac
    printf '%s' "$fmt"
}
# The notes are the wire's own kind spellings, pinned separately in the stub
# options guarded below.
PERMISSION_ROW_DENY=$(permission_row 1 Deny reject_once) || exit 1
PERMISSION_ROW_ONCE=$(permission_row 2 'Allow Once' allow_once) || exit 1
# The two rows whose answer outlives the question say what they cover and
# how long for, off the request's own tool kind. Asserted as a prefix: at
# this width the row is a few columns wider than the panel's interior, the
# same truncation `REVIEW_KEY_HINT` is read through.
require_template "$PERMISSION_RS" '"all {tool} this session"' || exit 1
require_template "$PERMISSION_RS" '"no {tool} this session"' || exit 1
# Short enough that both rows' prefixes are inside the panel at this width.
PERMISSION_ROW_COLS=27
PERMISSION_ROW_ALWAYS=$(permission_row 3 'Always Allow' 'all edit this session') || exit 1
PERMISSION_ROW_ALWAYS=${PERMISSION_ROW_ALWAYS:0:$PERMISSION_ROW_COLS}
PERMISSION_ROW_NEVER=$(permission_row 3 'Always Reject' 'no execute this session') || exit 1
PERMISSION_ROW_NEVER=${PERMISSION_ROW_NEVER:0:$PERMISSION_ROW_COLS}
for stub_option in \
    '{ "optionId": "reject-once", "name": "Deny", "kind": "reject_once" }' \
    '{ "optionId": "allow-once", "name": "Allow Once", "kind": "allow_once" }' \
    '{ "optionId": "allow-always", "name": "Always Allow", "kind": "allow_always" }' \
    '{ "optionId": "reject-always", "name": "Always Reject", "kind": "reject_always" }'; do
    require_template "$STUB_RS" "$stub_option" || exit 1
done
PERMISSION_KEY_HINT=$(const_str "$PERMISSION_RS" KEY_HINT) || exit 1
# What the panel says when a standing answer answered for the user. The
# template lives where the answer is given; `edit` and `execute` are the
# kinds the stub's own two requests name. Both are read as a prefix for the
# same width reason as the option rows above.
require_template "$REPO_ROOT/crates/view-core/src/update/ai.rs" \
    '"auto-allowed {tool_kind} (standing answer)"' || exit 1
require_template "$REPO_ROOT/crates/view-core/src/update/ai.rs" \
    '"auto-refused {tool_kind} (standing answer)"' || exit 1
AUTO_ALLOW_LINE='auto-allowed edit (standing'
AUTO_REFUSE_LINE='auto-refused execute (standing'

# How the first-run provisioning wait announces itself.
PROVISION_NOTICE=$(const_str "$REPO_ROOT/crates/view/src/ai_worker.rs" PROVISION_NOTICE_PREFIX)

# The shapes leg 1 reads its own log lines by. `vlog.rs` renders every AI
# event from a template of its own rather than a derive, so the field order
# is that file's to change; reading the templates from there is what turns a
# rename into a loud failure here rather than a grep that quietly matches
# nothing. The four status words are `ToolCallStatus` variant names as
# `{status:?}` spells them, each proved still to exist by the label the
# panel renders it as.
VLOG_RS="$REPO_ROOT/crates/view/src/vlog.rs"
require_template "$VLOG_RS" 'log_with("ai"' || exit 1
require_template "$VLOG_RS" \
    'MessageChunk {{ message_id: {message_id:?}, from_agent: {from_agent}' || exit 1
require_template "$VLOG_RS" \
    'ToolCallUpdate {{ tool_call_id: {tool_call_id:?}, title: {}, status: {status:?}' || exit 1
require_template "$VLOG_RS" 'ThoughtChunk {{ message_id: {message_id:?}, text: {} }}' || exit 1
NON_TERMINAL_STATUSES='Pending|InProgress'
TERMINAL_STATUSES='Completed|Failed'
# `InProgress` is the one arm with no constant of its own: it renders
# whichever `SPINNER_FRAMES` entry the tick has reached, already read into
# `SPINNER_ALTERNATION` above, so the arm is proved to still animate rather
# than to name a glyph.
grep -A 3 -E 'ToolCallStatus::InProgress =>' "$TRANSCRIPT_RS" | grep -q SPINNER_FRAMES || {
    printf 'FAIL: ToolCallStatus::InProgress no longer renders a spinner frame in %s\n' \
        "$TRANSCRIPT_RS" >&2
    exit 1
}
for status in Pending Completed Failed; do
    status_mark "$status" >/dev/null || exit 1
done

# The stub agent's first argument is the file whose appearance releases a
# held turn; one path serves every leg because no two sessions overlap.
PINNED_VERSION=$(grep -A 2 'id: "claude-code"' "$REPO_ROOT/crates/view-ai/src/provision.rs" |
    grep -oE 'version: "[^"]+"' | sed -E 's/.*"(.*)"/\1/')
[ -n "$PINNED_VERSION" ] || {
    printf 'FAIL: the claude-code row has no pinned version in provision.rs any more\n' >&2
    exit 1
}

# What the stub streams as reasoning, and the accounting it reports beside
# it -- read from the fixture that sends them so a reworded chunk or a
# changed count fails the waits that name them rather than passing on a
# pattern that now matches nothing.
STREAM_THOUGHT=$(grep -oE 'chunk\(stdout, "agent_thought_chunk", "[^"]+"\)' "$STUB_RS" |
    sed -E 's/.*"(.*)"\)/\1/')
[ -n "$STREAM_THOUGHT" ] || {
    printf 'FAIL: the stub agent no longer streams a thought chunk in %s\n' "$STUB_RS" >&2
    exit 1
}
STREAM_USED=$(grep -oE '"used": [0-9]+' "$STUB_RS" | grep -oE '[0-9]+$')
STREAM_SIZE=$(grep -oE '"size": [0-9]+' "$STUB_RS" | grep -oE '[0-9]+$')
[ -n "$STREAM_USED" ] && [ -n "$STREAM_SIZE" ] || {
    printf 'FAIL: the stub agent no longer streams a usage update in %s\n' "$STUB_RS" >&2
    exit 1
}
# How the panel spells that accounting once it holds it, from the format
# that builds the row.
require_template "$PANEL_RS" '"context {}/{}"' || exit 1
USAGE_ROW="context $STREAM_USED/$STREAM_SIZE"
# The file the filesystem legs name, and the content the write leg sends,
# both read from the same fixture for the same reason.
STUB_FS_FILE=$(grep -oE 'named_inside_cwd\("[^"]+"\)' "$STUB_RS" | head -1 |
    sed -E 's/.*"(.*)".*/\1/')
[ -n "$STUB_FS_FILE" ] || {
    printf 'FAIL: the stub agent names no file inside its own cwd in %s any more\n' "$STUB_RS" >&2
    exit 1
}
STUB_FS_WRITE_CONTENT='fn main() {}'
require_template "$STUB_RS" "\"content\": \"$STUB_FS_WRITE_CONTENT\"" || exit 1
# The two refusal codes leg 7 tells apart. Read from the crate that pins
# them against the wire's own table, since the whole assertion is which of
# the two an out-of-boundary path gets.
WIRE_RS=$REPO_ROOT/crates/view-ai/src/acp/wire.rs
INVALID_PARAMS=$(grep -oE 'pub const INVALID_PARAMS: i64 = -?[0-9]+' "$WIRE_RS" |
    grep -oE '\-?[0-9]+$')
RESOURCE_NOT_FOUND=$(grep -oE 'pub const RESOURCE_NOT_FOUND: i64 = -?[0-9]+' "$WIRE_RS" |
    grep -oE '\-?[0-9]+$')
[ -n "$INVALID_PARAMS" ] && [ -n "$RESOURCE_NOT_FOUND" ] || {
    printf 'FAIL: the filesystem refusal codes are not constants in %s any more\n' "$WIRE_RS" >&2
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
    leg_cancel_mid_turn leg_agent_crash leg_permission_overlap leg_filesystem_round_trip
    leg_permission_keys_and_grant leg_prompt_awaiting_its_answer)
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
# A subset says what it did not cover, by name, before it runs. A partial
# run that reads like a full one is how a repair came to be signed off
# against legs that never touched its subject: the count at the end says
# how many passed, and this says which ones were never asked.
for leg in "${LEGS[@]}"; do
    case " ${selected[*]} " in
    *" $leg "*) ;;
    *) printf 'SKIP: %s -- not selected for this run\n' "$leg" ;;
    esac
done
for leg in "${selected[@]}"; do "$leg"; done

printf 'ai conformance: %s of %s legs green\n' "${#selected[@]}" "${#LEGS[@]}"
