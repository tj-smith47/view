#!/usr/bin/env bash
#
# A power assertion for the length of a timed run, on the one host class
# that needs one. Sourced by a script that measures, or executed as a
# wrapper around one: `bash scripts/hold-awake.sh cargo bench -p view-core`.
#
# Every threshold this repo asserts is a duration the code under test reads
# off a monotonic clock, and every wait that bounds it is wall time read off
# `date`. macOS parts the two on its own: on AC and unattended, it takes a
# maintenance sleep whenever it likes (`pmset -g log`: "Entering Sleep state
# due to 'Maintenance Sleep'"), and a Mach monotonic clock does not advance
# across one. Measured on this repo's own macOS host, view's log advanced
# 11s over 313s of wall; a 30s escalation came up 21s short of its threshold
# after 50s of wall. A bench row measured across one is a number nothing
# produced, a wait that has been sleeping five minutes reads as a five-minute
# stall in whatever it was waiting on, and none of it is the code's.
#
# This is the one place the assertion is taken, because a second copy is a
# copy that drifts: `crates/view-oracle/tests/macos_clock.rs` walks the
# Taskfile and the workflows and fails on a timed harness that does not come
# through here.
#
# `-w $$` rather than a trap: the assertion is released when this shell is
# gone, however it went, including the kill an aborted run takes. `exec`
# below keeps that pid, so the assertion tracks the harness itself.
if [ "$(uname -s)" = Darwin ]; then
    if command -v caffeinate >/dev/null 2>&1; then
        caffeinate -dims -w $$ &
    else
        # announced rather than skipped quietly: without the assertion every
        # threshold is measured against a clock that stops, and a run that
        # fails for that reads exactly like one that failed for the code
        printf 'WARNING: no caffeinate on this Darwin host, so nothing holds it awake -- a maintenance sleep will read as a stalled threshold\n' >&2
    fi
fi

# Executed rather than sourced, and given something to run: hold the
# assertion and become the harness.
if [ "${BASH_SOURCE[0]}" = "$0" ] && [ "$#" -gt 0 ]; then
    exec "$@"
fi
