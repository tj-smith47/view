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
CONF="$ROOT/scripts/acceptance/ai-conformance.sh"

# the shipped definitions rather than copies: a copy here would keep
# passing while the reader the legs actually call drifted away from it
eval "$(grep -m 1 -- '^holds() {' "$CONF")"
eval "$(awk '/^matches\(\) \{/,/^\}/' "$CONF")"

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

printf '%s cases, %s failures\n' "$cases" "$failures"
[ "$failures" -eq 0 ] || exit 1
