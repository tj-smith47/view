#!/usr/bin/env bash
# Grading sweep for check-budget-drift.sh. The case matrix proves each rule
# fires on a planted input; it says nothing about whether the rules between
# them reach every figure the shipped tree publishes. Twice a review found
# the grading partial by hand -- a page paragraph with no cell id beside its
# numbers, a unit the ratio rule skipped, two whole pages nothing read -- so
# this walks the population instead.
#
# The population is every figure on the three pages and in every
# [[shortfall]] why that equals a recorded seat of any class, and
# scripts/lib/moment-grading.sh classifies each of them in its classify
# mode. A figure it resolves to a cell is perturbed by one digit in a
# scratch copy of the tree, and every member of every trials array is moved
# outside the band its entry's seat allows; the checker must then fail
# naming that file and line. A figure it excludes is printed with the ground
# the rule states and left alone. A figure it does neither for is a recorded
# value nothing grades -- the blind spot the sweep is for -- and the run
# fails naming it.
#
# Each half was once the whole population. Taken by value alone it held
# three bare-Neovim readings the day a re-seat made them equal a cell, and
# the checker was right not to grade them. Taken as the figures the grading
# resolves it could not hold a blind spot at all, which is the one thing it
# exists to find.
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

# The population is every figure on the three pages and in every `why` that
# equals a recorded seat of any class, and the grading classifies each of
# them. A population taken by value alone cannot tell the bare-engine
# reading beside view own from view own, and three engine readings entered
# it the day a re-seat made them equal a cell. A population taken as the
# figures the grading resolves has the opposite defect and it is the worse
# one: by construction it can never hold a figure the check grades nothing
# for, which is the blind spot this sweep exists to report.
#
# So the grading answers for all of them, in three verdicts:
#
#   resolved   the check resolves it to a cell -- perturbed here, and the
#              check has to fail naming the file and the line;
#   excluded   a rule of the check says it is not a reading of a cell -- an
#              engine reading, a stated difference, a bound, a unit no cell
#              is recorded in, a fenced block, a ledger draw. Not perturbed,
#              and printed with its ground so a reader can refuse one;
#   unaccounted  equal to a seat, resolved by nothing and excluded by
#              nothing. That is a recorded value the check grades nowhere,
#              and this sweep fails on it.
#
# One line per figure: file, line, which figure token on that line it is,
# the value as printed, the edit its rule has to catch, and what it resolved
# to or was excluded on.
classified="$WORK/classified.tsv"
: > "$classified"

for rel in $PAGES; do
  [ -f "$TREE/$rel" ] || continue
  prog="$MOMENT_GRADE_AWK"
  case "$rel" in (docs/benchmarking.md) prog="$RATIO_GRADE_AWK" ;; esac
  awk -v page="$rel" -v fallback="$(awk "$MOMENT_CLASS_AWK" "$TREE/$rel")" \
    -v mode="classify" "$prog" "$seats" "$TREE/$rel" >> "$classified"
done

# A why figure resolves against its own entry class, scenario, fixture and
# metric, and the ledger draws beside it are graded by a band rather than by
# a site, which the classification says in its ground.
awk -v file="$BUDGETS" -v mode="classify" "$WHY_GRADE_AWK" \
  "$seats" "$TREE/$BUDGETS" >> "$classified"

population="$WORK/population.tsv"
excluded="$WORK/excluded.tsv"
unaccounted="$WORK/unaccounted.tsv"
awk -F'\t' '$1 == "CLS" && $6 ~ /^resolved:/ {
  print $2 "\t" $3 "\t" $4 "\t" $5 "\tdigit\t" substr($6, 10)
}' "$classified" > "$population"
awk -F'\t' '$1 == "CLS" && $6 ~ /^excluded:/ {
  print $2 ":" $3 ": " $5 " -- " substr($6, 10)
}' "$classified" > "$excluded"
awk -F'\t' '$1 == "CLS" && $6 == "unaccounted" {
  print $2 ":" $3 ": " $5
}' "$classified" > "$unaccounted"

