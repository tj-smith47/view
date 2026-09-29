#!/usr/bin/env bash
# Lints every shell script this tree carries at warning level: the shebang
# population under scripts/ and every other `.sh` git lists, untracked ones
# included, since `task commit` runs the lint before it stages a new file.
set -euo pipefail

cd "$(dirname "$0")/.."
# shellcheck source=scripts/lib/script-population.sh
. scripts/lib/script-population.sh

if ! command -v shellcheck >/dev/null 2>&1; then
  echo "lint:shell: shellcheck is not on PATH; install it (CI pins the version in .github/workflows/ci.yml)" >&2
  exit 1
fi

script_population_read .
# its own capture, so a failing git stops the lint: the shebang population
# alone would otherwise read clean
listed=$(git ls-files --cached --others --exclude-standard -- '*.sh') || {
  echo "lint:shell: git ls-files failed, so the scripts outside scripts/ cannot be listed" >&2
  exit 1
}
scripts=$({ printf '%s\n' "$SCRIPT_POPULATION"; printf '%s\n' "$listed"; } |
  grep . | LC_ALL=C sort -u || true)
if [ -z "$scripts" ]; then
  echo "lint:shell: no shell script found to lint" >&2
  exit 1
fi

files=()
while IFS= read -r script; do
  files+=("$script")
done <<<"$scripts"

