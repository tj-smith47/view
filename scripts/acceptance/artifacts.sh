#!/usr/bin/env bash
#
# The binaries an acceptance run measures, guaranteed to be the tree's own.
#
# Sourced, never executed. Every script here drives a compiled binary out of
# `target/`, and a path under `target/` is not evidence of anything on its
# own: it is whatever the last build that touched it left, including a build
# from source that has since been edited away. A run that trusts one is
# capable of reporting a defect that was fixed hours earlier, which is worse
# than a run that fails -- it sends a reviewer looking for a race in code
# that never ran.
#
# So the default path is BUILT here rather than checked for existence, on
# every run, which cargo answers in a fraction of a second when nothing has
# changed. A path the caller pinned is not this script's to build -- it may
# not even come from this tree -- but it is still refused when the tree has
# moved past it, because the same stale artifact produces the same false
# verdict either way.
#
# Callers must define `REPO_ROOT` before sourcing.

# A fractional-seconds clock every leg's wait loops time out against.
#
# BSD `date` (macOS, an established validation host for this repo) has no
# `%N` and answers `<epoch>.N`, which awk reads as the integer seconds --
# so every `elapsed` returns 0.00 and no wait loop ever expires. A sweep
# that hangs on a defect instead of failing on it is worse than one with
# whole-second resolution, which is what the fallback here gives.
now() {
    local t
    t=$(date +%s.%N)
    case "$t" in
    *N*) date +%s ;;
    *) printf '%s\n' "$t" ;;
    esac
}

# A directory for the pane dumps a failing run keeps, with the ones earlier
# failures kept reaped first.
#
# Nothing removes a kept dump afterwards -- deliberately, since it is the
# evidence -- so on a host whose TMPDIR is a small tmpfs a run of failures
# fills it and the next run has nowhere to write. A day is long enough for
# whoever the dump was kept for to have read it.
dump_dir() {
    local prefix="$1" parent="${TMPDIR:-/tmp}"
    find "$parent" -maxdepth 1 -name "$prefix-*" -type d -mtime +1 \
        -exec rm -rf {} + 2>/dev/null || true
    mktemp -d "$parent/$prefix-XXXXXX"
}

