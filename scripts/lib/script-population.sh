#!/usr/bin/env bash
# The population every gate over scripts/ grades: a file whose first line
# names bash or `sh`. Sourced, never run, and shared by check-style.sh's
# comment rules and temp-file walk, check-portability.sh's userland scan and
# check-budget-drift-cases.sh's bash-3.2 parse leg, because four spellings of
# "the scripts" grade four different sets: a rule written over `*.sh` misses
# the eight remote-test fixtures that carry no suffix, one written over
# `scripts/*.sh` misses scripts/acceptance/ as well, and the difference is
# where a finding sits unread.
#
# Sets SCRIPT_POPULATION to the selected paths, one per line, relative to the
# root it is handed. Names on stdout what it passed over, returns 1 naming
# what it could not open, and leaves emptiness to the caller: each gate says
# something different about a tree with nothing to grade.
script_population_read() {
  local root="${1:-.}" entries skipped graded unreadable first
  SCRIPT_POPULATION=""
  # symlinks are listed beside regular files so a dangling one is named by
  # the loop below rather than dropped by the selection: a selection that
  # skips what it cannot open leaves the population short, and a short
  # population grades its survivors and reads exactly like a tree with
  # nothing to report
  entries=$(cd "$root" && find scripts \( -type f -o -type l \) | LC_ALL=C sort)
  # a fifo, socket or device node is readable and could never carry a
  # shebang, so it is named and passed over: reddening a gate for an entry
  # nobody can rewrap is a false red nobody can act on
  skipped=$(cd "$root" && find scripts ! -type d ! -type f ! -type l | LC_ALL=C sort)
  if [ -n "$skipped" ]; then
    printf '%s\n' "$skipped" |
      sed 's/$/: not a regular file or a symlink, the shebang selection passes it over/'
  fi
  # selected by a loop rather than by an xargs awk: xargs splits a path on a
  # blank and awk takes a fatal on the fragment, which drops the file from
  # every rule reading this list with the run still green. Read and select in
  # the one pass, so no entry can pass the readability vet and then be lost
  # by the selection below it
  graded=$(cd "$root" && printf '%s\n' "$entries" | while IFS= read -r entry; do
    [ -n "$entry" ] || continue
    if { [ -f "$entry" ] && [ -r "$entry" ]; } &&
      first=$(head -n 1 "$entry"); then
      case "$first" in ('#!'*bash | '#!'*/sh) printf 'take %s\n' "$entry" ;; esac
    else
      printf 'drop %s\n' "$entry"
    fi
  done)
  unreadable=$(printf '%s\n' "$graded" | sed -n 's/^drop //p')
  if [ -n "$unreadable" ]; then
    printf '%s\n' "$unreadable" | sed 's/$/: the shebang selection cannot read it/'
    return 1
  fi
  # every reader of this name is a sourcing script, which shellcheck does
  # not follow from here
  # shellcheck disable=SC2034
  SCRIPT_POPULATION=$(printf '%s\n' "$graded" | sed -n 's/^take //p')
  return 0
}

# The reader every walk over that population shares: an awk prelude that
# turns a line into the text a shell reads as commands. Three gates grade a
# word (`ln`, `case`) or a comment by where it sits, and each of them drew
# the boundary in its own way -- a line anchor, a list of the operators a
# command can follow, a block that ended at the first line starting with
# `)`. The boundary is one property of the file, so it is read once here.
#
# `script_code_scan(line)` sets CODE to the line with its comment cut, and to
# the empty string inside a here-doc body; CMT to the comment and CMTDEPTH to
# the substitution nesting the comment sits in; WAS and DEPTH to the nesting
# before and after the line; SUBS to the running count of substitutions
# entered. Quote, here-doc and nesting state carry across lines the way 3.2
# carries them, so a `#` inside an open string is not a comment, a paren
# inside one is not counted, and `"$(awk ...` -- the shape this population
# writes most of its substitutions in -- is entered rather than skipped. The
# caller passes SQ="'", since a lone quote cannot be written in the program.
#
# The ceilings, both deliberate. A string literal is command text to this
# reader once a substitution inside it is entered, so a word assembled in one
# is graded: the alternative blinds every walk to a generator, and the
# population carries no such string. A `(` that opens a plain subshell is not
# pushed while its `)` pops, so `$( ( x ) )` ends early; the population writes
# none. A `(` that opens a subshell or gives a `case` pattern its leading
# paren is pushed and popped without changing the nesting, so the pattern the
# page mandates stays inside its substitution; a pattern written without that
# paren pops the substitution, which is the miscount 3.2 itself makes.
# shellcheck disable=SC2034
SCRIPT_CODE_AWK='
  FNR == 1 { DEPTH = 0; STACK = ""; HD = "" }
  function script_code_top() {
    return (STACK == "") ? "" : substr(STACK, length(STACK), 1)
  }
  function script_code_push(c) {
    STACK = STACK c
    if (c == "(") { DEPTH++ }
  }
  function script_code_pop(   c) {
    c = script_code_top()
    STACK = substr(STACK, 1, length(STACK) - 1)
    if (c == "(") { DEPTH-- }
  }
  function script_code_heredoc(line,   t) {
    t = line
    gsub(/<<</, "", t)
    if (!match(t, /<<-?[[:space:]]*["]?[A-Za-z_][A-Za-z0-9_]*/) &&
      !match(t, "<<-?[[:space:]]*" SQ "?[A-Za-z_][A-Za-z0-9_]*")) { return "" }
    t = substr(t, RSTART, RLENGTH)
    HDDASH = (t ~ /^<<-/)
    sub(/^<<-?[[:space:]]*/, "", t)
    sub(/^["]/, "", t)
    sub("^" SQ, "", t)
    return t
  }
  function script_code_scan(line,   i, n, c, prev, top, j) {
    CODE = ""; CMT = ""; CMTDEPTH = 0; WAS = DEPTH
    if (HD != "") {
      if ((HDDASH && line ~ "^[[:space:]]*" HD "[[:space:]]*$") || line == HD) {
        HD = ""
      }
      return
    }
    n = length(line); prev = ""
    for (i = 1; i <= n; i++) {
      c = substr(line, i, 1)
      top = script_code_top()
      if (top == SQ) {
        if (c == SQ) { script_code_pop() }
        prev = c
        continue
      }
      if (c == "(" && prev == "$" && substr(line, i + 1, 1) == "(") {
        j = index(substr(line, i), "))")
        if (j > 0) { i = i + j; prev = ")"; continue }
        script_code_push("("); script_code_push("("); i++
        prev = "("
        continue
      }
      if (top == "\"") {
        if (c == "\\") { i++; prev = ""; continue }
        if (c == "\"") { script_code_pop() }
        else if (c == "(" && prev == "$") { script_code_push("("); SUBS++ }
        prev = c
        continue
      }
      if (c == "#" && (i == 1 || prev == " " || prev == "\t")) {
        CMT = substr(line, i)
        CMTDEPTH = DEPTH
        break
      }
      if (c == "\\") { i++; prev = ""; continue }
      if (c == "\"" || c == SQ) { script_code_push(c); prev = c; continue }
      if (c == "(") {
        if (prev == "$" || prev == "<" || prev == ">") {
          script_code_push("("); SUBS++
        } else {
          script_code_push("s")
        }
      } else if (c == ")" && (top == "(" || top == "s")) {
        script_code_pop()
      }
      prev = c
    }
    CODE = (CMT == "") ? line : substr(line, 1, length(line) - length(CMT))
    top = script_code_top()
    if (top != SQ && top != "\"") { HD = script_code_heredoc(CODE) }
  }
'