# A `disable` keeps a finding, which holds only where a reader shellcheck
# cannot see uses the construct, so the comment line above it names that
# reader. It sits directly on the command it covers, where a reader sees
# both. One ahead of the file's first command covers the whole file, so it
# goes on the command that needs it. shellcheck applies stacked directives
# alike, and one directive carrying the codes joined by a comma keeps the
# reader comment above the command. A line inside a here-doc body or a
# quoted string is text no shell reads as a comment, and each directive gets
# one message naming everything wrong with it.
bare=$(awk -v SQ="'" "$SCRIPT_CODE_AWK"'
  function directive_codes(line,    c) {
    c = line
    sub(/^.*disable=/, "", c)
    sub(/[[:space:]].*$/, "", c)
    return c
  }
  function settle(next_line) {
    if (site == "") { return }
    if (next_line == "last") {
      why = why "; is on the last line of the file and covers no command"
    } else if (next_line ~ /^[[:space:]]*#[[:space:]]*shellcheck[[:space:]]+disable=/) {
      why = why "; is stacked on another, and the codes join with a comma in one directive (disable=" codes "," directive_codes(next_line) ")"
    } else if (next_line ~ /^[[:space:]]*(#|$)/) {
      why = why "; has a blank or a comment under it, and belongs directly above the command it covers"
    }
    if (why != "") { print site ": a shellcheck disable" substr(why, 2) }
    site = ""; why = ""
  }
  FNR == 1 { settle("last"); above = ""; commands = 0 }
  {
    top = script_code_top()
    text = (HD != "" || top == SQ || top == "\"")
    script_code_scan($0)
    if (text) { next }
  }
  { settle($0) }
  /^[[:space:]]*#[[:space:]]*shellcheck[[:space:]]+disable=/ {
    if (above !~ /^[[:space:]]*#/ || above ~ /^[[:space:]]*#[[:space:]]*shellcheck[[:space:]]/ || above !~ /(^|[^[:alnum:]])read/) {
      why = why "; has no comment above it naming the reader shellcheck cannot see"
    }
    if (commands == 0) {
      why = why "; sits ahead of the first command of the file, where it covers the whole file"
    }
    site = FILENAME ":" FNR
    codes = directive_codes($0)
    next
  }
  $0 !~ /^[[:space:]]*(#|$)/ { commands++ }
  { above = $0 }
  END { settle("last") }
' "${files[@]}")
if [ -n "$bare" ]; then
  printf '%s\n' "$bare" >&2
  exit 1
fi

# A `case` pattern carries its leading paren, because bash 3.2 reads the `)`
# of a bare one inside a `$( )` as the end of the substitution
# (.claude/rules/shell.md). A pattern stands after the header's `in` and
# after each `;;`, so those positions are what is read, the one-line
# `case … esac` and a header opened on an arm line included. A `case`
# keyword whose `in` the line does not carry is reported, because the
# patterns under it would go unread. The fallthrough terminators are bash 4
# and the portability check refuses them. The rule covers the scripts under
# scripts/, so the walk does too.
own=()
for script in "${files[@]}"; do
  case "$script" in (scripts/*) own+=("$script") ;; esac
done
unparened=""
[ "${#own[@]}" -eq 0 ] || unparened=$(awk -v SQ="'" "$SCRIPT_CODE_AWK"'
  function bare_arm(where, s) {
    if (s !~ /^\(/) { print where ": a case pattern with no leading paren: " s }
  }
  function top_of(st) { return substr(st, length(st), 1) }
  function word_at(s, i, w,    a) {
    if (substr(s, i, length(w)) != w) { return 0 }
    a = substr(s, i + length(w), 1)
    return (a == "" || a ~ /[[:space:]]/)
  }
  # whether a word after `before` starts a command: the line start, or after
  # an operator, an opening paren or brace, `!`, or a keyword that takes a
  # command after it
  function command_position(before) {
    sub(/[[:space:]]+$/, "", before)
    return before == "" || before ~ /[;&|({!]$/ ||
      before ~ /(^|[^[:alnum:]_])(then|do|else|elif|if|while|until|time)$/
  }
  # The position after the header `in`, 0 for a line with no `case`
  # keyword, -1 for a keyword whose `in` is not on the line. The word
  # between them may hold quotes, `$( )`, `$(( ))` and `${ }` with blanks
  # inside, so `in` counts only outside all of them.
  function case_header(s,    i, n, c, st, top, kw, lvl) {
    n = length(s); st = ""; kw = 0
    for (i = 1; i <= n; i++) {
      c = substr(s, i, 1); top = top_of(st)
      if (top == SQ) {
        if (c == SQ) { st = substr(st, 1, length(st) - 1) }
        continue
      }
      if (c == "\\") { i++; continue }
      if (c == "$" && (substr(s, i + 1, 1) == "(" || substr(s, i + 1, 1) == "{")) {
        st = st substr(s, i + 1, 1); i++; continue
      }
      if (top == "\"") {
        if (c == "\"") { st = substr(st, 1, length(st) - 1) }
        continue
      }
      if (c == SQ || c == "\"" || c == "(") { st = st c; continue }
      if ((c == ")" && top == "(") || (c == "}" && top == "{")) {
        st = substr(st, 1, length(st) - 1)
        if (kw && length(st) < lvl) { return -1 }
        continue
      }
      if (!kw && word_at(s, i, "case") && command_position(substr(s, 1, i - 1))) {
        kw = 1; lvl = length(st); i += 3; continue
      }
      if (kw && length(st) == lvl && word_at(s, i, "in") && substr(s, i - 1, 1) ~ /[[:space:]]/) {
        return i + 2
      }
    }
    return kw ? -1 : 0
  }
  # where the pattern of an arm line ends: its closing paren outside quotes
  function pattern_end(s,    i, n, c, q, d) {
    n = length(s); q = ""; d = 0
    for (i = 1; i <= n; i++) {
      c = substr(s, i, 1)
      if (q != "") { if (c == q) { q = "" }; continue }
      if (c == "\\") { i++; continue }
      if (c == SQ || c == "\"") { q = c; continue }
      if (c == "(" && i > 1) { d++; continue }
      if (c == ")") { if (d == 0) { return i }; d-- }
    }
    return 0
  }
  # where the first `;;` outside quotes and substitutions starts, 0 for none.
  # A `)` with nothing open is the one closing the substitution the text
  # was cut from, or a bare pattern, and closes nothing here.
  function dsemi_at(s,    i, n, c, st, top) {
    n = length(s); st = ""
    for (i = 1; i < n; i++) {
      c = substr(s, i, 1); top = top_of(st)
      if (top == SQ) {
        if (c == SQ) { st = substr(st, 1, length(st) - 1) }
        continue
      }
      if (c == "\\") { i++; continue }
      if (c == "$" && (substr(s, i + 1, 1) == "(" || substr(s, i + 1, 1) == "{")) {
        st = st substr(s, i + 1, 1); i++; continue
      }
      if (top == "\"") {
        if (c == "\"") { st = substr(st, 1, length(st) - 1) }
        continue
      }
      if (c == SQ || c == "\"" || c == "(") { st = st c; continue }
      if ((c == ")" && top == "(") || (c == "}" && top == "{")) {
        st = substr(st, 1, length(st) - 1); continue
      }
      if (st == "" && c == ";" && substr(s, i + 1, 1) == ";") { return i }
    }
    return 0
  }
  function trim(s) {
    sub(/^[[:space:]]+/, "", s)
    sub(/[[:space:]]+$/, "", s)
    return s
  }
  # One line of code, read left to right. `at_pattern` is whether the text
  # at hand stands where a pattern does: after a header `in` or after a
  # `;;`. A `;;` anywhere on the line opens such a position, an `esac`
  # closes its level, and a header opens one, so a line holding several of
  # them is read the same as the lines they would be split onto.
  function read_line(where, s,    at_pattern, e, d, seg, at) {
    at_pattern = (arms > 0 && want[arms])
    while (1) {
      s = trim(s)
      if (s == "") { break }
      if (arms > 0 && s ~ /^esac([^[:alnum:]_]|$)/) {
        arms--
        s = substr(s, 5); at_pattern = 0
        continue
      }
      if (substr(s, 1, 2) == ";;") {
        s = substr(s, 3); at_pattern = (arms > 0)
        continue
      }
      if (at_pattern) {
        bare_arm(where, s)
        at_pattern = 0
        e = pattern_end(s)
        if (e == 0) { break }
        s = substr(s, e + 1)
        continue
      }
      d = dsemi_at(s)
      seg = d ? substr(s, 1, d - 1) : s
      if (seg ~ /(^|[^[:alnum:]_])case([^[:alnum:]_]|$)/) {
        at = case_header(seg)
        if (at > 0) {
          arms++
          s = substr(s, at); at_pattern = 1
          continue
        }
        if (at < 0) {
          print where ": a case keyword with no `in` after it on its line, so its patterns go unread: " seg
        }
      }
      if (!d) { break }
      s = substr(s, d)
    }
    if (arms > 0) { want[arms] = at_pattern }
  }
  FNR == 1 { arms = 0 }
  {
    top = script_code_top()
    text = (HD != "" || top == SQ || top == "\"")
    script_code_scan($0)
    if (text) { next }
    s = trim(CODE)
    if (s == "") { next }
    read_line(FILENAME ":" FNR, s)
  }
' "${own[@]}")
if [ -n "$unparened" ]; then
  printf '%s\n' "$unparened" >&2
  exit 1
fi

shellcheck -S warning -- "${files[@]}"
echo "lint:shell: ${#files[@]} scripts clean"
