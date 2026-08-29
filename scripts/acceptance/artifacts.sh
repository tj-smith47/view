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
#
# One convention every acceptance script sourcing this file holds to: an
# acceptance assertion's expected color is read from the live scheme by
# probe, never from a config's text. A `sed` over a colorscheme's Lua can
# only ever assert the one scheme the repo ships, and it goes on asserting
# it after the run has been pointed somewhere else -- so what a group
# resolves to comes back from a headless nvim started under the very XDG
# environment the driven session uses. `scripts/check-style.sh` fails any
# script here that reads a fixture's colors with `sed` or `grep`, and
# `visual-sweep.sh`'s own header carries the long form.

# The macOS power assertion every timed run in this repo is held awake by,
# taken here in the sourcing leg's own shell so it is released when that leg
# is gone. One definition, in one file, for the reason its own doc gives.
# shellcheck source=scripts/hold-awake.sh
. "$REPO_ROOT/scripts/hold-awake.sh"

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

# The seconds between two `now` readings, to two decimals.
#
# Two decimals, not one: a recovery this fast rounds to a flat zero at one,
# and "recovers in 0.0s" reads as a measurement that did not happen.
elapsed() { awk -v a="$1" -v b="$2" 'BEGIN { printf "%.2f", b - a }'; }

# Whether a measurement is at or under a bound, whether it stands inside
# one, and the sum of two of them -- in awk, since the shell compares no
# fractions.
under() { awk -v v="$1" -v hi="$2" 'BEGIN { exit !(v <= hi) }'; }
in_range() { awk -v v="$1" -v lo="$2" -v hi="$3" 'BEGIN { exit !(v >= lo && v <= hi) }'; }
plus() { awk -v a="$1" -v b="$2" 'BEGIN { printf "%.2f", a + b }'; }

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
    # `comm` is the name on Linux and the whole invocation path on macOS,
    # so the basename is the one spelling both hosts agree on. `ucomm` is
    # not the alternative it looks like: it reports the binary a symlink
    # resolves to, which on macOS is what a stand-in named `view` is
    # recorded as -- the self-check below runs exactly that shape
    # `|| true` on the pipeline, not decoration: `ps` exits 1 for a pid that
    # has already gone, and under `set -euo pipefail` that status would end
    # the leg where it stands -- with none of the diagnostics below printed,
    # so the run reports nothing about the session it could not read
    local name
    name=$(ps -o comm= -p "$pane_pid" 2>/dev/null | tr -d ' ' || true)
    if [ "${name##*/}" = view ]; then
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

# The call shape, across every script at once, on every run.
#
# `watch_view` records into an array; a caller that wraps the call in a
# command substitution still gets the pid it wanted, so nothing at the call
# site looks wrong -- the record dies with the substitution's subshell and
# only the reaper, holding an empty list, is left reporting every run
# clean. That is how the recorder shipped broken once already.
#
# Walks all of `scripts`, not just the acceptance directory: the shape is
# wrong wherever it is written, and a helper one directory over is exactly
# where the next caller appears.
refuse_subshelled_watch_view() {
    # spelled in two halves so this line is not itself the thing it refuses
    local needle subshelled
    needle='$(watch'"_view"
    subshelled=$(grep -rlF "$needle" "$REPO_ROOT/scripts" 2>/dev/null | tr '\n' ' ' || true)
    if [ -n "$subshelled" ]; then
        printf 'FAIL: watch_view is called in a command substitution (%s), where the pid it records dies with the subshell\n' \
            "$subshelled" >&2
        exit 1
    fi
}

