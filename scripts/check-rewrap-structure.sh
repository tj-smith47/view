#!/usr/bin/env bash
# What a re-wrap spent, read across two revisions rather than inside one tree.
# check-style.sh reddens a bullet a wrap pulled into the line above it, but the
# other half of that damage -- a paragraph break the wrap deleted -- is
# ordinary prose in the tree that ships it, and only the revision before it
# says the break was there.
#
# Reported and never gating, which is what the measurement supports: the merge
# shape below names a paragraph that legitimately gained a sentence as readily
# as one a wrap swallowed, so the verdict belongs to whoever ran the sweep.
# The status is 0 whatever it finds.
#
# usage: scripts/check-rewrap-structure.sh BASE HEAD
set -uo pipefail

if [ "$#" -ne 2 ]; then
  printf 'usage: %s BASE HEAD\n' "$0" >&2
  exit 2
fi
base="$1"
head_rev="$2"

for rev in "$base" "$head_rev"; do
  if ! git rev-parse --verify -q "$rev^{commit}" >/dev/null; then
    printf '%s: bad revision: %s\n' "$0" "$rev" >&2
    exit 2
  fi
done

WORK=$(mktemp -d)
trap 'rm -rf "$WORK"' EXIT

# each blank-separated block on one line, fenced samples dropped and every run
# of whitespace flattened, so that a re-wrap of the same words is no difference
# and the structure around them is the whole subject
BLOCKS_AWK='
  /^[[:space:]]*(```|~~~)/ {
    if (cur != "") { print cur; cur = "" }
    fenced = !fenced
    next
  }
  fenced { next }
  /^[[:space:]]*$/ { if (cur != "") { print cur; cur = "" } next }
  {
    line = $0
    gsub(/[[:space:]]+/, " ", line)
    sub(/^ /, "", line)
    sub(/ $/, "", line)
    cur = (cur == "") ? line : cur " " line
  }
  END { if (cur != "") print cur }
'

for page in $(git diff --name-only "$base" "$head_rev" -- "*.md"); do
  git show "$base:$page" > "$WORK/base" 2>/dev/null || continue
  git show "$head_rev:$page" > "$WORK/head" 2>/dev/null || continue
  # the counts are read only where the words did not change, because a page
  # that gained a sentence legitimately gains and loses structure with it and
  # every such page would report. A sweep is a re-wrap and nothing else, so on
  # the pages it touched this is the whole population
  base_words=$(tr -s '[:space:]' ' ' < "$WORK/base")
  head_words=$(tr -s '[:space:]' ' ' < "$WORK/head")
  if [ "$base_words" = "$head_words" ]; then
    base_blanks=$(grep -c '^[[:space:]]*$' "$WORK/base")
    head_blanks=$(grep -c '^[[:space:]]*$' "$WORK/head")
    # the two spellings of a list opener written as two anchored branches: a
    # group after the leading run reads as a private copy of the shared
    # command-start list to the walk that keeps that list single
    items_re='^[[:space:]]*[-*+][[:space:]]|^[[:space:]]*[0-9]+[.)][[:space:]]'
    base_items=$(grep -cE "$items_re" "$WORK/base")
    head_items=$(grep -cE "$items_re" "$WORK/head")
    if [ "$head_blanks" -lt "$base_blanks" ] || [ "$head_items" -lt "$base_items" ]; then
      printf '%s: the same words hold less structure -- blank lines %s to %s, list openers %s to %s\n' \
        "$page" "$base_blanks" "$head_blanks" "$base_items" "$head_items"
    fi
  fi
  awk "$BLOCKS_AWK" "$WORK/base" > "$WORK/base-blocks"
  awk "$BLOCKS_AWK" "$WORK/head" > "$WORK/head-blocks"
  # a block of the base surviving verbatim inside a bigger block of the head is
  # the deleted break itself: the words are all still there, one paragraph
  # shorter. Short blocks are left out because a heading, a table row and a
  # one-line item sit inside a paragraph that quotes them without anything
  # having moved
  awk -v page="$page" '
    NR == FNR { held[FNR] = $0; n = FNR; seen[$0] = 1; next }
    length($0) < 40 { next }
    $0 in seen { next }
    {
      for (i = 1; i <= n; i++) {
        if (index(held[i], $0) > 0) {
          printf "%s: a block survives inside a bigger one -- %s\n",
            page, substr($0, 1, 70)
          break
        }
      }
    }
  ' "$WORK/head-blocks" "$WORK/base-blocks"
done
exit 0
