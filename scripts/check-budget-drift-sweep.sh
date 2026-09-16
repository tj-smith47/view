#!/usr/bin/env bash
# Grading sweep for check-budget-drift.sh. The case matrix proves each rule
# fires on a planted input; it says nothing about whether the rules between
# them reach every figure the shipped tree publishes. Twice a review found
# the grading partial by hand -- a page paragraph with no cell id beside its
# numbers, a unit the ratio rule skipped, two whole pages nothing read -- so
# this walks the population instead.
#
# Every figure the checker resolves to a cell -- on the three pages and in
# every [[shortfall]] why -- is perturbed by one digit in a scratch copy of
# the tree, and every member of every trials array is moved outside the band
# its entry's seat allows; the checker must then fail naming that file and
# line. A figure that survives is one its own rule does not reach, which
# goes stale in silence at the next --record run, and it is printed with its
# site.
#
# Which figures those are comes from the grading itself, through
# scripts/lib/moment-grading.sh in its population mode, and never from a
# second read of the seat table: a seat equals the bare-engine reading beside
# view's as readily as view's own, and three of those entered a population
# taken by value the day a re-seat made them equal a cell.
#
#   bash scripts/check-budget-drift-sweep.sh
#   bash scripts/check-budget-drift-sweep.sh --checker /path/to/copy
#
# It is not in `task ci`: it runs the checker once per figure and costs
# minutes where the gate costs a second. `task drift:sweep` and its own CI
# step are where it runs.
#
# Written to stock POSIX-ish bash: macOS ships /bin/bash 3.2.
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

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
if [ -z "$CHECKER" ]; then
  CHECKER="$ROOT/scripts/check-budget-drift.sh"
fi
if [ ! -f "$CHECKER" ]; then
  printf 'checker not found: %s\n' "$CHECKER" >&2
  exit 2
fi
# Beside the checker under test rather than beside this script, so a run
# pointed at a copy grades the population that copy resolves.
CHECKER_LIB="$(cd "$(dirname "$CHECKER")" && pwd)/lib/moment-grading.sh"
if [ ! -f "$CHECKER_LIB" ]; then
  printf 'grading library not found beside the checker: %s\n' "$CHECKER_LIB" >&2
  exit 2
fi
# shellcheck source=lib/moment-grading.sh
. "$CHECKER_LIB"

BUDGETS='crates/view-bench/budgets.toml'
BASELINES='crates/view-bench/baselines'
PAGES='README.md docs/performance.md docs/benchmarking.md'

WORK=$(mktemp -d "${TMPDIR:-/tmp}/check-budget-drift-sweep.XXXXXX")
trap 'rm -rf "$WORK"' EXIT
TREE="$WORK/tree"
mkdir -p "$TREE"
# The checker reads a tree, so it gets one: the tracked files, which is every
# input it resolves and nothing a peer session left lying in the worktree.
(cd "$ROOT" && git ls-files -z) | (cd "$ROOT" && xargs -0 tar -cf - ) | (cd "$TREE" && tar -xf -)
if [ ! -f "$TREE/$BUDGETS" ]; then
  printf 'sweep: %s did not reach the scratch tree, so nothing would be graded\n' "$BUDGETS" >&2
  exit 2
fi

