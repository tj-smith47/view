#!/usr/bin/env bash
# Case matrix for check-budget-drift.sh. Every case builds a scratch tree --
# a budgets file, a spec with its two claim-bearing sections, and the three
# pages a reader takes a claim from -- points the check at it, and asserts
# BOTH the exit status and the exact set of findings.
#
#   bash scripts/check-budget-drift-cases.sh
#   bash scripts/check-budget-drift-cases.sh --checker /path/to/copy
#
# The rule these cases exist for is the word-form one. The check shipped
# reading metric identifiers only, and the claim it was written to refuse --
# a diagnostic quoted as a win -- was published in words, with no identifier
# anywhere near it. A case matrix is what stops it regressing to the
# identifier-only form: the four probes below are that claim's own shapes.
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
# Resolved from this file's own directory rather than $PWD, so a run started
# from anywhere grades the checker that ships beside these cases.
if [ -z "$CHECKER" ]; then
  CHECKER="$(cd "$(dirname "$0")" && pwd)/check-budget-drift.sh"
fi
if [ ! -f "$CHECKER" ]; then
  printf 'checker not found: %s\n' "$CHECKER" >&2
  exit 2
fi

printf 'checker under test: %s\n' "$CHECKER"

WORK=$(mktemp -d "${TMPDIR:-/tmp}/check-budget-drift-cases.XXXXXX")
trap 'rm -rf "$WORK"' EXIT

n=0
failures=0
CASE=""

BUDGETS='crates/view-bench/budgets.toml'
SPEC='.claude/specs/2026-07-17-view-design.md'
README='README.md'
PERF='docs/performance.md'
BENCH='docs/benchmarking.md'
BASELINES='crates/view-bench/baselines'

# One felt row and one diagnostic that decomposes it, plus a [[shortfall]]
# whose own scenario/metric pair is nowhere near either: a reader of this
# file that stops resetting on a table header reads the felt kind of the row
# above into the shortfall, and `first_paint.marker_cold_ms` becomes an
# anchor no budget declares.
plant_budgets() {
  cat > "$CASE/$BUDGETS" <<'TOML'
[[budget]]
spec_row = "Keypress -> cell change end-to-end, steady typing"
scenario = "echo"
metric = "view_p99_ms"
max = 8.0
kind = "felt"
felt = "you press a key and the character appears"
config = "real"

[[budget]]
spec_row = "Embedded engine startup cost"
scenario = "startup"
metric = "server_delta_ms"
max = 5.0
kind = "diagnostic"
decomposes = "echo.view_p99_ms"
config = "real"

[[shortfall]]
scenario = "first_paint"
fixture = "minimal"
metric = "marker_cold_ms"
class = "dev-linux"
accepted = 31.0
why = "a shortfall carries a scenario and a metric of its own"
TOML
}

plant_spec() {
  cat > "$CASE/$SPEC" <<'MD'
# view design

## 1. Product definition

view is Neovim, written in Rust, with a modern coherent UI.

## 2. Shape

### 3.1 Budgets (CI-gated once the harness lands, P3)

| Metric | Budget | Measured how |
|---|---|---|
| Keypress -> cell change end-to-end, steady typing (`echo.view_p99_ms`) | p99 <= 8 ms | bench suite |
| Embedded engine startup cost (`server_delta_ms`) | <= 5 ms. **Diagnostic:** one segment of the launch moment | bench suite |

## 4. After the budgets
MD
}

# The two user-facing pages state moments and paired numbers, name no
# identifier, and carry no comparative: naming the cell that would anchor
# one is what the identifier rule already refuses on these two pages.
plant_pages() {
  cat > "$CASE/$README" <<'MD'
# view

You press a key and the character appears. Plugin-free, view's worst
keystroke in a thousand takes 0.73 ms and Neovim's takes 0.67 ms.
MD
  mkdir -p "$CASE/docs"
  cat > "$CASE/$PERF" <<'MD'
# Performance

Under a plugin-free config, view's worst keystroke in a thousand takes
0.73 ms and Neovim's takes 0.67 ms.
MD
  cat > "$CASE/$BENCH" <<'MD'
# Benchmarking

The harness pins the terminal at 120x40 for every cell.

| what | view | bare Neovim |
|---|---|---|
| steady typing (`echo.view_p99_ms`) | 0.73 ms | 0.67 ms |

`gh-linux` carries its first-paint ratios as `withdrawn` until it is
re-seated from a run under the answering pty. dev-linux is re-seated.
MD
}

