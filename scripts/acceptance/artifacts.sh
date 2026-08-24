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
