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
[ "$SITES_SEEN" -eq "$((SITES_ALL - 1))" ]
check "every sweep mention of rust_const is a call site this file grades" 0 $?

while read -r var name; do
    [ -n "$name" ] || continue
    eval "rs=\$$var"
    [ -n "$(rust_const "$rs" "$name" 2>/dev/null)" ]
    check "the sweep reads $name out of ${rs#"$ROOT"/}" 0 $?
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

[ -z "$(unparted_escapes "$ROOT"/scripts/acceptance/*.sh)" ]
check "no acceptance leg writes a key into the window an Escape is held for" 0 $?

PLANTED=$(mktemp "$(scratch_root)/reader-cases-XXXXXX")
trap 'rm -f "$PLANTED"' EXIT
cat >"$PLANTED" <<'PLANT'
leg_planted() {
    send_key Escape
    send_text ':View ai close'
}
PLANT
[ -n "$(unparted_escapes "$PLANTED")" ]
check "an unparted Escape is found rather than read as a parted one" 0 $?

printf '%s cases, %s failures\n' "$cases" "$failures"
[ "$failures" -eq 0 ] || exit 1
