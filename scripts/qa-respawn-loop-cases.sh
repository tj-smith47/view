#!/usr/bin/env bash
#
# Cases for the three decisions in scripts/qa-respawn-loop.sh that are made
# outside a driven session: which pids a run is allowed to touch, which of
# them it reports as left behind, and which shape names it accepts. All
# three shipped wrong -- an empty view pid named init and kthreadd, a
# respawn's own engine was never sampled, and an unknown shape measured the
# plain one under the wrong name -- and none of them is visible from a
# tally.
set -euo pipefail

SCRIPT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
LOOP=$SCRIPT_DIR/qa-respawn-loop.sh
FAILED=0

# shellcheck source=/dev/null
QA_RESPAWN_SOURCED=1 . "$LOOP"

report() {
    if [ "$1" = ok ]; then
        printf 'ok   %s\n' "$2"
    else
        printf 'FAIL %s: %s\n' "$2" "$3"
        FAILED=1
    fi
}

# `task audit` runs this file inside `task ci`, and CI's windows leg runs
# that under Git Bash, which ships no pgrep. The two cases below grade a
# guard around pgrep itself, so where the tool is absent there is no guard
# to grade -- named out loud rather than passed over, the way
# scripts/acceptance/artifacts.sh prints its class skips, because a case
# that quietly stops running is a case nobody can tell from a green one.
if command -v pgrep >/dev/null 2>&1; then
    named=$(engine_children '')
    if [ -n "$named" ]; then
        report fail 'an empty view pid names no children' "named:$named"
    else
        report ok 'an empty view pid names no children'
    fi

    sleep 30 &
    FIXTURE=$!
    named=$(engine_children "$$")
    case " $named " in
        (*" $FIXTURE "*) report ok 'a live pid names its own children' ;;
        (*) report fail 'a live pid names its own children' "named:$named" ;;
    esac
    kill "$FIXTURE" 2>/dev/null || true
    wait "$FIXTURE" 2>/dev/null || true
else
    printf 'SKIPPED the two pid cases (no pgrep on this host)\n'
fi

# `still_running` needs no pgrep, so it is graded everywhere. The duplicate
# is the shape the teardown re-sample produces: the same engine, sampled
# once before the quit and once after it.
sleep 30 &
FIXTURE=$!
live=$(still_running "$FIXTURE" "$FIXTURE")
if [ "$live" = " $FIXTURE" ]; then
    report ok 'a pid sampled twice is reported once'
else
    report fail 'a pid sampled twice is reported once' "live:$live"
fi
kill "$FIXTURE" 2>/dev/null || true
wait "$FIXTURE" 2>/dev/null || true
live=$(still_running "$FIXTURE")
if [ -z "$live" ]; then
    report ok 'a reaped pid is reported by nobody'
else
    report fail 'a reaped pid is reported by nobody' "live:$live"
fi

# Graded on what it says and not on its status: 2 is also what the script
# returns for a missing view binary, a missing tmux and a missing nvim
# configuration, and the audit job has none of the three -- so a status-only
# case passes with the whole shape validation deleted.
said=$("$LOOP" --shape no-such-shape --runs 1 2>&1 >/dev/null || true)
case "$said" in
    (*"unknown shape: no-such-shape"*) report ok 'an unknown shape is refused by name' ;;
    (*) report fail 'an unknown shape is refused by name' "said:$said" ;;
esac

# Graded the same way and for the same reason: a signal name `kill` refuses
# is refused once here, where an unvalidated one is refused once per run and
# reads as thirty failures of the measurement.
said=$("$LOOP" --shape signal-settled --signal NOPE --runs 1 2>&1 >/dev/null || true)
case "$said" in
    (*"unknown signal: NOPE"*) report ok 'an unknown signal is refused by name' ;;
    (*) report fail 'an unknown signal is refused by name' "said:$said" ;;
esac

# `await_reaped` is the signal shapes' whole assertion, so both of its
# answers are graded: a pid that never leaves has to come back on the bound
# naming itself, and one already gone has to come back at once saying
# nothing. A wait that returned 0 on the bound would report every stray as
# a clean reaping.
sleep 30 &
FIXTURE=$!
if left=$(await_reaped 1 "$FIXTURE"); then
    report fail 'a pid that never leaves is reported on the bound' "left:$left"
else
    case " $left " in
        (*" $FIXTURE "*) report ok 'a pid that never leaves is reported on the bound' ;;
        (*) report fail 'a pid that never leaves is reported on the bound' "left:$left" ;;
    esac
fi
kill "$FIXTURE" 2>/dev/null || true
wait "$FIXTURE" 2>/dev/null || true
if left=$(await_reaped 1 "$FIXTURE"); then
    if [ -z "$left" ]; then
        report ok 'a reaped pid ends the wait saying nothing'
    else
        report fail 'a reaped pid ends the wait saying nothing' "left:$left"
    fi
else
    report fail 'a reaped pid ends the wait saying nothing' "left:$left"
fi

# The pre-attach shape waits on this rather than on a screen, and a session
# name nobody started has to answer no: answering yes would signal a pid the
# loop never watched start.
if engine_appeared 'view-qa-no-such-session'; then
    report fail 'a session that does not exist has no engine' 'said yes'
else
    report ok 'a session that does not exist has no engine'
fi

if [ "$FAILED" -eq 0 ]; then
    printf 'qa-respawn-loop cases: ok\n'
    exit 0
fi
exit 1