# The seat table, built the way the checker builds it: class, fixture,
# scenario.metric, value. It is rebuilt here rather than asked of the checker
# because the checker takes a tree and no flag, and a sweep that could only
# ask it for the table would need one.
seats="$WORK/seats.tsv"
: > "$seats"
for class_file in "$TREE/$BASELINES"/*.toml; do
  [ -f "$class_file" ] || continue
  class=$(basename "$class_file" .toml)
  # The pattern carries a leading paren: bash 3.2 counts the closing one of
  # a bare pattern as the end of the enclosing substitution and dies parsing.
  case "$class" in (*.*) continue ;; esac
  awk -v class="$class" '
    /^\[/ {
      h = $0; gsub(/[][]/, "", h); n = split(h, part, ".")
      scenario = (part[1] == "withdrawn" || n < 2) ? "" : part[1]
      fixture = (scenario == "") ? "" : part[2]
      next
    }
    scenario != "" && /^[a-z_0-9]+ = -?[0-9.]+$/ { print class "\t" fixture "\t" scenario "." $1 "\t" $3 }
  ' "$class_file" >> "$seats"
done
if [ ! -s "$seats" ]; then
  printf 'sweep: no class baseline records a value, so no figure has a seat to go stale against\n' >&2
  exit 2
fi

# The population is the figures the check resolves to a cell, asked of the
# grading that resolves them rather than derived from the seat table a second
# time. The value a figure equals cannot decide: a seat equals the
# bare-engine reading beside view's as readily as view's own, and a number
# standing beside one cell id equals a sibling cell's value often enough --
# both entered a population taken by value as readings nothing grades, and
# both were right to be ungraded.
# One line per figure in the population: file, line, which number token on
# that line it is, the value as printed, and the cell it resolves to.
population="$WORK/population.tsv"
: > "$population"

# The sites one page states, as ` line:value=cell ` runs the walk below looks
# a token up in. docs/benchmarking.md resolves a number against the nearest
# cell id before it; the two user-facing pages name no identifier and resolve
# it against the moment they state in words.
page_population() {
  local rel="$1" class prog
  class=$(awk "$MOMENT_CLASS_AWK" "$TREE/$rel")
  prog="$MOMENT_GRADE_AWK"
  case "$rel" in (docs/benchmarking.md) prog="$RATIO_GRADE_AWK" ;; esac
  awk -v page="$rel" -v fallback="$class" -v mode="population" \
    "$prog" "$seats" "$TREE/$rel" |
    awk -F'\t' '$1 == "POP" { printf " %s:%s=%s ", $2, $3, $4 }'
}

# The walk that turns those sites into perturbations. It counts the number
# tokens of a line the way the perturbation edits them, so the index it
# records is the token the edit lands on.
collect_sites() {
  local rel="$1" graded="$2"
  [ -n "$graded" ] || return 0
  awk -v file="$rel" -v graded="$graded" '
    function clean(t) {
      gsub(/[][`*~()>]/, "", t)
      sub(/[,;:.]+$/, "", t)
      return t
    }
    {
      m = split($0, w, /[[:space:]]+/)
      idx = 0
      for (j = 1; j <= m; j++) {
        num = clean(w[j])
        if (num !~ /^-?[0-9]+\.[0-9]+$/) { continue }
        # Counted before the lookup: a number nothing grades still moves the
        # token the next edit lands on.
        idx++
        key = " " FNR ":" num "="
        at = index(graded, key)
        if (at == 0) { continue }
        cell = substr(graded, at + length(key))
        cell = substr(cell, 1, index(cell, " ") - 1)
        printf "%s\t%d\t%d\t%s\t%s\t%s\n", file, FNR, idx, num, cell, "digit"
      }
    }
  ' "$TREE/$rel" >> "$population"
}

for rel in $PAGES; do
  [ -f "$TREE/$rel" ] || continue
  collect_sites "$rel" "$(page_population "$rel")"
done

# A why figure resolves against its own entry class, scenario, fixture and
# metric, so the same grading is asked for those sites too: a why settles a
# question with a cell from a scenario the entry does not measure, and the
# value it states can equal another cell seat while resolving to that one.
collect_sites "$BUDGETS" "$(awk -v file="$BUDGETS" -v mode="population" \
  "$WHY_GRADE_AWK" "$seats" "$TREE/$BUDGETS" |
  awk -F'\t' '$1 == "POP" { printf " %s:%s=%s ", $2, $3, $4 }')"

# Every trials member is a draw of the metric the entry seats, and what
# grades it is a band around that seat rather than a value of its own. So the
# whole array is in the population and the edit that has to redden is the one
# that puts a member outside the band: a member no rule reaches is a foreign
# quantity parked in the ledger, which is what four arrays held.
awk -v file="$BUDGETS" '
  function clean(t) {
    gsub(/[][`*~()>]/, "", t)
    sub(/[,;:.]+$/, "", t)
    return t
  }
  /^\[\[/ { block = ($0 ~ /shortfall/); acc = ""; next }
  !block { next }
  /^accepted = / { acc = $0; sub(/^accepted = /, "", acc); next }
  /^trials = / {
    idx = 0
    m = split($0, w, /[[:space:]]+/)
    for (j = 1; j <= m; j++) {
      num = clean(w[j])
      if (num !~ /^-?[0-9]+\.[0-9]+$/) { continue }
      idx++
      if (acc == "") { continue }
      printf "%s\t%d\t%d\t%s\t%s\t%s\n", file, FNR, idx, num, "a draw of the accepted " acc, "band"
    }
  }
' "$TREE/$BUDGETS" >> "$population"

total=$(wc -l < "$population" | tr -d ' ')
if [ "$total" = "0" ]; then
  printf 'sweep: the check resolves no figure on any page or in any why, which is not a tree this sweep can grade\n' >&2
  exit 2
fi

# The edit the figure's own rule has to catch. A quoted reading is graded
# against the seat it equals, so one digit in the last place printed is the
# smallest edit that makes it a value nothing recorded -- the shape a stale
# quote has. A draw is graded against a band around its entry's seat, so the
# edit that retires it is one that lands it outside that band.
perturb() {
  awk -v line="$1" -v want="$2" -v mode="$3" '
    function clean(t) {
      gsub(/[][`*~()>]/, "", t)
      sub(/[,;:.]+$/, "", t)
      return t
    }
    FNR != line { print; next }
    {
      idx = 0
      out = ""
      m = split($0, w, / /)
      for (j = 1; j <= m; j++) {
        tok = w[j]
        num = clean(tok)
        if (num ~ /^-?[0-9]+\.[0-9]+$/) {
          idx++
          if (idx == want) {
            at = index(tok, num)
            if (mode == "band") {
              edited = sprintf("%." (length(num) - index(num, ".")) "f", num * 2)
            } else {
              last = substr(num, length(num), 1)
              edited = substr(num, 1, length(num) - 1) \
                ((last == "9") ? "8" : sprintf("%d", last + 1))
            }
            tok = substr(tok, 1, at - 1) edited substr(tok, at + length(num))
          }
        }
        out = (j == 1) ? tok : out " " tok
      }
      print out
    }
  ' "$file_path"
}

checked=0
surviving=""
while IFS=$'\t' read -r rel lineno idx value cells mode; do
  checked=$((checked + 1))
  file_path="$TREE/$rel"
  cp "$file_path" "$WORK/pristine"
  perturb "$lineno" "$idx" "$mode" > "$WORK/perturbed"
  cp "$WORK/perturbed" "$file_path"
  out=$("$BASH" "$CHECKER" "$TREE" 2>&1)
  rc=$?
  cp "$WORK/pristine" "$file_path"
  site=$(printf '%s' "$rel" | sed 's/[.]/[.]/g')
  named=$(printf '%s\n' "$out" | grep -cE "$site:$lineno([^0-9]|\$)")
  if [ "$rc" -ne 0 ] && [ "$named" -gt 0 ]; then
    continue
  fi
  surviving="$surviving$rel:$lineno: $value ($cells)"$'\n'
done < "$population"

printf '\n%s figure(s) checked against %s seat(s)\n' "$checked" "$(wc -l < "$seats" | tr -d ' ')"
if [ -n "$surviving" ] && [ "$surviving" != $'\n' ]; then
  printf 'BUDGET DRIFT SWEEP FAIL: a figure survives the rewrite its own rule has to catch, so nothing grades it and a record run leaves it standing:\n' >&2
  printf '%s' "$surviving" | grep -v '^$' | sed 's/^/  /' >&2
  exit 1
fi
printf 'budget drift sweep: every recorded figure on the pages and in the ledger is graded\n'
