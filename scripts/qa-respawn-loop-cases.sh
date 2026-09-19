#!/usr/bin/env bash
#
# Cases for the two decisions in scripts/qa-respawn-loop.sh that are made
# before any session is driven: which pids a run is allowed to touch, and
# which shape names it accepts. Both shipped wrong -- an empty view pid
# named init and kthreadd, and an unknown shape measured the plain one
# under the wrong name -- and neither is visible from a tally.
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

status=0
"$LOOP" --shape no-such-shape --runs 1 >/dev/null 2>&1 || status=$?
if [ "$status" -eq 2 ]; then
    report ok 'an unknown shape is refused'
else
    report fail 'an unknown shape is refused' "status:$status"
fi

if [ "$FAILED" -eq 0 ]; then
    printf 'qa-respawn-loop cases: ok\n'
    exit 0
fi
exit 1
