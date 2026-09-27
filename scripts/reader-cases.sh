#!/usr/bin/env bash
# The readers the acceptance legs assert with, on one fixed two-row
# capture.
#
# Three of them coexist and each is right only for its own call sites:
# `matches` reads a row at a time, `holds` matches across the newline
# between rows, and the theme-cache reader is anchored to a whole line. The
# distinction is invisible until a pattern carries an anchor or a needle
# spans a row, at which point the wrong reader answers confidently. This is
# where the next reader added trips over it.
set -uo pipefail

ROOT=$(cd -- "$(dirname -- "$0")/.." && pwd)
# shellcheck source=scripts/lib/scratch.sh
. "$ROOT/scripts/lib/scratch.sh"
SHARED="$ROOT/scripts/acceptance/artifacts.sh"

# the shipped definitions rather than copies: a copy here would keep
# passing while the reader the legs actually call drifted away from it.
# Lifted out rather than sourced -- sourcing the shared helper takes a power
# assertion, resolves the target root and runs the class gate, none of which
# a unit that reads two strings has any business doing
eval "$(grep -m 1 -- '^holds() {' "$SHARED")"
eval "$(awk '/^matches\(\) \{/,/^\}/' "$SHARED")"

# the form `matches` replaced, kept only as the thing the cases below
# measure the distinction against
whole_capture() { [[ $2 =~ $1 ]]; }

CAPTURE=$'first row here\nsecond row: | think'

cases=0
failures=0

# `check` takes the status as an argument because the caller has to run the
# reader itself: running it here would put it behind a function boundary
# that hides which reader answered
check() {
    cases=$((cases + 1))
    if [ "$2" = "$3" ]; then
        return 0
    fi
    printf 'FAIL: %s -- expected rc %s, got %s\n' "$1" "$2" "$3" >&2
    failures=$((failures + 1))
}

# a test run as its own case: the status of a `[` read through `$?` on the
# next line is one shellcheck cannot tell from a command substitution inside
# the brackets, and a reordering that put one between them would grade the
# substitution
check_that() {
    local desc=$1
    shift
    "$@"
    check "$desc" 0 $?
}

# `matches`: a row at a time, so an anchor means the ends of a row and a
# needle does not span the newline -- and every answer is the one `grep -qE`
# gives, which is what the patterns in the legs were written against
for pattern in '^second row' 'here$' '^first row here$' 'here.second' '^nothing like this' '(\||/) think'; do
    matches "$pattern" "$CAPTURE"
    mine=$?
    grep -qE -- "$pattern" <<<"$CAPTURE"
    reference=$?
    check "matches /$pattern/ answers what grep -qE answers" "$reference" "$mine"
done

# the distinction itself: over the whole capture as one string, `^` and `$`
# are the ends of the capture and a needle spans the newline
whole_capture '^second row' "$CAPTURE"
check "the whole-capture form reads a row anchor as a miss" 1 $?
whole_capture 'here.second' "$CAPTURE"
check "the whole-capture form reads across the newline" 0 $?

# `holds`: literal, and across rows by design -- the one thing its glob
# does that `grep -F` never did
holds 'row here' "$CAPTURE"
check "holds finds a literal inside a row" 0 $?
holds $'here\nsecond' "$CAPTURE"
check "holds spans the newline between two rows" 0 $?
holds 'not on this screen' "$CAPTURE"
check "holds reports a needle nothing carries" 1 $?

# the theme-cache reader: `-x` anchors the pattern to a whole line, which
# is neither of the other two
grep -xE -- 'first row here' <<<"$CAPTURE" >/dev/null
check "grep -xE matches a whole row" 0 $?
grep -xE -- 'first row' <<<"$CAPTURE" >/dev/null
check "grep -xE refuses a partial row" 1 $?