# Where cargo puts what it builds for this tree.
#
# `CARGO_TARGET_DIR` is how a build is redirected out of the checkout --
# which is what an isolated export of the tree is measured from, so a
# harness that spelled `$REPO_ROOT/target` refuses every leg on the one
# procedure the A/B and bisect recipes are run under. A relative value is
# resolved against the repo; cargo resolves one against the cwd of the
# invocation instead, and the two agree only because every target and
# script here runs from the root.
#
# The variable rather than `cargo metadata`: a `build.target-dir` in a cargo
# config file is the one redirection this does not see, and reading it costs
# a subprocess on every leg of every script here.
target_root() {
    case "${CARGO_TARGET_DIR:-}" in
    "") printf '%s/target\n' "$REPO_ROOT" ;;
    /*) printf '%s\n' "$CARGO_TARGET_DIR" ;;
    *) printf '%s/%s\n' "$REPO_ROOT" "$CARGO_TARGET_DIR" ;;
    esac
}

# The target directory against a known answer, in both directions, on every
# run: a locator stuck on the checkout's own `target` is invisible on the
# host that has no `CARGO_TARGET_DIR` set, which is every host but the one
# the redirection exists for.
check_target_root() {
    local out
    out=$(CARGO_TARGET_DIR=/somewhere/else target_root)
    if [ "$out" != /somewhere/else ]; then
        printf 'FAIL: a declared CARGO_TARGET_DIR is not where the harness looks (said: %s)\n' "$out" >&2
        exit 1
    fi
    out=$(CARGO_TARGET_DIR=elsewhere target_root)
    if [ "$out" != "$REPO_ROOT/elsewhere" ]; then
        printf 'FAIL: a relative CARGO_TARGET_DIR is not resolved against the repo (said: %s)\n' "$out" >&2
        exit 1
    fi
    out=$(CARGO_TARGET_DIR= target_root)
    if [ "$out" != "$REPO_ROOT/target" ]; then
        printf 'FAIL: with nothing declared the harness must look in the target directory the tree owns (said: %s)\n' "$out" >&2
        exit 1
    fi
}
check_target_root
TARGET_ROOT=$(target_root)

# Every `view` a run has started, reaped by pid when it ends, and the one
# the most recent call recorded.
VIEW_PIDS=()
VIEW_PID=""

# Records the `view` behind a tmux session's pane, as `VIEW_PID`.
#
#   watch_view "$SESSION" || return 1
#
# A statement, never a command substitution: a substitution runs in a
# subshell, so the recording would die with it while the caller still got
# its pid back -- a reaper holding an empty list and reporting every run
# clean. `check_view_reaping` refuses the shape outright.
#
# The pane command is `view` itself in most legs and a shell that runs view
# in the exit legs, so both shapes are looked for. Taken while the session
# is known good rather than in the cleanup: a leg that ends its own session
# takes the last chance to read the pid with it.
watch_view() {
    local session="$1" pane_pid pid
    pane_pid=$(tmux list-panes -t "$session" -F '#{pane_pid}' 2>/dev/null | head -1)
    if [ -z "$pane_pid" ]; then
        printf 'FAIL: tmux session %s has no pane to read a pid from\n' "$session" >&2
        return 1
    fi
    if [ "$(ps -o comm= -p "$pane_pid" 2>/dev/null | tr -d ' ')" = view ]; then
        pid=$pane_pid
    else
        pid=$(pgrep -P "$pane_pid" -x view | head -1 || true)
    fi
    if [ -z "$pid" ]; then
        printf 'FAIL: the pane of %s is neither view nor a shell with a view child\n' "$session" >&2
        return 1
    fi
    VIEW_PIDS+=("$pid")
    VIEW_PID=$pid
}

# Kills every `view` this run started and refuses to let the run end while
# one is still alive. Called from each script's cleanup, before the sessions
# are killed.
#
# `tmux kill-session` delivers a SIGHUP and returns; a view that has stopped
# honouring signals -- the state several legs induce on purpose -- outlives
# the run holding its scratch root, and a removal that overtakes it walks
# past directories it then re-creates. The survivor check is an assertion
# rather than a wait because an orphan reaching here is the harness failing
# to notice the very leak it exists to catch.
#
# Nothing outside a bash cleanup can pin one: what stands in for a pin is
# that this is the check the run performs on itself, on every exit path.
reap_views() {
    local pid alive
    for pid in ${VIEW_PIDS[@]+"${VIEW_PIDS[@]}"}; do
        # continued before it is killed: a stopped process reaped by nothing
        # outlives the run holding its address space
        kill -CONT "$pid" 2>/dev/null || true
        kill -9 "$pid" 2>/dev/null || true
    done
    for _ in 1 2 3 4 5 6 7 8 9 10; do
        alive=""
        for pid in ${VIEW_PIDS[@]+"${VIEW_PIDS[@]}"}; do
            kill -0 "$pid" 2>/dev/null && alive="$alive $pid"
        done
        [ -z "$alive" ] && return 0
        sleep 0.2
    done
    printf 'FAIL: view survived this run (pid%s), so a leg left an orphan holding its scratch root\n' \
        "$alive" >&2
    return 1
}

# The recorder and the reaper end to end, on every run, through the call
# shape the legs use: a stand-in named `view` in a tmux pane of its own,
# recorded by `watch_view` and then reaped.
#
# Both halves need it. A reaper that killed nothing would let every orphan
# through while the run reported green; a recorder whose pid never left its
# own subshell would leave that reaper nothing to kill, and it too would
# report green by finding nothing. Seeding the list by hand pins only the
# first half, which is how the second half shipped broken.
#
# The victim is a tmux pane's process, so it is no child of this shell --
# `kill -0` answers for a zombie as though it were alive, and a real orphan
# is never one.
check_view_reaping() {
    local tmp session before
    tmp=$(mktemp -d)
    # a shell under the name the recorder looks for. The trailing `:` is
    # what keeps that name: given one command, a shell execs it and becomes
    # `sleep` instead of staying the `view` this has to find
    ln -s /bin/sh "$tmp/view"
    session="view-acc-selfcheck-$$"
    tmux new-session -d -s "$session" "$tmp/view -c 'sleep 300; :'" || {
        printf 'FAIL: tmux could not start the session the self-check reaps\n' >&2
        exit 1
    }
    before=${#VIEW_PIDS[@]}
    VIEW_PID=""
    for _ in 1 2 3 4 5 6 7 8 9 10; do
        watch_view "$session" 2>/dev/null && break
        sleep 0.2
    done
    if [ -z "$VIEW_PID" ] || [ "${#VIEW_PIDS[@]}" -eq "$before" ]; then
        printf 'FAIL: watch_view recorded no pid, so the reaper has nothing to kill\n' >&2
        tmux kill-session -t "$session" 2>/dev/null || true
        exit 1
    fi
    if ! reap_views; then
        printf 'FAIL: the reaper could not reap a live view\n' >&2
        exit 1
    fi
    if kill -0 "$VIEW_PID" 2>/dev/null; then
        printf 'FAIL: the reaper reported a clean run with its victim still alive\n' >&2
        kill -9 "$VIEW_PID" 2>/dev/null || true
        exit 1
    fi
    tmux kill-session -t "$session" 2>/dev/null || true
    rm -rf "$tmp"
    VIEW_PIDS=()
    VIEW_PID=""

    # the call shape, across every script at once. The end-to-end above
    # proves the recorder works when called as a statement; only this
    # proves the legs call it that way, and the subshell form is invisible
    # at the call site -- the caller still gets a pid, and only the reaper
    # is left with nothing
    # spelled in two halves so this line is not itself the thing it refuses
    local needle subshelled
    needle='$(watch'"_view"
    subshelled=$(grep -rlF "$needle" "$REPO_ROOT/scripts/acceptance" 2>/dev/null | tr '\n' ' ' || true)
    if [ -n "$subshelled" ]; then
        printf 'FAIL: watch_view is called in a command substitution (%s), where the pid it records dies with the subshell\n' \
            "$subshelled" >&2
        exit 1
    fi
}
command -v tmux >/dev/null || {
    printf 'FAIL: tmux is required (every leg here drives a real terminal session)\n' >&2
    exit 1
}
check_view_reaping

# This host's machine-class name, in `budgets.toml`'s vocabulary.
#
# Derived the way the harness's own campaigns derive it
# (crates/view-harness/tests/heartbeat_cost.rs): the platform is all a
# machine can tell about itself, so a host is `dev-<platform>` unless an
# operator says otherwise. A `controlled-` class is a claim about a quiet
# machine that no probe here can verify, so it is declared and never
# guessed -- a guessed one turns an uncontrolled host's noise into a gate
# everyone downstream trusts.
#
# The declaration is `CLASS`, the same word `task bench` and `task
# perf-audit` already take (`CLASS=controlled-linux task acceptance`), so a
# machine's class is spelled one way across every target that asks.
host_class() {
    if [ -n "${CLASS:-}" ]; then
        printf '%s\n' "$CLASS"
        return
    fi
    case "$(uname -s)" in
    Darwin) printf 'dev-macos\n' ;;
    CYGWIN* | MINGW* | MSYS*) printf 'dev-windows\n' ;;
    *) printf 'dev-linux\n' ;;
    esac
}

# Ends the calling leg, successfully, when this host is outside its classes.
#
#   skip_unless_class remote-rtt controlled-linux
#
# A leg whose bound is armed on some classes only (`budgets.toml`'s
# `classes` field) has no bar to measure against anywhere else, and a bar
# invented for an uncontrolled host is worse than no measurement at all.
# Aborting is worse still: `task acceptance` runs its legs in sequence, so
# one refusal takes every later leg down with it and the drop is silent.
# Hence the announced skip, in a shape a gate log cannot be read past.
#
# It exits here rather than returning a code the caller acts on: every leg
# runs under `set -e`, so one written without the `|| exit 0` suffix would
# abort the sweep exactly the way this exists to stop. Leaves `CLASS`
# holding the resolved class, for a leg that has to pass it on.
skip_unless_class() {
    local leg="$1" want wanted
    shift
    CLASS=$(host_class)
    for want in "$@"; do
        if [ "$CLASS" = "$want" ]; then
            return 0
        fi
    done
    # joined first: a pattern applied to `$*` directly rewrites each
    # parameter and never the spaces bash joins them with
    wanted="$*"
    printf '%s: SKIPPED (class %s, host is %s)\n' "$leg" "${wanted// /, }" "$CLASS"
    exit 0
}

# The class gate against a known answer, in both directions, on every run.
#
# The direction nothing else catches is the gate stuck open: a
# `skip_unless_class` that skipped unconditionally would leave a declared
# controlled host printing SKIPPED, exiting 0, and the sweep reading green
# with the proof it exists for never taken. Two subshells, no processes
# beyond the `uname` each already pays for.
check_class_gate() {
    local out
    out=$(CLASS=dev-linux skip_unless_class selftest controlled-linux && echo ran)
    if [ "$out" != "selftest: SKIPPED (class controlled-linux, host is dev-linux)" ]; then
        printf 'FAIL: the class gate does not skip an out-of-class host (said: %s)\n' "$out" >&2
        exit 1
    fi
    out=$(CLASS=controlled-linux skip_unless_class selftest controlled-linux && echo ran)
    if [ "$out" != ran ]; then
        printf 'FAIL: the class gate skips a host inside its own class (said: %s)\n' "$out" >&2
        exit 1
    fi
}
check_class_gate

# Makes sure `path` is a binary this tree's source produced.
#
#   ensure_artifact "$VIEW_BIN" "$REPO_ROOT/target/release/view" \
#       cargo build --release -p view
#
# `default` is the path the caller uses when nothing was pinned; everything
# after it is the build command that produces it.
ensure_artifact() {
    local path="$1" default="$2"
    shift 2
    if [ "$path" = "$default" ]; then
        printf 'building %s\n' "${path#"$REPO_ROOT/"}" >&2
        (cd -- "$REPO_ROOT" && "$@" >&2) || {
            printf 'FAIL: %s could not be built (%s)\n' "${path#"$REPO_ROOT/"}" "$*" >&2
            return 1
        }
    else
        [ -x "$path" ] || {
            printf 'FAIL: no binary at %s (%s, or unset the override)\n' "$path" "$*" >&2
            return 1
        }
        local newer
        newer=$(find "$REPO_ROOT/crates" -name '*.rs' -newer "$path" -print -quit 2>/dev/null || true)
        if [ -n "$newer" ]; then
            printf 'FAIL: %s predates %s, so it is not the tree it would be measuring\n' \
                "$path" "${newer#"$REPO_ROOT/"}" >&2
            printf '      rebuild it (%s) or point at one that is current\n' "$*" >&2
            return 1
        fi
    fi
    [ -x "$path" ] || {
        printf 'FAIL: %s is not an executable file after the build that owns it\n' "$path" >&2
        return 1
    }
}

# A key in nvim notation as a terminal must type it, with `<leader>`
# expanded to nvim's own default -- which every fixture here leaves alone.
#
# A notation this cannot spell fails loudly rather than being skipped: an
# entry point nothing drives is the hole the legs that press keys exist to
# close.
tmux_key() {
    local lhs="$1" typed="${1/<leader>/\\}"
    case "$typed" in
    *'<'* | *'>'*)
        printf 'FAIL: %s carries a key notation this script cannot type; teach tmux_key to spell it\n' "$lhs" >&2
        return 1
        ;;
    esac
    printf '%s' "$typed"
}

# The review's own keys as `lhs verb` lines, out of the table the maps are
# generated from.
#
#   REVIEW_KEYS=$(review_keys_of "$MAPPINGS_RS") || exit 1
#
# Checked against the array's own declared length, because the reader
# recognizes the fields by name and in the order they are written: a renamed
# or reordered field would leave it silently short, and a leg that pressed
# nothing for the verb it dropped would report green.
review_keys_of() {
    local mappings_rs="$1" table declared read_count
    REVIEW_KEYS_SOURCE=$mappings_rs
    table=$(awk '
        /^static REVIEW_KEYS/ { inside = 1 }
        inside && /lhs: "/  { l = $0; sub(/.*lhs: "/, "", l); sub(/".*/, "", l) }
        inside && /verb: "/ { v = $0; sub(/.*verb: "/, "", v); sub(/".*/, "", v); print l, v }
        inside && /^\];/ { exit }
    ' "$mappings_rs")
    declared=$(grep -oE '^static REVIEW_KEYS: \[ReviewKey; [0-9]+\]' "$mappings_rs" |
        grep -oE '[0-9]+')
    read_count=$(printf '%s\n' "$table" | grep -c . || true)
    if [ -z "$declared" ] || [ "$read_count" != "$declared" ]; then
        printf 'FAIL: %s declares %s review keys and this read %s of them; the table has changed shape\n' \
            "$mappings_rs" "${declared:-no}" "$read_count" >&2
        return 1
    fi
    printf '%s\n' "$table"
}

# The key one review verb is pressed with, spelled for a terminal, out of
# the `REVIEW_KEYS` the caller read with `review_keys_of` -- which is also
# what names the file in the failure below.
review_key() {
    local verb="$1" lhs
    lhs=$(printf '%s\n' "$REVIEW_KEYS" | awk -v v="$verb" '$2 == v { print $1 }')
    [ -n "$lhs" ] || {
        printf 'FAIL: no review key invokes %s in %s any more\n' "$verb" "$REVIEW_KEYS_SOURCE" >&2
        return 1
    }
    tmux_key "$lhs"
}
