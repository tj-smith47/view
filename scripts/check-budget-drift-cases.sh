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
keystroke in a thousand takes 0.73 ms and Neovim's takes 0.67 ms. Every number here was taken on a shared Linux dev host, whose `dev-linux` is the default class of this page.
MD
  mkdir -p "$CASE/docs"
  cat > "$CASE/$PERF" <<'MD'
# Performance

Under a plugin-free config, view's worst keystroke in a thousand takes
0.73 ms and Neovim's takes 0.67 ms. Every number here was taken on a shared Linux dev host, whose `dev-linux` is the default class of this page.

| | view | Neovim | on |
|---|---|---|---|
| keypress to glyph, worst case in a thousand | 0.73 ms | 0.67 ms | same host, same run, no plugins |
| the same keypress, with view drawing the glyph it expects | 0.32 ms | 1.25 ms | same host, same run, no plugins |
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

# A row inside the spec's own 3.1 section, which is the second surface the
# transport window is read on. It lands immediately before the section that
# closes 3.1, so the line it is reported at is the same in every case.
add_spec_row() {
  awk -v row="$1" '/^## 4[.] After the budgets$/ { print row } { print }' \
    "$CASE/$SPEC" > "$CASE/$SPEC.tmp"
  mv "$CASE/$SPEC.tmp" "$CASE/$SPEC"
}

# The seat those draws were reduced to. The band is a fraction of it, so a
# case about what the band reaches needs a seat of the magnitude the foreign
# figure has.
set_accepted() {
  awk -v v="$1" '/^accepted = / { print "accepted = " v; next } { print }' \
    "$CASE/$BUDGETS" > "$CASE/$BUDGETS.tmp"
  mv "$CASE/$BUDGETS.tmp" "$CASE/$BUDGETS"
}

# The draws a record run took, on the shortfall the tree plants. They land
# above its why, so the line the finding names is the same in every case.
set_trials() {
  awk -v arr="$1" '/^why = / { print "trials = " arr } { print }' \
    "$CASE/$BUDGETS" > "$CASE/$BUDGETS.tmp"
  mv "$CASE/$BUDGETS.tmp" "$CASE/$BUDGETS"
}

