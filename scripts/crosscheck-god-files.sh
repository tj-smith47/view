#!/usr/bin/env bash
# Compares the god-file gate's production-line counter against an
# independently written one over exactly the files the gate measures.
#
# The interpreter is resolved rather than hard-coded: Windows runners ship
# `python` with no `python3` alias, and a cross-check that skipped there would
# leave the counter unverified on the platform whose path handling differs
# most. No interpreter at all is a hard failure, never a skip.
set -euo pipefail
cd "$(git rev-parse --show-toplevel 2>/dev/null || pwd)"

PY=""
for candidate in python3 python py; do
    if command -v "$candidate" >/dev/null 2>&1; then
        PY="$candidate"
        break
    fi
done
if [[ -z "$PY" ]]; then
    echo "crosscheck: no python interpreter (tried python3, python, py); the" >&2
    echo "  god-file counter cannot be cross-checked and must not be trusted" >&2
    exit 1
fi
if [[ "$PY" == py ]]; then
    set -- -3
fi

bash scripts/audit-god-files.sh --counts |
    "$PY" ${1+"$@"} scripts/audit-god-files-crosscheck.py --compare
