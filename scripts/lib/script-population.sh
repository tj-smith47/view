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

# Where a command can start, for the walks that grade an English word.
# `case` and `test` are ordinary words this population writes in prose, so
# neither can be read bare the way the `ln` walk reads its own: nine prose
# `case` words inside string literals were measured. Written once because
# three scans had drawn this boundary three different ways and none of them
# was the union, and a fourth spelling is what the next scan invents. The
# words are the union of what those three carried: dropping `elif`, `while`
# or `until` leaves a walk guarded behind one of them fail-open, with the
# path it guards never demanded and nothing said.
# shellcheck disable=SC2034
SCRIPT_COMMAND_START='((^|[;&|({!])[[:space:]]*|(^|[[:space:]])(if|then|do|else|elif|while|until)[[:space:]]+)'

# The here-doc tokenizer both scans over this population share: the tags a
# line opens, in the order their bodies arrive, one per line of the returned
# string and prefixed `-` where the terminator may be tab-indented. Reading a
# `<<` the shell does not read as an opener swallows the rest of that file as
# data and hides every finding behind it, which is the one direction a scan
# over this population may never fail in, so the boundary is drawn once here
# rather than twice. A `<<` opens nothing inside a quoted string, inside an
# ANSI-C string past an escaped quote, past the `#` that starts a trailing
# comment, or inside `(( ))`, where it is a left shift and the operand after
# it is a number; `<<<` is a here-string, one line of data with no body. The
# blanks the shell allows between the operator and its word are skipped, so
# `cat << TAG` opens the body `cat <<TAG` opens: read as no operator at all,
# the body under it is scanned as commands, which is the direction that
# hides findings behind text no shell ever runs.
# shellcheck disable=SC2034
SCRIPT_HEREDOC_AWK='function tags_of(line,   i, n, c, q, qc, rest, t, dash, out, ansi, adepth) {
  out = ""
  n = length(line)
  q = ""
  ansi = 0
  adepth = 0
  i = 1
  while (i <= n) {
    c = substr(line, i, 1)
    if (q != "") {
      if ((q == "\"" || ansi) && c == "\\") { i += 2; continue }
      if (c == q) { q = ""; ansi = 0 }
      i += 1
      continue
    }
    if (c == "\\") { i += 2; continue }
    if (c == SQ || c == "\"") {
      q = c
      ansi = (c == SQ && i > 1 && substr(line, i - 1, 1) == "$")
      i += 1
      continue
    }
    if (c == "#" && (i == 1 || substr(line, i - 1, 1) ~ /[[:space:];&|(]/)) break
    if (c == "(" && substr(line, i + 1, 1) == "(") { adepth += 1; i += 2; continue }
    if (c == ")" && substr(line, i + 1, 1) == ")" && adepth > 0) { adepth -= 1; i += 2; continue }
    if (c != "<" || substr(line, i + 1, 1) != "<") { i += 1; continue }
    if (adepth > 0) { i += 2; continue }
    rest = substr(line, i + 2)
    if (substr(rest, 1, 1) == "<") { i += 3; continue }
    dash = ""
    if (substr(rest, 1, 1) == "-") { dash = "-"; rest = substr(rest, 2) }
    sub(/^[[:space:]]+/, "", rest)
    qc = substr(rest, 1, 1)
    if (qc == SQ || qc == "\"" || qc == "\\") rest = substr(rest, 2)
    t = ""
    while (rest != "" && substr(rest, 1, 1) ~ /[A-Za-z0-9_]/) {
      t = t substr(rest, 1, 1)
      rest = substr(rest, 2)
    }
    if (t != "" && (qc == SQ || qc == "\"") && substr(rest, 1, 1) == qc) rest = substr(rest, 2)
    if (t != "") out = out dash t "\n"
    i = n - length(rest) + 1
  }
  return out
}
'

# The reader every walk over that population shares: an awk prelude that
# turns a line into the text a shell reads as commands. Three gates grade a
# word (`ln`, `case`) or a comment by where it sits, and each of them drew
# the boundary in its own way -- a line anchor, a list of the operators a
# command can follow, a block that ended at the first line starting with
# `)`. The boundary is one property of the file, so it is read once here.
#
# `script_code_scan(line)` sets CODE to the line with its comment cut, and to
# the empty string inside a here-doc body; BARE to the part of CODE a shell
# reads outside every quote, which is where a `<<` is the here-doc operator
# and a brace is structure; CMT to the comment and CMTDEPTH to the
# substitution nesting the comment sits in; WAS and DEPTH to the nesting
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
# population carries no such string. A `(` that opens a subshell or gives a
# `case` pattern its leading paren is pushed and popped without changing the
# nesting, so the pattern the page mandates stays inside its substitution and
# a plain `$( ( x ) )` holds its nesting across the group; a pattern written
# without that paren pops the substitution, which is the miscount 3.2 itself
# makes.
#
# The here-doc operator is read by the tokenizer above, over BARE, so a tag
# inside a quoted argument (`printf '%s' "<<x"`) opens nothing and the line
# after it is still code, and the tags a line opens are queued in the order
# their bodies arrive. Every spelling of the tag reaches it: bare, single-
# and double-quoted, backslash-quoted, `<<-`, and any of those written with
# the blank the shell allows after the operator. A tag that is the operator
# and never terminates leaves CODE empty to the end of the file: every walk
# then reads no code there, which is the fail-closed half -- a harvest stops
# rather than running on into text the handler never runs.
# shellcheck disable=SC2034
SCRIPT_CODE_AWK="$SCRIPT_HEREDOC_AWK"'
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
  function script_code_body(line,   cur, n) {
    n = index(HD, "\n")
    cur = substr(HD, 1, n - 1)
    if (substr(cur, 1, 1) == "-") { sub(/^\t+/, "", line); cur = substr(cur, 2) }
    if (line == cur) { HD = substr(HD, n + 1) }
  }
  function script_code_scan(line,   i, n, c, prev, top, j) {
    CODE = ""; BARE = ""; CMT = ""; CMTDEPTH = 0; WAS = DEPTH
    if (HD != "") { script_code_body(line); return }
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
      if (c == "\\") {
        # a backslash quotes the tag it introduces, so `<<\TAG` is the
        # operator with a quoted `TAG` after it. The character the backslash
        # quotes is read into BARE with it there and nowhere else: dropped,
        # the tag arrives a letter short, the terminator below never matches
        # it, and the rest of the file is read as body
        if (BARE ~ /<<-?[[:space:]]*$/) { BARE = BARE c substr(line, i + 1, 1) }
        else { BARE = BARE c }
        i++; prev = ""; continue
      }
      if (c == "\"" || c == SQ) {
        # a here-doc tag is quoted as often as it is bare, and quoting it
        # disables expansion in the body rather than making the `<<` text, so
        # the tag is read into BARE with its quotes instead of being skipped
        # with every other quoted word. It closes on its own line or it is no
        # tag. The quote that opens an ordinary word is not read into BARE at
        # all: the tokenizer below reads BARE as a line of its own, and an
        # opening quote with no partner there would put the `<<` after it
        # inside a string that never ends.
        if (BARE ~ /<<-?[[:space:]]*$/) {
          j = index(substr(line, i + 1), c)
          if (j > 0) {
            BARE = BARE c substr(line, i + 1, j)
            i = i + j
            prev = c
            continue
          }
        }
        script_code_push(c); prev = c; continue
      }
      BARE = BARE c
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
    if (top != SQ && top != "\"") { HD = tags_of(BARE) }
  }
'