# A felt statement on the speculated row, which is the third surface: the
# budgets file states the moment in a person's words the way the pages do.
add_felt_row() {
  cat >> "$CASE/$BUDGETS" <<TOML

[[budget]]
spec_row = "Keypress -> cell change end-to-end, steady typing"
scenario = "echo_speculated"
metric = "speculated_ratio_p50"
max = 8.0
kind = "felt"
felt = "$1"
config = "real"
TOML
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
    /^BUDGET DRIFT FAIL: moment-(drift|scope|default) / {
      c = $5; sub(/:$/, "", c); print $4 ":" c; next
    }
    /^BUDGET DRIFT FAIL: why-drift / {
      c = $5; sub(/:$/, "", c); print "why:" c; next
    }
    /^BUDGET DRIFT FAIL: why-figure / {
      c = $5; sub(/:$/, "", c); print "unattributed:" c; next
    }
    /^BUDGET DRIFT FAIL: trials-band / {
      c = $5; sub(/:$/, "", c); print "trials:" c; next
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
# both directions. A page saying view is behind the engine names it in a
# comparative exactly as one saying view is ahead of it does, and a word list
# of wins alone passed "9.6% behind Neovim's" on the page a person reads.
# ---------------------------------------------------------------------------
new_case
printf '\nWith no plugins at all the same screen is 9.6%% behind Neovim.\n' \
  >> "$CASE/$PERF"
expect 1 'claim:docs/performance.md:11' 'a page saying view trails the engine, in the word that takes it directly'

new_case
printf '\nUnder your config the same screen is slower than bare Neovim.\n' \
  >> "$CASE/$PERF"
expect 1 'claim:docs/performance.md:11' 'a page saying view is slower than the engine'

new_case
printf '\nThe mark lands later than Neovim does under the same load.\n' \
  >> "$CASE/$PERF"
expect 1 'claim:docs/performance.md:11' 'a page saying view lands later than the engine'

new_case
printf '\nThe tail reads worse than bare nvim on this class.\n' >> "$CASE/$PERF"
expect 1 'claim:docs/performance.md:11' 'a page saying view reads worse than the engine'

new_case
printf '\nWith no plugins at all the same screen is 9.6%% behind on that screen.\n' \
  >> "$CASE/$PERF"
expect 0 '' 'the same sentence with the engine name gone, which is what the page may say'

new_case
printf '\n| first paint (`echo.view_p99_ms`) | 1.2x slower than bare nvim |\n' \
  >> "$CASE/$BENCH"
expect 1 'claim:docs/benchmarking.md:12' \
  'a row naming the engine it trails, standing beside a felt cell id'

# ---------------------------------------------------------------------------
# the pages wrap at 80 characters, so the comparative and its object sit on
# either side of the margin as often as on one line, and markup sits on the
# comparative the way it sits on the engine's name
# ---------------------------------------------------------------------------
new_case
printf '\nThe moments where view is currently *slower*\nthan Neovim are written down with the rest.\n' \
  >> "$CASE/$PERF"
expect 1 'claim:docs/performance.md:11' 'a comparative wrapped across the margin, reported where it starts'

new_case
printf '\nThe moments where view is currently *slower* than Neovim are here.\n' \
  >> "$CASE/$PERF"
expect 1 'claim:docs/performance.md:11' 'a comparative wearing the markup the pages emphasise it with'

new_case
printf '\nview is 1.5 ms behind on that screen, and the tree writes it down.\n' \
  >> "$CASE/$PERF"
expect 0 '' 'a gap stated with no engine named, which the rule does not touch'

new_case
printf '\nThe started mark lands 1.64 ms later under view than under the TUI.\n' \
  >> "$CASE/$PERF"
expect 0 '' 'a subject standing between the comparative and its preposition, past the reach'

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
# no header word buys a table out of the walk: a word in a heading cell used
# to take every figure under it out of this check and out of the sweep at
# once, which is a bypass one edit wide
# ---------------------------------------------------------------------------
new_case
seat_echo_ratio
printf '\n| measurement (superseded) | reading |\n| --- | --- |\n| steady typing, no plugins (`echo.view_p99_ms`, `echo.ratio_p50`) | ~1.107x slower |\n' \
  >> "$CASE/$BENCH"
expect 1 'ratio:docs/benchmarking.md:14' \
  'a drifted figure under a header cell carrying the word superseded'

new_case
seat_echo_ratio
printf '\n| measurement (superseded) | reading |\n| --- | --- |\n| steady typing, no plugins (`echo.view_p99_ms`, `echo.ratio_p50`) | ~1.130x slower |\n' \
  >> "$CASE/$BENCH"
expect 0 '' 'the same table where the figure states the seat it names'

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
# a millisecond absolute resolves like a ratio where a millisecond cell id
# stands beside it: the page shipped one column of another class's absolute
# for months, passing because an absolute carries its unit. The paired
# bare-engine column names no cell of view's own and stays ungraded
# ---------------------------------------------------------------------------
seat_echo_tail() {
  cat >> "$CASE/$BASELINES/dev-linux.toml" <<'TOML'

[echo.minimal]
view_p99_ms = 0.726575
TOML
}

new_case
seat_echo_tail
printf '\n| the tail, no plugins (`echo.view_p99_ms` 0.73 ms) | budget 8 ms |\n' \
  >> "$CASE/$BENCH"
expect 0 '' 'a millisecond absolute equal to what the cell id beside it records'

new_case
seat_echo_tail
printf '\n| the tail, no plugins (`echo.view_p99_ms` 0.80 ms) | budget 8 ms |\n' \
  >> "$CASE/$BENCH"
expect 1 'ratio:docs/benchmarking.md:12' \
  'a millisecond absolute no value that cell records rounds to'

new_case
seat_echo_tail
printf '\n| the tail, no plugins (`echo.view_p99_ms`) | 0.73 ms | 0.67 ms |\n' \
  >> "$CASE/$BENCH"
expect 0 '' 'the paired bare-engine column, which names no cell of its own'

# the view column resolves against one class like every other number: a
# figure that is a sibling class's draw is the failure a re-record leaves at
# every site the page quoted the old one at
seat_macos_echo_tail() {
  cat > "$CASE/$BASELINES/dev-macos.toml" <<'TOML'
machine_class = "dev-macos"

[echo.minimal]
view_p99_ms = 5.244
TOML
}

new_case
seat_echo_tail
seat_macos_echo_tail
printf '\n| the tail, no plugins (`echo.view_p99_ms` 5.24 ms) | budget 8 ms |\n' \
  >> "$CASE/$BENCH"
expect 1 'ratio:docs/benchmarking.md:12' \
  'another class millisecond draw, quoted where the page resolves to its default class'

# a diagnostic records a negative, and a page number regex without a sign
# read one as no number at all -- ungraded on the page while the ledger
# graded the same figure
seat_startup_delta() {
  cat >> "$CASE/$BASELINES/dev-linux.toml" <<'TOML'

[startup.minimal]
server_delta_ms = -0.07600000000000051
TOML
}

new_case
seat_startup_delta
printf '\n| the engine own startup, no plugins (`startup.server_delta_ms` -0.076 ms) | a diagnostic |\n' \
  >> "$CASE/$BENCH"
expect 0 '' 'a negative millisecond absolute equal to what the cell id beside it records'

new_case
seat_startup_delta
printf '\n| the engine own startup, no plugins (`startup.server_delta_ms` -0.099 ms) | a diagnostic |\n' \
  >> "$CASE/$BENCH"
expect 1 'ratio:docs/benchmarking.md:12' \
  'a negative millisecond absolute no value that cell records rounds to'

# ---------------------------------------------------------------------------
# an absolute is graded whatever unit it carries, because the cell id beside
# it says which cell it is and the unit says which cells it can be. Reading
# milliseconds alone left the footprint and the input path ungraded on the
# page that publishes both
# ---------------------------------------------------------------------------
seat_memory_pss() {
  cat >> "$CASE/$BASELINES/dev-linux.toml" <<'TOML'

[memory.minimal]
pss_mb = 4.961331
TOML
}

new_case
seat_memory_pss
printf '\n| the footprint, no plugins (`memory.pss_mb` 4.96 MB) | a resource |\n' \
  >> "$CASE/$BENCH"
expect 0 '' 'a megabyte absolute equal to what the cell id beside it records'

new_case
seat_memory_pss
printf '\n| the footprint, no plugins (`memory.pss_mb` 5.20 MB) | a resource |\n' \
  >> "$CASE/$BENCH"
expect 1 'ratio:docs/benchmarking.md:12' \
  'a megabyte absolute no value that cell records rounds to'

seat_input_path_us() {
  cat >> "$CASE/$BASELINES/dev-linux.toml" <<'TOML'

[input_path.minimal]
key_to_rpc_p99_us = 73.295
TOML
}

new_case
seat_input_path_us
printf '\n| the key to the engine, no plugins (`input_path.key_to_rpc_p99_us` 73.3 us) | a diagnostic |\n' \
  >> "$CASE/$BENCH"
expect 0 '' 'a microsecond absolute equal to what the cell id beside it records'

new_case
seat_input_path_us
printf '\n| the key to the engine, no plugins (`input_path.key_to_rpc_p99_us` 70.1 us) | a diagnostic |\n' \
  >> "$CASE/$BENCH"
expect 1 'ratio:docs/benchmarking.md:12' \
  'a microsecond absolute no value that cell records rounds to'

# ---------------------------------------------------------------------------
# the two user-facing pages carry no identifier -- the identifier rule
# refuses one there -- so the moment they state in words is what a figure
# resolves against, on the class the page declares. Ungraded, those figures
# were the only recorded values on the tree a re-record left standing
# ---------------------------------------------------------------------------
seat_echo_user_tail() {
  cat >> "$CASE/$BASELINES/dev-linux.toml" <<'TOML'

[echo.user]
view_p99_ms = 1.5773
TOML
}

# the pages are prose, so a case that moves one figure says so with sed
rewrite_readme() {
  sed "s|$1|$2|" "$CASE/$README" > "$CASE/$README.tmp"
  mv "$CASE/$README.tmp" "$CASE/$README"
}

new_case
seat_echo_tail
expect 0 '' 'a moment stated in words, at the value the class the page declares records'

new_case
seat_echo_tail
rewrite_readme 'takes 0.73 ms' 'takes 0.83 ms'
expect 1 'moment-drift:README.md:4' \
  'the same moment quoting a figure that cell records on no class'

new_case
seat_echo_tail
rewrite_readme ' Every number here.*$' ''
expect 1 'moment-default:README.md:4' \
  'a moment on a page that declares no class, whose host no reader is told'

new_case
seat_echo_tail
seat_echo_user_tail
rewrite_readme 'Plugin-free, view' 'View'
expect 1 'moment-scope:README.md:4' \
  'a moment naming no leg, where the cell is recorded on two of them'

new_case
rewrite_readme '`dev-linux` is the default' '`dev-solaris` is the default'
expect 1 'moment-default:README.md' \
  'a page declaring a default class no baseline in the tree ships under'

# ---------------------------------------------------------------------------
# a number resolves to one cell, not to the union of every cell its unit
# names: the nearest id before it in its own sentence, and the unit's first
# id where its sentence names none. A union passed a sibling metric's draw
# and a sibling scenario's alike
# ---------------------------------------------------------------------------
seat_echo_pair() {
  cat >> "$CASE/$BASELINES/dev-linux.toml" <<'TOML'

[echo.minimal]
ratio_p50 = 1.1301046068104472
ratio_p99 = 1.0916525318862538
TOML
}

seat_scroll_ratio() {
  cat >> "$CASE/$BASELINES/dev-linux.toml" <<'TOML'

[scroll.minimal]
ratio_p50 = 1.6053
TOML
}

new_case
seat_echo_pair
printf '\n| steady typing, no plugins | `echo.ratio_p99` 1.09 at the tail; `echo.ratio_p50` reads 1.130 |\n' \
  >> "$CASE/$BENCH"
expect 0 '' 'two cells on one row, each number standing beside the id it belongs to'

new_case
seat_echo_pair
printf '\n| steady typing, no plugins | `echo.ratio_p99` 1.09 at the tail; `echo.ratio_p50` reads 1.09 |\n' \
  >> "$CASE/$BENCH"
expect 1 'ratio:docs/benchmarking.md:12' \
  'a sibling metric draw, quoted beside the id that records another'

new_case
seat_echo_pair
seat_scroll_ratio
printf '\n| plugin-free | `echo.ratio_p50` 1.130 | `scroll.ratio_p50` 1.605 |\n' \
  >> "$CASE/$BENCH"
expect 0 '' 'two scenarios on one row, each number beside its own id'

new_case
seat_echo_pair
seat_scroll_ratio
printf '\n| plugin-free | `echo.ratio_p50` 1.130 | `scroll.ratio_p50` 1.130 |\n' \
  >> "$CASE/$BENCH"
expect 1 'ratio:docs/benchmarking.md:12' \
  'a sibling scenario draw, quoted beside the id that records another'

new_case
seat_echo_pair
printf '\n| steady typing, no plugins (`echo.ratio_p50`) | 1.09 |\n' \
  >> "$CASE/$BENCH"
expect 1 'ratio:docs/benchmarking.md:12' \
  'a number whose own sentence names no id, resolved against the unit first id'

# ---------------------------------------------------------------------------
# every figure a why states is the value of a cell that why names, or an
# observation of the trials the record run drew. The ledger wrote most of
# its figures in words, and an unattributed one goes stale in silence
# ---------------------------------------------------------------------------
seat_echo_control() {
  cat >> "$CASE/$BASELINES/dev-linux.toml" <<'TOML'

[echo_control.minimal]
control_ratio_p50 = 0.9937197455771009
TOML
}

# the substitution is delimited on a pipe, not a slash: a why the cases
# rewrite may itself carry slashes
rewrite_why() {
  sed "s|why = \"a shortfall carries a scenario and a metric of its own\"|why = \"$1\"|" \
    "$CASE/$BUDGETS" > "$CASE/$BUDGETS.tmp"
  mv "$CASE/$BUDGETS.tmp" "$CASE/$BUDGETS"
}

new_case
rewrite_why 'the residual is 1.17 of the paired run'
expect 1 'unattributed:dev-linux/first_paint.minimal' \
  'a why figure standing in a sentence that names no cell'

new_case
rewrite_why 'three trials read 1.17 / 1.18 / 1.19 on this cell'
expect 1 'unattributed:dev-linux/first_paint.minimal' \
  'the draws a record run took, left in the prose, where they are attributed to nothing'

new_case
rewrite_why 'the trials put marker_ratio_p50 at 1.17'
expect 1 'why:dev-linux/first_paint.minimal' \
  'a trial sentence quoting the entry own seat at a value it does not record'

new_case
rewrite_why 'the trials put marker_ratio_p50 at 1.094'
expect 0 '' 'the same trial sentence at the value that cell records'

new_case
seat_echo_control
rewrite_why 'the echo_control row costs echo_control.control_ratio_p50 0.994 here'
expect 0 '' 'a why naming a cell of another scenario in full, at the value it records'

new_case
seat_echo_control
rewrite_why 'the echo_control row costs echo_control.control_ratio_p50 1.038 here'
expect 1 'why:dev-linux/first_paint.minimal' \
  'the same cell written in full, quoting a figure no baseline records'

new_case
rewrite_why 'marker_ratio_p50 1.0937, 9.4 percent over the 1.0 bar'
expect 0 '' 'a percentage stating the distance from the bar its own sentence names'

new_case
rewrite_why 'marker_ratio_p50 1.0937, 9.0 percent over the 1.0 bar'
expect 1 'why:dev-linux/first_paint.minimal' \
  'a percentage no distance from that bar rounds to'

new_case
rewrite_why 'A probe reading: the trials put marker_ratio_p50 at 1.17'
expect 1 'why:dev-linux/first_paint.minimal' \
  'the same drifted figure behind the word a retired-reading escape read for'

# ---------------------------------------------------------------------------
# the draws a record run took are a field, and every member of it is a draw
# of the metric the entry seats: one further out than the band the statistic
# draws in is a figure of another quantity, refused in the file check and in
# the loader alike
# ---------------------------------------------------------------------------
new_case
set_trials '[31.0, 30.2, 31.4]'
expect 0 '' 'an array of draws of the metric the entry seats'

new_case
set_trials '[31.0, 30.2, 39.4]'
expect 1 'trials:dev-linux/first_paint.minimal' \
  'a member further from the accepted value than a draw of that metric goes'

new_case
set_trials '[31.0, 55.079, 50.037]'
expect 1 'trials:dev-linux/first_paint.minimal' \
  'the accepted value prepended to two readings of the paired bare-engine arm'

new_case
set_trials '[23.3, 30.2, 38.7]'
expect 0 '' 'the widest draws the band admits, which a noisy statistic reaches'

# What a band on magnitude cannot reach, stated rather than implied: the
# paired bare-engine arm of a near-1 ratio reads in the same milliseconds
# the seat does, so it sits inside the band and passes. The rule that keeps
# it out is the one this round applied by hand -- one quantity per entry,
# the arm in the round report.
new_case
set_accepted 16.914409
set_trials '[16.914, 17.480]'
expect 0 '' 'the paired arm cadence beside a seat of its own magnitude, which no band on magnitude refuses'

# ---------------------------------------------------------------------------
# the predicted glyph is a local reading, and the acceptance RTT leg is the
# only surface a transport claim may rest on. A table is one subject spread
# over its rows, so the scope is the table and the finding is the row: the
# shipped defect carried no speculated word of its own and stood in the
# table whose subject is the predicted glyph
# ---------------------------------------------------------------------------
new_case
printf '| with the engine on the far side of a network | not yet recorded | your config, same host, same run |\n' \
  >> "$CASE/$PERF"
expect 1 'transport:docs/performance.md:10' \
  'a row in the speculated table stating a transport condition the cell never had'

new_case
printf '| attach-plus-takeover round trip after VimEnter | 1.4 ms | n/a | same host, same run |\n' \
  >> "$CASE/$PERF"
expect 0 '' 'a local round trip, planted in the speculated table, which is this tree own phrase for the serial attach'

new_case
printf '\nview draws the character it expects before the network answers.\n' \
  >> "$CASE/$PERF"
expect 1 'transport:docs/performance.md:11' \
  'the predicted glyph described beside a transport word, with no leg named'

new_case
printf '\nview draws the character it expects before the network answers; the round trip is the one scripts/acceptance/remote-rtt.sh injects.\n' \
  >> "$CASE/$PERF"
expect 0 '' 'the same sentence, naming the leg that measures the transport'

new_case
printf '\n### Landing before v0.1\n\n- [x] **Remote editing.** `view --remote host:path`: engine over SSH,\n      paint and input local, keystrokes echoed without waiting for the\n      round trip.\n' \
  >> "$CASE/$README"
expect 0 '' 'a roadmap bullet naming remote editing and no speculated moment'

# a list item is its own unit, and the leg it names is the licensed surface:
# the true sentence about remote editing says both what the transport is and
# what view draws while it waits, which passes on the bullet that names the
# leg injecting the round trip and fails on the one that names none
new_case
printf '\n### Landing before v0.1\n\n- [x] **Remote editing.** `view --remote host:path`: engine over SSH,\n      keystrokes echoed without waiting for the round trip, view drawing\n      the character it expects; the injected round trips are the\n      acceptance leg scripts/acceptance/remote-rtt.sh.\n' \
  >> "$CASE/$README"
expect 0 '' 'a roadmap bullet describing the predicted glyph and naming the leg that injects the round trip'

new_case
printf '\n### Landing before v0.1\n\n- [x] **Remote editing.** `view --remote host:path`: engine over SSH,\n      keystrokes echoed without waiting for the round trip, view drawing\n      the character it expects; the injected round trips are the\n      acceptance leg scripts/acceptance/remote-rtt.sh.\n- [ ] **Detach and reconnect.** view keeps drawing the character it\n      expects while the engine is on the far side of a network.\n' \
  >> "$CASE/$README"
expect 1 'transport:README.md:12' \
  'a licensed item beside one naming no leg, where only the second rests on nothing'

new_case
printf '\n### Landing before v0.1\n\n- [x] **Remote editing.** `view --remote host:path` runs the engine\n      over SSH.\n- [ ] **Detach and reconnect.** view keeps drawing the character it expects\n      while the link is down.\n' \
  >> "$CASE/$README"
expect 0 '' 'two sibling bullets, one carrying a transport word and one a speculated moment'

new_case
printf '\n### Landing before v0.1\n\n1. **Remote editing.** `view --remote host:path` runs the engine\n   over SSH.\n2. **Detach and reconnect.** view keeps drawing the character it expects\n   while the link is down.\n' \
  >> "$CASE/$README"
expect 0 '' 'two sibling numbered items, one carrying a transport word and one a speculated moment'

new_case
printf '\n### Landing before v0.1\n\n+ **Remote editing.** `view --remote host:path` runs the engine\n  over SSH.\n+ **Detach and reconnect.** view keeps drawing the character it expects\n  while the link is down.\n' \
  >> "$CASE/$README"
expect 0 '' 'the same two items under the third bullet marker markdown allows'

# ---------------------------------------------------------------------------
# a transport word inside a denial is the page saying the reading was not
# taken across one, which is the sentence this rule most wants written. The
# rule read co-occurrence alone, so every true denial failed
# ---------------------------------------------------------------------------
new_case
printf '\nA prediction of when remote editing lands is not something this README makes.\n' \
  >> "$CASE/$README"
expect 0 '' 'a sentence predicting a release date, which predicts no glyph'

new_case
printf '\nThe picker prediction cache is unrelated to the remote engine.\n' \
  >> "$CASE/$README"
expect 0 '' 'the picker cache, which is the other thing this tree calls a prediction'

new_case
printf '\nview speaks to a remote engine over SSH; no prediction is involved in the numbers on this page.\n' \
  >> "$CASE/$README"
expect 0 '' 'a page saying the numbers rest on no prediction at all'

new_case
printf '\nEvery number here was measured with the engine local to the terminal rather than on another machine, and the predicted glyph is no exception.\n' \
  >> "$CASE/$README"
expect 0 '' 'a page stating the condition its readings were taken under, which is local'

# The same statement as the case above wrote it before the licence was
# narrowed to the transport word's own clause. It is refused now: the
# `local` stands in an earlier clause, and `nothing` is neither a denial
# this check reads nor within the five tokens the window looks back over.
new_case
printf '\nEvery number here is local: nothing on this page was measured with the engine on another machine, and the predicted glyph is no exception.\n' \
  >> "$CASE/$README"
expect 1 'transport:README.md:6' \
  'the same statement with its local in an earlier clause, which the window does not reach'

# the licence belongs to the clause that carries `local` and to no other: an
# unrelated clause reporting a local picker cache licensed the transport
# claim beside it, and a clause denying localness licensed its own inversion
new_case
printf '\nThe predicted glyph here is not local: it was measured with the engine on another machine.\n' \
  >> "$CASE/$README"
expect 1 'transport:README.md:6' \
  'a unit denying localness in one clause and asserting the transport in the next'

new_case
printf '\nThe picker cache is local, and the predicted glyph was measured on another machine.\n' \
  >> "$CASE/$README"
expect 1 'transport:README.md:6' \
  'a local reported in a clause the transport claim does not stand in'

new_case
printf '\nThe predicted glyph is not local when the engine happens to sit on another machine.\n' \
  >> "$CASE/$README"
expect 1 'transport:README.md:6' \
  'one clause carrying both, with the local itself denied and the transport word out of the window'

new_case
printf '\nThe predicted glyph was measured on another machine -- the reading this page publishes is local.\n' \
  >> "$CASE/$README"
expect 1 'transport:README.md:6' \
  'a local behind the em dash this tree writes as two hyphens, which ends the clause'

# the window itself, on each of the three surfaces that read it: a denial
# beside the transport word licenses the unit and the same denial six tokens
# ahead of it licenses nothing, so removing the scan reddens every surface
new_case
printf '\nNo network stands between the two engines when the predicted glyph is drawn.\n' \
  >> "$CASE/$README"
expect 0 '' 'a denial one token before the transport word on the page'

new_case
printf '\nNo engine anywhere near this page was measured with the predicted glyph drawn across a network.\n' \
  >> "$CASE/$README"
expect 1 'transport:README.md:6' \
  'the same denial six tokens ahead of the transport word, which denies nothing about it'

new_case
add_spec_row '| Keystroke to predicted glyph | never measured across a network | bench suite |'
expect 0 '' 'a denial beside the transport word in a spec 3.1 row'

new_case
add_spec_row '| Keystroke to predicted glyph | no bench leg has ever put the engine across a network | bench suite |'
expect 1 'transport:.claude/specs/2026-07-17-view-design.md:16' \
  'the same denial six tokens ahead of it in a spec 3.1 row'

new_case
add_felt_row 'the glyph view expects, never drawn across a network'
expect 0 '' 'a denial beside the transport word in a felt statement'

new_case
add_felt_row 'the glyph view expects, drawn with no engine this bench leg ever put across a network'
expect 1 'transport:crates/view-bench/budgets.toml:33' \
  'the same denial six tokens ahead of it in a felt statement'

# the two multi-word transport phrases are reachable only from the phrase own
# match position: no whitespace token ever equals `another machine`, so a
# token walk refused every true denial written around one
new_case
printf '\nThe predicted glyph was never measured on another machine.\n' \
  >> "$CASE/$README"
expect 0 '' 'a denial before a two-word transport phrase'

new_case
printf '\nThe predicted glyph was drawn on the far side of a network.\n' \
  >> "$CASE/$README"
expect 1 'transport:README.md:6' \
  'the same phrase with no denial anywhere before it'

# the rules file own example of a licensed unit, planted exactly as it is
# written there, so the page teaching the rule and the check reading it
# cannot drift apart
new_case
printf '\nNaming the acceptance RTT leg by its script name is the licensed\nsurface: a unit that says `scripts/acceptance/remote-rtt.sh` (or the\n`remote_memory` row) may describe the predicted glyph and a transport\ncondition together, because the leg it names is where the round trip is\ninjected. So the true sentence about remote editing -- keystrokes echoed\nwithout waiting for the round trip, view drawing the character it expects\n-- passes where it names that leg and fails where it names none.\n' \
  >> "$CASE/$README"
expect 0 '' 'the licensed-surface paragraph, exactly as the rules file writes it'

new_case
printf '\nEvery number here was measured with the engine on another machine, and the predicted glyph is no exception.\n' \
  >> "$CASE/$README"
expect 1 'transport:README.md:6' \
  'the same sentence asserting the transport the recording never had'

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
# ungraded. Selected through scripts/lib/script-population.sh, the same
# shebang read check-style.sh and check-portability.sh make: the remote-test
# fixtures under scripts/test-fixtures/ are #!/bin/sh with no suffix and run
# on the macOS host too, and a `grep -l` of its own answered a first-line
# rule with a match anywhere in the file. The walk also keeps a new script
# graded without an edit here.
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
# shellcheck source=lib/script-population.sh
. "$ROOT/scripts/lib/script-population.sh"

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
# reports the scripts parsed having parsed none of them. A population the
# selection could not finish reading is the same finding, for the same
# reason.
empty=""
GUARDED=""
if ! script_population_read "$ROOT"; then
  empty="a file under scripts/ could not be read, so the population is short"
elif [ -z "$SCRIPT_POPULATION" ]; then
  empty="scripts/ holds no file whose first line names bash or sh, so nothing was graded"
else
  GUARDED="$SCRIPT_POPULATION"
fi

# The tokens whose spelling would otherwise match this line are written with
# their first character bracketed: same language, and the scan grades this
# file by the same rule as every other without matching its own pattern.
#
# A pattern substitution is on the list for its cost rather than its
# vintage: bash 3.2 rescans the string per match, so an emptiness test
# written as "delete every blank and see what is left" never returns on a
# multi-line variable, and the drift check spun in bash itself on a 12 kB
# one where a glob test answered at once.
MODERN='declare[[:space:]]+-[a-zA-Z]*[An]|local[[:space:]]+-[a-zA-Z]*[An]|typeset[[:space:]]+-[a-zA-Z]*[An]|\b[m]apfile\b|\b[r]eadarray\b|\[\[[[:space:]]+-v[[:space:]]|\$\{!?[A-Za-z_][A-Za-z_0-9]*(\[[^]]*\])?(,,|\^\^|//)|\|[&]|[&]>>|;;[&]'
if [ -n "$empty" ]; then
  modern="$empty"
else
  modern=$(cd "$ROOT" && grep -nE "$MODERN" $GUARDED) || true
fi
report 'no bash-4-only construct in the scripts under scripts/' "$modern"

# The list is worth what it refuses, so one is planted and the same pattern
# is asked about it. The construct is assembled through a %s rather than
# written out, so planting it here does not make this file a hit.
planted="$WORK/planted.sh"
printf '#!/usr/bin/env bash\nseats=$1\nprintf %%s "$%s{seats//x/y}"\n' '' > "$planted"
caught=$(grep -nE "$MODERN" "$planted") || true
missed=""
if [ -z "$caught" ]; then
  missed="the planted pattern substitution went unrefused"
fi
report 'the construct list refuses a pattern substitution planted in a script' "$missed"

# The indirect spelling costs what the direct one costs -- it rescans the
# string of whatever the name holds -- so it is in the class and the list is
# asked about it separately.
printf '#!/usr/bin/env bash\nname=seats\nprintf %%s "$%s{!name//x/y}"\n' '' > "$planted"
caught=$(grep -nE "$MODERN" "$planted") || true
missed=""
if [ -z "$caught" ]; then
  missed="the planted indirect pattern substitution went unrefused"
fi
report 'the construct list refuses an indirect pattern substitution' "$missed"

# The anchored spellings rebuild the string once rather than once per match,
# so they are siblings in spelling and not in cost: refusing them would ban a
# construct the rule has no ground to ban.
printf '#!/usr/bin/env bash\nseats=$1\nprintf %%s "$%s{seats/#x/y}$%s{seats/%%x/y}"\n' '' '' > "$planted"
caught=$(grep -nE "$MODERN" "$planted") || true
missed=""
if [ -n "$caught" ]; then
  missed="an anchored substitution was refused: $caught"
fi
report 'the construct list allows the anchored substitutions, which match once' "$missed"

# The grep above reads a line for a spelling, and the shape that stopped the
# style matrix parsing on macOS is not one: a `case` whose word ran onto the
# next line, so the closing paren of a pattern below it ended the command
# substitution the whole thing sat in. It has a tell a line-at-a-time scan
# can see -- a `case` with no `in` beside it -- and every one in this
# population is written on one line, so the tell is the rule. Read wherever a
# command can start rather than at the start of a line: the same header written
# after a brace, a `;` or a `&&` breaks 3.2 identically, and the spelling that
# broke the macOS run was at line start only because that is where someone
# happened to write it. `!` is on the list beside them, for the same reason.
#
# The position list stays, where the link walk in check-style-cases.sh dropped
# its own: `case` is an ordinary English word and this population writes it in
# prose inside nine string literals, so a walk that read the bare word would
# redden all nine. It is the one list, held in scripts/lib/script-population.sh
# beside the reader, and the guard harvest in check-style-cases.sh reads the
# same one for the same reason -- `test` is an English word too, and two lists
# for one boundary is how the third spelling gets written. What the walks
# share beyond the list is the boundary itself: all of them read the reader
# command text, so a `case` in a comment or in a here-doc body is not a header
# wherever the punctuation in front of it sits. The ceiling is a `case` word
# reached some way the list does not name; the parse leg under a stock 3.2 is
# what answers for that.
SPLIT_CASE="$SCRIPT_COMMAND_START"'case([[:space:]]|$)'
split_case_headers() {
  awk -v SQ="'" -v CW="$SPLIT_CASE" "$SCRIPT_CODE_AWK"'
    {
      script_code_scan($0)
      if (CODE ~ CW && CODE !~ /[[:space:]]in([[:space:]]|$)/) {
        print FILENAME ":" FNR ": " $0
      }
    }
  ' "$@"
}
if [ -n "$empty" ]; then
  split="$empty"
else
  split=$(cd "$ROOT" && split_case_headers $GUARDED)
fi
report 'no case header split across lines, whose paren bash 3.2 then miscounts' "$split"

# planted, because the scan above is worth what it refuses, and the spelling
# it refuses is one no file in the population still carries
printf '%s\n' '#!/usr/bin/env bash' 'case "' '$x' '" in' '  *"' '$d' '"*) ;;' 'esac' > "$planted"
caught=$(split_case_headers "$planted")
missed=""
if [ -z "$caught" ]; then
  missed="the planted split case header went unrefused"
fi
report 'the split-case scan refuses a case whose word runs onto the next line' "$missed"

# the same header inside a one-line function, which is where this population
# writes most of its `case` and where a line-start anchor sees nothing
# the header is assembled through a variable, so planting one here does not
# make this file a hit
header='case "'
printf '%s\n' '#!/usr/bin/env bash' "holds() { $header" '$x' '" in' '  *) ;;' 'esac; }' > "$planted"
caught=$(split_case_headers "$planted")
missed=""
if [ -z "$caught" ]; then
  missed="the planted split header after a brace went unrefused"
fi
report 'the split-case scan reads a case wherever a command starts' "$missed"

# The `if` position, which the list carried for the guard harvest and not for
# this one until the two became the same list.
printf '%s\n' '#!/usr/bin/env bash' "if $header" '$x' '" in' '  *) ;;' 'esac; then :; fi' > "$planted"
caught=$(split_case_headers "$planted")
missed=""
if [ -z "$caught" ]; then
  missed="the planted split header after an if went unrefused"
fi
report 'the split-case scan reads a case after an if' "$missed"

# The reason the list is a list: the word in prose is not a header, and this
# population writes it inside nine string literals.
printf '%s\n' '#!/usr/bin/env bash' 'msg="a case for the shared list"' 'printf %s "$msg"' > "$planted"
report 'the split-case scan reads no case word written as prose in a string' "$(split_case_headers "$planted")"

# The other side of that boundary: prose and a here-doc body are not
# commands. The comment here opens with a `;` and the here-doc body starts at
# column one, which are the two shapes a raw line scan reddens.
printf '%s\n' '#!/usr/bin/env bash' "# a note; $header" 'cat <<EOF' "$header" 'EOF' 'echo done' > "$planted"
report 'the split-case scan grades no comment and no here-doc body' "$(split_case_headers "$planted")"

# The same boundary read the other way: a `<<TAG` written inside a quoted
# argument is text and opens no body, so the lines under it are still
# commands. Read as an operator it would blind this scan from there to the end
# of that file, which is the direction no scan over this population may fail
# in -- and this file writes such a spelling itself, in the fixture above.
printf '%s\n' '#!/usr/bin/env bash' "msg='cat <<EOF'" "$header" '$x' '" in' '  *) ;;' 'esac' > "$planted"
caught=$(split_case_headers "$planted")
missed=""
if [ -z "$caught" ]; then
  missed="the split header under a quoted tag spelling went unrefused"
fi
report 'the split-case scan reads the lines under a tag spelling written in a string' "$missed"

# The second instance of the same defect, and the one with a spelling a
# reader can see: a comment inside a multi-line $( ) or <( ). 3.2 is already
# counting parens and quotes there and knows nothing about the `#`, so a
# comment whose parens do not balance ends the substitution early and one
# carrying a lone quote swallows the rest. Balanced parens are left alone --
# the count comes back, which is why the shipped comment naming two sidecars
# in check-budget-drift.sh is not a finding.
#
# Entered and left by the shared reader's nesting count, because the two line
# anchors this replaces were wrong in both directions. Opening only on a `$(`
# with nothing after it entered 3 substitutions in the whole tree and never
# the `x="$(awk '` spelling this population writes most of them in. Closing at
# the first line starting with `)` ended a block at a `done)` or an `esac)`
# and then read every later comment in the file as if it sat inside one, so
# the 596 comment lines here carrying a stray apostrophe or paren were one
# such closing line away from being gate failures.
substitution_comments() {
  awk -v SQ="'" "$SCRIPT_CODE_AWK"'
    {
      script_code_scan($0)
      if (WAS == 0 && DEPTH > 0) { spanning++ }
      if (WAS > 0) { carried++ }
      if (CMT == "" || CMTDEPTH == 0) { next }
      t = CMT
      if (gsub(/\(/, "(", t) != gsub(/\)/, ")", t) \
        || gsub(SQ, SQ, t) % 2 || gsub(/"/, "\"", t) % 2) {
        print FILENAME ":" FNR ": " $0
      }
    }
    END {
      printf "substitutions: %d entered, %d spanning more than one line, %d lines read inside one\n", \
        SUBS, spanning, carried > "/dev/stderr"
    }
  ' "$@"
}
subcount="$WORK/substitutions.count"
: > "$subcount"
if [ -n "$empty" ]; then
  commented="$empty"
else
  commented=$(cd "$ROOT" && substitution_comments $GUARDED 2> "$subcount")
fi
report 'no comment inside a command substitution whose parens or quotes 3.2 miscounts' "$commented"

# planted, because the walk above is worth what it refuses and the shipped
# tree carries no such comment to refuse
printf '%s\n' '#!/usr/bin/env bash' 'x=$(' '# a comment with a ) paren' 'echo hi' ')' > "$planted"
caught=$(substitution_comments "$planted" 2> /dev/null)
missed=""
if [ -z "$caught" ]; then
  missed="the planted comment paren went unrefused"
fi
report 'the comment walk refuses a paren in a comment inside a substitution' "$missed"

# The spelling the line-end anchor never entered, and the one this population
# writes most of its substitutions in: opened with content still on the line
# and carried over the next. The defect here is the lone quote rather than
# the paren, since that is the half that swallows the rest of the file.
printf '%s\n' '#!/usr/bin/env bash' 'x=$(grep foo bar |' \
  "  # the population's own prose style" "  sed 's/a/b/'" ')' > "$planted"
caught=$(substitution_comments "$planted" 2> /dev/null)
missed=""
if [ -z "$caught" ]; then
  missed="the planted apostrophe in an inline-opened substitution went unrefused"
fi
report 'the comment walk enters a substitution opened with content on the line' "$missed"

# The other direction, which is the expensive one: a substitution that closes
# on a `done)` or an `esac)` rather than on a line starting with `)`. Every
# comment below those lines is outside, and this population writes 596 of
# them carrying a stray apostrophe or paren.
printf '%s\n' '#!/usr/bin/env bash' 'x=$(' '  for l in a b; do' '    echo "$l"' \
  '  done)' '# outside the substitution, and it carries a ) paren' 'y=$(' \
  '  case $z in (a) echo a ;; esac)' \
  "# outside too, and it carries the population's apostrophe" > "$planted"
report 'the comment walk leaves a substitution closed on a done or an esac' \
  "$(substitution_comments "$planted" 2> /dev/null)"

# The nesting the reader's header claims over a plain `( )` group inside a
# substitution: pushed and popped without changing the depth, so the group
# closing leaves the substitution open and the substitution ends at its own
# paren. Three comments read it -- one inside the group, one between the two
# closing parens, and one after both, of which the first two must be refused
# and the third must not.
printf '%s\n' '#!/usr/bin/env bash' 'a=$( (' '  # inside the group, and it carries a ) paren' \
  '  echo hi' ')' '  # inside the substitution still, and it carries a ) paren' ')' \
  '# outside both, carrying a ) paren of its own' > "$planted"
caught=$(substitution_comments "$planted" 2> /dev/null)
missed=""
case "$caught" in
  (*":3: "*) ;;
  (*) missed="the comment inside the subshell group went unrefused" ;;
esac
case "$caught" in
  (*":6: "*) ;;
  (*) missed=$(printf '%s%s\n' "${missed:+$missed
}" "the group closing ended the substitution that held it") ;;
esac
case "$caught" in
  (*":8: "*) missed=$(printf '%s%s\n' "${missed:+$missed
}" "the comment after the substitution closed was read as inside it") ;;
esac
report 'the reader holds its nesting across a subshell group and closes where the substitution ends' "$missed"

# Two levels deep, with the defect on the inner one, which the boolean the
# walk used to carry could not tell from the outer.
printf '%s\n' '#!/usr/bin/env bash' 'outer=$(grep foo bar | sed "$(printf %s p |' \
  '  # an inner comment with a ) paren' '  cat)"' ')' > "$planted"
caught=$(substitution_comments "$planted" 2> /dev/null)
missed=""
if [ -z "$caught" ]; then
  missed="the planted comment inside the inner substitution went unrefused"
fi
report 'the comment walk reads the inner level of a nested substitution' "$missed"

# The two line shapes where the reader's quote pass and the shared tokenizer
# disagree, which is what tells the halves of that boundary apart: a string
# closing on the line that carries `cat <<EOF` is an opener to anything
# reading the raw line and none to a reader that knows the quote opened two
# lines up, and a left shift whose operand is a name is a tag to a regex and
# none to the tokenizer's arithmetic branch. Either read swallows the comment
# under it. Without these two, a matrix reddens only when the tokenizer and
# the feed it is handed are wrong together, and grades neither alone.
printf '%s\n' '#!/usr/bin/env bash' "msg='a string that opens here" "cat <<EOF'" \
  'x=$(' '# a comment with a ) paren' 'echo hi' ')' 'if (( 1 << n )); then :; fi' \
  'y=$(' '# another comment with a ) paren' 'echo hi' ')' > "$planted"
caught=$(substitution_comments "$planted" 2> /dev/null | wc -l | tr -d ' ')
missed=""
if [ "$caught" != "2" ]; then
  missed=$(printf '2 comments planted under the two disagreeing feeds, %s read back\n%s\n' \
    "$caught" "$(substitution_comments "$planted" 2> /dev/null)")
fi
report 'the comment walk reads the lines under a tag only a raw line or a regex opens' "$missed"

# What the walk read, printed rather than asserted in prose: the anchor it
# replaces entered 3 substitutions in the whole tree, so a floor is what
# tells a later narrowing apart from a green run.
#
# One floor, on the lines read inside an open substitution and graded as a
# ratio to the substitutions entered. That is the number the regression this
# case exists for takes to zero: a reader whose quote and paren state stops
# carrying across lines reads no line inside one, while its count of opens
# per line goes up rather than down, so a floor on those opens is inert
# against it. A ratio rather than a count because both numbers are sums over
# files: an absolute grades the size of the population and falls on an
# ordinary deletion, where a ratio moves numerator and denominator together.
# The opens spanning more than one line and the total entered are printed
# beside the ratio and graded on their own by neither.
if [ -n "$empty" ]; then
  short="$empty"
else
  printf '# %s\n' "$(cat "$subcount")"
  entered=$(sed -n 's/^substitutions: \([0-9]*\) entered.*/\1/p' "$subcount")
  carried=$(sed -n 's/^substitutions: .* \([0-9]*\) lines read inside one$/\1/p' "$subcount")
  ratio=0
  if [ "${entered:-0}" -gt 0 ]; then
    ratio=$(( ${carried:-0} * 100 / entered ))
  fi
  short=""
  if [ "$ratio" -lt 50 ]; then
    short="the walk read ${carried:-0} lines inside an open substitution against ${entered:-0} entered, $ratio per 100 opens, and the tree reads at least 50"
  fi
fi
report 'the comment walk carries a substitution across the lines it spans, reading at least one line inside every two it enters' "$short"

# The margin the floor above has today, so a fresh clone can read the
# headroom without re-deriving it: measured 2026-09-15, the shipped tree
# reads 148 per 100 opens, deleting scripts/check-budget-drift.sh alone (the
# heaviest single file, 525 of the 1161 carried lines against 35 of the 783
# opens) leaves 85, and the floor sits at 50 -- 35 points under that
# deletion. The case below reddens once the shipped figure drifts more than
# 10 points from what the tree now measures, which is the tell that this
# comment is stale rather than the tree.
shipped_ratio=148
if [ -n "${ratio:-}" ]; then
  drift=$((ratio - shipped_ratio))
  [ "$drift" -lt 0 ] && drift=$((-drift))
  stale=""
  if [ "$drift" -gt 10 ]; then
    stale="the comment states $shipped_ratio, the tree now measures $ratio, $drift points apart -- update the comment's shipped figure"
  fi
else
  stale="$empty"
fi
report 'the shipped-ratio comment above is within 10 points of what the tree measures today' "$stale"

# The grep above reads constructs; it cannot see the shape that made 3.2
# refuse this very checker -- a case pattern inside a process substitution,
# whose closing paren 3.2 miscounts. Only a parse under a pre-4 bash catches
# that one, and the host that has one is the host the contract is about, so
# the leg runs where /bin/bash is old and says so where it is not.
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

# The two legs above grade a script by reading it, and neither notices a
# construct that runs for minutes on a variable this tree actually builds:
# the shipped baselines make the drift check's seat table 12 kB across 216
# lines, where every case tree above plants a handful of rows. So the
# shipped tree is run under an alarm, read-only, on the interpreter the
# contract is about. perl ships on macOS and on the CI runner images, and
# the leg says so where it does not.
#
# It runs under the interpreter the sibling leg above resolves, never under
# whichever bash launched this file: on the host the cost is about, the two
# differ whenever a newer bash sits ahead of /bin/bash on PATH, and the one
# leg that can see a 3.2 cost then never met a 3.2. Where the stock bash is
# newer the leg still runs, because a checker that stops finishing is a cost
# regression under any interpreter.
timed_bash="$stock"
timed_major="$stock_major"
if [ ! -x "$timed_bash" ]; then
  timed_bash="$BASH"
  timed_major=$("$timed_bash" -c 'echo "${BASH_VERSINFO[0]}"' 2>/dev/null || echo unknown)
fi
if command -v perl > /dev/null 2>&1; then
  timed=$(cd "$ROOT" && perl -e 'alarm 120; exec @ARGV' "$timed_bash" "$CHECKER" "$ROOT" 2>&1)
  timed_rc=$?
  slow=""
  if [ "$timed_rc" -ge 128 ]; then
    slow="the checker did not finish in 120 s on the shipped tree, which is a cost regression whatever caused it. The cause to check first is a bash 3.2 pattern substitution: it rescans the string per match, so an emptiness test written as one is unbounded on the seat table the shipped baselines build. What the run printed before the alarm:
$timed"
  elif [ "$timed_rc" -ne 0 ]; then
    slow="$timed"
  fi
  report "the checker finishes on the shipped tree inside two minutes under $timed_bash (bash $timed_major)" "$slow"
else
  n=$((n + 1))
  printf 'ok %s - %s # skip perl is not installed, so no alarm to run it under\n' \
    "$n" "the checker finishes on the shipped tree inside two minutes under $timed_bash (bash $timed_major)"
fi

# ---------------------------------------------------------------------------
# the population the sweep perturbs is this grading's own answer
# ---------------------------------------------------------------------------
# check-budget-drift-sweep.sh used to take every figure on these pages that
# equalled any recorded seat, and a re-seat that made three bare-Neovim
# readings equal a cell put them in its population as figures this check was
# expected to grade. It grades one reading per sentence -- view's -- so the
# sweep asks it which, through scripts/lib/moment-grading.sh in population
# mode. The seats below seat both readings of the planted row, which is the
# state a value-shaped population could not tell apart.
# shellcheck source=lib/moment-grading.sh
. "$(cd "$(dirname "$CHECKER")" && pwd)/lib/moment-grading.sh"
# The pages alone, and no new_case: that helper numbers a case of its own,
# and report() below numbers this one.
CASE="$WORK/population"
mkdir -p "$CASE"
plant_pages
printf 'dev-linux\tminimal\techo.view_p99_ms\t0.7312\n' > "$CASE/seats.tsv"
printf 'dev-linux\tminimal\tpicker.first_page_p99_ms\t0.6689\n' >> "$CASE/seats.tsv"
got=$(awk -v page="$PERF" -v fallback="dev-linux" -v mode="population" \
  "$MOMENT_GRADE_AWK" "$CASE/seats.tsv" "$CASE/$PERF" | tr '\t' ' ')
want='POP 4 0.73 echo.view_p99_ms
POP 8 0.73 echo.view_p99_ms'
mismatch=""
if [ "$got" != "$want" ]; then
  mismatch="want
$want
got
$got"
fi
report 'the population holds the view reading of a row and not the engine reading beside it, both seated' "$mismatch"

printf '\n%s cases, %s failures\n' "$n" "$failures"
[ "$failures" -eq 0 ]