# Every trials member is a draw of the metric the entry seats, and what
# grades it is a band around that seat rather than a value of its own. So the
# whole array is in the population and the edit that has to redden is the one
# that puts a member outside the band: a member no rule reaches is a foreign
# quantity parked in the ledger, which is what four arrays held.
awk -v file="$BUDGETS" "$GRADE_COMMON_AWK"'
  /^\[\[/ { block = ($0 ~ /shortfall/); acc = ""; next }
  !block { next }
  /^accepted = / { acc = $0; sub(/^accepted = /, "", acc); next }
  /^trials = / {
    idx = 0
    m = split($0, w, /[[:space:]]+/)
    for (j = 1; j <= m; j++) {
      num = clean(w[j])
      if (!figure(num)) { continue }
      idx++
      if (acc == "") { continue }
      printf "%s\t%d\t%d\t%s\t%s\t%s\n", file, FNR, idx, num, "band", "a draw of the accepted " acc
    }
  }
' "$TREE/$BUDGETS" >> "$population"

total=$(wc -l < "$classified" | tr -d ' ')
if [ "$total" = "0" ]; then
  printf 'sweep: no figure on any page or in any why equals a recorded seat, which is not a tree this sweep can grade\n' >&2
  exit 2
fi

# The edit the figure's own rule has to catch. A quoted reading is graded
# against the seat it equals, so one digit in the last place printed is the
# smallest edit that makes it a value nothing recorded -- the shape a stale
# quote has. A draw is graded against a band around its entry's seat, so the
# edit that retires it is one that lands it outside that band.
perturb() {
  awk -v line="$1" -v want="$2" -v mode="$3" "$GRADE_COMMON_AWK"'
    FNR != line { print; next }
    {
      idx = 0
      out = ""
      m = split($0, w, / /)
      for (j = 1; j <= m; j++) {
        tok = w[j]
        num = clean(tok)
        if (figure(num)) {
          idx++
          if (idx == want) {
            # The digits alone, so a multiplier or a percentage keeps the
            # suffix it is glued to and stays the figure the grading reads.
            num = bare(num)
            at = index(tok, num)
            if (mode == "band") {
              edited = sprintf("%." decimals(num) "f", num * 2)
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
while IFS=$'\t' read -r rel lineno idx value mode cells; do
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

nexcluded=$(wc -l < "$excluded" | tr -d ' ')
nunaccounted=$(wc -l < "$unaccounted" | tr -d ' ')
nresolved=$(grep -c 'resolved:' "$classified" | tr -d ' ')
printf '\n%s figure(s) checked against %s seat(s)\n' "$checked" "$(wc -l < "$seats" | tr -d ' ')"
printf 'resolved %s / excluded %s / unaccounted %s, and %s ledger draw(s) banded\n' \
  "$nresolved" "$nexcluded" "$nunaccounted" "$((checked - nresolved))"
# Printed on every run, passing or failing: an exclusion a reader never sees
# is a rule nobody can refuse, and every hand-listed exemption this check
# has carried was refused the first time somebody read it.
if [ "$nexcluded" != "0" ]; then
  printf 'excluded, each on a ground one of the rules states:\n'
  sed 's/^/  /' "$excluded"
fi
if [ "$nunaccounted" != "0" ]; then
  printf 'BUDGET DRIFT SWEEP FAIL: a figure equals a recorded seat and the check neither resolves it to a cell nor excludes it on a ground, so nothing grades it and a record run leaves it standing:\n' >&2
  sed 's/^/  /' "$unaccounted" >&2
  exit 1
fi
if [ -n "$surviving" ] && [ "$surviving" != $'\n' ]; then
  printf 'BUDGET DRIFT SWEEP FAIL: a figure survives the rewrite its own rule has to catch, so nothing grades it and a record run leaves it standing:\n' >&2
  printf '%s' "$surviving" | grep -v '^$' | sed 's/^/  /' >&2
  exit 1
fi
printf 'budget drift sweep: every recorded figure on the pages and in the ledger is graded\n'
