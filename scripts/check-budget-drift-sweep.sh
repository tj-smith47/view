#!/usr/bin/env bash
# Grading sweep for check-budget-drift.sh. The case matrix proves each rule
# fires on a planted input; it says nothing about whether the rules between
# them reach every figure the shipped tree publishes. Twice a review found
# the grading partial by hand -- a page paragraph with no cell id beside its
# numbers, a unit the ratio rule skipped, two whole pages nothing read -- so
# this walks the population instead.
#
# Every figure on the three pages and in every [[shortfall]] why that equals
# a value some class baseline records is perturbed by one digit in a scratch
# copy of the tree, and every member of every trials array is moved outside
# the band its entry's seat allows; the checker must then fail naming that
# file and line. A figure that survives is a value nothing grades, which
# goes stale in silence at the next --record run, and it is printed with its
# site.
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

# The population is every figure on the page against the seats of every
# class, never the seats of the class the page declares: the page quotes
# gh-linux, gh-macos, dev-macos and controlled-linux seats throughout, the
# checker resolves each of them where its own unit names it, and a
# population scoped to the default class graded none of them.
# One line per figure in the population: file, line, which number token on
# that line it is, the value as printed, and the seats it equals.
population="$WORK/population.tsv"
: > "$population"

collect_page() {
  local rel="$1"
  awk -v file="$rel" '
    FNR == NR {
      split($0, f, "\t")
      key = f[3]
      sub(/^[a-z_]+\./, "", key)
      unit = "x"
      if (key ~ /_ms$/) { unit = "ms" }
      else if (key ~ /_us$/) { unit = "us" }
      else if (key ~ /_mb$/) { unit = "mb" }
      else if (key ~ /_ratio(_p[0-9]+)?$/ || key ~ /^ratio/) { unit = "" }
      seatunit[f[3]] = unit
      vals[f[3]] = vals[f[3]] " " f[4]
      next
    }
    function clean(t) {
      gsub(/[][`*~()>]/, "", t)
      sub(/[,;:.]+$/, "", t)
      return t
    }
    # A figure carrying a unit no cell is recorded in, and one carrying the
    # word that makes it a bound rather than a reading, are outside the
    # population: the checker grades neither and is right not to.
    function unit_of(t) {
      if (t == "ms") { return "ms" }
      if (t == "us" || t == "\xc2\xb5s") { return "us" }
      if (t == "MB") { return "mb" }
      if (t ~ /^(s|min|GB|%|bar|bars|budget|bound|frame)$/) { return "x" }
      return ""
    }
    function decimals(num) {
      return index(num, ".") == 0 ? 0 : length(num) - index(num, ".")
    }
    # A difference a page states between the paired numbers is neither
    # side reading, so it is outside the population however close it lands
    # to a seat.
    function delta_after(w, j,   nxt) {
      nxt = clean(w[j + 1])
      if (unit_of(nxt) != "" || nxt == "%") { nxt = clean(w[j + 2]) }
      return (nxt ~ /^(more|less|fewer|behind|ahead|earlier|later|further)$/)
    }
    # A fenced block is a sample of a file, not a reading of a cell.
    /^[[:space:]]*```/ { fenced = !fenced; next }
    fenced { next }
    {
      m = split($0, w, /[ \t]+/)
      idx = 0
      for (j = 1; j <= m; j++) {
        num = clean(w[j])
        if (num !~ /^-?[0-9]+\.[0-9]+$/) { continue }
        idx++
        if (delta_after(w, j)) { continue }
        u = unit_of(clean(w[j + 1]))
        fmt = "%." decimals(num) "f"
        hit = ""
        for (k in vals) {
          if (seatunit[k] != u) { continue }
          n = split(vals[k], v, " ")
          for (i = 1; i <= n; i++) {
            if (sprintf(fmt, v[i]) + 0 == num + 0) { hit = hit " " k }
          }
        }
        if (hit != "") { printf "%s\t%d\t%d\t%s\t%s\t%s\n", file, FNR, idx, num, substr(hit, 2), "digit" }
      }
    }
  ' "$seats" "$TREE/$rel" >> "$population"
}

for rel in $PAGES; do
  [ -f "$TREE/$rel" ] || continue
  collect_page "$rel"
done

# A why is read against its own entry's class, and every sentence of it is
# read: the checker holds no escape a word buys, so neither does the
# population.
awk -v file="$BUDGETS" '
  FNR == NR {
    split($0, f, "\t")
    key = f[3]
    sub(/^[a-z_]+\./, "", key)
    unit = "x"
    if (key ~ /_ms$/) { unit = "ms" }
    else if (key ~ /_us$/) { unit = "us" }
    else if (key ~ /_mb$/) { unit = "mb" }
    else if (key ~ /_ratio(_p[0-9]+)?$/ || key ~ /^ratio/) { unit = "" }
    seatunit[f[3]] = unit
    vals[f[3]] = vals[f[3]] " " f[4]
    next
  }
  function clean(t) {
    gsub(/[][`*~()>]/, "", t)
    sub(/[,;:.]+$/, "", t)
    return t
  }
  function unit_of(t) {
    if (t == "ms") { return "ms" }
    if (t == "us" || t == "\xc2\xb5s") { return "us" }
    if (t == "MB") { return "mb" }
    if (t ~ /^(s|min|GB|%|percent|bar|bars|budget|bound|frame)$/) { return "x" }
    return ""
  }
  function decimals(num) {
    return index(num, ".") == 0 ? 0 : length(num) - index(num, ".")
  }
  /^\[\[/ { block = ($0 ~ /shortfall/); acc = ""; next }
  !block { next }
  /^accepted = / { acc = $0; sub(/^accepted = /, "", acc); next }
  # Every trials member is a draw of the metric the entry seats, and what
  # grades it is a band around that seat rather than a value of its own. So
  # the whole array is in the population and the edit that has to redden is
  # the one that puts a member outside the band: a member no rule reaches is
  # a foreign quantity parked in the ledger, which is what four arrays held.
  /^trials = / {
    idx = 0
    m = split($0, w, /[ \t]+/)
    for (j = 1; j <= m; j++) {
      num = clean(w[j])
      if (num !~ /^-?[0-9]+\.[0-9]+$/) { continue }
      idx++
      if (acc == "") { continue }
      printf "%s\t%d\t%d\t%s\t%s\t%s\n", file, FNR, idx, num, "a draw of the accepted " acc, "band"
    }
    next
  }
  /^why = / {
    idx = 0
    n = split($0, sent, /\. /)
    for (s = 1; s <= n; s++) {
      m = split(sent[s], w, /[ \t]+/)
      for (j = 1; j <= m; j++) {
        num = clean(w[j])
        if (num !~ /^-?[0-9]+\.[0-9]+$/) { continue }
        idx++
        u = unit_of(clean(w[j + 1]))
        fmt = "%." decimals(num) "f"
        hit = ""
        for (k in vals) {
          if (seatunit[k] != u) { continue }
          c = split(vals[k], v, " ")
          for (i = 1; i <= c; i++) {
            if (sprintf(fmt, v[i]) + 0 == num + 0) { hit = hit " " k }
          }
        }
        if (hit != "") { printf "%s\t%d\t%d\t%s\t%s\t%s\n", file, FNR, idx, num, substr(hit, 2), "digit" }
      }
    }
  }
' "$seats" "$TREE/$BUDGETS" >> "$population"

total=$(wc -l < "$population" | tr -d ' ')
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
