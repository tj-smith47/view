#!/usr/bin/env bash
# Case matrix for the two width walks in check-style.sh (the Lua chunk walk
# and the multi-line string literal walk). Every case builds a scratch crate
# tree, points the walks at it, and asserts BOTH the exit status and the
# exact set of reported lines: a case expecting one over-width line has to
# fail when a second one is reported, and a case expecting silence has to
# fail when a walk narrows itself and reports nothing for the wrong reason.
#
#   bash scripts/check-style-cases.sh
#   bash scripts/check-style-cases.sh --checker /path/to/copy
#
# Written to stock POSIX-ish bash: macOS ships /bin/bash 3.2, and the walks'
# own portability (BSD awk, BSD find, BSD grep) is only proven by running
# this there. Both empty-population guards below are unreachable on a real
# tree, so nothing else in the gate exercises them; the three blind spots
# the width checks have shipped (a content heuristic, hardcoded counts, a
# silent exit under set -e) were each found by hand, and these cases are
# those fixtures frozen so the next one trips here instead.
set -uo pipefail

CHECKER=""
while [ $# -gt 0 ]; do
  case "$1" in
    --checker)
      CHECKER="${2:-}"
      shift 2
      ;;
    -h | --help)
      printf 'usage: %s [--checker PATH]\n' "$0"
      exit 0
      ;;
    *)
      printf 'unknown argument: %s\n' "$1" >&2
      exit 2
      ;;
  esac
done
# Resolved from this file's own directory rather than $PWD, so a run started
# from anywhere grades the checker that ships beside these cases.
if [ -z "$CHECKER" ]; then
  CHECKER="$(cd "$(dirname "$0")" && pwd)/check-style.sh"
fi
if [ ! -f "$CHECKER" ]; then
  printf 'checker not found: %s\n' "$CHECKER" >&2
  exit 2
fi

printf 'checker under test: %s\n' "$CHECKER"

WORK=$(mktemp -d "${TMPDIR:-/tmp}/check-style-cases.XXXXXX")
trap 'rm -rf "$WORK"' EXIT

n=0
failures=0
CASE=""

SRC='crates/view-engine/src'
TESTS='crates/view-engine/tests'
NVIM='crates/view-engine/src/nvim_api.rs'
LIT='crates/view-engine/tests/lit.rs'

new_case() {
  n=$((n + 1))
  CASE="$WORK/case$n"
  mkdir -p "$CASE/$SRC" "$CASE/$TESTS"
}

