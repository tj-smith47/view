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

# A tree handed in as the single argument replaces the one this script lives
# in, which is how the case matrix points it at a fixture tree.
root="${1:-$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)}"
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
if [[ -z "${felt_ids// /}" ]]; then
  echo "BUDGET DRIFT FAIL: $budgets declares no felt metric, so no claim anywhere could name one" >&2
  exit 1
fi

# a multiplier ("5.2x", "1.10x"), or a comparative that names what it beats
# ("faster than bare Neovim", "ahead of the round trip"). The multiplier
# stops short of a digit so that a terminal size (120x40) is a size and not
# a claim. Exported rather than passed with -v, because awk expands escape
# sequences inside a -v assignment and would eat the regex's backslashes.
export CLAIM='[0-9](\.[0-9]+)?[ ]*(x|\xc3\x97)([^0-9]|$)|(faster|sooner|quicker|snappier|ahead)[ ]+(than|of)([^a-z]|$)'

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
    BEGIN { names = split(ids, id, " "); claim = ENVIRON["CLAIM"] }
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
if [[ -n "${claimed//[$'\n' ]/}" ]]; then
  echo "BUDGET DRIFT FAIL: a comparative claim stands in a paragraph that names no felt metric. A win is stated by the cell that earns it -- name that cell's metric beside the claim, or state the moment and its paired numbers in words:" >&2
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
# itself declares -- budgets.toml's scenarios, metrics and fixtures, plus
# the names the shipped class baselines record, since a row may cite a cell
# that is measured and reported without being bounded.
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
)

bounds="$(section_bounds '### 3.1 Budgets (CI-gated once the harness lands, P3)' '^#{2,3} ')"
if [[ -n "$bounds" ]]; then
  while IFS= read -r token; do
    [[ -n "$token" ]] || continue
    # Identifiers §3.1 names because they no longer exist, spelled out rather
    # than pattern-matched so a typo cannot hide behind one: cold_ms and
    # ratio_vs_nvim were withdrawn from the content-marker row by 2026-07-27's
    # amendment, which names the defect that withdrew them, and echo_path is a
    # decomposition row of the diagnostic matrix that publishes no baseline and
    # carries no budget.
    case "$token" in
      cold_ms | ratio_vs_nvim | echo_path) continue ;;
    esac
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
    echo "BUDGET DRIFT FAIL: spec-id $token: a spec 3.1 row names it, and neither budgets.toml nor the shipped class baselines declare a scenario, metric or fixture by that name" >&2
    fail=1
  done < <(
    sed -n "${bounds%:*},${bounds#*:}p" "$spec" |
      grep '^[[:space:]]*|' |
      grep -oE '`[a-z_]+(\.[a-z_0-9]+)?`' |
      tr -d '`' | sort -u
  )
fi

if [[ $entries -eq 0 ]]; then
  echo "BUDGET DRIFT FAIL: no [[budget]] entries found in $budgets" >&2
  exit 1
fi

if [[ $fail -ne 0 ]]; then
  exit 1
fi
echo "budget drift: $entries budget(s) cross-checked against spec 3.1"