# One class that owes the re-seat and one that has taken it. The page names
# the first and not the second, which is the state the fourth rule passes.
plant_baselines() {
  cat > "$CASE/$BASELINES/gh-linux.toml" <<'TOML'
machine_class = "gh-linux"

[withdrawn.first_paint.minimal]
marker_ratio_p50 = "taken on a harness pty that never answered nvim's DSR"
TOML
  cat > "$CASE/$BASELINES/dev-linux.toml" <<'TOML'
machine_class = "dev-linux"

[first_paint.minimal]
marker_ratio_p50 = 1.0936971456650568
TOML
}

new_case() {
  n=$((n + 1))
  CASE="$WORK/case$n"
  mkdir -p "$CASE/crates/view-bench" "$CASE/.claude/specs" "$CASE/docs" \
    "$CASE/$BASELINES"
  plant_budgets
  plant_spec
  plant_pages
  plant_baselines
}

# Each failure the check can raise collapses to one token. Headers collapse
# to a guard name so that rewording a diagnostic is not a regression, while
# the two rules that name a place keep it: an identifier finding is only
# right if it names the page and the identifier, and a claim finding is only
# right if it names the line the claim stands on.
findings() {
  awk '
    /^BUDGET DRIFT FAIL: spec_row .* matches no line in the spec$/ {
      print "spec-row-missing"; next
    }
    /^BUDGET DRIFT FAIL: budgets\.toml bounds / { print "max-drift"; next }
    /^BUDGET DRIFT FAIL: .* is a .* budget, and its spec row does not say/ {
      print "marker:" $4; next
    }
    /^BUDGET DRIFT FAIL: .* names the metric identifier / {
      id = $9; sub(/\.$/, "", id); print "identifier:" $4 ":" id; next
    }
    /^BUDGET DRIFT FAIL: a comparative claim stands/ { claims = 1; next }
    /^BUDGET DRIFT FAIL: the spec carries no section headed/ {
      print "section-missing"; next
    }
    /^BUDGET DRIFT FAIL: .* declares no felt metric/ { print "no-felt"; next }
    /^BUDGET DRIFT FAIL: reseat-(named|unnamed) / {
      c = $5; sub(/:$/, "", c); print $4 ":" c; next
    }
    claims && /^  [^ ]+:[0-9]+: / {
      where = $1; sub(/:$/, "", where); print "claim:" where; next
    }
  ' | LC_ALL=C sort -u | tr '\n' ' ' | sed 's/ *$//'
}