# Every exit path of the self-check, including the ones that abort it.
#
# The stand-in is a directory named `view` and the session holds a live
# process: an abort that left them behind would outlive the run it aborted,
# and the next run would find a `view` it never started.
selfcheck_abort() {
    printf 'FAIL: %s\n' "$1" >&2
    if [ -n "${VIEW_PID:-}" ]; then
        kill -9 "$VIEW_PID" 2>/dev/null || true
    fi
    tmux kill-session -t "${SELFCHECK_SESSION:-}" 2>/dev/null || true
    rm -rf "${SELFCHECK_TMP:-}"
    exit 1
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
    # local, so the two names this needs do not become globals in every
    # script that sources this file: bash's dynamic scoping still shows
    # them to `selfcheck_abort` while this frame is live, which is the only
    # reader either of them has
    local before SELFCHECK_TMP SELFCHECK_SESSION
    SELFCHECK_TMP=$(mktemp -d)
    SELFCHECK_SESSION="view-acc-selfcheck-$$"
    # a shell under the name the recorder looks for. The trailing `:` is
    # what keeps that name: given one command, a shell execs it and becomes
    # `sleep` instead of staying the `view` this has to find
    ln -s /bin/sh "$SELFCHECK_TMP/view"
    VIEW_PID=""
    tmux new-session -d -s "$SELFCHECK_SESSION" "$SELFCHECK_TMP/view -c 'sleep 300; :'" ||
        selfcheck_abort 'tmux could not start the session the self-check reaps'
    before=${#VIEW_PIDS[@]}
    for _ in 1 2 3 4 5 6 7 8 9 10; do
        watch_view "$SELFCHECK_SESSION" 2>/dev/null && break
        sleep 0.2
    done
    if [ -z "$VIEW_PID" ] || [ "${#VIEW_PIDS[@]}" -eq "$before" ]; then
        selfcheck_abort 'watch_view recorded no pid, so the reaper has nothing to kill'
    fi
    reap_views || selfcheck_abort 'the reaper could not reap a live view'
    if kill -0 "$VIEW_PID" 2>/dev/null; then
        selfcheck_abort 'the reaper reported a clean run with its victim still alive'
    fi
    tmux kill-session -t "$SELFCHECK_SESSION" 2>/dev/null || true
    rm -rf "$SELFCHECK_TMP"
    VIEW_PIDS=()
    VIEW_PID=""
}
refuse_subshelled_watch_view

# The live half runs only for a script that records views. It needs a tmux
# session of its own, and `remote-rtt.sh` starts none: it was aborting at
# source over a tool it never calls, with a message that was false for it.
# Read off the caller's own text rather than a flag it must remember to
# set, so a script is covered the run it starts recording.
if grep -q watch_view "${BASH_SOURCE[${#BASH_SOURCE[@]} - 1]}" 2>/dev/null; then
    command -v tmux >/dev/null || {
        printf 'FAIL: tmux is required (this leg drives a real terminal session)\n' >&2
        exit 1
    }
    check_view_reaping
fi

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

# Where the panel spells the glyph each transcript entry opens with.
TRANSCRIPT_RS=$REPO_ROOT/crates/view-core/src/native/ai_panel/transcript.rs

# The glyph the marker constant `name` holds in the panel's own source,
# with the escape it is written as resolved.
#
#   USER_MARK=$(mark_str USER_MARK) || exit 1
#
# Read rather than spelled here: a script carrying its own copy of a glyph
# keeps asserting a character the panel may have stopped painting, and
# passes every run until someone reads the screen by eye.
mark_str() {
    local name="$1" value
    value=$(grep -oE "^const $name: &str = \"[^\"]+\"" "$TRANSCRIPT_RS" |
        sed -E 's/.*"(.*)"/\1/') || true
    if [ -z "$value" ]; then
        printf 'FAIL: %s is not a marker constant in %s any more\n' \
            "$name" "$TRANSCRIPT_RS" >&2
        return 1
    fi
    printf '%b' "$(printf '%s' "$value" | sed -E 's/\\u\{([0-9a-fA-F]+)\}/\\u\1/g')"
}

# Every frame an animating marker cycles through, as one ERE alternation.
#
#   SPINNER_ALTERNATION=$(spinner_alternation) || exit 1
#
# Which frame is on screen depends on when the screen was read, so an
# assertion that something is still running has to accept any of them --
# and an entry whose turn is still in flight wears one of these in place of
# the marker its role would otherwise open with.
spinner_alternation() {
    local frames
    frames=$(awk '
        /^const SPINNER_FRAMES/ { inside = 1; next }
        inside && /^\];/ { exit }
        inside && match($0, /"[^"]+"/) {
            frame = substr($0, RSTART + 1, RLENGTH - 2)
            sub(/ $/, "", frame)
            printf "%s%s", (n++ ? "|" : ""), frame
        }
    ' "$TRANSCRIPT_RS")
    if [ -z "$frames" ]; then
        printf 'FAIL: SPINNER_FRAMES no longer lists the running marker frames in %s\n' \
            "$TRANSCRIPT_RS" >&2
        return 1
    fi
    printf '%b' "$(printf '%s' "$frames" | sed -E 's/\\u\{([0-9a-fA-F]+)\}/\\u\1/g')"
}

# The `&str` constant `name` holds, read from the file that owns it, `pub`
# or not: a key the source names is typed by a leg exactly as it is spelled
# there rather than copied into the script.
const_str() {
    local file="$1" name="$2" value
    value=$(grep -oE "(pub )?const $name: &str = \"[^\"]+\"" "$file" | sed -E 's/.*"(.*)"/\1/') || true
    if [ -z "$value" ]; then
        printf 'FAIL: %s is not a &str constant in %s any more\n' "$name" "$file" >&2
        return 1
    fi
    printf '%s' "$value"
}

