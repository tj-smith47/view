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
BUILDS='crates/view-harness/src/builds.rs'

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

The harness pins the terminal at 120x40 for every cell, and `dev-linux` is the default class of this page.

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

# The harness's row table: a scenario it dispatches is a name a spec row may
# cite, and echo_path is the shipped case -- a decomposition row measured and
# reported every run that publishes no baseline and carries no budget.
plant_harness() {
  cat > "$CASE/$BUILDS" <<'RS'
pub const MEASURED_BUILD: &[(&str, Option<&str>)] = &[
    ("echo", Some(VIEW_BIN)),
    ("echo_path", Some(TAPS_VIEW_BIN)),
];
RS
}

new_case() {
  n=$((n + 1))
  CASE="$WORK/case$n"
  mkdir -p "$CASE/crates/view-bench" "$CASE/.claude/specs" "$CASE/docs" \
    "$CASE/$BASELINES" "$CASE/crates/view-harness/src"
  plant_budgets
  plant_spec
  plant_pages
  plant_baselines
  plant_harness
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
    /^BUDGET DRIFT FAIL: spec-id / {
      c = $5; sub(/:$/, "", c); print "spec-id:" c; next
    }
    /^BUDGET DRIFT FAIL: ratio-drift / {
      c = $5; sub(/:$/, "", c); print "ratio:" c; next
    }
    /^BUDGET DRIFT FAIL: ratio-scope / {
      c = $5; sub(/:$/, "", c); print "scope:" c; next
    }
    /^BUDGET DRIFT FAIL: ratio-default / { print "default"; next }
    /^BUDGET DRIFT FAIL: why-drift / {
      c = $5; sub(/:$/, "", c); print "why:" c; next
    }
    /^BUDGET DRIFT FAIL: a transport condition stands/ { next }
    /^  transport [^ ]+:[0-9]+: / {
      c = $2; sub(/:$/, "", c); print "transport:" c; next
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
  # $BASH, not a PATH-resolved `bash`: a matrix run under stock 3.2 that
  # graded the checker under a homebrew 5 would report a portability the
  # contributor running it does not have.
  out=$("$BASH" "$CHECKER" "$CASE" 2>&1)
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
printf '\n| speculative echo (`echo.view_p99_ms`) | 5.2x faster |\n' \
  >> "$CASE/$BENCH"
expect 0 '' 'a claim standing beside the felt cell that earns it'

# ---------------------------------------------------------------------------
# a felt anchor licenses a multiplier on the row, never a comparative naming
# the engine: the anchor cannot tell a bound from a win, so the row that
# names what it beats is refused wherever it stands
# ---------------------------------------------------------------------------
new_case
printf '\n| first paint (`echo.view_p99_ms`) | 5.2x faster than bare Neovim |\n' \
  >> "$CASE/$BENCH"
expect 1 'claim:docs/benchmarking.md:12' \
  'a row naming the engine it beats, standing beside a felt cell id'

new_case
printf '\n| settled screen (`echo.view_p99_ms`) | never faster than the server that draws it |\n' \
  >> "$CASE/$BENCH"
expect 0 '' 'a comparative on an anchored row that names no engine'

new_case
printf '\n| first paint (`echo.view_p99_ms`) | 5.2x faster than plain old Neovim |\n' \
  >> "$CASE/$BENCH"
expect 1 'claim:docs/benchmarking.md:12' \
  'a row naming the engine through words no determiner list foresaw'

new_case
printf '\n| first paint (`echo.view_p99_ms`) | 5.2x faster than `nvim` |\n' >> "$CASE/$BENCH"
expect 1 'claim:docs/benchmarking.md:12' 'a row naming the engine in backticks'

new_case
printf '\n| first paint (`echo.view_p99_ms`) | 5.2x faster than **bare Neovim** |\n' >> "$CASE/$BENCH"
expect 1 'claim:docs/benchmarking.md:12' 'a row naming the engine in bold, the pages'"'"' house style'

new_case
printf '\n| settled screen (`echo.view_p99_ms`) | ahead of that boundary, a cost paid identically by bare nvim |\n' \
  >> "$CASE/$BENCH"
expect 0 '' 'a comparative whose object is a boundary, with the engine in a later clause'

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
# a table row is its own anchor unit: the felt id one row up anchors nothing
# ---------------------------------------------------------------------------
new_case
awk '{ print } /echo\.view_p99_ms/ {
  print "| speculative echo | 5.2x faster than bare Neovim |"
}' "$CASE/$BENCH" > "$CASE/$BENCH.tmp"
mv "$CASE/$BENCH.tmp" "$CASE/$BENCH"
expect 1 'claim:docs/benchmarking.md:8' \
  'a claim on a table row whose felt id sits on a different row'

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
sed 's/| p99 <= 8 ms |/| p99 <= 8 ms, 5.2x ahead of the round trip |/' \
  "$CASE/$SPEC" > "$CASE/$SPEC.tmp"
mv "$CASE/$SPEC.tmp" "$CASE/$SPEC"
expect 0 '' 'a claim in the budget table, on the row whose own cell id anchors it'

new_case
sed 's/| p99 <= 8 ms |/| p99 <= 8 ms, 5.2x ahead of bare Neovim |/' \
  "$CASE/$SPEC" > "$CASE/$SPEC.tmp"
mv "$CASE/$SPEC.tmp" "$CASE/$SPEC"
expect 1 'claim:.claude/specs/2026-07-17-view-design.md:13' \
  'a budget row naming the engine it beats, anchored by its own cell id'

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
# an identifier a spec row carries is one some file declares
# ---------------------------------------------------------------------------
new_case
sed 's/`echo\.view_p99_ms`/`echo.view_p99_millis`/' "$CASE/$SPEC" > "$CASE/$SPEC.tmp"
mv "$CASE/$SPEC.tmp" "$CASE/$SPEC"
expect 1 'spec-id:echo.view_p99_millis' \
  'a spec row naming a cell id no budget and no baseline declares'

new_case
sed 's/| bench suite |/| bench suite, `first_paint.minimal` |/' "$CASE/$SPEC" > "$CASE/$SPEC.tmp"
mv "$CASE/$SPEC.tmp" "$CASE/$SPEC"
expect 0 '' 'a spec row naming a cell the class baselines record and no budget bounds'

new_case
sed 's/| bench suite |/| bench suite, `echo_path` |/' "$CASE/$SPEC" > "$CASE/$SPEC.tmp"
mv "$CASE/$SPEC.tmp" "$CASE/$SPEC"
expect 0 '' 'a spec row naming a live harness row that publishes no baseline'

new_case
sed 's/| bench suite |/| bench suite, `echo_path` |/' "$CASE/$SPEC" > "$CASE/$SPEC.tmp"
mv "$CASE/$SPEC.tmp" "$CASE/$SPEC"
rm "$CASE/$BUILDS"
expect 1 'spec-id:echo_path' \
  'a spec row naming a row no harness table declares any more'

# ---------------------------------------------------------------------------
# a retired identifier is exempt where its own row retracts it, and only there
# ---------------------------------------------------------------------------
new_case
sed 's/| bench suite |/| bench suite, `cold_ms` |/' "$CASE/$SPEC" > "$CASE/$SPEC.tmp"
mv "$CASE/$SPEC.tmp" "$CASE/$SPEC"
expect 1 'spec-id:cold_ms' 'a retired identifier in a row that says nothing about withdrawing it'

new_case
sed 's/| bench suite |/| bench suite. `cold_ms` was withdrawn from this row |/' \
  "$CASE/$SPEC" > "$CASE/$SPEC.tmp"
mv "$CASE/$SPEC.tmp" "$CASE/$SPEC"
expect 0 '' 'a retired identifier inside the sentence that withdraws it'

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

# ---------------------------------------------------------------------------
# a ratio quoted beside a cell id is that cell's own recorded value: the
# anchor rule is satisfied by naming a cell, and a stale number beside the
# right cell is what a re-record leaves behind everywhere it was quoted
# ---------------------------------------------------------------------------
seat_echo_ratio() {
  cat >> "$CASE/$BASELINES/dev-linux.toml" <<'TOML'

[echo.minimal]
ratio_p50 = 1.1301046068104472
TOML
}

new_case
seat_echo_ratio
printf '\n| steady typing (`echo.view_p99_ms`, `echo.ratio_p50`) | ~1.17x slower |\n' \
  >> "$CASE/$BENCH"
expect 1 'ratio:docs/benchmarking.md:12' \
  'a multiplier beside the cell it names, quoting the draw a re-record replaced'

new_case
seat_echo_ratio
printf '\n| steady typing (`echo.view_p99_ms`, `echo.ratio_p50`) | ~1.13x slower |\n' \
  >> "$CASE/$BENCH"
expect 0 '' 'the same multiplier, equal to what that cell records at the digits printed'

# ---------------------------------------------------------------------------
# a number resolves against one class and one fixture: the ones its own unit
# names, the default class the page declares where it names none, and the one
# fixture its cells are recorded on where its words name none. Resolving
# against every class and fixture the unit mentioned passed one class's draw
# as another's and the 15-plugin leg's as the plugin-free one
# ---------------------------------------------------------------------------
seat_echo_user_ratio() {
  cat >> "$CASE/$BASELINES/dev-linux.toml" <<'TOML'

[echo.user]
ratio_p50 = 1.1097269280826585
TOML
}

seat_macos_echo_ratio() {
  cat > "$CASE/$BASELINES/dev-macos.toml" <<'TOML'
machine_class = "dev-macos"

[echo.minimal]
ratio_p50 = 1.1066140177690031
TOML
}

new_case
seat_echo_ratio
seat_macos_echo_ratio
printf '\n| steady typing, no plugins (`echo.view_p99_ms`, `echo.ratio_p50`) | ~1.107x slower |\n' \
  >> "$CASE/$BENCH"
expect 1 'ratio:docs/benchmarking.md:12' \
  'another class draw, quoted where the page resolves to its default class'

new_case
seat_echo_ratio
seat_macos_echo_ratio
printf '\n| steady typing, no plugins, dev-macos (`echo.view_p99_ms`, `echo.ratio_p50`) | ~1.107x slower |\n' \
  >> "$CASE/$BENCH"
expect 0 '' 'the same draw on the row that names the class recording it'

new_case
seat_echo_ratio
seat_echo_user_ratio
printf '\n| steady typing, login-shaped (`echo.view_p99_ms`, `echo.ratio_p50`) | ~1.130x slower |\n' \
  >> "$CASE/$BENCH"
expect 1 'ratio:docs/benchmarking.md:12' \
  'the plugin-free draw, quoted on the row that names the login-shaped leg'

new_case
seat_echo_ratio
seat_echo_user_ratio
printf '\n| steady typing, login-shaped (`echo.view_p99_ms`, `echo.ratio_p50`) | ~1.110x slower |\n' \
  >> "$CASE/$BENCH"
expect 0 '' 'the login-shaped draw on the row that names that fixture'

new_case
seat_echo_ratio
seat_echo_user_ratio
printf '\n| steady typing (`echo.view_p99_ms`, `echo.ratio_p50`) | ~1.130x slower |\n' \
  >> "$CASE/$BENCH"
expect 1 'scope:docs/benchmarking.md:12' \
  'a row naming no fixture, where the class records the cell on two'

new_case
seat_echo_ratio
seat_macos_echo_ratio
printf '\nOn dev-linux and dev-macos alike, `echo.ratio_p50` reads 1.130 plugin-free.\n' \
  >> "$CASE/$BENCH"
expect 1 'scope:docs/benchmarking.md:12' \
  'a paragraph naming two classes, with a number that belongs to one'

new_case
seat_echo_ratio
sed 's/, and `dev-linux` is the default class of this page//' "$CASE/$BENCH" > "$CASE/$BENCH.tmp"
mv "$CASE/$BENCH.tmp" "$CASE/$BENCH"
printf '\n| steady typing, no plugins (`echo.view_p99_ms`, `echo.ratio_p50`) | ~1.130x slower |\n' \
  >> "$CASE/$BENCH"
expect 1 'scope:docs/benchmarking.md:12' \
  'a page that declares no default class, where the row names none either'

new_case
seat_echo_ratio
sed 's/`dev-linux` is the default class/`dev-solaris` is the default class/' "$CASE/$BENCH" > "$CASE/$BENCH.tmp"
mv "$CASE/$BENCH.tmp" "$CASE/$BENCH"
expect 1 'default' 'a declared default class no shipped baseline records'

# ---------------------------------------------------------------------------
# a percentage states the same ratio as its distance from 1, and a percentage
# of a population states a share of it
# ---------------------------------------------------------------------------
new_case
seat_echo_ratio
printf '\n| steady typing, no plugins (`echo.ratio_p50`) | about 13%% behind |\n' \
  >> "$CASE/$BENCH"
expect 0 '' 'a percentage equal to the cell it stands beside, as its distance from 1'

new_case
seat_echo_ratio
printf '\n| steady typing, no plugins (`echo.ratio_p50`) | about 15%% behind |\n' \
  >> "$CASE/$BENCH"
expect 1 'ratio:docs/benchmarking.md:12' \
  'a percentage no value the cell records rounds to'

new_case
seat_echo_ratio
printf '\n| steady typing, no plugins (`echo.ratio_p50`) | a prediction answered 99.9%% of the samples |\n' \
  >> "$CASE/$BENCH"
expect 0 '' 'a percentage of a population, which is a share and not a ratio'

# ---------------------------------------------------------------------------
# the ledger quotes the same measurement the page does: a shortfall names its
# own class, scenario, fixture and metric, so a figure in its why beside a
# cell of that entry resolves exactly
# ---------------------------------------------------------------------------
new_case
sed 's/why = "a shortfall carries a scenario and a metric of its own"/why = "the recorded marker_ratio_p50 is 1.17"/' \
  "$CASE/$BUDGETS" > "$CASE/$BUDGETS.tmp"
mv "$CASE/$BUDGETS.tmp" "$CASE/$BUDGETS"
expect 1 'why:dev-linux/first_paint.minimal' \
  'a why quoting the draw a re-record replaced, beside the cell it names'

new_case
sed 's/why = "a shortfall carries a scenario and a metric of its own"/why = "the recorded marker_ratio_p50 is 1.094"/' \
  "$CASE/$BUDGETS" > "$CASE/$BUDGETS.tmp"
mv "$CASE/$BUDGETS.tmp" "$CASE/$BUDGETS"
expect 0 '' 'the same why, equal to what that cell records at the digits printed'

# ---------------------------------------------------------------------------
# the predicted glyph is a local reading, and the acceptance RTT leg is the
# only surface a transport claim may rest on
# ---------------------------------------------------------------------------
new_case
printf '\n| with the engine on the far side of a network | not yet recorded | your config, same host, same run |\n' \
  >> "$CASE/$PERF"
expect 1 'transport:docs/performance.md:6' \
  'a row on a user page stating a transport condition the cell never had'

new_case
printf '\nview draws the character it expects before the round trip is back.\n' \
  >> "$CASE/$PERF"
expect 1 'transport:docs/performance.md:6' \
  'the predicted glyph described beside a transport word, with no leg named'

new_case
printf '\nview draws the character it expects before the round trip is back; the round trip is the one scripts/acceptance/remote-rtt.sh injects.\n' \
  >> "$CASE/$PERF"
expect 0 '' 'the same sentence, naming the leg that measures the transport'

# ---------------------------------------------------------------------------
# the gate runs under the bash macOS ships
# ---------------------------------------------------------------------------
# Taskfile.yml runs each of these as `bash scripts/...`, so whichever bash is
# first on PATH decides whether the gate runs at all. A bash-4 construct is
# not a syntax error under 3.2, it is a gate that dies mid-run with a message
# reading as a script bug -- so the population is graded, not just the two
# scripts this matrix is about. Every script in the tree, not only the ones
# Taskfile.yml names: the release path runs scripts/package-bundle.sh from a
# workflow and scripts/mbp-build-leg.sh runs on the macOS host the contract
# is about, so a Taskfile-derived list left the five scripts no task names
# ungraded. Selected by shebang, not extension: the remote-test fixtures
# under scripts/test-fixtures/ are #!/bin/sh with no suffix and run on the
# macOS host too. The walk also keeps a new script graded without an edit
# here.
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
GUARDED=$(cd "$ROOT" && find scripts -type f -exec grep -lE '^#!.*(bash|/sh)$' {} + | LC_ALL=C sort) || true

report() {
  n=$((n + 1))
  if [ -z "$2" ]; then
    printf 'ok %s - %s\n' "$n" "$1"
    return
  fi
  failures=$((failures + 1))
  printf 'not ok %s - %s\n' "$n" "$1"
  printf '%s\n' "$2" | sed 's/^/  | /'
}

# An empty list is a finding on both legs and a pass on neither: grep with no
# file argument reads stdin, and the parse loop below iterates zero times and
# reports the scripts parsed having parsed none of them.
empty=""
if [ -z "$GUARDED" ]; then
  empty="scripts/ holds no *.sh file, so nothing was graded"
fi

# The tokens whose spelling would otherwise match this line are written with
# their first character bracketed: same language, and the scan grades this
# file by the same rule as every other without matching its own pattern.
if [ -n "$empty" ]; then
  modern="$empty"
else
  modern=$(cd "$ROOT" && grep -nE \
    'declare[[:space:]]+-[a-zA-Z]*[An]|local[[:space:]]+-[a-zA-Z]*[An]|typeset[[:space:]]+-[a-zA-Z]*[An]|\b[m]apfile\b|\b[r]eadarray\b|\[\[[[:space:]]+-v[[:space:]]|\$\{[A-Za-z_][A-Za-z_0-9]*(\[[^]]*\])?(,,|\^\^)|\|[&]|[&]>>|;;[&]' \
    $GUARDED) || true
fi
report 'no bash-4-only construct in the scripts under scripts/' "$modern"

# The grep above reads constructs; it cannot see the two shapes that made 3.2
# refuse this very checker -- a case pattern and an apostrophe in a comment,
# both inside a process substitution, whose closing paren 3.2 miscounts. Only
# a parse under a pre-4 bash catches those, and the host that has one is the
# host the contract is about, so the leg runs where /bin/bash is old and says
# so where it is not.
stock=/bin/bash
stock_major=$("$stock" -c 'echo "${BASH_VERSINFO[0]}"' 2>/dev/null || echo 9)
if [ -n "$empty" ]; then
  report "the scripts parse under $stock" "$empty"
elif [ "${stock_major:-9}" -ge 4 ]; then
  n=$((n + 1))
  printf 'ok %s - %s # skip %s is bash %s, nothing pre-4 to parse under\n' \
    "$n" 'the scripts parse under stock bash' "$stock" "$stock_major"
else
  stale=$(cd "$ROOT" && for f in $GUARDED; do
    out=$("$stock" -n "$f" 2>&1) || true
    [ -n "$out" ] && printf '%s\n' "$out"
  done) || true
  report "the scripts parse under $stock (bash $stock_major)" "$stale"
fi

printf '\n%s cases, %s failures\n' "$n" "$failures"
[ "$failures" -eq 0 ]
