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
arrow=$'\xe2\x86\x92'

# Reads the [[budget]] blocks: spec_row, max. Deliberately not a TOML parser;
# the file's own loader is the parser of record and its schema test guards
# the shape. This only needs the two fields to cross-check.
while IFS=$'\t' read -r spec_row max; do
  entries=$((entries + 1))

  # the spec writes arrows as U+2192 and wraps identifiers in backticks where
  # budgets.toml writes them plainly, so both sides are normalised and then
  # matched whole. Matching a prefix instead would accept a spec_row edited to
  # point at a different row that merely starts the same way.
  # The arrow's bytes are expanded by the shell, not by sed: \xNN inside a sed
  # expression is a GNU extension, so spelling it there would make the
  # normalisation, and with it the gate's verdict, depend on the host's sed.
  row="$(sed "s/$arrow/->/g; s/\`//g" "$spec" | grep -F "$spec_row" | head -1 || true)"
  if [[ -z "$row" ]]; then
    echo "BUDGET DRIFT FAIL: spec_row \"$spec_row\" matches no line in the spec" >&2
    fail=1
    continue
  fi

  # 232.0 in TOML and 232 in the spec's prose are the same value, and so are
  # 1.1 and 1.10 -- a plain substring match on the trimmed text handles
  # neither correctly: it missed 1.1-written-as-1.10, and separately it
  # accepted $max's digits appearing inside an unrelated larger number ("6"
  # inside this very row's own "4.962" headroom citation). Extracting every
  # number-shaped token from the row and comparing each to $max as a float
  # fixes both: token boundaries stop the false match, and numeric (not
  # string) equality stops the false miss.
  found=0
  while IFS= read -r num; do
    if awk -v a="$num" -v b="$max" 'BEGIN{exit !(a+0==b+0)}'; then
      found=1
      break
    fi
  done < <(grep -oE '[0-9]+(\.[0-9]+)?' <<<"$row")
  if [[ $found -ne 1 ]]; then
    echo "BUDGET DRIFT FAIL: budgets.toml bounds \"$spec_row\" at $max, but that number does not appear as its own value in the spec row:" >&2
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
