#!/usr/bin/env bash
# One verdict line of a case file, sourced by the case files that print
# `ok` and `FAIL` rows. Sourced, never run. The caller starts FAILED at 0
# and exits with it.

# Usage: report ok NAME, or report fail NAME WHY
report() {
    if [ "$1" = ok ]; then
        printf 'ok   %s\n' "$2"
    else
        printf 'FAIL %s: %s\n' "$2" "$3"
        # the case file that sources this one reads it for its exit status
        # shellcheck disable=SC2034
        FAILED=1
    fi
}
