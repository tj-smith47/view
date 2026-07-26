#!/usr/bin/env bash
# The bench gate reads budgets.toml; a person reads spec 3.1. Two places
# holding the same number is drift waiting to happen, and the direction that
# matters is silent: a budget loosened in the file alone would gate green
# against a bar the spec never agreed to.
#
# So every [[budget]] entry must name a spec_row that appears in the spec,
# and its max must appear in that same row's text.
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
budgets="$root/crates/view-bench/budgets.toml"
spec="$root/.claude/specs/2026-07-17-view-design.md"

for f in "$budgets" "$spec"; do
  [[ -f "$f" ]] || { echo "BUDGET DRIFT FAIL: $f not found" >&2; exit 1; }
done

fail=0
entries=0

# Reads the [[budget]] blocks: spec_row, max. Deliberately not a TOML parser;
# the file's own loader is the parser of record and its schema test guards
# the shape. This only needs the two fields to cross-check.
while IFS=$'\t' read -r spec_row max; do
  entries=$((entries + 1))

  # the spec writes arrows as U+2192 and wraps identifiers in backticks where
  # budgets.toml writes them plainly, so both sides are normalised and then
  # matched whole. Matching a prefix instead would accept a spec_row edited to
  # point at a different row that merely starts the same way.
  row="$(sed 's/\xe2\x86\x92/->/g; s/`//g' "$spec" | grep -F "$spec_row" | head -1 || true)"
  if [[ -z "$row" ]]; then
    echo "BUDGET DRIFT FAIL: spec_row \"$spec_row\" matches no line in the spec" >&2
    fail=1
    continue
  fi

  # 232.0 in TOML is written 232 in the spec's prose; accept either
  trimmed="${max%.0}"
  if ! grep -qF "$trimmed" <<<"$row"; then
    echo "BUDGET DRIFT FAIL: budgets.toml bounds \"$spec_row\" at $max, but that number does not appear in the spec row:" >&2
    echo "  ${row:0:200}" >&2
    fail=1
  fi
done < <(awk '
  /^\[\[budget\]\]/ { row=""; max=""; next }
  /^spec_row = / { row=$0; sub(/^spec_row = "/, "", row); sub(/"$/, "", row) }
  /^max = / { max=$0; sub(/^max = /, "", max); if (row != "") print row "\t" max }
' "$budgets")

if [[ $entries -eq 0 ]]; then
  echo "BUDGET DRIFT FAIL: no [[budget]] entries found in $budgets" >&2
  exit 1
fi

if [[ $fail -ne 0 ]]; then
  exit 1
fi
echo "budget drift: $entries budget(s) cross-checked against spec 3.1"