expect() {
  want_rc="$1"
  want="$2"
  desc="$3"
  out=$(bash "$CHECKER" "$CASE" 2>&1)
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
# the tree as it should ship
# ---------------------------------------------------------------------------
new_case
expect 0 '' 'a tree whose pages state moments in words and anchor their one ratio'

# ---------------------------------------------------------------------------
# the reviewer's four probes
# ---------------------------------------------------------------------------
new_case
printf '\nview is 5.2x faster than bare Neovim on first paint.\n' >> "$CASE/$README"
expect 1 'claim:README.md:6' 'a word-form win claim on the page a user reads first'

new_case
sed 's/\*\*Diagnostic:\*\* //' "$CASE/$SPEC" > "$CASE/$SPEC.tmp"
mv "$CASE/$SPEC.tmp" "$CASE/$SPEC"
expect 1 'marker:server_delta_ms' 'a diagnostic budget whose spec row no longer says which it is'

new_case
printf '\nThe cell behind those two figures is `view_p99_ms`.\n' >> "$CASE/$PERF"
expect 1 'identifier:performance.md:view_p99_ms' 'a metric identifier on a page that states moments'

new_case
printf '\n| speculative echo (`echo.view_p99_ms`) | 5.2x faster than bare Neovim |\n' \
  >> "$CASE/$BENCH"
expect 0 '' 'a claim standing beside the felt cell that earns it'

# ---------------------------------------------------------------------------
# the same claim, anchored by nothing, by a diagnostic, and by an id the
# budgets file never declared -- the three ways the shipped bug read as fine
# ---------------------------------------------------------------------------
new_case
printf '\n| speculative echo | 5.2x faster than bare Neovim |\n' >> "$CASE/$BENCH"
expect 1 'claim:docs/benchmarking.md:12' 'a claim in a paragraph that names no cell at all'

new_case
printf '\n| engine startup (`startup.server_delta_ms`) | 5.2x faster |\n' >> "$CASE/$BENCH"
expect 1 'claim:docs/benchmarking.md:12' 'a diagnostic cell quoted as the thing that won'

new_case
printf '\n| cold start (`first_paint.marker_cold_ms`) | 2.1 to 5.2x sooner |\n' \
  >> "$CASE/$BENCH"
expect 1 'claim:docs/benchmarking.md:12' 'a claim anchored by a shortfall row, which declares no kind'

# ---------------------------------------------------------------------------
# the comparative without a number, and the number that is not a comparative
# ---------------------------------------------------------------------------
new_case
printf '\nEvery cell here lands ahead of the same cell under bare Neovim.\n' \
  >> "$CASE/$BENCH"
expect 1 'claim:docs/benchmarking.md:12' 'a comparative naming what it beats, with no multiplier'

new_case
printf '\nThe picker fixture is a 120x40 terminal holding 100000 entries.\n' \
  >> "$CASE/$BENCH"
expect 0 '' 'a terminal size, which is a shape and not a ratio'

# ---------------------------------------------------------------------------
# both spec sections are read, and a heading that moves is not silence
# ---------------------------------------------------------------------------
new_case
sed 's/^view is Neovim, written in Rust, with a modern coherent UI\./view starts 5.2x sooner than bare Neovim./' \
  "$CASE/$SPEC" > "$CASE/$SPEC.tmp"
mv "$CASE/$SPEC.tmp" "$CASE/$SPEC"
expect 1 'claim:.claude/specs/2026-07-17-view-design.md:5' 'a claim in the spec section that defines the product'

new_case
sed 's/| p99 <= 8 ms |/| p99 <= 8 ms, 5.2x ahead of bare Neovim |/' \
  "$CASE/$SPEC" > "$CASE/$SPEC.tmp"
mv "$CASE/$SPEC.tmp" "$CASE/$SPEC"
expect 0 '' 'a claim in the budget table, on the row whose own cell id anchors it'

new_case
sed 's/^## 1\. Product definition$/## 1. What view is/' "$CASE/$SPEC" > "$CASE/$SPEC.tmp"
mv "$CASE/$SPEC.tmp" "$CASE/$SPEC"
expect 1 'section-missing' 'a renamed spec section, whose claims would otherwise go unread'

# ---------------------------------------------------------------------------
# a re-seat that moved the baseline and left the page behind, and its converse
# ---------------------------------------------------------------------------
new_case
cat > "$CASE/$BASELINES/gh-linux.toml" <<'TOML'
machine_class = "gh-linux"

[first_paint.minimal]
marker_ratio_p50 = 1.0936971456650568
TOML
expect 1 'reseat-named:gh-linux' 'a class re-seated in its baseline and still named as owing the ratio'

new_case
cat > "$CASE/$BASELINES/dev-linux.toml" <<'TOML'
machine_class = "dev-linux"

[withdrawn.first_paint.minimal]
marker_ratio_p50 = "taken on a harness pty that never answered nvim's DSR"
TOML
expect 1 'reseat-unnamed:dev-linux' 'a class that withdrew a ratio the page tells no reader about'

# ---------------------------------------------------------------------------
# the two cross-checks that shipped before this one
# ---------------------------------------------------------------------------
new_case
sed 's/^max = 8\.0$/max = 9.0/' "$CASE/$BUDGETS" > "$CASE/$BUDGETS.tmp"
mv "$CASE/$BUDGETS.tmp" "$CASE/$BUDGETS"
expect 1 'max-drift' 'a budget loosened in the file the gate reads and nowhere else'

new_case
sed 's/^spec_row = "Embedded engine startup cost"$/spec_row = "Engine startup cost"/' \
  "$CASE/$BUDGETS" > "$CASE/$BUDGETS.tmp"
mv "$CASE/$BUDGETS.tmp" "$CASE/$BUDGETS"
expect 1 'marker:server_delta_ms spec-row-missing' \
  'a budget naming a spec row the spec does not carry'

new_case
sed 's/^kind = "felt"$/kind = "diagnostic"/' "$CASE/$BUDGETS" > "$CASE/$BUDGETS.tmp"
mv "$CASE/$BUDGETS.tmp" "$CASE/$BUDGETS"
expect 1 'marker:view_p99_ms no-felt' \
  'a budgets file with no felt row, which would anchor every claim by having none'

printf '\n%s cases, %s failures\n' "$n" "$failures"
[ "$failures" -eq 0 ]