# A line of exactly WIDTH columns, opened by PREFIX and closed by SUFFIX,
# padded between them. The widths are computed rather than written out: a
# fixture whose 81st column came from a hand-counted string is one editor
# reflow away from testing 79.
pad() {
  fill=$(($1 - ${#2} - ${#3}))
  printf '%s%s%s\n' "$2" "$(printf "%${fill}s" '' | tr ' ' 'a')" "$3"
}

# The chunk walk's one shape: a const declaration opening a concat!, whose
# body line is padded to the width the case is about.
plant_chunk() {
  {
    printf 'const A_CHUNK: &str = concat!(\n'
    pad "$1" '    "' '\n",'
    printf ');\n'
  } > "$CASE/$NVIM"
}

# Both literal opener shapes in one file: the literal that opens on its own
# line (padded to the first width) and the one sharing the line with the
# assignment that carries it (whose body carries the second width).
plant_literals() {
  {
    printf 'fn a() {\n    let s = format!(\n'
    pad "$1" '        "' ' \'
    printf '        continues here");\n}\n'
    printf 'const M: &str = "\\\n'
    pad "$2" '' '\'
    printf 'the tail line here";\n'
  } > "$CASE/$LIT"
}

# Both walks report the same two ways: `STYLE FAIL:` headers, each of which
# names one guard, and `file:line: N columns` lines naming an over-width
# line. Headers collapse to a guard token so that rewording a diagnostic is
# not a regression, while the two mismatch guards keep both of their counts
# (walked/declared) -- a mismatch reporting the wrong pair is the bug those
# guards exist to catch, and the count is the whole content of the report.
# The advice lines under a header are not tokens: they repeat no fact.
findings() {
  awk '
    /^STYLE FAIL: .* missing; cannot check Lua chunk width$/ {
      print "chunk-missing"; next
    }
    /^STYLE FAIL: no _CHUNK declaration found/ { print "chunk-none"; next }
    /^STYLE FAIL: the width check walked [0-9]+ Lua chunks/ {
      walked = $7; guard = "chunk-walk"; next
    }
    /^STYLE FAIL: a Lua chunk line is over 80 columns$/ {
      print "chunk-width"; next
    }
    /^STYLE FAIL: no view-engine sources found/ { print "lit-missing"; next }
    /^STYLE FAIL: no multi-line string literal found/ {
      print "lit-none"; next
    }
    /^STYLE FAIL: the width check walked [0-9]+ multi-line string literals/ {
      walked = $7; guard = "lit-walk"; next
    }
    /^STYLE FAIL: a line inside a string literal is over 80 columns$/ {
      print "lit-width"; next
    }
    /^ +but grep counts [0-9]+ / {
      if (guard != "") { print guard ":" walked "/" $4; guard = "" }
      next
    }
    /:[0-9]+: [0-9]+ columns$/ {
      loc = $1; sub(/:$/, "", loc); print loc ":" $2; next
    }
  ' | LC_ALL=C sort -u | tr '\n' ' ' | sed 's/ *$//'
}

expect() {
  want_rc="$1"
  want="$2"
  desc="$3"
  out=$(bash "$CHECKER" --widths "$CASE" 2>&1)
  rc=$?
  got=$(printf '%s\n' "$out" | findings)
  if [ "$rc" = "$want_rc" ] && [ "$got" = "$want" ]; then
    printf 'ok %s - %s\n' "$n" "$desc"
    return
  fi
  failures=$((failures + 1))
  printf 'not ok %s - %s\n  want rc=%s findings [%s]\n  got  rc=%s findings [%s]\n' \
    "$n" "$desc" "$want_rc" "$want" "$rc" "$got"
  printf '%s\n' "$out" | sed 's/^/  | /'
}

# ---------------------------------------------------------------------------
# a tree both walks reach fully, with nothing to report
# ---------------------------------------------------------------------------
new_case
plant_chunk 60
plant_literals 60 60
expect 0 '' 'a tree whose chunk and both literal shapes are inside the width'

new_case
plant_chunk 80
plant_literals 80 80
expect 0 '' 'exactly 80 columns in each of the three shapes is inside the width'

# ---------------------------------------------------------------------------
# one column past, in each shape: the finding is the line, and only it
# ---------------------------------------------------------------------------
new_case
plant_chunk 81
plant_literals 60 60
expect 1 "chunk-width $NVIM:2:81" 'a chunk body line one column over'

new_case
plant_chunk 60
plant_literals 81 60
expect 1 "$LIT:3:81 lit-width" 'a literal opening on its own line, one column over'

new_case
plant_chunk 60
plant_literals 60 81
expect 1 "$LIT:7:81 lit-width" 'a literal opened by an assignment, its body one column over'

# ---------------------------------------------------------------------------
# a declaration shape that drifts out of a walk's own match: the walk reaches
# less than the grep counts, which is the one direction that would otherwise
# pass silently while vouching for lines nobody read
# ---------------------------------------------------------------------------
new_case
plant_chunk 60
plant_literals 60 60
sed 's/^const A_CHUNK/pub static A_CHUNK/' "$CASE/$NVIM" > "$CASE/$NVIM.tmp"
mv "$CASE/$NVIM.tmp" "$CASE/$NVIM"
expect 1 'chunk-walk:0/1' 'a chunk declared as a static is counted but not walked'

new_case
plant_chunk 60
plant_literals 60 60
printf 'const N: &str = "\nnot followed";\n' >> "$CASE/$LIT"
expect 1 'lit-walk:2/3' 'a literal opener with no continuation backslash is counted but not walked'

# ---------------------------------------------------------------------------
# empty populations: a walk that reached nothing must say so rather than
# report a clean tree, and neither can happen on a real checkout
# ---------------------------------------------------------------------------
new_case
printf 'fn a() {}\n' > "$CASE/$NVIM"
plant_literals 60 60
expect 1 'chunk-none' 'a nvim_api.rs carrying no chunk declaration at all'

new_case
plant_chunk 60
expect 1 'lit-none' 'a view-engine carrying no multi-line string literal at all'

new_case
rm -rf "$CASE/crates"
expect 1 'chunk-missing lit-missing' 'a tree with no crate path for either walk to read'

printf '\n%s cases, %s failures\n' "$n" "$failures"
[ "$failures" -eq 0 ]
