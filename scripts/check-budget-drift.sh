#!/usr/bin/env bash
# The bench gate reads budgets.toml; a person reads spec 3.1. Two places
# holding the same number is drift waiting to happen, and the direction that
# matters is silent: a budget loosened in the file alone would gate green
# against a bar the spec never agreed to.
#
# So every [[budget]] entry must name a spec_row that appears in the spec,
# and its max must appear in that same row's text.
#
# Written to stock POSIX-ish bash: macOS ships /bin/bash 3.2, and a gate that
# needs a newer one is a gate that silently does not run for whoever has it.
set -euo pipefail

# The cell vocabulary alone, printed and nothing else. check-style.sh's
# doc-figure walk grades a figure in a Rust doc comment by whether the line
# names a cell, and a second reader of budgets.toml would be a second answer
# to "what is a cell" -- one of which would go stale the next time the file's
# shape moves.
cells_only=0
if [[ "${1:-}" == "--cell-ids" ]]; then
  cells_only=1
  shift
fi

# A tree handed in as the single argument replaces the one this script lives
# in, which is how the case matrix points it at a fixture tree.
root="${1:-$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)}"
budgets="$root/crates/view-bench/budgets.toml"
spec="$root/.claude/specs/2026-07-17-view-design.md"

for f in "$budgets" "$spec"; do
  [[ -f "$f" ]] || { echo "BUDGET DRIFT FAIL: $f not found" >&2; exit 1; }
done