# The value of a `Duration::from_secs` constant, read from the file that
# owns it. A threshold that moved and a script that did not would silently
# assert the wrong window, which is the one failure a timing acceptance
# cannot afford.
const_secs() {
    local file="$1" name="$2" value
    value=$(grep -oE "pub const $name: Duration = Duration::from_secs\([0-9]+\)" "$file" |
        grep -oE '[0-9]+' | tail -1) || true
    if [ -z "$value" ]; then
        printf 'FAIL: %s is not a from_secs constant in %s any more\n' "$name" "$file" >&2
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

# The pane as it stands, for the session the leg drives. Empty rather than
# failing when there is no session to capture: the callers are assertions,
# and a capture that failed is a screen holding none of what they look for.
pane() { tmux capture-pane -t "$SESSION" -p 2>/dev/null || true; }

# Whether `haystack` holds `needle`, literally.
#
# `case` rather than a pipe into `grep -q`: `grep -q` exits at its first
# match and kills whatever is feeding it with SIGPIPE, which `set -o
# pipefail` then reports as a failed pipeline -- so a match reads as no
# match and the assertion built on it passes for the wrong reason. A
# captured string has no pipe to fail.
#
# The one thing the glob does that `grep -F` did not: it spans the newlines
# between rows, so a needle written across a row boundary would match here
# where a line-at-a-time search never matched. Every needle passed in today
# is a single on-screen string.
holds() { case "$2" in *"$1"*) return 0 ;; *) return 1 ;; esac; }

# Whether `haystack` matches the extended regular expression `needle`. The
# no-pipe form of `grep -qE`, for the same reason `holds` is `grep -qF`'s.
#
# A row at a time, not the capture as one string: `[[ =~ ]]` over the whole
# pane would give `^` and `$` the ends of the capture, where every pattern
# written for `grep -qE` means the ends of a row. Reading it line by line is
# what keeps an anchored pattern meaning what its author wrote.
matches() {
    local line
    while IFS= read -r line; do
        if [[ $line =~ $1 ]]; then
            return 0
        fi
    done <<<"$2"
    return 1
}

# What `<leader>` expands to when a caller has not read one off the config
# it is driving: nvim's own default, which every fixture here leaves alone.
# A caller that drives someone else's config sets this from `vim.g.mapleader`
# -- a config that rebinds it (a space, almost always) types every mapped
# key differently, and a leg typing the default would press nothing and
# report the feature as unreachable.
LEADER=${LEADER:-\\}

# A key in nvim notation as a terminal must type it, with `<leader>`
# expanded per [`LEADER`].
#
# A notation this cannot spell fails loudly rather than being skipped: an
# entry point nothing drives is the hole the legs that press keys exist to
# close.
tmux_key() {
    local lhs="$1" typed=${1/<leader>/"$LEADER"}
    case "$typed" in
    *'<'* | *'>'*)
        printf 'FAIL: %s carries a key notation this script cannot type; teach tmux_key to spell it\n' "$lhs" >&2
        return 1
        ;;
    esac
    printf '%s' "$typed"
}

# A named key in nvim's `<...>` notation as tmux spells it: the two agree on
# everything the modal legs bind once the brackets are off.
#
# What a leg types into the session it drives: text literally, and a key by
# the name tmux knows it under.
send_text() { tmux send-keys -t "$SESSION" -l -- "$1"; }
send_key() { tmux send-keys -t "$SESSION" "$1"; }

# The session the leg drove, gone.
end_session() {
    tmux kill-session -t "$SESSION" 2>/dev/null || true
}

# One assertion that held, named with the leg that was running.
pass() { printf 'ok   [%s] %s\n' "$CURRENT_LEG" "$1"; }

# One assertion the driven config put out of reach, named with the leg and
# with the reason it could not be made. A skip is not a pass: it prints on
# its own marker so a reader counting green legs cannot read it as one, and
# it exists so that a config which owns a key or a surface view also wants
# leaves a line saying exactly that rather than a leg failing for a reason
# the log does not carry.
skip() { printf 'skip [%s] %s\n' "$CURRENT_LEG" "$1"; }

# Waits for `pattern` to be on the pane, answering how long that took, and
# for it to leave again.
#
# `fail` is the caller's own: each leg reports a miss in its own vocabulary
# and keeps its own dumps, which is why these two live here and that one
# does not.
wait_on_pane() {
    local pattern="$1" budget="$2" what="$3" start el
    start=$(now)
    while :; do
        if holds "$pattern" "$(pane)"; then
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
        if ! holds "$pattern" "$(pane)"; then
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

# Beside `tmux_key` and not inside it: that one refuses bracket notation on
# purpose, because a mapping's own lhs is typed literally and a `<...>` it
# cannot spell is a leg pressing nothing. This one is given the bracketed
# name deliberately. Two jobs, two names -- they were one name in two files
# before, and the copy that loaded last decided which rule a script got.
tmux_named_key() {
    local notation="$1"
    notation=${notation#<}
    printf '%s' "${notation%>}"
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
        grep -oE '[0-9]+') || true
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