# The `&str` constants the visual sweep reads out of Rust source. Checked
# here as well as on the sweep own run, because that run needs tmux and a
# live nvim: a source the sweep can no longer read is a red acceptance leg
# on a machine that can take one and nothing at all on `task ci`, which is
# how the message-history title went unread from the commit that stopped
# writing it as a literal until CI reached the sweep. The population is
# taken from the sweep own call sites, so a constant added there is graded
# without this file being touched.
SWEEP="$ROOT/scripts/acceptance/visual-sweep.sh"
eval "$(awk '/^rust_const\(\) \{/,/^\}/' "$SWEEP")"
# the sources are named by the sweep own `*_RS` assignments, lifted rather
# than copied for the reason the readers above are
REPO_ROOT="$ROOT"
eval "$(grep -E '^[A-Z_]+_RS=\$REPO_ROOT/' "$SWEEP")"

CONST_SITES=$(grep -oE 'rust_const "\$[A-Z_]+_RS" [A-Z_]+' "$SWEEP" |
    tr -d '"$' | sed -E 's/^rust_const +//')
# a call-site spelling this file can no longer find grades nothing at all,
# which is the silence the cases below exist to refuse. Counted against the
# sweep own mentions rather than a floor: a floor passes a fourth read added
# in a spelling the pattern above cannot see, which is the same silence
# arriving one call site later. The one subtracted is the definition
SITES_SEEN=$(printf '%s\n' "$CONST_SITES" | grep -c .)
SITES_ALL=$(grep -c 'rust_const' "$SWEEP")
check_that "every sweep mention of rust_const is a call site this file grades" \
    [ "$SITES_SEEN" -eq "$((SITES_ALL - 1))" ]

while read -r var name; do
    [ -n "$name" ] || continue
    rs=${!var}
    check_that "the sweep reads $name out of ${rs#"$ROOT"/}" \
        [ -n "$(rust_const "$rs" "$name" 2>/dev/null)" ]
done <<<"$CONST_SITES"

rust_const "$PALETTE_RS" A_CONSTANT_NO_SOURCE_DECLARES >/dev/null 2>&1
check "a constant no source declares fails rather than reading as an empty title" 1 $?

# An Escape written straight before another keystroke. view holds a lone
# Escape for `ttimeoutlen` in case more of a sequence is coming, so a write
# that lands inside that window joins the run and decodes as a chord --
# `<M-:>`, `<M-G>` -- which no leg means to send and no overlay answers.
# Graded here because the legs themselves need tmux and a live nvim: the
# sweep dismissal shipped this shape and passed for months on the legs that
# happened not to depend on the close.
unparted_escapes() {
    awk '
        /^[[:space:]]*send_key Escape[[:space:]]*$/ {
            site = FILENAME ":" FNR
            scanning = 1
            next
        }
        scanning == 0 { next }
        /^[[:space:]]*(#|$)/ { next }
        # the enclosing block ends the scan: a write opening the next
        # function is nothing this Escape can reach
        /^\}/ || /^[a-zA-Z_]+\(\)/ { scanning = 0; next }
        /send_text|send_key|command_line|submit |tmux send-keys/ {
            print site
            scanning = 0
            next
        }
        /part_escape|settle|sleep|wait_|until_gone|capture/ { scanning = 0 }
    ' "$@"
}

check_that "no acceptance leg writes a key into the window an Escape is held for" \
    [ -z "$(unparted_escapes "$ROOT"/scripts/acceptance/*.sh)" ]

PLANTED=$(mktemp "$(scratch_root)/reader-cases-XXXXXX")
trap 'rm -f "$PLANTED"' EXIT
cat >"$PLANTED" <<'PLANT'
leg_planted() {
    send_key Escape
    send_text ':View ai close'
}
PLANT
check_that "an unparted Escape is found rather than read as a parted one" \
    [ -n "$(unparted_escapes "$PLANTED")" ]