if [[ $cells_only -eq 1 ]]; then
  awk '
    /^\[/ { metric=""; scenario=""; next }
    /^scenario = / { scenario=$0; sub(/^scenario = "/, "", scenario); sub(/"$/, "", scenario) }
    /^metric = / { metric=$0; sub(/^metric = "/, "", metric); sub(/"$/, "", metric) }
    { if (metric != "" && scenario != "") { print scenario "." metric; metric="" } }
  ' "$budgets" | LC_ALL=C sort -u
  exit 0
fi

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

# Second cross-check: what a row *means* travels with its number. A budget
# that carries no claim a person can feel -- a diagnostic segment, a
# footprint -- is the one thing a docs page will happily quote as a win,
# because it is usually the flattering number. Two rules, both greppable:
# the spec row of such a budget says so in its own text, and the two
# user-facing surfaces name no metric identifier at all. The second is the
# stricter form of "a diagnostic is never cited as a win", and it is the
# form with no false positives: prose about a moment reads "the key reaches
# nvim in under a tenth of a millisecond", never `key_to_rpc_p99_us`, so
# any identifier on those pages is a number quoted out of the gate's
# vocabulary. Identifiers belong in docs/benchmarking.md, which is written
# for whoever runs the harness and is not checked here.
while IFS=$'\t' read -r spec_row kind metric; do
  case "$kind" in
    diagnostic) want="Diagnostic" ;;
    resource) want="Resource" ;;
    *) continue ;;
  esac
  row="$(sed "s/$arrow/->/g; s/\`//g" "$spec" | grep -F "$spec_row" | head -1 || true)"
  if ! grep -qF "$want" <<<"$row"; then
    echo "BUDGET DRIFT FAIL: $metric is a $kind budget, and its spec row does not say \"$want\". A row that cannot carry a claim has to say so where a person reads it:" >&2
    echo "  ${row:0:200}" >&2
    fail=1
  fi
done < <(awk '
  /^\[\[budget\]\]/ { row=""; kind=""; metric=""; next }
  /^spec_row = / { row=$0; sub(/^spec_row = "/, "", row); sub(/"$/, "", row) }
  /^metric = / { metric=$0; sub(/^metric = "/, "", metric); sub(/"$/, "", metric) }
  /^kind = / { kind=$0; sub(/^kind = "/, "", kind); sub(/"$/, "", kind); if (row != "") print row "\t" kind "\t" metric }
' "$budgets")

facing=("$root/README.md" "$root/docs/performance.md")
while IFS= read -r metric; do
  for page in "${facing[@]}"; do
    [[ -f "$page" ]] || continue
    hits="$(grep -nF "$metric" "$page" || true)"
    if [[ -n "$hits" ]]; then
      echo "BUDGET DRIFT FAIL: $(basename "$page") names the metric identifier $metric. The pages a user reads state moments in words and cite no identifiers; the identifiers live in docs/benchmarking.md:" >&2
      sed 's/^/  /' <<<"$hits" >&2
      fail=1
    fi
  done
done < <(grep -E '^metric = ' "$budgets" | sed 's/^metric = "//; s/"$//' | sort -u)

# Third cross-check: a win claim names the cell that earns it. The
# identifier rule above is the strict form and it only reaches the two
# user-facing pages; the claim this whole vocabulary exists to refuse -- a
# diagnostic quoted as a win -- was written in words and carried no
# identifier at all ("First paint ... 25.2 ms vs 130.3 ms, 5.2x faster").
# So every surface that states a claim is read for the shape of one: a
# multiplier, or a comparative naming what it beats. A paragraph carrying
# one passes only if it also names a felt metric -- which is why the two
# user-facing pages carry no comparative at all, since naming an
# identifier there breaks the rule above: they state moments and paired
# numbers, and the comparisons live where the cell that earns them can be
# named beside them.
#
# Paragraph rather than line, because a markdown table is one claim spread
# over its rows and the identifier that anchors it sits in whichever row
# names the cell.
# Whole cell ids (scenario.metric), never the bare metric name: `ratio_p50`
# is felt on the echo row and diagnostic on the fixture rows, and it is a
# substring of the diagnostic `marker_ratio_p50` besides -- so a bare name
# would let the very cell this rule refuses anchor the claim it was quoted
# for.
felt_ids="$(awk '
  # every table header, not just [[budget]]: a [[shortfall]] carries a
  # scenario and a metric of its own, and a block that only reset on
  # [[budget]] read the felt kind of the row above straight into them.
  /^\[/ { kind=""; metric=""; scenario=""; next }
  /^scenario = / { scenario=$0; sub(/^scenario = "/, "", scenario); sub(/"$/, "", scenario) }
  /^metric = / { metric=$0; sub(/^metric = "/, "", metric); sub(/"$/, "", metric) }
  /^kind = / { kind=$0; sub(/^kind = "/, "", kind); sub(/"$/, "", kind) }
  { if (kind == "felt" && metric != "" && scenario != "") { print scenario "." metric; metric="" } }
' "$budgets" | sort -u | tr '\n' ' ')"
# Emptiness by glob, never by deleting every blank and looking at what is
# left: bash 3.2 rescans the string per match in a pattern substitution, so
# an emptiness test written as one never returns on a multi-line variable.
if [[ $felt_ids != *[![:space:]]* ]]; then
  echo "BUDGET DRIFT FAIL: $budgets declares no felt metric, so no claim anywhere could name one" >&2
  exit 1
fi

# a multiplier ("5.2x", "1.10x"), or a comparative that names what it beats
# ("faster than bare Neovim", "ahead of the round trip"). The multiplier
# stops short of a digit so that a terminal size (120x40) is a size and not
# a claim. Exported rather than passed with -v, because awk expands escape
# sequences inside a -v assignment and would eat the regex's backslashes.
export CLAIM='[0-9](\.[0-9]+)?[ ]*(x|\xc3\x97)([^0-9]|$)|(faster|sooner|quicker|snappier|ahead)[ ]+(than|of)([^a-z]|$)'

# A comparative that names the engine, on a table row, is refused whatever
# anchors it. A felt anchor licenses a multiplier -- the cell that earns the
# number is named beside it -- but it cannot tell a bound ("ratio_p50 <= 0.30x
# the paired bare-nvim run") from a win, so on a row the anchor bought the
# writer permission to state either. A row states a win by publishing the
# paired numbers; the adjective is the part a rule can refuse. The engine has
# to be the comparative's own object, at most three words away: a fixed
# list of determiners lets "plain old Neovim" through, and reaching to the
# end of the sentence refuses "ahead of that boundary ... paid identically
# by bare nvim", where the comparative names no engine and the engine is
# another clause's subject. A window word is any non-space token and
# markup may sit before the engine: the pages write `**bare Neovim**` and
# `` `nvim` ``, and a letters-only class let both through.
export ENGINE='(faster|sooner|quicker|snappier|ahead)[ ]+(than|of)([ ]+[^ ]+){0,3}[ -]+[^A-Za-z]*([Nn]vim|NVIM|[Nn]eovim)'

claims_in() {
  local page="$1" first="${2:-1}" last="${3:-}"
  local shown="${page#"$root"/}"
  [[ -f "$page" ]] || return 0
  sed -n "${first},${last:-\$}p" "$page" | awk -v ids="$felt_ids" -v page="$shown" -v off="$((first - 1))" '
    # A table row is its own anchor unit. Paragraph scope reads an 18-row
    # table as one claim, so the felt id on the echo row anchors every
    # diagnostic row under it -- which is the shipped defect'"'"'s own shape,
    # a diagnostic quoted as a win, surviving on the surface the rule was
    # written to cover. Prose keeps the paragraph scope: a sentence takes
    # its anchor from the ones around it, a row does not.
    function row_verdict(line, at,   j) {
      if (line ~ engine) { print page ":" (at + off) ": " line; return }
      if (line !~ claim) { return }
      for (j = 1; j <= names; j++) {
        if (index(line, id[j]) > 0) { return }
      }
      print page ":" (at + off) ": " line
    }
    function verdict(   i, j, anchored) {
      if (hits == 0) { return }
      anchored = 0
      for (i = 1; i <= lines; i++) {
        for (j = 1; j <= names; j++) {
          if (index(para[i], id[j]) > 0) { anchored = 1 }
        }
      }
      if (anchored) { return }
      for (i = 1; i <= lines; i++) {
        if (para[i] ~ claim) { print page ":" (no[i] + off) ": " para[i] }
      }
    }
    BEGIN {
      names = split(ids, id, " ")
      claim = ENVIRON["CLAIM"]
      engine = ENVIRON["ENGINE"]
    }
    /^[[:space:]]*\|/ { row_verdict($0, NR); next }
    /^[[:space:]]*$/ { verdict(); lines = 0; hits = 0; next }
    {
      lines++
      para[lines] = $0
      no[lines] = NR
      if ($0 ~ claim) { hits++ }
    }
    END { verdict() }
  '
}

# The spec is read at its two claim-bearing sections rather than whole: the
# rest of it is a design document whose measurements stand in their own
# context, and section 1 and section 3.1 are where a reader takes a claim
# from.
section_bounds() {
  awk -v heading="$1" -v closer="$2" '
    $0 == heading { start = NR; next }
    start && $0 ~ closer { print start ":" (NR - 1); found = 1; exit }
    END { if (start && !found) { print start ":" NR } }
  ' "$spec"
}

claimed=""
for page in "$root/README.md" "$root/docs/performance.md" "$root/docs/benchmarking.md"; do
  claimed+="$(claims_in "$page")"$'\n'
done
# heading|closing-pattern; a pipe, because a tab inside a here-doc is one
# editor away from becoming spaces and neither heading carries one.
while IFS='|' read -r heading closer; do
  bounds="$(section_bounds "$heading" "$closer")"
  if [[ -z "$bounds" ]]; then
    echo "BUDGET DRIFT FAIL: the spec carries no section headed \"$heading\", so the claims a reader takes from it go unread" >&2
    fail=1
    continue
  fi
  claimed+="$(claims_in "$spec" "${bounds%:*}" "${bounds#*:}")"$'\n'
done <<SECTIONS
## 1. Product definition|^## 
### 3.1 Budgets (CI-gated once the harness lands, P3)|^#{2,3} 
SECTIONS
if [[ $claimed == *[![:space:]]* ]]; then
  echo "BUDGET DRIFT FAIL: a comparative claim stands in a paragraph that names no felt metric. A win is stated by the cell that earns it -- name that cell's metric beside the claim, or state the moment and its paired numbers in words. A row that names the engine it beats is refused whatever anchors it: a row states a win by publishing the paired numbers:" >&2
  printf '%s' "$claimed" | grep -v '^$' | sed 's/^/  /' >&2
  fail=1
fi

# Fourth cross-check: a re-seat moves the baseline, the ledger and the prose
# together. docs/benchmarking.md is where a reader learns which classes still
# owe the DSR re-seat, and the failure this rule exists for is silent in both
# directions: a commit that deletes a class's withdrawal and leaves the
# sentence standing publishes a measurement as never taken, and a class that
# withdraws a ratio without joining the sentence is a missing bar nobody is
# told about. Sentence scope rather than paragraph, because the same
# paragraph names the classes that have been re-seated beside the ones that
# have not.
baselines_dir="$root/crates/view-bench/baselines"
bench_page="$root/docs/benchmarking.md"
if [[ -d "$baselines_dir" && -f "$bench_page" ]]; then
  owing="$(awk '
    { buf = buf $0 " " }
    END {
      n = split(buf, sentence, /\. /)
      for (i = 1; i <= n; i++) {
        if (sentence[i] ~ /withdrawn/ && (sentence[i] ~ /first.paint/ || sentence[i] ~ /marker_ratio/)) {
          print sentence[i]
        }
      }
    }' "$bench_page")"
  for class_file in "$baselines_dir"/*.toml; do
    [[ -f "$class_file" ]] || continue
    class="$(basename "$class_file" .toml)"
    # a sidecar (dev-linux.headroom, dev-linux.measured) is not a class: it
    # keeps its class's name and a suffix, and records no ratio of its own.
    case "$class" in *.*) continue ;; esac
    seated=0
    grep -q '^\[withdrawn\.first_paint\.' "$class_file" && seated=1
    listed=0
    grep -qF "$class" <<<"$owing" && listed=1
    if [[ $listed -eq 1 && $seated -eq 0 ]]; then
      echo "BUDGET DRIFT FAIL: reseat-named $class: docs/benchmarking.md names it as carrying a withdrawn first-paint ratio, and baselines/$class.toml carries none -- a re-seat rewrites the prose and the ledger in the commit that lands it" >&2
      fail=1
    fi
    if [[ $listed -eq 0 && $seated -eq 1 ]]; then
      echo "BUDGET DRIFT FAIL: reseat-unnamed $class: baselines/$class.toml withdraws a first-paint ratio that docs/benchmarking.md names no class as owing, so the missing bar is one nobody reading the page is told about" >&2
      fail=1
    fi
  done
fi

# Fifth cross-check: an identifier a spec row carries is one that exists.
# The two rules above read a spec row for its text and its Diagnostic
# marker; neither ever reads a backticked token as an identifier, so
# `shell_visible_ms` named a metric no file declares and would have kept
# passing under any later rename. The vocabulary is what the bench system
# itself declares -- budgets.toml's scenarios, metrics and fixtures, the
# names the shipped class baselines record, since a row may cite a cell
# that is measured and reported without being bounded, and the scenario
# names the harness dispatches, since a decomposition row is measured and
# reported every run while publishing no baseline and carrying no budget.
# Newline-joined and matched whole with grep -Fqx rather than keyed in a map:
# an identifier is a whole word here, and a set of them is a set either way.
vocab_scenario=""
vocab_leaf=""
while IFS=$'\t' read -r kind name; do
  [[ -n "$name" ]] || continue
  case "$kind" in
    s) vocab_scenario="$vocab_scenario$name"$'\n' ;;
    *) vocab_leaf="$vocab_leaf$name"$'\n' ;;
  esac
done < <(
  awk '
    /^scenario = / { v=$0; sub(/^scenario = "/, "", v); sub(/"$/, "", v); print "s\t" v }
    /^metric = / { v=$0; sub(/^metric = "/, "", v); sub(/"$/, "", v); print "m\t" v }
    /^fixture = / { v=$0; sub(/^fixture = "/, "", v); sub(/"$/, "", v); print "m\t" v }
    /^fixtures = / {
      v=$0; gsub(/[^a-z_,]/, "", v); n=split(v, f, ",")
      for (i = 1; i <= n; i++) { if (f[i] != "fixtures") print "m\t" f[i] }
    }
  ' "$budgets"
  # A class baseline is the record of what a row actually publishes; a
  # sidecar (dev-linux.headroom, dev-linux.measured) keeps the class name it
  # suffixes and adds no name of its own. No apostrophe in a comment inside
  # this substitution: bash 3.2 reads one as a quote and never finds the end.
  for class_file in "$root/crates/view-bench/baselines"/*.toml; do
    [[ -f "$class_file" ]] || continue
    # The pattern carries a leading paren: bash 3.2 counts the closing one of
    # a bare pattern as the end of the enclosing substitution and dies parsing.
    case "$(basename "$class_file" .toml)" in (*.*) continue ;; esac
    awk '
      /^\[/ {
        h=$0; gsub(/[][]/, "", h); n=split(h, part, ".")
        if (part[1] == "withdrawn") { if (n >= 3) { print "s\t" part[2]; print "m\t" part[3] } }
        else if (n >= 2) { print "s\t" part[1]; print "m\t" part[2] }
        table=1; next
      }
      table && /^[a-z_0-9]+ = / { print "m\t" $1 }
    ' "$class_file"
  done
  # The row table the harness dispatches from, read the way the baselines
  # are: a scenario it names is a name the vocabulary holds, so a live row
  # needs no exemption spelled out beside the retired ones to go stale in.
  # No apostrophe in a comment inside this substitution: bash 3.2 reads one
  # as a quote and never finds the end.
  builds="$root/crates/view-harness/src/builds.rs"
  if [ -f "$builds" ]; then
    awk '
      /MEASURED_BUILD/ { table=1; next }
      table && /^\];/ { table=0 }
      table && /^[[:space:]]*\("[a-z_0-9]+"/ {
        v=$0; sub(/^[^"]*"/, "", v); sub(/".*$/, "", v); print "s\t" v
      }
    ' "$builds"
  fi
)

bounds="$(section_bounds '### 3.1 Budgets (CI-gated once the harness lands, P3)' '^#{2,3} ')"
if [[ -n "$bounds" ]]; then
  while IFS= read -r token; do
    [[ -n "$token" ]] || continue
    if [[ "$token" == *.* ]]; then
      left="${token%%.*}"
      right="${token#*.}"
      if grep -Fqx "$left" <<<"$vocab_scenario" &&
        grep -Fqx "$right" <<<"$vocab_leaf"; then
        continue
      fi
    elif grep -Fqx "$token" <<<"$vocab_scenario" ||
      grep -Fqx "$token" <<<"$vocab_leaf"; then
      continue
    fi
    echo "BUDGET DRIFT FAIL: spec-id $token: a spec 3.1 row names it, and no budgets.toml entry, shipped class baseline or harness row declares a scenario, metric or fixture by that name, and no sentence in that row withdraws it" >&2
    fail=1
  done < <(
    # An identifier a row withdraws is exempt where the withdrawal stands:
    # the sentence that retracts it is what earns the exemption, so the
    # exemption dies with the sentence rather than outliving it in a
    # hand-kept list nothing grades. Sentence scope, not row scope, so the
    # rest of a row that retracts one identifier is still read.
    sed -n "${bounds%:*},${bounds#*:}p" "$spec" |
      awk '
        function emit(text,   n, part, i, tok) {
          n = split(text, part, "`")
          for (i = 2; i <= n; i += 2) {
            tok = part[i]
            if (tok ~ /^[a-z_]+(\.[a-z_0-9]+)?$/) { print tok }
          }
        }
        /^[[:space:]]*\|/ {
          n = split($0, sentence, /\. /)
          for (i = 1; i <= n; i++) {
            if (sentence[i] ~ /[Ww]ithdraw/) { continue }
            emit(sentence[i])
          }
        }
      ' | sort -u
  )