# Every default key the engine registers, walked through the shapes the
# entry-points leg knows. Twenty of twenty-five once had none, and the leg
# failed at the first of them on a machine that could run it and passed
# `task ci` everywhere. A surface's marker is resolved from the sweep's
# own arms with its runtime titles stood in.
eval "$(awk '/^entry_shape\(\) \{/,/^\}/' "$SWEEP")"
eval "$(awk '/^entry_points_of\(\) \{/,/^\}/' "$SWEEP")"
eval "$(awk '/^marker_for\(\) \{/,/^\}/' "$SWEEP")"
eval "$(awk '/^PICKER_MARKERS=\$\(awk/,/^'"'"' "\$SURFACES_RS" "\$PICKER_RS"\)$/' "$SWEEP")"
DRIVES=$(awk '/^drive_action\(\) \{/,/^\}/' "$SWEEP" | grep -E '^    [a-z][a-z |]*\)$' | tr -d ' )' | tr '|' '\n')
# read by the sweep functions evaluated above, which shellcheck cannot follow
# shellcheck disable=SC2034
ROOT=/sweep/root HISTORY_TITLE=history PANEL_TITLE=panel NARROW_FOCUSED_TITLE=narrow PROMPT_MARK='>'
ROWS_SEEN=0
while read -r feature lhs verb; do
    [ -n "$feature" ] || continue
    ROWS_SEEN=$((ROWS_SEEN + 1))
    shape=$(entry_shape "$feature" "$verb" 2>/dev/null)
    check "the $feature $verb key ($lhs) has a shape the entry-points leg knows" 0 $?
    case "$shape" in
    (surface) [ -n "$(marker_for "$feature" "$verb" 2>/dev/null)" ] ;;
    (pause) true ;;
    (*) grep -Fqx -- "$shape" <<<"$DRIVES" ;;
    esac
    check "the $feature $verb key ($lhs) has a marker or a drive for its $shape shape" 0 $?
done <<<"$(entry_points_of "$MAPPINGS_RS")"
check_that "the walk read every row DEFAULT_MAPS declares" \
    [ "$ROWS_SEEN" -eq "$(grep -oE '^static DEFAULT_MAPS: \[MappingSpec; [0-9]+\]' "$MAPPINGS_RS" | grep -oE '[0-9]+' | tail -1)" ]
entry_shape brand new >/dev/null 2>&1
check "a pair with no shape fails the leg" 1 $?

# The staleness check the legs run on a binary before driving it. A test
# file the binary never compiled was once enough to fail it, which pushed a
# run into touching a binary by hand.
eval "$(awk '/^newer_source\(\) \{/,/^\}/' "$SHARED")"
BUILT=$(mktemp -d "$(scratch_root)/reader-cases-built-XXXXXX")
trap 'rm -f "$PLANTED"; rm -rf "$BUILT"' EXIT
mkdir -p "$BUILT/crates/a/src" "$BUILT/crates/a/tests" "$BUILT/target/debug"
BIN=$BUILT/target/debug/view
: >"$BUILT/crates/a/src/lib.rs"
: >"$BUILT/crates/a/src/with space.rs"
: >"$BUILT/crates/a/tests/live.rs"
: >"$BIN"
printf '%s: %s crates/a/src/with\\ space.rs\n' "$BIN" "$BUILT/crates/a/src/lib.rs" >"$BIN.d"
touch -t 202601010000 "$BUILT/crates/a/src/lib.rs" "$BUILT/crates/a/src/with space.rs" \
    "$BUILT/crates/a/tests/live.rs"
touch -t 202601010100 "$BIN"
# read by newer_source, evaluated above out of the shared leg code
# shellcheck disable=SC2034
REPO_ROOT=$BUILT
check_that "a binary built after every source it lists is current" \
    [ -z "$(newer_source "$BIN")" ]
touch -t 202601010200 "$BUILT/crates/a/tests/live.rs"
check_that "an edit to a test the binary never compiled leaves it current" \
    [ -z "$(newer_source "$BIN")" ]
touch -t 202601010200 "$BUILT/crates/a/src/with space.rs"
check_that "an edit to a source the dep-info lists makes the binary stale" \
    [ "$(newer_source "$BIN")" = "$BUILT/crates/a/src/with space.rs" ]
touch -t 202601010000 "$BUILT/crates/a/src/with space.rs"
rm "$BIN.d"
[ "$(newer_source "$BIN")" = "$BUILT/crates/a/tests/live.rs" ]
check "with no dep-info every source under crates/ is compared" 0 $?

printf '%s cases, %s failures\n' "$cases" "$failures"
[ "$failures" -eq 0 ] || exit 1