fi

# Sixth cross-check: a ratio quoted beside a cell id is that cell's own
# recorded value, on the one class and the one fixture the unit it stands in
# names. docs/benchmarking.md is where the vocabulary allows a comparative,
# on the condition that it names the cell that earns it -- and a named cell
# whose baseline holds a different number is the anchor rule passing a claim
# no measurement supports. The failure is silent by construction: a
# re-record moves the baseline and leaves every sentence quoting the old
# draw standing, reading as current.
#
# A cell id carries no class and no fixture, so the unit around it has to.
# Resolving against every class and every fixture the unit names passed
# dev-macos's draw as dev-linux's and the 15-plugin leg's as the plugin-free
# one -- the identifier and the words disagreeing, which is the shape this
# rule was minted for. So a unit resolves against one class and one fixture:
# the ones it names, the page's declared default class where it names none,
# and the single fixture its cells are recorded on where the words name none.
# Anything else is reported as ambiguous rather than guessed at.
#
# Only a dimensionless number is graded -- a multiplier, a bare decimal with
# no unit behind it, or a percentage, which states the same ratio as its
# distance from 1 -- because that is the shape a ratio is written in; an
# absolute carries its unit, and a bound carries the word bar. Every table is
# read: a header word bought a whole table out of this check and out of the
# sweep that grades its population at once, which is a bypass one edit wide.
seats=""
for class_file in "$baselines_dir"/*.toml; do
  [[ -f "$class_file" ]] || continue
  class="$(basename "$class_file" .toml)"
  # The pattern carries a leading paren: bash 3.2 counts the closing one of
  # a bare pattern as the end of the enclosing substitution and dies parsing.
  case "$class" in (*.*) continue ;; esac
  seats="$seats$(awk -v class="$class" '
    /^\[/ {
      h=$0; gsub(/[][]/, "", h); n=split(h, part, ".")
      scenario = (part[1] == "withdrawn" || n < 2) ? "" : part[1]
      fixture = (scenario == "") ? "" : part[2]
      next
    }
    scenario != "" && /^[a-z_0-9]+ = -?[0-9.]+$/ { print class "\t" fixture "\t" scenario "." $1 "\t" $3 }
  ' "$class_file")"$'\n'
done

# The class a unit resolves against when it names none. The page declares it
# in its own words and this reads that declaration, because a default kept
# here instead would be a class the reader of the page is never told about.
default_class=""
if [[ -f "$bench_page" ]]; then
  # Sentence scope, over the page joined: the declaration is prose and wraps
  # wherever the paragraph does, so a line-at-a-time read finds it on the
  # line whose half of the sentence happens to carry the class name.
  default_class="$(awk '
    { buf = buf $0 " " }
    END {
      n = split(buf, sentence, /\. /)
      for (i = 1; i <= n; i++) {
        if (sentence[i] !~ /default class/) { continue }
        m = split(sentence[i], part, "`")
        for (j = 2; j <= m; j += 2) {
          if (part[j] ~ /^[a-z0-9]+-[a-z0-9]+$/) { print part[j]; exit }
        }
      }
    }' "$bench_page")"
fi
if [[ -n "$default_class" && ! -f "$baselines_dir/$default_class.toml" ]]; then
  echo "BUDGET DRIFT FAIL: ratio-default $default_class: docs/benchmarking.md declares it the default class its numbers resolve against, and no class baseline ships under that name" >&2
  fail=1
  default_class=""
fi

if [[ -f "$bench_page" && $seats == *[![:space:]]* ]]; then
  quoted="$(awk -v page="${bench_page#"$root"/}" -v fallback="$default_class" '
    FNR == NR {
      split($0, f, "\t")
      if (f[3] == "") { next }
      seat[f[1] SUBSEP f[2] SUBSEP f[3]] = f[4]
      cellseen[f[3]] = 1
      if (index(fixtures[f[1] SUBSEP f[3]], " " f[2] " ") == 0) {
        fixtures[f[1] SUBSEP f[3]] = fixtures[f[1] SUBSEP f[3]] " " f[2] " "
      }
      next
    }
    function classes_of(text,   i, n, name, named) {
      n = split("controlled-linux dev-macos dev-linux gh-macos gh-linux", name, " ")
      named = ""
      for (i = 1; i <= n; i++) {
        if (index(text, name[i]) > 0 && index(named, " " name[i] " ") == 0) {
          named = named " " name[i] " "
        }
      }
      return named
    }
    # The words the page writes a fixture in, beside the names the baselines
    # record it under. A page that says plugin-free means minimal, and the
    # rule has to read the words to grade the number standing next to them.
    function fixtures_of(text,   named) {
      named = ""
      if (text ~ /plugin-free|no plugins|`minimal`|[.]minimal/) { named = named " minimal " }
      if (text ~ /15-plugin|`heavy`|[.]heavy/) { named = named " heavy " }
      if (text ~ /login-shaped|full login|`user`|[.]user/) { named = named " user " }
      return named
    }
    function clean(t) {
      gsub(/[`*~()>]/, "", t)
      sub(/[,;:.]+$/, "", t)
      return t
    }
    function decimals(num) {
      return index(num, ".") == 0 ? 0 : length(num) - index(num, ".")
    }
    function seated(num, vals,   n, v, i, fmt) {
      fmt = "%." decimals(num) "f"
      n = split(vals, v, " ")
      for (i = 1; i <= n; i++) {
        if (sprintf(fmt, v[i]) + 0 == num + 0) { return 1 }
      }
      return 0
    }
    # A percentage states the same ratio as its distance from 1, which is how
    # the page writes a gap a reader thinks in percent about.
    function seated_pct(num, vals,   n, v, i, fmt, off) {
      fmt = "%." decimals(num) "f"
      n = split(vals, v, " ")
      for (i = 1; i <= n; i++) {
        off = (v[i] > 1 ? v[i] - 1 : 1 - v[i]) * 100
        if (sprintf(fmt, off) + 0 == num + 0) { return 1 }
      }
      return 0
    }
    # The unit a cell is recorded in, read off the metric name rather than a
    # whitelist of the one unit the rule started with: a page quoting a
    # megabyte or a microsecond seat was left ungraded by a rule that only
    # knew about milliseconds, and writing the id beside it did not help.
    function unit_suffix(u) {
      if (u == "ms") { return "_ms$" }
      if (u == "MB") { return "_mb$" }
      return "_us$"
    }
    function scope(at, num, what) {
      printf "BUDGET DRIFT FAIL: ratio-scope %s:%d: %s is quoted where %s, and a number resolves against one class and one fixture or against neither\n",
        page, at, num, what
    }
    # One number resolves to one cell, never to the union of every cell the
    # unit names: a union passed a sibling metric and a sibling scenario as
    # the number the id beside it stands for, which is the same disagreement
    # between the identifier and the words the class and fixture rules were
    # minted for. So a number takes the nearest cell id before it in its own
    # sentence -- a table cell is a sentence, since a row states one column
    # at a time -- and the unit first id where its sentence names none.
    function grade(   i, j, m, n, text, part, w, num, nxt, pct, ntok, ngraded,
                     ncell, cell, namedcells, cls, nc, klass, fx, nf, fixn,
                     klass_one, fixture, seen, ok, after, line, tok, cur, held) {
      if (uc == 0) { return }
      text = ""
      for (i = 1; i <= uc; i++) { text = text " " ul[i] }
      ncell = 0
      namedcells = ""
      n = split(text, part, "`")
      for (i = 2; i <= n; i += 2) {
        if (part[i] !~ /^[a-z_]+\.[a-z_0-9]+$/) { continue }
        if (!(part[i] in cellseen)) { continue }
        if (index(namedcells, " " part[i] " ") > 0) { continue }
        ncell++
        cell[ncell] = part[i]
        namedcells = namedcells " " part[i] " "
      }
      if (ncell == 0) { uc = 0; return }
      ntok = 0
      for (i = 1; i <= uc; i++) {
        line = ul[i]
        gsub(/\|/, " \001 ", line)
        gsub(/\. /, " \001 ", line)
        m = split(line, w, /[[:space:]]+/)
        for (j = 1; j <= m; j++) { ntok++; tk[ntok] = w[j]; tl[ntok] = uno[i] }
      }
      ngraded = 0
      cur = ""
      for (i = 1; i <= ntok; i++) {
        if (tk[i] == "\001") { cur = ""; continue }
        tok = clean(tk[i])
        if (tok ~ /^[a-z_]+\.[a-z_0-9]+$/ && (tok in cellseen)) { cur = tok; continue }
        num = tok
        nxt = clean(tk[i + 1])
        pct = 0
        after = nxt
        # a leading minus is part of the number: a diagnostic records one, and
        # a regex without it left the page ungraded where the ledger was not
        if (num ~ /^-?[0-9]+(\.[0-9]+)?%$/) { pct = 1; sub(/%$/, "", num) }
        else if (num ~ /^-?[0-9]+(\.[0-9]+)?$/ && nxt == "%") { pct = 1; after = clean(tk[i + 2]) }
        else if (num !~ /^-?[0-9]+\.[0-9]+x?$/) { continue }
        # an absolute resolves like a ratio where a cell of its own unit
        # stands beside it, and nowhere else: the column holding the paired
        # bare-engine reading names no cell of view own and states an
        # absolute this file records nothing for.
        else if (nxt ~ /^(ms|us|\xc2\xb5s|MB)$/) {
          if (cur == "" || cur !~ unit_suffix(nxt)) { continue }
        }
        else if (nxt ~ /^(s|min|GB|bar|bars|budget|bound|frame)$/) { continue }
        # a percentage OF something is a share of a population, not a ratio
        # stated as its distance from 1
        if (pct && after == "of") { continue }
        sub(/x$/, "", num)
        ngraded++
        gnum[ngraded] = num
        gpct[ngraded] = pct
        gat[ngraded] = tl[i]
        gcell[ngraded] = (cur != "") ? cur : cell[1]
      }
      if (ngraded == 0) { uc = 0; return }
      cls = classes_of(text)
      nc = split(cls, klass, " ")
      if (nc > 1) {
        scope(gat[1], gnum[1], "the unit names " nc " classes (" cls ")")
        uc = 0
        return
      }
      if (nc == 1) { klass_one = klass[1] }
      else if (fallback != "") { klass_one = fallback }
      else {
        scope(gat[1], gnum[1], "the unit names no class and the page declares no default class")
        uc = 0
        return
      }
      fx = fixtures_of(text)
      nf = split(fx, fixn, " ")
      if (nf > 1) {
        scope(gat[1], gnum[1], "the unit names " nf " fixtures (" fx ")")
        uc = 0
        return
      }
      if (nf == 1) { fixture = fixn[1] }
      else {
        seen = ""
        for (k = 1; k <= ncell; k++) {
          n = split(fixtures[klass_one SUBSEP cell[k]], fixn, " ")
          for (i = 1; i <= n; i++) {
            if (index(seen, " " fixn[i] " ") == 0) { seen = seen " " fixn[i] " " }
          }
        }
        nf = split(seen, fixn, " ")
        if (nf != 1) {
          if (nf == 0) { uc = 0; return }
          scope(gat[1], gnum[1], klass_one " records" namedcells "on " nf " fixtures (" seen ") and the unit names none")
          uc = 0
          return
        }
        fixture = fixn[1]
      }
      for (i = 1; i <= ngraded; i++) {
        if (!((klass_one SUBSEP fixture SUBSEP gcell[i]) in seat)) { continue }
        held = seat[klass_one SUBSEP fixture SUBSEP gcell[i]]
        ok = gpct[i] ? seated_pct(gnum[i], held) : seated(gnum[i], held)
        if (!ok) {
          printf "BUDGET DRIFT FAIL: ratio-drift %s:%d: %s%s is quoted beside %s on %s %s, and the value recorded there does not round to it at the digits printed\n",
            page, gat[i], gnum[i], (gpct[i] ? "%" : ""), gcell[i], klass_one, fixture
        }
      }
      uc = 0
    }
    /^[[:space:]]*\|/ {
      grade()
      ul[1] = $0; uno[1] = FNR; uc = 1; grade()
      next
    }
    /^[[:space:]]*$/ { grade(); next }
    { uc++; ul[uc] = $0; uno[uc] = FNR }
    END { grade() }
  ' <(printf '%s' "$seats") "$bench_page")"
  if [[ -n "$quoted" ]]; then
    printf '%s\n' "$quoted" >&2
    fail=1
  fi
fi

# Seventh cross-check: the ledger quotes the same measurement the page does.
# The rule above guards docs/benchmarking.md alone, and the stale ratio it
# was minted for stood in two places -- the page and the [[shortfall]] why
# beside the entry it belongs to. Resolution is exact here rather than read
# out of prose: an entry names its own class, scenario, fixture and metric,
# so a sentence in its why that names a cell of that entry has its figures
# graded against what that cell records. A sentence that names another class
# or another fixture resolves there instead. No sentence is read for
# nothing: a word cannot buy a live figure out of the grading, which is what
# the retired-reading escape did for every sentence that carried one, so a
# reading a fix replaced belongs in the commit that replaced it. The draws a
# run took have a field of their own (`trials`), because they are the one
# figure a why could state that no cell holds.
#
# A draw sits within this fraction of the seat its run reduced them to, or
# it is a figure of another quantity: the ledger has held a ratio prepended
# to the paired arm's own milliseconds, which is what the band refuses. What
# it reaches is a figure of another magnitude and no more -- the paired arm
# of a near-1 ratio, and the same percentile on the other subject, sit
# inside it and are told apart by nothing here, so those readings go to the
# round report and an entry carries one quantity.
# view-harness/src/budgets.rs states the same fraction as TRIALS_BAND, and
# the cases pin both sides to the same edges.
TRIALS_BAND=0.25
if [[ $seats == *[![:space:]]* ]]; then
  stale="$(awk -v file="${budgets#"$root"/}" -v TRIALS_BAND="$TRIALS_BAND" '
    FNR == NR {
      split($0, f, "\t")
      if (f[3] == "") { next }
      seat[f[1] SUBSEP f[2] SUBSEP f[3]] = f[4]
      next
    }
    function value(line,   v) {
      v = line
      sub(/^[a-z_]+ = /, "", v)
      sub(/^"/, "", v)
      sub(/"$/, "", v)
      return v
    }
    function fixtures_of(text,   named) {
      named = ""
      if (text ~ /plugin-free|no plugins|[.]minimal/) { named = named " minimal " }
      if (text ~ /15-plugin|[.]heavy/) { named = named " heavy " }
      if (text ~ /login-shaped|full login|[.]user/) { named = named " user " }
      return named
    }
    function classes_of(text,   i, n, name, named) {
      n = split("controlled-linux dev-macos dev-linux gh-macos gh-linux", name, " ")
      named = ""
      for (i = 1; i <= n; i++) {
        if (index(text, name[i]) > 0 && index(named, " " name[i] " ") == 0) {
          named = named " " name[i] " "
        }
      }
      return named
    }
    function clean(t) {
      gsub(/[`*~()>]/, "", t)
      sub(/[,;:.]+$/, "", t)
      return t
    }
    function decimals(num) {
      return index(num, ".") == 0 ? 0 : length(num) - index(num, ".")
    }
    function seated(num, vals,   n, v, i, fmt) {
      fmt = "%." decimals(num) "f"
      n = split(vals, v, " ")
      for (i = 1; i <= n; i++) {
        if (sprintf(fmt, v[i]) + 0 == num + 0) { return 1 }
      }
      return 0
    }
    # A percentage in a why states the distance from the bar the sentence
    # names, which is how a ledger entry writes a miss; the pages write the
    # distance from 1 instead, so both spellings are read and the sentence
    # decides which by naming a bar or not.
    function bar_of(w, m,   j, t, nx) {
      for (j = 1; j <= m; j++) {
        t = clean(w[j])
        if (t !~ /^[0-9]+(\.[0-9]+)?$/) { continue }
        nx = clean(w[j + 1])
        if (nx ~ /^(bar|bars|budget|bound|frame)$/) { return t }
        if (nx == "ms" && clean(w[j + 2]) ~ /^(bar|bars|budget|bound|frame)$/) { return t }
      }
      return ""
    }
    function off_pct(held, bar) {
      if (bar != "" && bar + 0 != 0) { held = held / bar }
      return (held > 1 ? held - 1 : 1 - held) * 100
    }
    function seated_pct(num, held, bar,   fmt) {
      fmt = "%." decimals(num) "f"
      return sprintf(fmt, off_pct(held, bar)) + 0 == num + 0
    }
    # Every figure a why states is the value of a cell that why names: an
    # entry names its own class, scenario, fixture and metric, so a number
    # written beside an identifier resolves exactly. A figure with no
    # identifier in its sentence is attributed to nothing and goes stale in
    # silence at the next record run, which is what the stale ratio did in
    # both places it stood.
    function sentence_verdict(text, at,   j, k, m, w, tok, cls, nc, klass,
                              fx, nf, fixn, klass_one, fixture, cellid, nids,
                              idname, idat, isid, num, nxt, pct, after, held,
                              ok, bar, want) {
      cls = classes_of(text)
      nc = split(cls, klass, " ")
      if (nc > 1) { return }
      klass_one = (nc == 1) ? klass[1] : class
      fx = fixtures_of(text)
      nf = split(fx, fixn, " ")
      if (nf > 1) { return }
      fixture = (nf == 1) ? fixn[1] : fixt
      m = split(text, w, /[[:space:]]+/)
      bar = bar_of(w, m)
      nids = 0
      for (j = 1; j <= m; j++) {
        tok = clean(w[j])
        cellid = ""
        # a bare metric name belongs to the entry own scenario; a written
        # out scenario.metric names its own, since a why may settle a
        # question with a cell from a scenario the entry does not measure
        if (tok ~ /^[a-z_0-9]+$/ && ((klass_one SUBSEP fixture SUBSEP scen "." tok) in seat)) {
          cellid = scen "." tok
        } else if (tok ~ /^[a-z_]+\.[a-z_0-9]+$/ && ((klass_one SUBSEP fixture SUBSEP tok) in seat)) {
          cellid = tok
        }
        isid[j] = (cellid != "")
        if (cellid != "") { nids++; idname[nids] = cellid; idat[nids] = j }
      }
      for (j = 1; j <= m; j++) {
        if (isid[j]) { continue }
        num = clean(w[j])
        nxt = clean(w[j + 1])
        pct = 0
        after = nxt
        if (num ~ /^-?[0-9]+(\.[0-9]+)?%$/) { pct = 1; sub(/%$/, "", num) }
        else if (num ~ /^-?[0-9]+(\.[0-9]+)?$/ && (nxt == "%" || nxt == "percent")) {
          pct = 1; after = clean(w[j + 2])
        }
        else if (num !~ /^-?[0-9]+\.[0-9]+x?$/) { continue }
        else if (nxt ~ /^(s|min|GB|bar|bars|budget|bound|frame)$/) { continue }
        # a percentage OF something is a share of a population
        if (pct && after == "of") { continue }
        sub(/x$/, "", num)
        if (nids == 0) {
          printf "BUDGET DRIFT FAIL: why-figure %s/%s.%s: %s:%d states %s%s in a sentence that names no cell, so the figure is attributed to nothing and a record run leaves it standing\n",
            klass_one, scen, fixture, file, at, num, (pct ? "%" : "")
          continue
        }
        want = idname[1]
        for (k = 1; k <= nids; k++) { if (idat[k] < j) { want = idname[k] } }
        held = seat[klass_one SUBSEP fixture SUBSEP want]
        ok = pct ? seated_pct(num, held, bar) : seated(num, held)
        if (!ok) {
          printf "BUDGET DRIFT FAIL: why-drift %s/%s.%s: %s:%d quotes %s%s beside %s, and the value recorded there does not round to it at the digits printed\n",
            klass_one, scen, fixture, file, at, num, (pct ? "%" : ""), want
        }
      }
    }
    function flush(   n, i, sent) {
      if (why == "" || scen == "" || metr == "" || class == "") { why = ""; return }
      n = split(why, sent, /\. /)
      for (i = 1; i <= n; i++) { sentence_verdict(sent[i], whyat) }
      why = ""
    }
    # The draws a record run took are a field rather than a sentence, and
    # every member is a draw of the metric the entry seats. One further out
    # than the band is a figure of another quantity -- the paired arm own
    # milliseconds is the shape the ledger shipped -- parked where no cell
    # can grade it. The band is the loader own.
    function check_trials(line, at,   body, n, t, i, num, off, span) {
      body = line
      sub(/^trials = /, "", body)
      gsub(/[][]/, "", body)
      if (acc == "") {
        printf "BUDGET DRIFT FAIL: trials-band %s/%s.%s: %s:%d lists the draws of an entry that states no accepted value, so the array is anchored to nothing\n",
          class, scen, fixt, file, at
        return
      }
      span = (acc < 0 ? -acc : acc) * TRIALS_BAND
      n = split(body, t, /,/)
      for (i = 1; i <= n; i++) {
        num = t[i]
        gsub(/[[:space:]]/, "", num)
        if (num !~ /^-?[0-9]+(\.[0-9]+)?$/) { continue }
        off = num - acc
        if (off < 0) { off = -off }
        if (off > span) {
          printf "BUDGET DRIFT FAIL: trials-band %s/%s.%s: %s:%d lists the trial %s, further from the accepted %s than a draw of this metric goes, so the array states a quantity this cell does not draw\n",
            class, scen, fixt, file, at, num, acc
        }
      }
    }
    /^\[\[/ {
      flush()
      block = ($0 ~ /shortfall/)
      scen = ""; fixt = ""; metr = ""; class = ""; why = ""; acc = ""
      next
    }
    !block { next }
    /^accepted = / { acc = value($0); next }
    /^trials = / { check_trials($0, FNR); next }
    /^scenario = / { scen = value($0); next }
    /^fixture = / { fixt = value($0); next }
    /^metric = / { metr = value($0); next }
    /^class = / { class = value($0); next }
    /^why = / { why = value($0); whyat = FNR; flush(); next }
    END { flush() }
  ' <(printf '%s' "$seats") "$budgets")"
  if [[ -n "$stale" ]]; then
    printf '%s\n' "$stale" >&2
    fail=1
  fi
fi

# Eighth cross-check: the speculated paint is a local reading, and the only
# surface that measures a transport is the acceptance leg. The page that
# said view had been measured with its engine across a network stated a
# condition the cell was never recorded under, and no rule above reaches it:
# it carries no identifier, no multiplier and no comparative. So a unit that
# speaks of the predicted glyph -- by cell id here, by the words the user
# pages write it in there -- and also states a transport condition has to
# name the leg that injects one. The scope is the speculated moment and
# nothing wider: `round trip` is the tree's own phrase for the local
# post-VimEnter attach, so a rule that fired on the word alone told the
# author of a local row to rest it on the acceptance leg, which is advice
# about a claim that row never made.
export TRANSPORT='remote|network|far side|another machine|RTT'
# The predicted glyph, named by cell id or in the words the pages write it
# in. The bare word `prediction` is not one of them: the pages use it for a
# picker cache and for what a feature is for, and a rule that read it as the
# speculated moment indicted three true sentences about remote editing that
# make no claim about the cell at all.
export SPECULATED='echo_speculated|predicted glyph|glyph it expects|character it expects|predict[a-z]*[ ]+(the[ ]+)?(glyph|character|keystroke)'
export RTT_LEG='remote-rtt\.sh|remote_memory'
# A transport word inside a negation is the page denying the condition, which
# is the sentence this rule most wants written -- and a unit that says the
# reading is local has stated the condition it was recorded under. The
# negation comes before the word, within five tokens: a row reading `on the
# far side of a network | not yet recorded` carries a later `not` that
# denies the recording rather than the transport.
export TRANSPORT_NOT='not|no|never|without|nor'
# The clause a licence is confined to. This tree writes its em dash as two
# hyphens and the pages carry both spellings.
export TRANSPORT_CLAUSE='[,;:]|--|—'

# The window, written once and pasted into the three awk programs that read
# it: three copies of it drifted for one round already, and a window pinned
# in one copy is a window unpinned in the other two.
NEGATED_AWK='
    # The five tokens before the phrase own match position in the rejoined
    # text, never over whitespace tokens: no token ever equals `far side` or
    # `another machine`, so a token walk could not reach either phrase and
    # every true denial written around one was refused.
    function denied_before(text, phrase,   rest, before, n, w, j, lo) {
      rest = text
      while (match(rest, phrase) > 0) {
        before = substr(rest, 1, RSTART - 1)
        sub(/[[:space:]]+$/, "", before)
        n = split(before, w, /[[:space:]]+/)
        lo = (n - 4 < 1) ? 1 : n - 4
        for (j = lo; j <= n; j++) {
          if (tolower(w[j]) ~ ("^[^a-z]*(" negation ")[^a-z]*$")) { return 1 }
        }
        rest = substr(rest, RSTART + RLENGTH)
      }
      return 0
    }
    # `local` licenses the clause that carries it and nothing wider, and only
    # where it is not itself denied: an unrelated clause reporting a local
    # picker cache licensed the transport claim beside it, and a clause
    # saying the reading is not local licensed its own inversion.
    function local_licenses(text,   n, cl, i) {
      n = split(text, cl, ENVIRON["TRANSPORT_CLAUSE"])
      for (i = 1; i <= n; i++) {
        if (cl[i] !~ /(^|[^a-z])local([^a-z]|$)/) { continue }
        if (cl[i] !~ transport) { continue }
        if (denied_before(cl[i], "(^|[^a-z])local([^a-z]|$)")) { continue }
        return 1
      }
      return 0
    }
    # The negation has to sit beside the transport word rather than anywhere
    # in the unit: a bullet is one unit and a paragraph is several sentences,
    # and a `no` at the far end of either says nothing about the clause the
    # transport word stands in.
    function negated(text) {
      if (local_licenses(text)) { return 1 }
      return denied_before(text, transport)
    }
'

transport_in() {
  local page="$1"
  local shown="${page#"$root"/}"
  [[ -f "$page" ]] || return 0
  awk -v page="$shown" "$NEGATED_AWK"'
    function verdict(text, at) {
      if (text !~ transport) { return }
      if (text ~ leg) { return }
      if (negated(text)) { return }
      printf "transport %s:%d: %s\n", page, at, text
    }
    function para(   i, text) {
      if (lines == 0) { return }
      text = ""
      for (i = 1; i <= lines; i++) { text = text " " para_line[i] }
      # a phrase the vocabulary spells with single spaces survives the wrap
      # the page happens to have, and the indent a continuation line carries
      gsub(/[[:space:]]+/, " ", text)
      if (text ~ speculated) { verdict(text, para_no[1]) }
      lines = 0
    }
    # A table is one subject spread over its rows, so the speculated scope is
    # the table and the finding is the row. The shipped defect was a row
    # carrying no speculated word of its own, standing in the table whose
    # subject is the predicted glyph; scoping the test to the row alone
    # would read that row as a paragraph about nothing.
    function table(   i, text) {
      if (rows == 0) { return }
      text = ""
      for (i = 1; i <= rows; i++) { text = text " " row_line[i] }
      gsub(/[[:space:]]+/, " ", text)
      if (text ~ speculated) {
        for (i = 1; i <= rows; i++) { verdict(row_line[i], row_no[i]) }
      }
      rows = 0
    }
    BEGIN {
      transport = ENVIRON["TRANSPORT"]
      speculated = ENVIRON["SPECULATED"]
      leg = ENVIRON["RTT_LEG"]
      negation = ENVIRON["TRANSPORT_NOT"]
    }
    /^[[:space:]]*\|/ { para(); rows++; row_line[rows] = $0; row_no[rows] = FNR; next }
    /^[[:space:]]*$/ { para(); table(); next }
    # A list item is its own unit, its continuation lines included. A roadmap
    # read as one paragraph let a speculated word in one bullet indict a
    # transport word in an unrelated sibling, and the finding then named the
    # whole block rather than a sentence anyone wrote.
    # Every markdown list spelling, ordered ones included: a numbered roadmap
    # read as one paragraph let a transport item and a speculated item indict
    # each other, which is the defect the bullet rule was minted for
    # surviving on the sibling marker.
    /^[[:space:]]*([-*+][[:space:]]|[0-9]+[.)][[:space:]])/ { para() }
    { table(); lines++; para_line[lines] = $0; para_no[lines] = FNR }
    END { para(); table() }
  ' "$page"
}

transported=""
for page in "$root/README.md" "$root/docs/performance.md"; do
  transported+="$(transport_in "$page")"$'\n'
done
transported+="$(transport_in "$bench_page")"$'\n'
bounds="$(section_bounds '### 3.1 Budgets (CI-gated once the harness lands, P3)' '^#{2,3} ')"
if [[ -n "$bounds" ]]; then
  transported+="$(sed -n "${bounds%:*},${bounds#*:}p" "$spec" |
    awk -v page="${spec#"$root"/}" -v off="$((${bounds%:*} - 1))" "$NEGATED_AWK"'
      BEGIN {
        transport = ENVIRON["TRANSPORT"]
        speculated = ENVIRON["SPECULATED"]
        leg = ENVIRON["RTT_LEG"]
        negation = ENVIRON["TRANSPORT_NOT"]
      }
      $0 ~ transport && $0 ~ speculated && $0 !~ leg && !negated($0) {
        printf "transport %s:%d: %s\n", page, NR + off, $0
      }
    ')"$'\n'
fi
# The felt sentence is where the moment is stated in a persons words, so it
# is read the same way the pages are: the entry names the scenario, and a
# speculated row that reaches for a transport word states a condition its
# own recording never had.
transported+="$(awk -v file="${budgets#"$root"/}" "$NEGATED_AWK"'
  BEGIN {
    transport = ENVIRON["TRANSPORT"]
    leg = ENVIRON["RTT_LEG"]
    negation = ENVIRON["TRANSPORT_NOT"]
  }
  /^\[\[/ { scen = ""; next }
  /^scenario = / { scen = $0; sub(/^scenario = "/, "", scen); sub(/"$/, "", scen); next }
  /^felt = / && scen == "echo_speculated" && $0 ~ transport && $0 !~ leg && !negated($0) {
    printf "transport %s:%d: %s\n", file, FNR, $0
  }
' "$budgets")"$'\n'
if [[ $transported == *[![:space:]]* ]]; then
  echo "BUDGET DRIFT FAIL: a transport condition stands where the reading is local. The predicted glyph is measured with both engines on one host; the injected round trips are the acceptance RTT leg's (scripts/acceptance/remote-rtt.sh), which is the only surface a transport claim may rest on -- name it in the unit, or state the condition the cell was recorded under:" >&2
  printf '%s' "$transported" | grep -v '^$' | sed 's/^/  /' >&2
  fail=1
fi

# Ninth cross-check: the two user-facing pages quote the same measurement the
# ledger does. The ratio rule above reads docs/benchmarking.md alone, so the
# pages a person actually reads carried their figures ungraded -- rewriting
# view's own worst keystroke from 1.58 ms to 1.51 ms on both of them left the
# check green. They are graded by the same seat table and the same rounding,
# and they anchor a figure the only way a page that may name no identifier
# can: on the moment it states in words. The vocabulary is the pages own, the
# way `plugin-free` is the word a fixture is named in.
#
# A number resolves to the cell its own sentence names a moment for, on the
# class the page declares and the fixture its sentence or its paragraph names
# -- and the page declares a class for the same reason docs/benchmarking.md
# does, because a reader is otherwise never told which host the number came
# from. The first reading of a sentence is view's: these pages publish paired
# numbers, and the bare-engine one beside it is an absolute this tree records
# no cell for. A figure the sentence calls a difference (`0.6 ms more`) is
# neither side's reading and is passed over.
moment_in() {
  local page="$1" fallback="$2"
  local shown="${page#"$root"/}"
  [[ -f "$page" ]] || return 0
  awk -v page="$shown" -v fallback="$fallback" '
    FNR == NR {
      split($0, f, "\t")
      if (f[3] == "") { next }
      seat[f[1] SUBSEP f[2] SUBSEP f[3]] = f[4]
      cellseen[f[3]] = 1
      if (index(fixtures[f[1] SUBSEP f[3]], " " f[2] " ") == 0) {
        fixtures[f[1] SUBSEP f[3]] = fixtures[f[1] SUBSEP f[3]] " " f[2] " "
      }
      next
    }
    function classes_of(text,   i, n, name, named) {
      n = split("controlled-linux dev-macos dev-linux gh-macos gh-linux", name, " ")
      named = ""
      for (i = 1; i <= n; i++) {
        if (index(text, name[i]) > 0 && index(named, " " name[i] " ") == 0) {
          named = named " " name[i] " "
        }
      }
      return named
    }
    # The words these pages name a fixture in, which include the one
    # docs/benchmarking.md has no use for: a page written for a person says
    # your config where the maintainer page says login-shaped.
    function fixtures_of(text,   named) {
      named = ""
      if (text ~ /plugin-free|no plugins|Plugin-free/) { named = named " minimal " }
      if (text ~ /15-plugin/) { named = named " heavy " }
      if (text ~ /login-shaped|full login|your config/) { named = named " user " }
      return named
    }
    # The moment a sentence states and the cell that records it. These pages
    # may name no identifier -- the identifier rule refuses one here -- so
    # the words are the only anchor a figure has.
    function moment_of(text) {
      if (text ~ /predicted glyph|glyph it expects|character it expects/) {
        return "echo_speculated.speculated_paint_p99_ms"
      }
      if (text ~ /keypress to glyph|worst keystroke|keystroke in a thousand/) {
        return "echo.view_p99_ms"
      }
      if (text ~ /stale/) { return "scroll.staleness_p99_ms" }
      if (text ~ /cadence/) { return "flood.cadence_p99_ms" }
      if (text ~ /matching results/) { return "picker.match_paint_p99_ms" }
      if (text ~ /first page of results/) { return "picker.first_page_p99_ms" }
      if (text ~ /worst launch/) { return "startup.first_frame_cold_ms" }
      if (text ~ /own process holds/) { return "memory.pss_mb" }
      return ""
    }
    function cell_unit(cell) {
      if (cell ~ /_ms$/) { return "ms" }
      if (cell ~ /_mb$/) { return "MB" }
      if (cell ~ /_us$/) { return "us" }
      return ""
    }
    function unit_of(tok) {
      if (tok == "ms") { return "ms" }
      if (tok == "MB") { return "MB" }
      if (tok == "us" || tok == "\xc2\xb5s") { return "us" }
      return ""
    }
    function clean(t) {
      gsub(/[`*~()>]/, "", t)
      sub(/[,;:.]+$/, "", t)
      return t
    }
    function decimals(num) {
      return index(num, ".") == 0 ? 0 : length(num) - index(num, ".")
    }
    function seated(num, vals,   n, v, i, fmt) {
      fmt = "%." decimals(num) "f"
      n = split(vals, v, " ")
      for (i = 1; i <= n; i++) {
        if (sprintf(fmt, v[i]) + 0 == num + 0) { return 1 }
      }
      return 0
    }
    function sentence(a, b, ufx,   i, text, cell, cls, nc, klass, klass_one,
                      fx, nf, fixn, fixture, num, nxt, tail, held, pick) {
      if (b < a) { return }
      text = ""
      for (i = a; i <= b; i++) { text = text " " tk[i] }
      cell = moment_of(text)
      if (cell == "" || !(cell in cellseen)) { return }
      # The first reading of the sentence is view own: these pages publish
      # paired numbers, and the bare-engine one beside it is an absolute
      # this tree records no cell for. A figure the sentence calls a
      # difference is neither side reading.
      pick = 0
      for (i = a; i <= b; i++) {
        num = clean(tk[i])
        if (num !~ /^-?[0-9]+\.[0-9]+$/) { continue }
        nxt = clean(tk[i + 1])
        tail = (unit_of(nxt) != "") ? clean(tk[i + 2]) : nxt
        if (tail ~ /^(more|less|fewer|behind|ahead|earlier|later|further)$/) { continue }
        if (unit_of(nxt) != cell_unit(cell)) { continue }
        pick = i
        break
      }
      if (pick == 0) { return }
      num = clean(tk[pick])
      cls = classes_of(text)
      nc = split(cls, klass, " ")
      if (nc > 1) { return }
      if (nc == 1) { klass_one = klass[1] }
      else if (fallback != "") { klass_one = fallback }
      else {
        printf "BUDGET DRIFT FAIL: moment-default %s:%d: %s is quoted as the %s moment and the page declares no default class, so the host it was measured on is one no reader is told\n",
          page, tl[pick], num, cell
        return
      }
      fx = fixtures_of(text)
      nf = split(fx, fixn, " ")
      if (nf == 0) { nf = split(ufx, fixn, " ") }
      if (nf == 0) { nf = split(fixtures[klass_one SUBSEP cell], fixn, " ") }
      if (nf != 1) {
        if (nf > 1) {
          printf "BUDGET DRIFT FAIL: moment-scope %s:%d: %s is quoted as the %s moment where the unit names %d fixtures, and a number resolves against one fixture or against none\n",
            page, tl[pick], num, cell, nf
        }
        return
      }
      fixture = fixn[1]
      if (!((klass_one SUBSEP fixture SUBSEP cell) in seat)) { return }
      held = seat[klass_one SUBSEP fixture SUBSEP cell]
      if (!seated(num, held)) {
        printf "BUDGET DRIFT FAIL: moment-drift %s:%d: %s is quoted as the %s moment on %s %s, and the value recorded there does not round to it at the digits printed\n",
          page, tl[pick], num, cell, klass_one, fixture
      }
    }
    # A table row is one sentence spread over its columns here, not one per
    # column: the row states its moment in the label column and its reading
    # in the next, so splitting on the pipe would leave every figure in a
    # sentence naming no moment at all.
    function grade(   i, j, m, line, w, text, ufx, s0) {
      if (uc == 0) { return }
      text = ""
      for (i = 1; i <= uc; i++) { text = text " " ul[i] }
      ntok = 0
      for (i = 1; i <= uc; i++) {
        line = ul[i]
        gsub(/\|/, " ", line)
        gsub(/\. /, " \001 ", line)
        m = split(line, w, /[[:space:]]+/)
        for (j = 1; j <= m; j++) { ntok++; tk[ntok] = w[j]; tl[ntok] = uno[i] }
      }
      ufx = fixtures_of(text)
      s0 = 1
      for (i = 1; i <= ntok + 1; i++) {
        if (i == ntok + 1 || tk[i] == "\001") {
          sentence(s0, i - 1, ufx)
          s0 = i + 1
        }
      }
      uc = 0
    }
    /^[[:space:]]*\|/ { grade(); ul[1] = $0; uno[1] = FNR; uc = 1; grade(); next }
    /^[[:space:]]*$/ { grade(); next }
    /^[[:space:]]*([-*+][[:space:]]|[0-9]+[.)][[:space:]])/ { grade() }
    { uc++; ul[uc] = $0; uno[uc] = FNR }
    END { grade() }
  ' <(printf '%s' "$seats") "$page"
}

if [[ $seats == *[![:space:]]* ]]; then
  for page in "$root/README.md" "$root/docs/performance.md"; do
    [[ -f "$page" ]] || continue
    shown="${page#"$root"/}"
    page_class="$(awk '
      { buf = buf $0 " " }
      END {
        n = split(buf, sentence, /\. /)
        for (i = 1; i <= n; i++) {
          if (sentence[i] !~ /default class/) { continue }
          m = split(sentence[i], part, "`")
          for (j = 2; j <= m; j += 2) {
            if (part[j] ~ /^[a-z0-9]+-[a-z0-9]+$/) { print part[j]; exit }
          }
        }
      }' "$page")"
    if [[ -n "$page_class" && ! -f "$baselines_dir/$page_class.toml" ]]; then
      echo "BUDGET DRIFT FAIL: moment-default $shown: it declares $page_class the default class its numbers resolve against, and no class baseline ships under that name" >&2
      fail=1
      continue
    fi
    moment="$(moment_in "$page" "$page_class")"
    if [[ -n "$moment" ]]; then
      printf '%s\n' "$moment" >&2
      fail=1
    fi
  done
fi

if [[ $entries -eq 0 ]]; then
  echo "BUDGET DRIFT FAIL: no [[budget]] entries found in $budgets" >&2
  exit 1
fi

if [[ $fail -ne 0 ]]; then
  exit 1
fi
echo "budget drift: $entries budget(s) cross-checked against spec 3.1"
