#!/usr/bin/env bash
set -euo pipefail

# This file's own directory, resolved once and before any mode handler cds
# into the root it grades: from in there a relative $0 no longer names this
# script, and every helper resolved through it reads as missing.
SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"

# The shebang selection under scripts/, shared with check-portability.sh and
# check-budget-drift-cases.sh. Resolved beside this file, so a copy of the
# checker graded from a scratch root finds the helper it was copied beside
# rather than one under the root.
# shellcheck source=lib/script-population.sh
. "$SCRIPT_DIR/lib/script-population.sh"

# Where the one temp file this gate makes goes, resolved beside this file for
# the reason above.
# shellcheck source=lib/scratch.sh
. "$SCRIPT_DIR/lib/scratch.sh"

# Session-narrative, spec-task-tag, and SDD-ledger-row markers, shared
# between source comments (anchored on the language's own comment prefix,
# `anchor` = "(prefix).*") and doc prose (`anchor` = "", matching anywhere
# in the line -- README/docs/*.md have no comment prefix to anchor on, and
# a stray "Task 6" or "ledger 133" in prose is exactly as much a drift risk
# as the same citation inside a comment).
check_narrative_markers() {
  local anchor="$1"
  shift
  local targets=("$@")
  local fail=0
  if grep -rnE "${anchor}\\b(Phase|Task|Wave|Cycle|Session) [0-9]" "${targets[@]}"; then
    echo "STYLE FAIL: session-narrative comment marker"; fail=1
  fi
  # "step" checked separately, case-insensitively: this tree has committed
  # lowercase "step N" narrative references a case-sensitive check missed,
  # and unlike "task"/"session" (which read as ordinary lowercase words in
  # unrelated prose), "step" has no such legitimate lowercase reading that a
  # case-insensitive match would false-positive on in this tree today.
  if grep -rniE "${anchor}\\bstep [0-9]" "${targets[@]}"; then
    echo "STYLE FAIL: session-narrative comment marker (step)"; fail=1
  fi
  # "task" also checked case-insensitively, following the same shape as
  # "step" above: this tree has committed lowercase "task N" narrative
  # references (matcher.rs and budgets.rs) that the case-sensitive check
  # above missed. A bare digit-adjacency match false-positives on ordinary
  # prose where the number belongs to the NEXT phrase, not a task citation
  # ("spawn a background task 3 seconds before the deadline fires", "this
  # queue drains one task 4 times per tick under load") -- requiring the
  # digit be followed by a non-alphanumeric-non-space character (an
  # apostrophe, quote, comma, ...) or end of line, rather than by
  # whitespace then a word, isolates the citation shapes this tree actually
  # had ("task 16's...", `see task 19"`) from that prose. This is a
  # precision-over-recall trade, not a complete grammar: a citation phrased
  # as "task 16 owns the paired..." -- digit followed by whitespace then a
  # verb, structurally identical to the prose false positives above -- will
  # slip through uncaught, same as "as task 7 requires" would. Accepted
  # because a missed citation still reads as ordinary English and does no
  # harm left in place, where a false-positive failure blocks an unrelated,
  # correct commit.
  if grep -rniE "${anchor}\\btask [0-9]+([^[:alnum:] ]|\$)" "${targets[@]}"; then
    echo "STYLE FAIL: session-narrative comment marker (task)"; fail=1
  fi
  # references to a planning charter: the charters live under .claude/,
  # which no clone of this tree carries, so a comment or doc pointing at one
  # names a document its reader cannot open instead of stating the rule the
  # code holds. Case-insensitive and prefix-anchored on the word, which also
  # takes "charters", "chartered" and the possessive.
  if grep -rniE "${anchor}\\bcharter" "${targets[@]}"; then
    echo "STYLE FAIL: planning-charter reference"; fail=1
  fi
  # spec-task tags (T4/T5/T6): a comment/doc must state what the code does,
  # never which spec task produced it. Two shapes: a slash-joined sequence
  # (T4/T5/T6, T10/T11), which has no legitimate non-task-tag reading
  # anywhere in Rust syntax or prose, and a single tag standing alone
  # surrounded by whitespace ("the T4 brief", "done in T7."). Not a blanket
  # \bT[0-9]+\b ban, which would flag far more. Backtick-wrapped type
  # parameters never match (a backtick, not whitespace, precedes the T); a
  # bare prose mention of a T1-style name still trips the standalone
  # pattern, so backtick type params in rustdoc/prose stay clear of it.
  if grep -rnE "${anchor}\\bT[0-9]+/T[0-9]+" "${targets[@]}"; then
    echo "STYLE FAIL: spec-task tag sequence in comment"; fail=1
  fi
  if grep -rnE "${anchor}[[:space:]]T[0-9]+[.,:]?([[:space:]]|\$)" "${targets[@]}"; then
    echo "STYLE FAIL: spec-task tag in comment"; fail=1
  fi
  # roadmap-streak labels (S1.7, S2.10): the streak-and-task numbering of
  # the plan a change came out of, which names the conversation's structure
  # rather than anything the code does -- and points at a plan under
  # .claude/ that no clone carries. Requires the dotted shape, so an
  # ordinary "S3" or a `S1` type parameter never matches.
  if grep -rnE "${anchor}\bS[0-9]+\.[0-9]+\b" "${targets[@]}"; then
    echo "STYLE FAIL: roadmap-streak label in comment"; fail=1
  fi
  # SDD-internal ledger-row citation ("ledger 133", "ledger:164"): the exit
  # drain's own numbered deferred-item list, not a fact about the code. Not
  # a blanket \bledger\b ban -- "ledger" is also this tree's own accounting
  # term (the macOS phys_footprint ledger, the harness's shortfall/budget
  # ledger), which a bare word match would flag on every legitimate use;
  # requiring a directly adjacent number is what isolates the citation
  # shape from those, and today's tree has zero adjacent-number hits.
  if grep -rniE "${anchor}\\bledger[[:space:]]*:?[[:space:]]*[0-9]+" "${targets[@]}"; then
    echo "STYLE FAIL: SDD ledger-row reference in comment"; fail=1
  fi
  return $fail
}

# All content-pattern checks against source comments/prose. Parameterized on
# a target path, a language's own line-comment marker(s) (Rust/TOML: '//' or
# '#'; Lua: '--'), and file-type include globs, so the same narrative-marker
# patterns run against every language a scenario or fixture is authored in,
# not only Rust: a scenario TOML or fixture Lua file citing a gitignored
# session document is exactly as much a drift risk as a .rs file doing the
# same. `--file` mode (below) is scoped to Rust only, matching the
# post-edit-rs.sh hook's own single-file, single-language use.
check_content() {
  local target="$1"
  local comment_prefix="$2"
  shift 2
  local includes=("$@")
  local fail=0
  check_narrative_markers "(${comment_prefix}).*" "$target" "${includes[@]}" || fail=1
  if grep -rn '§' "$target" "${includes[@]}"; then
    echo "STYLE FAIL: section-symbol reference in code"; fail=1
  fi
  if grep -rnE "(${comment_prefix}).*\\b(we|I|Claude) (added|implemented|changed|fixed|removed)" "$target" "${includes[@]}"; then
    echo "STYLE FAIL: assistant-citation comment"; fail=1
  fi
  # bare first-person pronouns, which the verb-anchored check above misses:
  # a comment narrating "the window where we drop the Engine" cites the
  # session just as much as one saying "we changed", and comments address a
  # future reader who was never part of that "we". The `us` arm strips the
  # microsecond unit first, since this tree writes `0.37 us` in prose where
  # no greppable pronoun sense exists. Both strips spell their word
  # boundaries as a consumed-then-reinstated character class rather than
  # `\b`, which is a GNU sed extension: BSD sed accepts the expression and
  # silently substitutes nothing, so on a BSD userland the strip no-ops, the
  # microsecond prose survives into the grep below, and the gate's verdict
  # depends on which sed the host happens to ship. The unit-list strip loops
  # because its leading boundary consumes a character that an immediately
  # adjacent second match would otherwise need.
  if grep -rnE "(${comment_prefix}).*\\b(we|our|ours|ourselves|us|my|mine|Claude)\\b" \
      "$target" "${includes[@]}" \
      | sed -E -e 's/[0-9]+(\.[0-9]+)? us([^[:alnum:]_]|$)/\2/g' \
               -e ':m' \
               -e 's/(^|[^[:alnum:]_])ms([,/ ]+)us([^[:alnum:]_]|$)/\1ms\2\3/g' \
               -e 'tm' \
      | grep -E "\\b(we|our|ours|ourselves|us|my|mine|Claude)\\b"; then
    echo "STYLE FAIL: first-person pronoun in comment"; fail=1
  fi
  # standalone `I`, excluding `I/O`: the slash is a non-word character, so a
  # plain \bI\b would flag every I/O mention in the tree.
  if grep -rnE "(${comment_prefix}).*(^|[^/[:alnum:]_])I([^/[:alnum:]_]|\$)" "$target" "${includes[@]}"; then
    echo "STYLE FAIL: first-person pronoun in comment"; fail=1
  fi
  if grep -rnE '\bFinding [0-9]|\btest gap [0-9]|found in review|\bAudit [A-Z]?[0-9]' "$target" "${includes[@]}"; then
    echo "STYLE FAIL: review-finding reference in comment"; fail=1
  fi
  # caller references: a comment states what the code guarantees, never who
  # happens to call it -- a caller list is stale the moment a second one
  # appears, and it sends the reader off to a call site instead of telling
  # them the contract. Anchored on the phrase plus an identifier-shaped
  # target so ordinary prose ("used by default", "called from within the
  # same lock hold") does not trip it.
  if grep -rniE "(${comment_prefix}).*\\b(used by|called from|called by|invoked by|invoked from) [\`[]*[A-Za-z_][A-Za-z0-9_]*(::|\\.|\\(|\`|\\])" \
      "$target" "${includes[@]}"; then
    echo "STYLE FAIL: caller reference in comment"; fail=1
  fi
  # narrative/roadmap pointers: comments must state what the code does now,
  # never when it changes. P[0-9] is intentionally case-sensitive (not -i):
  # a lowercase p0/p1 reads as a coordinate or point variable, not a phase
  # tag, and the tree has no such roadmap-tagged identifiers to catch.
  if grep -rniE '\bthis phase\b|\ba later (phase|task|session)\b|\bin a later\b' "$target" "${includes[@]}"; then
    echo "STYLE FAIL: roadmap-phase comment marker"; fail=1
  fi
  # session-narrative markers the phase/task-number check above doesn't
  # catch: "this task" (no number attached, unlike "Task 10") and
  # "the RED/GREEN test" (TDD-status narration, not a fact about the code)
  if grep -rniE '\bthis task\b|\bthe (red|green) test\b' "$target" "${includes[@]}"; then
    echo "STYLE FAIL: task/TDD-status comment marker"; fail=1
  fi
  if grep -rnE '\bP[0-9]\b' "$target" "${includes[@]}"; then
    echo "STYLE FAIL: roadmap-phase tag in comment"; fail=1
  fi
  # review-finding tags (C2, I1, M3, and their possessive form C2's): a
  # comment must state what the code does, never which review finding
  # prompted it. Case-sensitive (not -i), matching this file's existing
  # P[0-9] check above: the review's own tag convention is always
  # uppercase-letter-plus-digit, and a case-insensitive match would also
  # catch lowercase tokens like "i2"/"m1" that read as ordinary identifiers
  # rather than finding tags, with no matches like that anywhere in this
  # tree today.
  if grep -rnE "\b[CIM][0-9]+\`?'s?\b" "$target" "${includes[@]}"; then
    echo "STYLE FAIL: review-finding tag in comment"; fail=1
  fi
  # TDD/session-narrative markers one synonym past the existing "this task"/
  # "the red/green test" check: "the RED/GREEN half" (a paired-test label),
  # "this fix"/"the unfixed" (fix-narrative instead of a code fact), and
  # "pre-image" (git-diff jargon for "the code before this change")
  if grep -rniE '\bthe (red|green) half\b|\bthis fix\b|\bthe unfixed\b|\bpre-image\b' "$target" "${includes[@]}"; then
    echo "STYLE FAIL: fix-narrative comment marker"; fail=1
  fi
  # bare git-style commit hashes cited in prose ("fa54c7c's replay", "by
  # eae8542"): a plain \b[0-9a-f]{7}\b would also match a real 7-hex-digit
  # constant (a color, a checksum, a magic number) with no possessive or
  # "by"-prefix reading, so this scopes to the two prose shapes this tree's
  # actual violations used instead of a blanket hex-token ban
  if grep -rnE "\b[0-9a-f]{7}\`?'s\b|\bby [0-9a-f]{7}\b" "$target" "${includes[@]}"; then
    echo "STYLE FAIL: commit-hash reference in comment"; fail=1
  fi
  # a comment must state what the code does, never who found it lacking or
  # what body prescribed it: "the reviewer flagged" / "coordinator
  # requirement" name a person or process, not a fact about the code
  if grep -rniE '\bthe (reviewer|coordinator|auditor)\b|\bcoordinator requirement\b' "$target" "${includes[@]}"; then
    echo "STYLE FAIL: reviewer/coordinator attribution in comment"; fail=1
  fi
  # banned outright, not just in comments: no current source file has a
  # string literal that legitimately needs one, so this is a plain content
  # scan rather than a comment-only grep
  if grep -rn '—' "$target" "${includes[@]}"; then
    echo "STYLE FAIL: emdash in source"; fail=1
  fi
  return $fail
}

if [ "${1:-}" = "--file" ]; then
  FILE="${2:-}"
  if [ -z "$FILE" ]; then
    echo "usage: $0 --file FILE" >&2
    exit 2
  fi
  [ -f "$FILE" ] || exit 0
  check_content "$FILE" '//|#' --include='*.rs' || exit 1
  exit 0
fi

# The measure every width walk in this file takes, in one place because a
# tree graded in two units is a tree where a narrower line reddens while a
# wider one passes: a run longer than the limit exempts itself in whatever
# unit the walk counts, so bytes redden a 68-character comment beside a
# 77-character one whose long token is subtracted whole.
#
# Characters, counted without a UTF-8 awk: under LC_ALL=C a character is a
# lead byte plus its continuation bytes, so dropping the continuations
# leaves one byte per character on gawk, mawk and BSD awk alike -- the same
# determinism a byte count was reached for, in the unit an editor shows.
# Every awk this feeds is run under LC_ALL=C, which the range needs to be a
# range of bytes at all: gawk in a UTF-8 locale refuses it as a collation
# character.
#
# The continuation range is built by sprintf out of two literal bytes rather
# than written as an octal escape inside the brackets: a backslash inside a
# bracket expression is undefined in POSIX awk, which is the construct
# .claude/rules/shell.md bans outright, and the three awks this tree runs
# under are free to disagree over it. Built once into CONT and reused,
# because a walk calls this per line.
#
# A character is not a terminal column, and every message this feeds says
# characters because that is what it counts: a double-width glyph counts
# one and paints two, a combining mark counts one and paints none, so the
# same measure passes a comment of 61 characters that fills 91 columns and
# reddens a decomposed one of 92 that fills 62. Both limits are stated in
# .claude/rules/shell.md and cased beside the width cases; a number carrying
# a unit it is not in is worse than no number at all.
AWK_COLS='function cols(s,   t) { if (CONT == "") { CONT = sprintf("[%c-%c]", 128, 191) }
                        t = s; gsub(CONT, "", t); return length(t) }
'
# One table-row reading for both width gates: a genuine row carries a second
# `|` past the leading one or opens the separator shape (`|-`, `| -`); a prose
# line that merely starts with a literal `|` has neither and is not exempt.
AWK_TABLE_ROW='function is_table_row(l,   t) {
      t = l; sub(/^[[:space:]]*/, "", t)
      if (t !~ /^\|/) { return 0 }
      if (t ~ /^\|.*\|/) { return 1 }
      if (t ~ /^\|[[:space:]]*-/) { return 1 }
      return 0
    }
'

# Every embedded Lua chunk wraps at 80 characters. The chunks are read beside
# rustfmt-held Rust and rustfmt does not reach inside a string literal, so
# nothing else in the toolchain catches a line past that width, and every
# chunk a docs/*-wire-capture.md fence publishes verbatim -- pinned
# byte-for-byte by the walk in nvim_api.rs's tests -- is republished at
# whatever width the source carries, into a fence that does not wrap.
#
# Two shapes carry Lua, and each has its own walk below: a
# `const ..._CHUNK: &str` declaration in nvim_api.rs (`concat!( ... );`,
# `"\` ... `";`, a one-line literal, or an alias re-exporting another
# chunk's const), and a multi-line string literal anywhere under
# view-engine's src/ or tests/. Each walk cross-checks what it reached
# against a plain grep for the same declarations: the grep is blind to
# where the declaration sits and to what closes it, so a shape that drifts
# out of the walk's own match is still counted, and the two numbers part.
check_lua_chunk_width() {
  local file='crates/view-engine/src/nvim_api.rs'
  if [ ! -f "$file" ]; then
    echo "STYLE FAIL: $file missing; cannot check Lua chunk width"
    return 1
  fi
  local declared report status walked
  # a grep that matches nothing exits non-zero, and that outcome is data
  # here rather than an error: without this the guard below never speaks
  declared=$(grep -c '_CHUNK: &str' "$file") || declared=0
  if [ "$declared" -eq 0 ]; then
    echo "STYLE FAIL: no _CHUNK declaration found in $file;"
    echo "  the width check walked nothing and cannot vouch for the file."
    return 1
  fi
  status=0
  report=$(LC_ALL=C awk "$AWK_COLS"'
    function check_width(line) {
      if (cols(line) > 80) {
        printf "%s:%d: %d characters\n", FILENAME, FNR, cols(line)
        over++
      }
    }
    inchunk {
      check_width($0)
      # an escaped quote inside the Lua does not close the Rust literal, so
      # it must not end the walk either -- it would skip the rest silently.
      # The escape is read by index rather than by a bracket expression
      # holding a backslash, which POSIX leaves undefined and the three awks
      # this tree runs under read differently
      if ($0 ~ /^\);$/ || $0 ~ /^";$/ ||
          ($0 ~ /";$/ && substr($0, length($0) - 2, 1) != "\\")) { inchunk = 0 }
      next
    }
    /^(pub(\(crate\))? )?const [A-Z_]+_CHUNK: &str =/ {
      found++
      check_width($0)
      if ($0 ~ /concat!\($/) { inchunk = 1; next }
      if ($0 ~ /_CHUNK;$/) { next }
      if ($0 ~ /";$/) { next }
      inchunk = 1
    }
    END { printf "CHUNKS %d\n", found; exit (over > 0) ? 1 : 0 }
  ' "$file") || status=$?
  walked=$(printf '%s\n' "$report" | sed -n 's/^CHUNKS //p')
  if [ "$walked" != "$declared" ]; then
    echo "STYLE FAIL: the width check walked $walked Lua chunks in $file,"
    echo "  but grep counts $declared _CHUNK declarations there: a declaration"
    echo "  shape stopped matching the walk. Widen the walk to reach it."
    return 1
  fi
  if [ "$status" -ne 0 ]; then
    printf '%s\n' "$report" | grep -v '^CHUNKS '
    echo "STYLE FAIL: a Lua chunk line is over 80 characters"
    echo "  Wrap it the way the rest of nvim_api.rs's chunks are: break at"
    echo "  an operator or after a comma, keep the chunk's indentation."
    return 1
  fi
  return 0
}

# The other shape: a multi-line string literal, which is how every live test
# hands Lua to nvim and how the crate carries its longer messages. rustfmt
# holds the line it opens on and nothing else about it, so the same 80
# characters apply to every line of one -- no test of what the literal holds,
# which is what let a Lua chunk broken with a trailing backslash
# (checktime_live.rs) and a fixture row (inline_review_live.rs) read as
# prose and go unchecked.
check_string_literal_width() {
  local files
  files=$(find crates/view-engine/src crates/view-engine/tests -name '*.rs' 2>/dev/null | sort) || files=""
  if [ -z "$files" ]; then
    echo "STYLE FAIL: no view-engine sources found; the literal width check did not run"
    return 1
  fi
  local declared report status walked
  # the same two opening shapes the awk matches, plus the one it cannot
  # follow (a quote left at the end of an assignment, with no backslash):
  # a shape the walk stops reaching is still counted here
  declared=$(grep -hE '^[[:space:]]*"[^"]+$|= "\\?$' $files | wc -l | tr -d ' ') || declared=0
  if [ "$declared" -eq 0 ]; then
    echo "STYLE FAIL: no multi-line string literal found under view-engine;"
    echo "  the width check walked nothing and cannot vouch for the tree."
    return 1
  fi
  status=0
  report=$(LC_ALL=C awk "$AWK_COLS"'
    function quotes(line,   n, i, len, c) {
      n = 0; i = 1; len = length(line)
      while (i <= len) {
        c = substr(line, i, 1)
        if (c == "\\") { i += 2; continue }
        if (c == "\"") { n++ }
        i++
      }
      return n
    }
    function check_width(line) {
      if (cols(line) > 80) {
        printf "%s:%d: %d characters\n", FILENAME, FNR, cols(line)
        over++
      }
    }
    inlit {
      check_width($0)
      if (quotes($0) > 0) { inlit = 0 }
      next
    }
    # the literal opens on its own line, or shares the line with the
    # assignment that carries it (`const NAME: &str = "\`)
    /^[[:space:]]*"/ || /= "\\$/ {
      if (quotes($0) == 1) {
        found++
        check_width($0)
        inlit = 1
      }
    }
    END { printf "LITERALS %d\n", found; exit (over > 0) ? 1 : 0 }
  ' $files) || status=$?
  walked=$(printf '%s\n' "$report" | sed -n 's/^LITERALS //p')
  if [ "$walked" != "$declared" ]; then
    echo "STYLE FAIL: the width check walked $walked multi-line string literals,"
    echo "  but grep counts $declared opening lines under view-engine: a literal"
    echo "  shape stopped matching the walk. Widen the walk to reach it."
    return 1
  fi
  if [ "$status" -ne 0 ]; then
    printf '%s\n' "$report" | grep -v '^LITERALS '
    echo "STYLE FAIL: a line inside a string literal is over 80 characters"
    echo "  Wrap it the way nvim_api.rs's chunks are: break at an operator or"
    echo "  after a comma, keep the literal's own indentation."
    return 1
  fi
  return 0
}

# An acceptance assertion's expected color is read from the live scheme by
# probe, never from a config's text (scripts/acceptance/artifacts.sh).
#
# Two literals, banned outright, rather than a pattern that names a reader
# and a path together: the code this exists to stop held the path in a
# variable assigned on one line and ran the `sed` on another, wrapped over
# a third with a `\` -- and grep is line-oriented, so every such pattern
# misses the only shape that ever carried the defect. A colorscheme file is
# not an acceptance script's to name at all (a `cp -R .../nvim dst` of a
# whole config still is), and a `#rrggbb` inside one is an expectation that
# outlives whichever scheme it was copied out of. Fixtures are excluded by
# the include filter: a colorscheme is made of the literals this bans.
# A notice a person reads is prose, and the pages under docs/ join a clause
# to the next with a full stop rather than with a two-hyphen dash
# (.claude/rules/docs.md). The notices are the same prose written in Rust,
# and one of them shipped with the dash after the rule was written.
#
# Each row is path, the text that identifies the line, and the grounds.
NOTICE_JOINER_EXEMPT='crates/view-core/src/native/ai_panel/review.rs|{position} -- {notice}|a review row: the position, and the notice beside it
crates/view-core/src/native/ai_panel/review.rs|{position} -- {keys}|a review row: the position, and its key legend beside it
crates/view/src/remote_guard.rs|env -- |the shell argument separator inside a command line view prints'

check_notice_joiners() {
  local files report status joiners
  files=$(find crates/view/src crates/view-core/src crates/view-native/src \
    crates/view-ai/src crates/view-tui/src -name '*.rs' 2>/dev/null | sort) || files=""
  if [ -z "$files" ]; then
    echo "STYLE FAIL: no sources found; the notice joiner check did not run"
    return 1
  fi
  status=0
  report=$(LC_ALL=C awk -v exempt="$NOTICE_JOINER_EXEMPT" '
    BEGIN {
      rows = split(exempt, row, "\n")
      for (i = 1; i <= rows; i++) {
        split(row[i], field, "|")
        epath[i] = field[1]; etext[i] = field[2]; ewhy[i] = field[3]; used[i] = 0
      }
    }
    FNR == 1 { intest = 0 }
    /#\[cfg\(test\)\]/ { intest = 1 }
    intest { next }
    {
      bare = $0
      sub(/^[[:space:]]+/, "", bare)
      if (substr(bare, 1, 2) == "//") next
      if (index($0, " -- ") == 0) next
      for (i = 1; i <= rows; i++) {
        if (epath[i] == FILENAME && index($0, etext[i]) > 0) { used[i] = 1; next }
      }
      printf "JOINER %s:%d: %s\n", FILENAME, FNR, bare
      found++
    }
    END {
      for (i = 1; i <= rows; i++) {
        if (used[i]) { printf "EXEMPT %s: %s, %s\n", epath[i], etext[i], ewhy[i] }
        else { printf "DEAD %s: %s\n", epath[i], etext[i]; dead++ }
      }
      exit (found > 0 || dead > 0) ? 1 : 0
    }
  ' $files) || status=$?
  printf "%s\n" "$report" | sed -n "s/^EXEMPT /  notice joiner exempt: /p"
  if [ "$status" -ne 0 ]; then
    joiners=$(printf "%s\n" "$report" | sed -n "s/^JOINER //p")
    [ -n "$joiners" ] && printf "%s\n" "$joiners"
    printf "%s\n" "$report" | sed -n "s/^DEAD /STYLE FAIL: an exemption matches no line any more: /p"
    if [ -n "$joiners" ]; then
      echo "STYLE FAIL: a notice joins its clauses with a two-hyphen dash"
      echo "  Write a full stop between the two thoughts, the way the pages"
      echo "  under docs/ do. A line that is no sentence at all (a row of"
      echo "  two fields, a CLI separator) goes in NOTICE_JOINER_EXEMPT with"
      echo "  its grounds."
    fi
    return 1
  fi
  return 0
}

check_acceptance_expectations() {
  local acceptfail=0
  [ -d scripts/acceptance ] || return 0
  if grep -rn --include='*.sh' -- 'nvim/colors' scripts/acceptance; then
    echo "STYLE FAIL: an acceptance script names a colorscheme file (probe the live scheme instead)"; acceptfail=1
  fi
  if grep -rnE --include='*.sh' '#[0-9a-fA-F]{6}' scripts/acceptance; then
    echo "STYLE FAIL: a color literal in an acceptance script (probe the live scheme instead)"; acceptfail=1
  fi
  return $acceptfail
}

# A program a test runs is a committed fixture under scripts/test-fixtures/,
# never one the test writes: a sibling test's fork landing inside the
# write's open-descriptor window inherits the writable descriptor into its
# child, and Linux then refuses the exec with ETXTBSY (`#!` scripts
# included). Measured, that window is 5-12 us wide and the odds run with the
# test binary's own fork rate, so the failure lands once in thousands of
# runs, never on demand, and surfaces as whatever the spawn's caller
# degrades a failed spawn to.
#
# Keyed on the mode literal itself rather than on the call that carries it:
# this tree spells the same act three ways -- `from_mode(0o755)`,
# `perms.set_mode(0o755)` and the UFCS `set_mode(&mut perms, 0o755)`, which
# is the spelling the site this pin exists for actually used, and which a
# pattern anchored on the call plus its first argument reads straight past.
# Counted per file rather than per line, the way the condition-notice pin
# below counts call sites: a line number moves under any edit above it and a
# count does not, while a second site in an already-listed file still parts
# the two numbers. Precision over recall, the trade this file makes
# elsewhere too -- 0o755 is the mode every site in this tree writes, and one
# written 0o700 or 0o777 would pass unseen.
#
# Each row is a path, its pinned number of occurrences, and the grounds that
# let them stay written rather than becoming a fixture.
WRITTEN_PROGRAM_SITES='
crates/view-ai/src/provision.rs 3 an installer making the binary it just unpacked runnable, the doc that states the mode, and one test writing a script that resolve_node only stats and never execs
crates/view-engine/src/process.rs 3 the ETXTBSY pin itself, where the write is the whole point, and two wrappers spawned through spawn_past_busy_text
crates/view-engine/tests/checktime_live.rs 1 a --nvim-bin wrapper, spawned through that same retry
crates/view-oracle/tests/smoke.rs 2 a --nvim-bin wrapper spawned through that retry, plus a directory mode this walk cannot tell apart from a file mode
'
# A long-lived child goes out through `view_proc::spawn_tied_to_this_process`
# (or a re-export of it), so a parent killed outright cannot leave it
# running: `Drop` covers the exits a process chooses and none of the ones it
# does not, and a child wedged past noticing its closed pipes never leaves on
# its own. Three of them were found reparented to init at 100% CPU for days.
#
# Not every spawn owes that. A child waited on to completion inside the call
# that made it, or bounded by its own deadline and killed on it, cannot
# outlive anything; a pty child spawned as a session leader is ended by the
# kernel when the master closes. What a row buys is that the answer was
# written down once, by whoever added the spawn, rather than rediscovered by
# whoever finds the stray.
#
# Keyed per file on the two constructors (`Command::new`,
# `CommandBuilder::new`) and on the calls that start something
# (`.spawn(`, `.spawn_command(`), over production lines only (the god-file
# scanner's own classifier, which drops `#[cfg(test)]` regions) and with
# comment lines dropped: a count moves when a spawn is added and does not
# move when a line above it does. The call spellings are what close the
# constructors' blind spot, where a helper is handed an already-built
# `Command` -- at the price of pinning thread and task spawns too, which
# say so in their own rows.
#
# Each row is a path, its pinned number of sites, and how those sites cannot
# leave a stray.
TIED_SPAWN_SITES='
crates/view-ai/src/acp/session.rs 5 the agent adapter, tied through view-proc on unix and by the job object it joins on windows, where tokio owns the pipes and the spawn; plus the thread that waits out a signalled adapter and the task that drives one
crates/view-ai/src/provision.rs 3 an npm install, bounded by its own deadline and killed on it, and the thread that reads it out
crates/view-ai/src/watch.rs 5 a git ls-files, waited on with a deadline and killed on it, and the watcher threads around it
crates/view-bench/src/remote_ui.rs 1 the headless control server, tied: it has no pty to hang up and no controlling terminal
crates/view-bench/src/scenarios/echo_speculated_rtt.rs 3 an interpreter probe that runs to completion, and the relay fixture the row waits out and kills
crates/view-bench/src/session.rs 1 a pty session leader, ended by the kernel when the master closes
crates/view-engine/src/process.rs 4 the engine and the remote leg ssh client, both tied through spawn_engine_child, each started again by the ETXTBSY retry
crates/view-harness/src/bin/bench.rs 1 a sysctl read, waited on to completion
crates/view-harness/src/bin/bench/replicates.rs 1 a git read, waited on to completion
crates/view-harness/src/bin/oracle/compat.rs 3 a cargo build and a reference nvim, both waited on to completion, and a pty-hosted view ended by its master closing
crates/view-harness/src/fixture.rs 1 an nvim --version probe, waited on to completion
crates/view-native/src/tree/git.rs 2 a git status, bounded by its own deadline and killed on it
crates/view-oracle/src/compat.rs 4 probe subprocesses and the plugin-cache bootstrap, each bounded by wait_with_timeout and killed on it
crates/view-oracle/src/hang.rs 1 a taskkill, waited on to completion
crates/view-oracle/src/pty.rs 2 the pty funnel: setsid and TIOCSCTTY make the child a session leader, so the master closing delivers SIGHUP
crates/view-oracle/src/remote.rs 1 a stub ssh client, waited on to completion
crates/view-proc/src/lib.rs 4 the anchor thread every tied spawn forks from, which lives as long as the process does, and the watcher off Linux, which is the tie itself and ends on the pipe this process closes by ending, plus the thread that builds it ahead of the first spawn
crates/view-test-support/src/lib.rs 3 a sysctl read and the two process-table probes off Linux, each waited on to completion
crates/view/src/ai_context_worker.rs 1 a worker thread, not a process
crates/view/src/clipboard.rs 2 worker threads, not processes
crates/view/src/remote_guard.rs 2 an ssh probe, bounded by its own deadline and killed on it
crates/view/src/runtime.rs 1 a worker thread, not a process
'
# The god-file scanner's own answer to "which lines are production code",
# read once: the walk costs a second and a half over a tree this size and
# all three pins below ask it the same question.
#
# Resolved beside this script rather than under the walked root: the case
# matrix grades both walks against scratch roots that hold crates/ alone.
#
# The scanner's own stderr is kept for the failure path, where it is the
# whole diagnosis (an untracked tree, a quoted path it cannot name) and a
# caller without it sends its reader to the wrong file.
PROD_LINES_CACHE=""
PROD_LINES_WHY=""
# global rather than local, so the EXIT trap below can still name it: a
# local is gone by the time the trap runs, and set -u aborts the exit path
# on the name it cannot resolve
PROD_LINES_ERR=""
read_prod_lines() {
  if [ -n "$PROD_LINES_CACHE" ]; then
    return 0
  fi
  if ! PROD_LINES_ERR=$(mktemp "$(scratch_root)/check-style-prod-lines-XXXXXX"); then
    PROD_LINES_WHY="mktemp under the scratch root failed"
    return 1
  fi
  # the scan is the slowest step in the gate, so the window in which a
  # Ctrl-C or a set -e abort would strand this file is the whole of it; the
  # straight-line remove below still covers the ordinary path
  trap 'rm -f "$PROD_LINES_ERR"' EXIT
  PROD_LINES_CACHE=$(bash "$SCRIPT_DIR/audit-god-files.sh" --prod-lines . 2> "$PROD_LINES_ERR") || PROD_LINES_CACHE=""
  if [ -z "$PROD_LINES_CACHE" ]; then
    PROD_LINES_WHY=$(head -3 "$PROD_LINES_ERR")
  fi
  rm -f "$PROD_LINES_ERR"
  [ -n "$PROD_LINES_CACHE" ]
}

check_tied_spawns() {
  local expected actual
  if ! read_prod_lines; then
    echo "STYLE FAIL: could not read production lines to check tied spawns${PROD_LINES_WHY:+ -- $PROD_LINES_WHY}"
    return 1
  fi
  expected=$(printf '%s\n' "$TIED_SPAWN_SITES" | awk 'NF { print $1, $2 }' | LC_ALL=C sort)
  actual=$(printf '%s\n' "$PROD_LINES_CACHE" \
    | grep -E '([^A-Za-z0-9_]Command(Builder)?::new|\.spawn\(|\.spawn_command\()' \
    | sed 's/:.*//' | LC_ALL=C sort | uniq -c | awk '{ print $2, $1 }' | LC_ALL=C sort) || actual=""
  if [ "$expected" = "$actual" ]; then
    return 0
  fi
  printf 'pinned:\n%s\nfound:\n%s\n' "$expected" "$actual"
  echo "STYLE FAIL: a production spawn site outside the pinned set"
  echo "  A child that outlives the process that spawned it is a stray nothing"
  echo "  reaps: spawn it through view_proc::spawn_tied_to_this_process (the"
  echo "  crate declares view-proc to reach it), or add a row to this file"
  echo "  saying how this one cannot outlive its parent. A thread or a task"
  echo "  earns a row that says so."
  return 1
}

# Every production site that names a geometry to the engine, pinned per file
# with the ground the pair it spends came from.
#
# An attach is refused outright below `view_core::model::ENGINE_MIN_SIZE`,
# and a spawn seeded at a size the attach does not repeat relayouts every
# window on screen, so a `(width, height)` reaching either call has to have
# come from `view_core::model::grid_target_for` -- and the terminal's own
# reading, which is what a caller has in hand, is exactly the pair that must
# not. Nothing at a call site says which one it holds: `startup.rs`
# legitimately spends a `width, height` bound off a channel, so an
# identifier walk either accepts the raw reading everywhere or rejects the
# one correct site. What holds instead is the shape the pin above uses for
# spawns: the whole population pinned per file with a grounds row each, so a
# new site fails by name until whoever adds it writes down where its
# geometry came from.
#
# Keyed on every spelling a geometry can reach the engine by, because a site
# is a site in whichever of them it is written: the attach as a method
# (`ui_attach`), as an effect variant (`UiAttach`) and as the wire method
# name (`nvim_ui_attach`, which the method substring covers), and the resize
# the same three ways (`try_resize(`, `TryResize`, `nvim_ui_try_resize`).
# The variant spelling is what reaches the three folds that build
# `RpcCall::TryResize` from `Model::grid_target`; a walk keyed on the method
# alone counted none of them. The seventh is the bare `attach(`, bounded on
# the left so the `ui_attach` spelling above does not answer for it: both
# public attach methods funnel through a private `fn attach` that is where
# the pair actually reaches the wire, so a further attach path added inside
# `nvim_api.rs` under that name would otherwise leave the pinned count
# untouched in the one file that owns the call.
#
# The eighth is `late_attach`, the one way a geometry reaches the engine
# without going over the wire at all: `with_late_attach` stores the pair
# and `late_attach_cmd` renders it into the child's `--cmd` argument, which
# is the spawn seed the rationale above turns on. Spelled bare rather than
# as a call, because the pair passes through the field and the destructure
# as well as the two calls, and a walk keyed on `late_attach(` counts
# neither the render nor what feeds it.
#
# Fail-closed on a trailing comment: `--prod-lines` emits the raw line, so a
# spelling written only after a `//` counts as a site. Eliding comments
# would take the two spellings that are string literals (`"nvim_ui_attach"`,
# `"nvim_ui_try_resize"`) with them, since the scanner's one elide removes
# string contents -- so the walk over-counts rather than blinds itself, and
# the answer to a red row whose code holds no geometry is to reword the
# comment or to move the row.
#
# `release(` is the attach guard's own hand-off of the geometry its spawn
# was seeded with, bounded on the left because `release` ends other names in
# this tree, and counted only in the four crates that own an engine attach
# (view, view-core, view-engine, view-oracle). Elsewhere the word reaches a
# lock, a permit or a scan gate: a `release(` added to view-bench or
# view-harness would otherwise fail this gate in a file whose owner never
# touches an attach.
#
# Some of the counted lines open a call, a variant, a pattern or a signature
# whose arguments wrap onto the lines below them, so the counted line itself
# names no pair. The walk derives that set rather than listing it -- a
# counted line ending in `(`, or ending in `{` and not a `fn` line, naming
# none of width, height, cols or rows -- and prints how many there are and
# how many files they sit in. Neither the rows nor this sentence carries the
# number: any re-wrapped signature in the tree moves it, and a hand-written
# one goes stale in silence the next time one is.
#
# Each row is a path, its pinned number of production lines, and where the
# geometry those lines spend came from -- true line by line, so a row that
# describes fewer sites than it counts is a row to rewrite.
GEOMETRY_CALLS='ui_attach|UiAttach|ui_try_resize|try_resize\(|TryResize|(^|[^A-Za-z0-9_])attach\(|late_attach'
GEOMETRY_ATTACH_CRATES='^crates/(view|view-core|view-engine|view-oracle)/'
GEOMETRY_SITES='
crates/view-core/src/model.rs 1 the one RpcCall::UiAttach production builds, from Model::grid_target -- grid_target_for over the model own terminal size
crates/view-core/src/msg.rs 3 the UiAttach, TryResize and TryResizeGrid variant declarations; the pair each variant carries is what its builder put in it
crates/view-core/src/update/ai_fs.rs 4 an AI filesystem lock release and its own helper, no geometry anywhere
crates/view-core/src/update/bridge.rs 1 the showtabline reading resizing the grid when the chrome row count moves, spending Model::grid_target
crates/view-core/src/update/look.rs 2 the look change building one TryResize for the outer grid and one TryResizeGrid for a window, each pair from Look arithmetic over the slot the registry already holds
crates/view-core/src/update/mod.rs 1 the fold resizing the grid when the paint area moves, spending Model::grid_target
crates/view-core/src/update/ui_event.rs 1 the tabline fold resizing the grid when the chrome row count moves, spending Model::grid_target
crates/view-engine/src/nvim_api.rs 11 the handle three public attach entry points and its two resize entry points, the private attach two of them hand off to and both hand-off lines, and the three wire method-name strings, which name the call rather than a pair; the rest spend what the caller hands them
crates/view-engine/src/process.rs 12 the spawn own geometry seed: the late_attach field and its None default, the builder and its assignment, the getter and its body, the attaches_late predicate, late_attach_cmd, and the two argv paths that destructure the field and render it into --cmd; the field, the default, the getter two lines and the predicate carry no pair, and the rest spend what main or recovery handed the config
crates/view-oracle/src/hang.rs 5 the adversarial harness attaching its own engine at the fixture size it opened the session with and, on the restart leg, at Model::grid_target, plus the TryResize and TryResizeGrid effects it forwards and the resize call forwarding the first of them
crates/view-oracle/src/lib.rs 3 the oracle driver attaching at the size its caller opened the session with, and the TryResize and TryResizeGrid effects it forwards
crates/view-oracle/src/reference.rs 2 the second applier attaching at the size the session under comparison was opened at, and resizing to the height the chrome row count leaves it
crates/view-oracle/src/speculate.rs 2 the speculative-echo battery attaching and resizing at its own fixture geometry
crates/view/src/engine_ops.rs 14 the EngineOps attach and resize surface: the two trait declarations and the three forwarding impls of each behind them, every impl spelled over its signature and the call it forwards to, spending the pair it was handed
crates/view/src/main.rs 2 the attach guard release and the spawn own geometry seed, both spending spawn_size -- what grid_target_for answered the terminal reading with
crates/view/src/native.rs 1 the native session resizing the grid for the row the statusline claims, spending Model::grid_target
crates/view/src/recovery.rs 1 the replacement engine own geometry seed, spending Model::grid_target
crates/view/src/runtime/executor.rs 4 the executor spending the pair the UiAttach, TryResize and TryResizeGrid effects carry, which update() built from the model
crates/view/src/startup.rs 7 the attach guard release and the one attach it feeds, spending the pair main released rather than a reading of their own, plus the restart pattern destructuring the UiAttach and the two attaches that pattern feeds, which spend the pair Model::takes_attach built from grid_target, the zero-argument attach() closure call that carries it, and the read-back of the config late_attach seed
crates/view/src/vlog.rs 2 the takeover topic naming the attach call of that batch: the pattern that matches the effect and the name it writes, which read how many ext surfaces were asked for and carry no pair
'
check_geometry_sites() {
  local expected actual sites
  if ! read_prod_lines; then
    echo "STYLE FAIL: could not read production lines to check geometry sites${PROD_LINES_WHY:+ -- $PROD_LINES_WHY}"
    return 1
  fi
  expected=$(printf '%s\n' "$GEOMETRY_SITES" | awk 'NF { print $1, $2 }' | LC_ALL=C sort)
  # keyed to the path and line number the scanner emits rather than to the
  # match, so a line carrying both a call and a release counts once
  sites=$({
    printf '%s\n' "$PROD_LINES_CACHE" | grep -E "$GEOMETRY_CALLS" || true
    printf '%s\n' "$PROD_LINES_CACHE" | grep -E "$GEOMETRY_ATTACH_CRATES" \
      | grep -E '(^|[^A-Za-z0-9_])release\(' || true
  } | LC_ALL=C sort -u) || sites=""
  actual=$(printf '%s\n' "$sites" | grep . | cut -d: -f1,2 | LC_ALL=C sort -u \
    | sed 's/:[0-9]*$//' | uniq -c | awk '{ print $2, $1 }' | LC_ALL=C sort) || actual=""
  # the wrapped openings, derived here so that nothing written by hand can go
  # stale: printed on the pass as well as the fail, because a carve-out
  # nobody sees the size of is a carve-out nobody re-derives
  printf '%s\n' "$sites" | grep . | awk -F: '
    {
      line = $0
      sub(/^[^:]*:[0-9]*:/, "", line)
      if (line ~ /width|height|cols|rows/) { next }
      if (line !~ /\($/ && (line !~ /\{[[:space:]]*$/ || line ~ /(^|[^A-Za-z0-9_])fn[[:space:]]/)) { next }
      wrapped += 1
      files[$1] = 1
    }
    END {
      n = 0
      for (f in files) { n += 1 }
      printf "geometry: %d counted lines, %d of them wrapped openings in %d files\n", NR, wrapped, n
    }
  ' || true
  if [ "$expected" = "$actual" ]; then
    return 0
  fi
  printf 'pinned:\n%s\nfound:\n%s\n' "$expected" "$actual"
  echo "STYLE FAIL: a production geometry site outside the pinned set"
  echo "  A pair that reaches the engine comes from"
  echo "  view_core::model::grid_target_for, never from the terminal own"
  echo "  reading: an attach is refused below ENGINE_MIN_SIZE and a spawn"
  echo "  seeded past its attach relayouts every window on screen. Add a row"
  echo "  to this file saying where this one geometry came from, or say"
  echo "  there that it carries none."
  return 1
}

# The text an armed EXIT trap runs: the trap lines themselves, the body of
# the function a handler names, and the body of a function that body calls --
# the removal sits one call deep about as often as it sits in the handler,
# and a `cleanup_root "$X"` whose callee removes `$1` is a correct script.
# Read through the shared code reader, so a `#`-led removal is a comment, a
# here-doc body is not structure, and a brace inside a string is text.
# `trap - EXIT` is not armed -- it clears the handler it otherwise reads as --
# and is left out here.
#
# Each harvested line carries both halves the reader made of it, the code
# text and the part outside every quote, as two records one after the other.
# The removal walk below needs both and cannot read the file itself: a
# here-doc opener survives into the harvested text while its terminator does
# not, so a second pass of the reader over this output opens a body that never
# closes and blanks every removal under it. Two records rather than one line
# and a separator byte, because a script line can carry any byte a separator
# could be: split on the first occurrence, a handler line holding that byte
# is cut where the script wrote it instead of where the harvest did, and the
# name on that line never reaches the pairing. A newline cannot occur inside
# a record awk read as a line, so the pairing here cannot be forged from a
# script.
temp_trap_handlers() {
  awk -v SQ="'" -v CS="$SCRIPT_COMMAND_START" "$SCRIPT_CODE_AWK"'
    # the braces a shell reads as structure, counted over the text outside
    # every quote, comment and here-doc body: a `${x}` there balances itself
    function braces(s,   t, d) {
      t = s; d = gsub(/[{]/, "", t)
      t = s; return d - gsub(/[}]/, "", t)
    }
    # where the quote that opened a trap command string closes it, counted
    # past a quote the string escapes: `trap "rm -rf \"$X\"" EXIT` closes on
    # the quote after the last escape, and stopping at the first `\"` cuts
    # the command to `rm -rf \` and loses the name it removes. Only the
    # double-quoted scan walks escapes, since a single-quoted string cannot
    # hold its own quote at all
    function closing_quote(s, q,   i, n, c) {
      n = length(s)
      for (i = 1; i <= n; i++) {
        c = substr(s, i, 1)
        if (q == "\"" && c == "\\") { i++; continue }
        if (c == q) { return i }
      }
      return 0
    }
    {
      script_code_scan($0)
      if (CODE ~ /^[[:space:]]*trap +(-- +)?[^-[:space:]]/ &&
          CODE ~ /(^|[^A-Za-z0-9_])EXIT([^A-Za-z0-9_]|$)/) {
        # what a trap runs is a string the shell re-parses, so the quotes
        # around it are not part of it, and the signals after it are not part
        # of it either. Printed with those quotes gone, a trap whose command
        # is written inline rather than as a handler name reads downstream as
        # the command it is -- a name in command position with a path beside
        # it -- where the raw line offers a quote-led word that is no name to
        # the extraction here and no call to the pairing below
        h = CODE
        sub(/^[[:space:]]*trap +(-- +)?/, "", h)
        q = substr(h, 1, 1)
        preexp = ""
        if (q == SQ || q == "\"") {
          j = closing_quote(substr(h, 2), q)
          h = (j > 0) ? substr(h, 2, j - 1) : substr(h, 2)
          # a name the shell expanded while it built this string is a path by
          # the time the trap is armed, so the quotes the re-parse reads sit
          # around a value and not around a name: the name travels outside
          # them. An escaped one is still a name at the re-parse, and the
          # quotes there decide whether it ever expands -- inside single
          # quotes it never does, and scripts/acceptance/artifacts.sh bakes
          # its own path in this way because the variable is a local
          if (q == "\"") {
            # the operand the name sits in rather than the name alone, since a
            # removal below pairs on a whole operand: the single quotes here
            # sit around a value the shell has already expanded and come off,
            # while a prefix or a suffix stays -- a trap removing "$X/sub"
            # removes nothing of the root
            pn = split(h, pw, "[[:space:]]+")
            for (pi = 1; pi <= pn; pi++) {
              pt = pw[pi]
              if (!match(pt, /[$][{]?[A-Za-z_][A-Za-z0-9_]*/)) { continue }
              if (RSTART > 1 && substr(pt, RSTART - 1, 1) == "\\") { continue }
              gsub(SQ, "", pt)
              preexp = preexp " " pt
            }
            # the shell drops the backslash while it builds the string it
            # re-parses, so the command the trap runs carries the quote alone,
            # and a `\$X` written here is a live `$X` by the time the trap
            # fires -- which is why the escape is removed here and blanked in
            # a handler body, where nothing re-parses it
            gsub(/\\"/, "\"", h)
            gsub(/\\[$]/, "$", h)
          }
        } else {
          sub(/[[:space:]].*/, "", h)
        }
        # the quotes left inside a re-parsed command string are the other
        # kind and are balanced, so a run between a matching pair is the
        # string it opens and the text outside them is where a call sits
        bareh = h
        gsub(/"[^"]*"/, "", bareh)
        gsub(SQ "[^" SQ "]*" SQ, "", bareh)
        armed = armed h preexp "\n" bareh "\n"
        name = h
        sub(/[[:space:]].*/, "", name)
        if (name ~ /^[A-Za-z_][A-Za-z0-9_]*$/) { want[name] = 1 }
      }
      if (inbody) {
        body[cur] = body[cur] CODE "\n" BARE "\n"
        # closed at the brace that closes the function, by depth, never at an
        # indentation: an indentation rule ends the body at a `{ ...; } >&2`
        # group or at a JSON here-doc `}` in column one, and the removal below
        # is then never read, which reddens a handler that is right
        depth = depth + braces(BARE)
        if (depth <= 0) { inbody = 0 }
        next
      }
      name = CODE
      sub(/^[[:space:]]*/, "", name)
      sub(/^function[[:space:]]+/, "", name)
      if (name !~ /^[A-Za-z_][A-Za-z0-9_]*[[:space:]]*\(\)/) { next }
      sub(/[[:space:]]*\(\).*/, "", name)
      cur = name
      body[cur] = CODE "\n" BARE "\n"
      depth = braces(BARE)
      if (depth > 0) { inbody = 1 }
    }
    END {
      printf "%s", armed
      n = 0
      for (f in want) { queue[++n] = f }
      # the queue grows as a printed body turns out to call another handler;
      # walked by index rather than by `for (f in want)`, which is undefined
      # the moment the walk adds a name to what it is walking
      for (i = 1; i <= n; i++) {
        f = queue[i]
        if (f in seen) { continue }
        seen[f] = 1
        if (!(f in body)) { continue }
        printf "%s", body[f]
        # the call is read outside every quote: a name written inside a
        # string is text the handler prints, and queueing its body pulls a
        # removal the handler never runs into the pairing below. Every second
        # record is that half, since each harvested line is a pair
        k = split(body[f], lines, "\n")
        for (j = 2; j <= k; j += 2) {
          out = lines[j]
          for (g in body) {
            if (!(g in seen) && out ~ CS g "([^A-Za-z0-9_(]|$)") {
              queue[++n] = g
            }
          }
        }
      }
    }
  ' "$@"
}

# The files a script sources, resolved one level. A callee defined in a
# sourced helper is the same removal written one file over, and a walk that
# stops at the file refuses a script that is right. Each path is read after
# the `$VAR/` or `$(...)/` that opens it -- the shape every source line in
# this population writes -- and tried beside the script, from the scan root,
# and under scripts/lib/. A path none of the three resolves is printed as the
# line writes it, with its line number, because a boundary the walk cannot
# cross has to be named in the verdict rather than left as a silent refusal,
# and a fragment of the operand names nothing.
#
# One call per script, and not one call over the whole subset with the file
# name in each record. The subset is 14 scripts and the single-awk shape still
# forks one awk, so it removes 13 forks; over ten interleaved runs those were
# worth 0.066 s of a 0.978 s walk (0.95-1.00 s against 0.88-0.98 s). The bar a
# rewrite of this shape has to clear is 0.2 s, and the measured cut sits inside
# the walk's own run-to-run spread of 0.074 s, so it is not a number a decision
# can rest on. The bucketing the shape needs in the shell costs more to read
# than the number is worth.
temp_trap_sources() {
  local script="$1" here at path written base cand got
  # every path this walk is handed carries a directory; a bare name would
  # resolve its candidates against the wrong root
  here=${script%/*}
  if [ "$here" = "$script" ]; then here="."; fi
  awk -v SQ="'" "$SCRIPT_CODE_AWK"'
    # the operand ends at the first blank outside every quote and outside the
    # substitution that opens it, never at the first blank: `source
    # "$(dirname "$0")/lib/x.sh"` -- the shape every source line in this
    # population writes -- carries two blanks inside its own `$( )`, and a
    # quoted path may carry one of its own, which is what the wire below
    # already assumes. An operand cut at either is a fragment that resolves to
    # nothing and names nothing a reader can act on
    function operand(s,   i, n, c, d, q, out) {
      n = length(s); d = 0; q = ""
      for (i = 1; i <= n; i++) {
        c = substr(s, i, 1)
        if (q != "") {
          if (c == q) { q = "" }
          out = out c
          continue
        }
        if (c == SQ || c == "\"") { q = c; out = out c; continue }
        if (c == "$" && substr(s, i + 1, 1) ~ /[({]/) {
          d += 1; i += 1; out = out "$" substr(s, i, 1); continue
        }
        if ((c == ")" || c == "}") && d > 0) { d -= 1; out = out c; continue }
        if (c ~ /[[:space:]]/ && d == 0) { break }
        out = out c
      }
      return out
    }
    {
      script_code_scan($0)
      if (CODE !~ /^[[:space:]]*(\.|source)[[:space:]]/) { next }
      t = CODE
      sub(/^[[:space:]]*(\.|source)[[:space:]]+/, "", t)
      op = operand(t)
      t = op
      gsub(/["]/, "", t)
      sub(/^[$][({][^)}]*[)}]\//, "", t)
      sub(/^[$][A-Za-z_][A-Za-z0-9_]*\//, "", t)
      # the operand as the line writes it travels beside the path the walk
      # tried, because the verdict names one and the resolution reads the
      # other. Three records rather than one line and a separator byte: a
      # script line can carry any byte a separator could be, and an operand
      # holding that byte is then cut where the script wrote it and the
      # verdict names a path the walk never tried. A newline cannot occur
      # inside a record awk read as a line
      if (t != "") { printf "%d\n%s\n%s\n", FNR, t, op }
    }
  ' "$script" | while IFS= read -r at && IFS= read -r path &&
    IFS= read -r written; do
    base=${path##*/}
    got=""
    for cand in "$here/$path" "$path" "scripts/lib/$base"; do
      if [ -f "$cand" ]; then
        got="$cand"
        break
      fi
    done
    if [ -n "$got" ]; then
      printf '%s\n' "$got"
    else
      printf '?%s:%s: %s\n' "$script" "$at" "$written"
    fi
  done
}

# The names an armed handler actually removes: every `$NAME` on a line that
# runs `rm`, every name handed to a function whose own body removes -- the
# path is the argument at the call and the `rm` one level down reads it as
# `$1` -- plus the list a removed loop variable was bound from, which is how
# every array-of-roots cleanup in this population is written (`for root in
# "${ROOTS[@]}"; do rm -rf "$root"; done` removes ROOTS by way of root).
# The ceiling on the call: a callee that takes a path and removes a different
# one still pairs the names at the call.
temp_trap_removals() {
  awk -v SQ="'" -v CS="$SCRIPT_COMMAND_START" '
    # the line with the runs the shell never expands blanked out. A $NAME
    # inside a single-quoted or an ANSI-C run is literal text: a handler
    # removing a path written that way removes a path whose own name is $X and
    # the temp root leaks, while the walk reads the name and pairs. The
    # double-quoted runs stay, since "$X" is the shape a removal is written in.
    # A backslash-escaped $ is the same leak in the quoting the single quotes
    # do not reach: `rm -rf "\$X"` and `rm -rf \$X` in a handler body each
    # remove a path literally called $X, so the pair is blanked wherever the
    # run it sits in is not re-parsed
    function expanded(line,   i, n, c, q, ansi, out) {
      n = length(line); q = ""; ansi = 0; out = ""
      for (i = 1; i <= n; i++) {
        c = substr(line, i, 1)
        if (q == SQ) {
          # an ANSI-C run keeps its own escapes, so an escaped quote inside one
          # is a literal quote and does not end the run
          if (ansi && c == "\\") { out = out "  "; i++; continue }
          if (c == SQ) { q = ""; ansi = 0 }
          out = out " "
          continue
        }
        if (q == "\"") {
          if (c == "\\") {
            if (substr(line, i + 1, 1) == "$") { out = out "  "; i++; continue }
            out = out c substr(line, i + 1, 1); i++; continue
          }
          if (c == "\"") { q = "" }
          out = out c
          continue
        }
        if (c == "\\" && substr(line, i + 1, 1) == "$") { out = out "  "; i++; continue }
        if (c == SQ) {
          ansi = (i > 1 && substr(line, i - 1, 1) == "$")
          q = SQ; out = out " "
          continue
        }
        if (c == "\"") { q = "\"" }
        out = out c
      }
      return out
    }
    function names_on(line,   s, n, out) {
      s = expanded(line); out = ""
      while (match(s, /[$][{]?[A-Za-z_][A-Za-z0-9_]*/)) {
        n = substr(s, RSTART, RLENGTH)
        sub(/^[$][{]?/, "", n)
        out = out n " "
        s = substr(s, RSTART + RLENGTH)
      }
      return out
    }
    # the name a removal reaches: the one whose own expansion is the operand.
    # Quote removal comes first, so that "$X"/sub and "$X/sub" are the one
    # path they are, and a trailing slash is allowed because rm -rf "$X"/
    # removes the root. A name read out of the middle of an operand pairs the
    # leak with a removal that never touched the root: rm -rf "pre$X",
    # rm -rf "$X.bak", rm -rf "$X/sub" and rm -rf "\\$X" each delete
    # something the root still holds, and the last of them stranded a temp
    # directory under a walk answering ok.
    function removed_names(line,   i, n, w, part, out, nm) {
      n = split(expanded(line), part, "[[:space:]]+")
      out = ""
      for (i = 1; i <= n; i++) {
        w = part[i]
        gsub(/"/, "", w)
        # the operators a word carries when a handler is written on one line:
        # rm -f "$RAW"; }  is the operand and the list separator behind it
        sub(/[;&)]+$/, "", w)
        sub(/\/+$/, "", w)
        if (w ~ /^[$][{][A-Za-z_][A-Za-z0-9_]*[}]$/) {
          out = out substr(w, 3, length(w) - 3) " "
          continue
        }
        # the four parameter-expansion forms whose value is $X when set --
        # :?/:-/?/- -- unlike #, % and /, which read out a different string
        if (w ~ /^[$][{][A-Za-z_][A-Za-z0-9_]*(:[?-]|[?-])[^}]*[}]$/) {
          nm = substr(w, 3)
          sub(/(:[?-]|[?-]).*$/, "", nm)
          out = out nm " "
          continue
        }
        if (w ~ /^[$][A-Za-z_][A-Za-z0-9_]*$/) { out = out substr(w, 2) " " }
      }
      return out
    }
    # a name in command position, never the header that defines it: `f()` is
    # excluded by the paren that follows the word
    function calls(line, g) {
      return (line ~ CS g "([^A-Za-z0-9_(]|$)")
    }
    {
      # the two halves the harvest made of each line, arriving as two records
      # of their own: the code text, which is where a `$NAME` sits, and the
      # part outside every quote, which is where a call sits. A name inside a
      # string is a word the handler prints, and reading it as a call pairs a
      # root against a removal that never runs
      if (NR % 2 == 1) { CODE = $0; next }
      BARE = $0
      r += 1
      text[r] = CODE
      outside[r] = BARE
      if (match(CODE, /^[[:space:]]*[A-Za-z_][A-Za-z0-9_]*[[:space:]]*\(\)/)) {
        fn = substr(CODE, RSTART, RLENGTH)
        sub(/^[[:space:]]*/, "", fn)
        sub(/[[:space:]]*\(\)$/, "", fn)
        defined[fn] = 1
      }
      owner[r] = fn
      # written as a string rather than a regex literal: the slash needed a
      # backslash only to get past the delimiter of the literal, and a
      # backslash inside a bracket expression is undefined
      if (BARE ~ "(^|[^A-Za-z0-9_./-])rm(dir)?[[:space:]]") {
        isrm[r] = 1
        removes[fn] = 1
      }
      if (match(BARE, /(^|[[:space:]])for[[:space:]]+[A-Za-z_][A-Za-z0-9_]*[[:space:]]+in[[:space:]]/)) {
        v = substr(BARE, RSTART, RLENGTH)
        sub(/^[[:space:]]*for[[:space:]]+/, "", v)
        sub(/[[:space:]]+in[[:space:]]*$/, "", v)
        bound[v] = bound[v] names_on(CODE)
      }
    }
    END {
      # to a fixed point rather than one level, so the depth a removal is
      # written at is not a verdict
      changed = 1
      while (changed) {
        changed = 0
        for (i = 1; i <= r; i++) {
          if (removes[owner[i]]) { continue }
          for (g in defined) {
            if (removes[g] && calls(outside[i], g)) {
              removes[owner[i]] = 1
              changed = 1
            }
          }
        }
      }
      for (i = 1; i <= r; i++) {
        if (isrm[i]) {
          removed = removed removed_names(text[i])
          continue
        }
        for (g in defined) {
          if (removes[g] && calls(outside[i], g)) {
            removed = removed removed_names(text[i])
            break
          }
        }
      }
      for (v in bound) {
        if (index(" " removed, " " v " ") > 0) { removed = removed bound[v] }
      }
      count = split(removed, seen, " ")
      for (i = 1; i <= count; i++) { print seen[i] }
    }
  '
}

# A script that makes a temp file removes it under a trap. A straight-line
# `rm` covers the ordinary path and nothing else: the gates here run for
# seconds over a whole tree, and a Ctrl-C or a `set -e` abort inside that
# window strands the file under `${TMPDIR:-/tmp}`. Keyed on the script
# rather than on the statement, because the removal legitimately sits far
# from the `mktemp` -- what matters is that the handler names the variable
# the temp path went into, since a trap that clears the handler and a trap
# that kills a child both read as a pairing to a walk that only asks for the
# word.
check_temp_traps() {
  local fail=0 f names name removed paired reach unread line
  local read_with
  if ! read_script_population; then
    return 1
  fi
  # fed by a here-doc rather than a pipe, so the loop runs in this shell and
  # the verdict it sets is the one read below
  while IFS= read -r f; do
    [ -n "$f" ] || continue
    grep -q 'mktemp' "$f" || continue
    # every variable a trap could name for this file: the ones a `mktemp`
    # path went into, plus any list one of those is appended to. The append
    # is the relationship a removal written over an array of roots actually
    # has -- it names the array and its own loop variable, never the ROOT the
    # mktemp assigned -- and it is the reason the match below can stay
    # case-sensitive. Lowercasing instead pairs `tmp=$(mktemp)` to a
    # `TMP=/var/cache/keepme` that names an unrelated path, and the temp file
    # leaks with this walk silent. An assignment that is not an append is not
    # a holder: `other=$ROOT/sub` names a path inside the root, and removing
    # that one removes nothing of this one. The seed match is the quoting that
    # still expands and no wider: `$(mktemp)`, `"$(mktemp)"` and `"'$(mktemp)'"`
    # each make a file and each seeds, while `'$(mktemp)'` and `$'$(mktemp)'`
    # are literal text, make nothing, and are excluded by the leading quote.
    names=$(awk '
      FNR == NR {
        if (match($0, /[A-Za-z_][A-Za-z0-9_]*=(["][^$]?)?[$][(]mktemp/)) {
          n = substr($0, RSTART, RLENGTH)
          sub(/=.*/, "", n)
          seed[n] = 1
        }
        next
      }
      {
        h = ""
        if (match($0, /^[[:space:]]*[A-Za-z_][A-Za-z0-9_]*\+=/)) {
          h = substr($0, RSTART, RLENGTH)
          sub(/^[[:space:]]*/, "", h)
          sub(/\+=$/, "", h)
        } else if (match($0, /^[[:space:]]*[A-Za-z_][A-Za-z0-9_]*=/)) {
          h = substr($0, RSTART, RLENGTH)
          sub(/^[[:space:]]*/, "", h)
          sub(/=$/, "", h)
          if ($0 !~ "[$][{]?" h "[}]?([^A-Za-z0-9_]|$)") { h = "" }
        }
        if (h == "") { next }
        for (s in seed) {
          if ($0 ~ "[$][{]?" s "[}]?([^A-Za-z0-9_]|$)") { hold[h] = 1 }
        }
      }
      END {
        for (s in seed) { print s }
        for (h in hold) { print h }
      }
    ' "$f" "$f" | LC_ALL=C sort -u) || names=""
    # the names a removal in the handler reaches, never the names it mentions:
    # a handler that prints an accumulator (`log="$log made $ROOT"`, `echo
    # "$log"`) names the holder and removes nothing, and the temp root leaks
    # with this walk silent. Delimited by blanks, which no variable name holds.
    # split in the shell rather than through a sed and a grep: this loop runs
    # once per script that makes a temp file, and two more processes each
    # time cost more than the walk they sort
    reach=$(temp_trap_sources "$f")
    unread=""
    # the resolved paths travel as array elements and not as a blank-separated
    # word list: a quoted source operand may hold a blank, so the path it
    # resolves to may hold one, and a split list hands awk half a path and
    # takes a fatal on it -- the whole gate then stops on a tree that is right
    read_with=()
    while IFS= read -r line; do
      case "$line" in
        ('') ;;
        ('?'*) unread="${unread:+$unread; }${line#\?}" ;;
        (*) read_with+=("$line") ;;
      esac
    done <<EOF
$reach
EOF
    removed=" $(temp_trap_handlers "$f" ${read_with[@]+"${read_with[@]}"} \
      | temp_trap_removals | tr '\n' ' ')"
    paired=0
    # a file whose every mktemp goes somewhere no removal reaches leaves this
    # loop unrun and is reported
    for name in $names; do
      case "$removed" in
        *" $name "*)
          paired=1
          break
          ;;
      esac
    done
    if [ "$paired" -eq 0 ]; then
      if [ -n "$unread" ]; then
        echo "$f: makes a temp file with no EXIT trap removing it, and the" \
          "walk could not read what it sources at $unread"
      else
        echo "$f: makes a temp file with no EXIT trap removing it"
      fi
      fail=1
    fi
  done <<EOF
$SCRIPT_POPULATION
EOF
  if [ "$fail" -eq 0 ]; then
    return 0
  fi
  echo "STYLE FAIL: a temp file with no trap to remove it"
  echo "  A straight-line rm covers the ordinary path alone: a signal or a"
  echo "  set -e abort inside the window leaves the file behind. Remove it"
  echo "  under trap ... EXIT beside the mktemp, in a handler that removes the"
  echo "  variable the path went into."
  return 1
}

# A `mktemp` names the directory it writes in.
#
# With no template it answers under `$TMPDIR`, which is `/tmp` wherever
# nothing set that -- a small tmpfs shared with every job running beside
# this one, and a name that says nothing about which script made it.
# `scripts/lib/scratch.sh` holds the root this population writes in, and a
# template under it is what puts a file there. Any other root a template
# names is still a root, and one thing the walk refuses is the call that
# names none.
#
# The other is a template rooted at `$TMPDIR` itself, which is that same
# tmpfs written longhand. `scripts/acceptance/` is the one place it stands:
# what a run of those legs makes is a session root a unix socket path is
# measured from, and the length a platform allows such a path is the whole
# reason that root is chosen by hand. Outside that directory the spelling
# buys nothing and lands on the shared tmpfs, so it is named here.
#
# Read off the line outside its single quotes, because the case files plant
# whole scripts through `printf '...'` and a spelling written there is a
# fixture rather than a call this tree makes. A here-doc body -- where the
# rest of those fixtures live -- is already invisible to the reader this
# shares with the trap walk. Lines the shell joins on a trailing backslash
# are joined here too: a call read only as far as the backslash is handed
# its options and no operand, which reads as a root the next line names.
check_temp_roots() {
  local fail=0 rootless=0 shared=0 f found exempt line
  if ! read_script_population; then
    return 1
  fi
  while IFS= read -r f; do
    [ -n "$f" ] || continue
    grep -q 'mktemp' "$f" || continue
    exempt=0
    case "$f" in (scripts/acceptance/*) exempt=1 ;; esac
    found=$(awk -v SQ="'" -v exempt="$exempt" "$SCRIPT_CODE_AWK"'
      # the text outside every single-quoted run, which is the text the
      # shell runs. A run left open at the end of the line takes the rest
      # of that line with it, and the caller says whether this line began
      # inside one
      function unquoted(s,   i, n, c, k, out) {
        n = length(s); i = 1; out = ""
        while (i <= n) {
          c = substr(s, i, 1)
          if (c == SQ) {
            k = index(substr(s, i + 1), SQ)
            if (k == 0) { return out }
            i = i + k + 1
            continue
          }
          out = out c
          i++
        }
        return out
      }
      # whether the line ends in a backslash the shell reads as a join. An
      # even run is that many escaped backslashes and joins nothing
      function continued(s,   i, c) {
        c = 0
        for (i = length(s); i >= 1 && substr(s, i, 1) == "\\"; i--) { c++ }
        return c % 2
      }
      # what the call at `at` is handed once its option words are off. The
      # option words go first, short and long, so `-d` and `--directory`
      # both read as the spelling that names nowhere, while the DIR after a
      # `-p` is left where it stands: it is the root the rule asks for
      function operand(s, at,   rest) {
        rest = substr(s, at + 6)
        while (rest ~ /^[[:space:]]+--?[A-Za-z]/) {
          sub(/^[[:space:]]+--?[A-Za-z][A-Za-z-]*/, "", rest)
        }
        sub(/^[[:space:]]+/, "", rest)
        if (substr(rest, 1, 1) ~ /[);<>&|`]/) { return "" }
        return rest
      }
      # whether that operand is rooted at TMPDIR. An opening double quote
      # is not part of the root, and a single one is already gone
      function under_tmpdir(op) {
        if (substr(op, 1, 1) == "\"") { op = substr(op, 2) }
        return substr(op, 1, 8) == "${TMPDIR"
      }
      # every call on one joined line, reported against the line the join
      # began on
      function grade(text, at_line,   pos, k, at, op) {
        pos = 0
        while (1) {
          k = index(substr(text, pos + 1), "mktemp")
          if (k == 0) { return }
          at = pos + k
          pos = at + 5
          if (at > 1 && substr(text, at - 1, 1) ~ /[A-Za-z0-9_-]/) { continue }
          if (substr(text, at + 6, 1) ~ /[A-Za-z0-9_]/) { continue }
          op = operand(text, at)
          if (op == "") { print at_line ":none"; continue }
          if (!exempt && under_tmpdir(op)) { print at_line ":shared" }
        }
      }
      FNR == 1 { PEND = ""; START = 0 }
      {
        opened = (script_code_top() == SQ)
        script_code_scan($0)
        if (CODE == "") { next }
        line = unquoted(opened ? SQ CODE : CODE)
        if (PEND == "") { START = FNR } else { line = PEND line }
        if (continued(line)) {
          PEND = substr(line, 1, length(line) - 1)
          next
        }
        PEND = ""
        grade(line, START)
      }
      # a file whose last line is continued leaves the call ungraded
      # otherwise, which is the direction that passes a defect in silence
      END { if (PEND != "") { grade(PEND, START) } }
    ' "$f")
    if [ -n "$found" ]; then
      while IFS= read -r line; do
        [ -n "$line" ] || continue
        case "$line" in
          (*:shared)
            echo "$f:${line%:shared}: roots a temp file at TMPDIR, which is the shared tmpfs"
            shared=1
            ;;
          (*)
            echo "$f:${line%:none}: makes a temp file with no template saying where it goes"
            rootless=1
            ;;
        esac
      done <<EOF
$found
EOF
      fail=1
    fi
  done <<EOF
$SCRIPT_POPULATION
EOF
  if [ "$fail" -eq 0 ]; then
    return 0
  fi
  if [ "$rootless" -eq 1 ]; then
    echo "STYLE FAIL: a temp file with no root named for it"
    echo "  A bare mktemp writes under TMPDIR, which is /tmp here: a small"
    echo "  tmpfs every parallel job shares, under a name saying nothing about"
    echo "  which script made it. Source scripts/lib/scratch.sh and hand the"
    echo "  call a template under \$(scratch_root)."
  fi
  if [ "$shared" -eq 1 ]; then
    echo "STYLE FAIL: a temp root on the shared tmpfs"
    echo "  A template rooted at \${TMPDIR:-/tmp} is that same tmpfs under a"
    echo "  longer spelling. Only scripts/acceptance/ names it, where the"
    echo "  session root a unix socket path is measured from has to be chosen"
    echo "  by hand. Source scripts/lib/scratch.sh and root the template at"
    echo "  \$(scratch_root)."
  fi
  return 1
}

check_written_programs() {
  local expected actual
  expected=$(printf '%s\n' "$WRITTEN_PROGRAM_SITES" | awk 'NF { print $1, $2 }' | LC_ALL=C sort)
  actual=$(grep -r --include='*.rs' '0o755' crates 2>/dev/null \
    | sed 's/:.*//' | LC_ALL=C sort | uniq -c | awk '{ print $2, $1 }' | LC_ALL=C sort) || actual=""
  if [ "$expected" = "$actual" ]; then
    return 0
  fi
  printf 'pinned:\n%s\nfound:\n%s\n' "$expected" "$actual"
  echo "STYLE FAIL: a source makes a file executable outside the pinned set"
  echo "  A program a test runs is a committed fixture under scripts/test-fixtures/,"
  echo "  because a sibling test's fork inside the write's descriptor window makes"
  echo "  the exec fail with ETXTBSY once in thousands of runs, naming nothing. If"
  echo "  this site is genuinely not that, move the pin in this file and say why."
  return 1
}

# A measurement figure written into a Rust doc comment is a reading nothing
# re-takes. The gate re-records the bench cells and the drift check grades
# every figure the three published pages quote against the seat it resolves
# to -- and reaches no doc comment, so a startup chunk's doc went on stating
# milliseconds from a round of measurement two branches old, with no reader
# able to tell it from a current one.
#
# So a `///` or `//!` line that states a reading fails unless it names a cell
# the drift check knows, which is the one place a figure can be looked up
# again. The vocabulary of cells is the drift check's own
# (`check-budget-drift.sh --cell-ids`): a second reader of budgets.toml would
# be a second answer to what a cell is, and one of the two would go stale the
# next time that file's shape moved.
#
# What counts as a reading, and the two limits, both stated:
#
#   * a number with a fractional digit, in ms, us, ns, s, a percentage or a
#     multiplier. A value someone chose is round -- the tree's own throttles
#     and timeouts read 20ms, 150 ms, 200 ms -- and a value someone read off
#     an instrument is not.
#   * any number in those units in a sentence that states an observation,
#     which is what reaches a reading rounded to a whole unit ("measured on
#     dev-linux ... R at 2990us", "spends 98 ms of it").
#
# The second half is what the first cannot do: a reading is as free to land on
# a whole unit as on a fraction, and the word "measured" is not how this tree
# writes most of them. So the vocabulary is the stems the regex below actually
# spells -- measure, observ, record, spen[dt], cost, took, tak(e[sn]|ing),
# walk(ed|s), clocked, timed, appear, pays, paid, land(s|ed), need(ed|s), runs
# and ran -- and the noun "reading", and a whole-unit figure standing in a
# sentence carrying one of them is graded exactly as a fractional figure is.
# `98 ms` in a startup chunk's doc passed for a round number a writer had
# chosen while the sentence around it said what the parse spent, and `pays
# ~13s`, `lands ~250us later` and `taking 83 ms` each shipped a reading past a
# vocabulary keyed on the verbs that were already in it.
#
# Every stem but `ran` is read as a bare substring, on purpose: `runs`
# inside `reruns`, `lands` inside `islands`, `taking` inside `undertaking`
# and `pays` inside `repays` all open a sentence the walk then grades, and
# that is the failure direction this check accepts. A substring false
# positive costs the author a reword (or an escape word already on the
# list); a stem narrowed to avoid one risks the opposite, a reading this
# tree writes that the check reads past, which is the failure this vocabulary
# is not allowed to make. `ran` alone takes a non-letter on each side,
# because bare `ran` sits inside `range`, `transient` and `guarantee` often
# enough that leaving it unbounded would grade three common words as
# openers.
#
# A figure carries its sign and a spread is written as a range, so a token
# shape reading neither passed ten measured figures: `+1.23%`, `+/-20%`,
# `0.62ms..92.5ms` and `8-10ms` all stood on the tree. `spread` below turns
# the three separators into blanks before the line is tokenised.
#
# A line naming a bar, a budget, a bound, a band, a tolerance, a deadline, a
# throttle, a debounce, a ceiling, a cap, a tier, a pace, or a figure the
# code derives -- in any of those words' own inflections, since the tree
# writes `throttled` and `capped` as readily as the nouns, and the stems
# that change spelling (`capped`, `tiered`, `bounded`) need `ed` and `ped`
# as well as `s` and `d` -- is refused as a reading at all, on the drift
# check's own grounds: each of those is a number the tree chose or computes
# from one it chose, and can be read back off the constant that holds it, so
# a re-record moves none of them. `derive` is what the other twelve could
# not say: `2 x` trials and the `31s` five doubled waits come to are
# arithmetic the code performs, and the only way to keep either was to drop
# the verb that made it a sentence. That is the one escape a writer has, and
# it is the same escape bench.md already grants the ledger. It exempts the
# figures on its own line and never the sentence the words on it opened, and
# the cell-id escape works the same way: skipping the line outright left
# "Deliberately not a latency bar. Measured pre-attach windows span roughly"
# opening no sentence, and the milliseconds wrapped below it went ungraded.
check_doc_figures() {
  local ids found rc
  ids="$(bash "$SCRIPT_DIR/check-budget-drift.sh" --cell-ids "$PWD" | tr '\n' ' ')" || ids=""
  # Fail closed: a vocabulary this walk could not read grades every figure as
  # unanchored, which reads as a tree full of findings, and an empty one
  # grades every figure as anchored, which reads as a clean tree. The second
  # is the dangerous direction and is the one refused here.
  case "$ids" in
    (*[!\ ]*) ;;
    (*)
      echo "STYLE FAIL: the doc-figure walk read no cell ids to grade against"
      echo "  Every figure in a doc comment would pass for want of a"
      echo "  vocabulary. Check scripts/check-budget-drift.sh --cell-ids."
      return 1
      ;;
  esac
  rc=0
  found=$(find crates -name '*.rs' -print0 | xargs -0 awk -v ids="$ids" '
    # A bracket expression here holds no backslash: POSIX leaves one
    # undefined and the three awks this tree runs under disagree in fact.
    # The closing bracket is written first, where it is literal, and the
    # opening one anywhere inside.
    function clean(t) { gsub(/[]`*~()>[,;:"]/, "", t); sub(/\.$/, "", t); return t }
    # A range or a band states two readings, and this tree writes the
    # separator four ways: `0.62ms..92.5ms`, `1..=5 ms`, `8-10ms` and
    # `+/-20%`. Each becomes a blank, except the hyphen, which becomes the
    # sign of the figure after it so that a written `-0.5ms` reads the same
    # way. The inclusive range takes its `=` with it: blanking the two dots
    # alone left `=5 ms`, which no token pattern reads.
    function spread(t) {
      gsub(/\.\.=?/, " ", t)
      gsub(/\+\/-/, " ", t)
      while (match(t, /[0-9]-[0-9]/)) {
        t = substr(t, 1, RSTART) " -" substr(t, RSTART + 2)
      }
      return t
    }
    # The finding names the token as the file spells it: `spread()` cuts a
    # range into two figures so each is graded, and a reader sent to `-10`
    # for source text `8-10ms` is left to work out which half the file
    # wrote. What it names is the first raw token whose own spread yields
    # the graded one, so a line repeating a figure resolves the same way
    # every run.
    function spelled(line, t,   k, r, j, m, u, i) {
      k = split(line, r, /[[:space:]]+/)
      for (j = 1; j <= k; j++) {
        m = split(spread(r[j]), u, /[[:space:]]+/)
        for (i = 1; i <= m; i++) {
          if (clean(u[i]) == t) { return clean(r[j]) }
        }
      }
      return t
    }
    BEGIN { names = split(ids, id, " ") }
    FNR == 1 { fenced = 0; reading = 0; held = "" }
    {
      body = $0
      sub(/^[[:space:]]*/, "", body)
      if (body !~ /^(\/\/\/|\/\/!)/) { reading = 0; fenced = 0; held = ""; next }
      sub(/^(\/\/\/|\/\/!)/, "", body)
      # A fenced block inside a doc comment is a sample of what something
      # prints or parses, quoted so a reader recognises the shape. Its
      # figures are the shape and not a claim, and rewriting them to prose
      # would delete the sample. Unlike the two escapes below, it is skipped
      # for the sentence state as well: sample text is not prose, so a
      # reading word inside one opens nothing and a `.` inside one closes
      # nothing, which leaves a sentence interrupted by a fence still open
      # over the prose after it.
      if (body ~ /^[[:space:]]*```/) { fenced = !fenced; next }
      if (fenced) { next }
      # Lowercased for both word tests: a reading word opens a sentence as
      # often as it stands inside one, and a case-sensitive read let every
      # capitalised `Measured` and `Observed` through.
      # A blank on each side so that `ran`, the one stem that is a word
      # inside other words (`range`, `transient`, `guarantee`), can be
      # written with a non-letter on each side without the pattern needing
      # `^` or `$`: an anchor in the middle of an alternation is not
      # portable across the three awks this tree runs under. The walk
      # supplies both blanks, because the line supplies neither: a doc
      # line written `///ran 83 ms` is one `cargo fmt` leaves alone and
      # nothing else in the gate asks for the space after the marker, and
      # the reading in it went ungraded while the front boundary rested on
      # that space.
      folded = " " tolower(body) " "
      # The reading state runs to the end of the sentence that opened it,
      # and not to the end of the line: a figure rustfmt wrapped onto the
      # line after the word that introduced it escapes a per-line test,
      # while a block-wide state grades every constant a block explains in
      # words as a reading.
      #
      # It is computed before either escape and updated below whether or not
      # one fired, because an escape exempts the figures on its own line and
      # never the sentence those words opened: skipping the line outright
      # left a reading word beside a bound to open no sentence at all, and
      # the figure wrapped onto the next line went ungraded.
      #
      # The word is as free to fall after the figure as before it, because
      # rustfmt wraps a sentence wherever the width runs out: a reading
      # state that only runs forward graded `moved 6x cross-boot` as a
      # constant for want of a `measured` that sat on the next line. An
      # integer the walk cannot grade where it stands is therefore held
      # until the sentence it stands in closes, and a reading word reached
      # first reports it -- unless a `.` stands between the two, which is
      # the same sentence boundary the forward state reads.
      had_word = (folded ~ /measure|observ|record|spen[dt]|cost|took|tak(e[sn]|ing)|walk(ed|s)|clocked|timed|reading|appear|pays|paid|land(s|ed)|need(ed|s)|runs|[^a-z]ran[^a-z]/)
      if (had_word) {
        head = folded
        sub(/(measure|observ|record|spen[dt]|cost|took|tak(e[sn]|ing)|walk(ed|s)|clocked|timed|reading|appear|pays|paid|land(s|ed)|need(ed|s)|runs|[^a-z]ran[^a-z]).*$/, "", head)
        if (head ~ /\.([[:space:]]|$)/) { held = "" }
        if (held != "") { printf "%s", held; held = "" }
        reading = 1
      }
      escaped = (folded ~ /(^|[^a-z])(bar|budget|bound|band|tolerance|deadline|throttle|debounce|ceiling|cap|tier|pace|derive)(s|d|ed|ped)?([^a-z]|$)/)
      anchored = 0
      for (j = 1; j <= names; j++) {
        if (index(body, id[j]) > 0) { anchored = 1 }
      }
      n = 0
      held_here = 0
      if (!escaped && !anchored) { n = split(spread(body), w, /[[:space:]]+/) }
      for (i = 1; i <= n; i++) {
        tok = clean(w[i])
        nxt = clean(w[i + 1])
        num = ""
        if (tok ~ /^[-+]?[0-9]+(\.[0-9]+)?(ms|us|ns|s|%|x)$/) {
          num = tok
          sub(/(ms|us|ns|s|%|x)$/, "", num)
        } else if (tok ~ /^[-+]?[0-9]+(\.[0-9]+)?$/ &&
                   (nxt == "ms" || nxt == "us" || nxt == "ns" || nxt == "s" ||
                    nxt == "%" || nxt == "percent" || nxt == "x")) {
          num = tok
        }
        if (num == "") { continue }
        if (num ~ /\./ || reading) {
          printf "%s:%d: %s\n", FILENAME, FNR, spelled(body, tok)
          break
        }
        if (held == "") {
          held = sprintf("%s:%d: %s\n", FILENAME, FNR, spelled(body, tok))
          held_here = 1
          held_tail = ""
          for (k = i + 1; k <= n; k++) { held_tail = held_tail " " w[k] }
        }
      }
      if (reading) {
        tail = folded
        if (had_word) { sub(/^.*(measure|observ|record|spen[dt]|cost|took|tak(e[sn]|ing)|walk(ed|s)|clocked|timed|reading|appear|pays|paid|land(s|ed)|need(ed|s)|runs|[^a-z]ran[^a-z])/, "", tail) }
        if (tail ~ /\.([[:space:]]|$)/) { reading = 0 }
      }
      # A held figure lives as long as its own sentence: what closes it is a
      # `.` after the figure on the line it stands on, or anywhere on a line
      # the sentence runs onto.
      if (held != "") {
        rest = held_here ? held_tail : folded
        if (rest ~ /\.([[:space:]]|$)/) { held = "" }
      }
    }
  ') || rc=$?
  if [ "$rc" -ne 0 ]; then
    echo "STYLE FAIL: the doc-figure walk could not be evaluated (find or awk failed)"
    return 1
  fi
  if [ -z "$found" ]; then
    return 0
  fi
  printf '%s\n' "$found"
  echo "STYLE FAIL: a doc comment states a measurement nothing re-takes"
  echo "  A figure in a doc comment is graded by nothing and re-recorded by"
  echo "  nobody. State the mechanism in words and leave the reading in the"
  echo "  commit that took it, or name the cell it belongs to and quote it in"
  echo "  docs/benchmarking.md beside that cell."
  return 1
}

# rustfmt does not wrap comments (`wrap_comments` defaults to false), so a
# `///` or `//!` line rustfmt leaves untouched can run past the width every
# code line in the same file is held to, and nothing in the toolchain says
# so. The limit is the crate's own: `max_width` out of a crate-level
# `rustfmt.toml`/`.rustfmt.toml` (`crates/<name>/`) where one exists, else
# the workspace root's, else 100 (rustfmt's own default) -- read once per
# crate rather than assumed, because a crate that ever sets one of its own
# is one edit away from a doc-comment gate grading against the wrong number.
#
# A `///`/`//!` line is measured whole, the way rustfmt would measure a code
# line: the marker and its leading indentation count. Three shapes cannot be
# re-wrapped and are exempt: a table row, which is one row of the table it
# stands in; a fenced block, which is a sample of what something prints or
# parses rather than prose (report.rs's paired-line format is quoted
# verbatim; splitting it would document a line break the program never
# prints), read the same way `check_doc_figures` reads one; and a single
# run that could not fit even alone on its own line -- a URL, or a
# markdown link whose target is a long path -- which has nowhere to break,
# so it is taken out before the rest of the line is measured. The
# threshold a run is held to is the limit less its own line's `///`/`//!`
# marker and indentation, never the bare limit `check_prose_width` uses
# for a markdown page: a doc-comment line pays for the marker before a
# single word of prose, where a markdown line does not. Everything else
# that reddens is prose with a space in it and re-wraps like any other.
read_rustfmt_max_width() {
  local dir="$1" default="$2" fmt_file limit=""
  for fmt_file in "$dir/rustfmt.toml" "$dir/.rustfmt.toml"; do
    if [ -f "$fmt_file" ]; then
      limit=$(awk -F= '
        /^[[:space:]]*max_width[[:space:]]*=/ {
          v = $2
          gsub(/[[:space:]]/, "", v)
          print v
          exit
        }
      ' "$fmt_file")
      break
    fi
  done
  case "$limit" in
    (*[!0-9]*|'') limit="$default" ;;
  esac
  printf '%s\n' "$limit"
}

check_doc_width() {
  local root_limit crate_dir limit files found rc fail any
  root_limit=$(read_rustfmt_max_width . 100)
  fail=0
  any=0
  for crate_dir in crates/*/; do
    crate_dir="${crate_dir%/}"
    [ -d "$crate_dir" ] || continue
    files=$(find "$crate_dir" -name '*.rs' | LC_ALL=C sort) || files=""
    [ -z "$files" ] && continue
    any=1
    limit=$(read_rustfmt_max_width "$crate_dir" "$root_limit")
    rc=0
    found=$(printf '%s\n' "$files" | LC_ALL=C xargs awk -v limit="$limit" "$AWK_COLS$AWK_TABLE_ROW"'
      FNR == 1 { fenced = 0 }
      {
        body = $0
        sub(/^[[:space:]]*/, "", body)
        if (body !~ /^(\/\/\/|\/\/!)/) { fenced = 0; next }
        rest = body
        sub(/^(\/\/\/|\/\/!)[[:space:]]*/, "", rest)
        sub(/[[:space:]]*$/, "", rest)
        if (rest ~ /^```/) { fenced = !fenced; next }
        if (fenced) { next }
        if (cols($0) <= limit) { next }
        if (is_table_row(rest)) { next }
        # a run that could not fit even alone on its own line -- the
        # marker and its indentation plus the run itself already past the
        # limit -- cannot wrap by moving words around it, so it is taken
        # out before the rest of the line is measured. The threshold is
        # against the limit less this line'"'"'s own prefix, never the bare
        # limit: unlike a markdown page, every doc-comment line pays for
        # `///`/`//!` and its indentation before a single word of prose
        match($0, /^[[:space:]]*(\/\/\/|\/\/!)[[:space:]]?/)
        plen = RLENGTH
        rest_cols = cols($0)
        n = split($0, w, /[[:space:]]+/)
        for (i = 1; i <= n; i++) {
          if (plen + cols(w[i]) > limit) { rest_cols -= cols(w[i]) }
        }
        if (rest_cols <= limit) { next }
        printf "%s:%d: %d characters (limit %d)\n", FILENAME, FNR, cols($0), limit
      }
    ') || rc=$?
    if [ "$rc" -ne 0 ]; then
      echo "STYLE FAIL: the doc-width walk could not be evaluated over $crate_dir (find or awk failed)"
      return 1
    fi
    if [ -n "$found" ]; then
      fail=1
      printf '%s\n' "$found"
    fi
  done
  if [ "$any" -eq 0 ]; then
    echo "STYLE FAIL: no crate sources found under crates/; the doc-width walk did not run"
    echo "  A walk handed an empty list reports nothing and reads like a tree"
    echo "  whose doc comments are inside the limit."
    return 1
  fi
  if [ "$fail" -eq 0 ]; then
    return 0
  fi
  echo "STYLE FAIL: a doc comment line runs past its crate's rustfmt max_width"
  echo "  rustfmt does not wrap \`///\`/\`//!\` lines, so this stays wide until"
  echo "  someone re-wraps it by hand. A table row, a fenced sample, and a"
  echo "  run that cannot fit even alone on its own line are already exempt;"
  echo "  everything else reported here is prose with a space in it."
  return 1
}

# A doc line wraps at 80 characters, which is the width the pages are
# written to. A line that cannot wrap is exempt and says which shape it is:
# a fenced block is a sample of a file rather than prose, a table row is one
# row, a heading is one line by construction, and a line that is one link or
# one URL has nowhere to break. A badge is the nested spelling of the link
# exemption: a link wrapping an image, whose alt text carries the only
# spaces on the line and cannot be broken without breaking the badge.
# A whitespace-free run longer than the limit (a captured declaration, a
# path) exempts itself and not the prose beside it: the line is measured
# with every such run taken out, so a page still wraps the sentence it
# writes around one long path. A program that stamps a block wraps its own
# output at this limit, which is why no marker takes a block out of the
# walk. Counted in characters, by the shared measure above.
PROSE_WIDTH=80
# A line this short with its paragraph still running is a sentence appended to
# a block nobody re-wrapped: the width walk grades the maximum alone and reads
# such a page as clean, and the shape arrives every time a page is edited by
# adding a sentence and re-wrapping only the tail that went over. 60 of the 80
# is the floor the pages already write to -- the walk over the three target
# directories reported 73 lines under it and none of them was a deliberate
# short line -- and a line that ends its paragraph is not graded at all, so a
# one-line paragraph and the last line of any other stay as they are.
PROSE_RAGGED=60
check_prose_width() {
  local pages graded wide ragged split merged unreadable rc
  pages=$(find "$@" -name '*.md' | LC_ALL=C sort)
  if [ -z "$pages" ]; then
    echo "STYLE FAIL: no markdown page found to grade for width"
    echo "  A width walk handed an empty list reports nothing and reads like"
    echo "  a tree whose prose is inside the limit."
    return 1
  fi
  # A page the walk cannot open is a red verdict naming it: awk reports its
  # own fatal on stderr and stops, and a walk whose status is discarded
  # reads exactly like a tree whose prose is inside the limit. A directory
  # named like a page passes -r and is one of those: awk warns on stderr
  # that it skipped it and exits 0, so the shape is refused here.
  unreadable=$(printf '%s\n' "$pages" | while IFS= read -r page; do
    { [ -f "$page" ] && [ -r "$page" ]; } ||
      printf '%s: the width walk cannot read it\n' "$page"
  done)
  if [ -n "$unreadable" ]; then
    printf '%s\n' "$unreadable"
    echo "STYLE FAIL: a page handed to the width walk cannot be read"
    return 1
  fi
  rc=0
  graded=$(printf '%s\n' "$pages" | LC_ALL=C xargs awk -v limit="$PROSE_WIDTH" \
    -v short="$PROSE_RAGGED" "$AWK_COLS$AWK_TABLE_ROW"'
    # the line a list item opens, which the ragged measure exempts and the
    # marker rule grades: written once because two rules reading the same
    # class out of two regexes drift apart at the first edit of either
    function opens_list(l) {
      return (l ~ /^[[:space:]]*([-*+]|[0-9]+[.)])[[:space:]]/)
    }
    # what a re-wrap may move a word onto or off: prose and the continuation
    # of a list item. A table row, a heading, a block quote, a rule and a
    # fence line each carry their own newline by construction, so neither the
    # short line nor the line after it is one of those
    function wraps(l) {
      if (l ~ /^[[:space:]]*$/) { return 0 }
      if (is_table_row(l)) { return 0 }
      if (l ~ /^[[:space:]]*[#>]/) { return 0 }
      # an html comment on its own line is markup, not prose: the generated
      # block in docs/surface-ownership.md sits under one, and a re-wrap that
      # pulled the paragraph up into the marker would be gone at the next run
      # of the test that writes that block
      if (l ~ /^[[:space:]]*<!--/) { return 0 }
      if (opens_list(l)) { return 0 }
      if (l ~ /^[[:space:]]*(---+|===+)[[:space:]]*$/) { return 0 }
      if (l ~ /^[[:space:]]*(```|~~~)/) { return 0 }
      return 1
    }
    # what a re-wrap would move onto the line above, which is a word unless it
    # opens an inline code span: a span is moved whole or not at all, and a
    # next word measured to the first blank inside one asks for the span to be
    # broken across the line instead
    function firstword(l,   t, w, c) {
      t = l; sub(/^[[:space:]]+/, "", t)
      w = ""
      while (t != "") {
        c = t; sub(/[[:space:]].*$/, "", c)
        w = (w == "") ? c : w " " c
        sub(/^[^[:space:]]+[[:space:]]*/, "", t)
        c = w
        # the span ends where its backticks balance, and the word it sits in
        # ends at the blank after that: the comma a span is followed by moves
        # with it, and a measure that stops at the closing backtick asks for a
        # line the re-wrap cannot write
        if (gsub(/`/, "", c) % 2 == 0) { break }
      }
      return w
    }
    # the run of backticks a code span opens on closes it at the same length
    # and at no other, so a span holding a lone backtick inside a pair is one
    # span rather than three. The state carries across lines: a span the line
    # leaves open has been broken by a wrap, and a test that reads its page by
    # a span goes red the next time the paragraph is touched. A span too long
    # to sit on a line of its own is left alone -- it has nowhere to go
    function span_step(l,   i, n, run, tick) {
      i = 1; n = length(l)
      while (i <= n) {
        if (substr(l, i, 1) != "`") {
          if (spanrun > 0) { spantext = spantext substr(l, i, 1) }
          i += 1
          continue
        }
        run = 0
        while (i + run <= n && substr(l, i + run, 1) == "`") { run += 1 }
        tick = substr(l, i, run)
        i += run
        if (spanrun == 0) { spanrun = run; spantext = tick; spanat = FNR; continue }
        spantext = spantext tick
        if (spanrun != run) { continue }
        if (spanat != FNR && cols(spantext) <= limit) {
          printf "span %s:%d: an inline code span runs on to line %d\n",
            FILENAME, spanat, FNR
        }
        spanrun = 0; spantext = ""
      }
      if (spanrun > 0) { spantext = spantext " " }
    }
    FNR == 1 { fenced = 0; held = ""; spanrun = 0; spantext = "" }
    # CommonMark closes a fence only with the character that opened it, and
    # with a run at least as long. One toggle for both spellings read a
    # sample containing the other as a close: the block ended early, the
    # closing fence opened a new one, and every line after it went ungraded.
    /^[[:space:]]*(```|~~~)/ {
      match($0, /`+|~+/)
      ch = substr($0, RSTART, 1)
      run = RLENGTH
      if (!fenced) { fenced = 1; fence_ch = ch; fence_run = run }
      else if (ch == fence_ch && run >= fence_run) { fenced = 0 }
      held = ""
      spanrun = 0; spantext = ""
      next
    }
    fenced { next }
    {
      # a code span ends at the paragraph it is written in: it cannot cross a
      # blank line and it cannot cross a fence. Carried past either, one stray
      # backtick pairs with the next real opening tick and every span below it
      # is read one tick out of phase -- a genuine split span then goes
      # unreported, which is the silent direction
      if ($0 ~ /^[[:space:]]*$/) { spanrun = 0; spantext = "" }
      span_step($0)
      # a list marker that follows the end of a sentence in prose is a bullet
      # a re-wrap pulled up onto the line above it: the item stops being an
      # item of its list, and nothing else in this walk can see it -- a list
      # opener is exempt from the ragged rule, a merged line is inside the
      # width, and a word-stream comparison reads the `-` either way. Anchored
      # on the sentence end because a bare marker mid-line is arithmetic
      # (`hi_vcol - lo_vcol + 1`) far more often than it is a bullet. The
      # line an item was merged into is as often the opener of the item above
      # as it is one of its continuations, so an opener is graded too -- what
      # is exempt from the ragged rule is not exempt from this
      if ((wraps($0) || opens_list($0)) &&
          $0 ~ /[.!?:]["*)]*[[:space:]]+([-*+]|[0-9]+[.)])[[:space:]]/) {
        printf "marker %s:%d: a list marker sits mid-line in prose\n", FILENAME, FNR
      }
      # the short line is graded on the line that follows it, and only where
      # the word that opens that line would have fitted: a paragraph whose
      # next word is a path longer than what is left is wrapped as tightly as
      # it can be, and reddening it asks for a line over the limit
      if (held != "" && wraps($0) && cols(held) + 1 + cols(firstword($0)) <= limit) {
        printf "ragged %s:%d: %d characters, and the paragraph runs on\n",
          FILENAME, at, cols(held)
      }
      held = ""
      if (wraps($0) && cols($0) < short) { held = $0; at = FNR }
    }
    /^[[:space:]]*#/ { next }
    is_table_row($0) { next }
    cols($0) <= limit { next }
    /^[[:space:]]*(<[^ >]+>|[^ ]+:\/\/[^ ]+)[.,]?[[:space:]]*$/ { next }
    /^[[:space:]]*!?\[[^]]*\](\([^)]*\))?[.,]?[[:space:]]*$/ { next }
    /^[[:space:]]*\[!?\[[^]]*\]\([^)]*\)\]\([^)]*\)[.,]?[[:space:]]*$/ { next }
    {
      # the run longer than the limit is what cannot wrap, so it is what is
      # exempt: the rest of the line is prose and is measured without it
      rest = cols($0)
      n = split($0, word, /[[:space:]]+/)
      for (i = 1; i <= n; i++) {
        if (cols(word[i]) > limit) { rest -= cols(word[i]) }
      }
      if (rest <= limit) { next }
      printf "wide %s:%d: %d characters\n", FILENAME, FNR, cols($0)
    }') || rc=$?
  if [ "$rc" -ne 0 ]; then
    printf '%s\n' "$graded"
    echo "STYLE FAIL: the width walk exited $rc instead of grading the pages"
    echo "  awk names the page it could not read on stderr above."
    return 1
  fi
  wide=$(printf '%s\n' "$graded" | sed -n 's/^wide //p')
  ragged=$(printf '%s\n' "$graded" | sed -n 's/^ragged //p')
  split=$(printf '%s\n' "$graded" | sed -n 's/^span //p')
  merged=$(printf '%s\n' "$graded" | sed -n 's/^marker //p')
  if [ -z "$wide" ] && [ -z "$ragged" ] && [ -z "$split" ] && [ -z "$merged" ]; then
    return 0
  fi
  if [ -n "$wide" ]; then
    printf '%s\n' "$wide"
    echo "STYLE FAIL: a doc line runs past $PROSE_WIDTH characters"
    echo "  Re-wrap the paragraph. A line that cannot wrap -- fenced, a table"
    echo "  row, a heading, or one link -- is already exempt, and a run longer"
    echo "  than the limit is taken out before the line is measured, so a line"
    echo "  reported here is prose with a space in it."
  fi
  if [ -n "$ragged" ]; then
    printf '%s\n' "$ragged"
    echo "STYLE FAIL: a doc line stops under $PROSE_RAGGED characters with its paragraph still running"
    echo "  Re-wrap the paragraph, not just the tail that went over the width:"
    echo "  a sentence added to a wrapped block leaves the seam behind it"
    echo "  short, and the width walk grades the maximum alone."
  fi
  if [ -n "$split" ]; then
    printf '%s\n' "$split"
    echo "STYLE FAIL: an inline code span is broken across a line"
    echo "  Move the whole span onto one line -- the line before it may end"
    echo "  short, which is what the ragged rule exempts. Tests read these"
    echo "  pages by the spans they are written as, so a span a wrap cut in"
    echo "  two is a page whose next edit reddens a test."
  fi
  if [ -n "$merged" ]; then
    printf '%s\n' "$merged"
    echo "STYLE FAIL: a list item was pulled into the prose above it"
    echo "  Put the marker back at the start of its own line and re-wrap"
    echo "  inside the item. A bullet spent to rejoin a code span or to"
    echo "  shorten a line is an item the list no longer has."
  fi
  return 1
}

# A page a person reads says what is true and what to do. A sentence that
# argues for the design, denies an alternative, or states the conditions
# that make a number fair is written for a reader who has doubted
# something, and `.claude/rules/docs.md` removes it. The spellings below
# are the visible half of that stance; the rule is the paragraph on that
# page, and this walk is the tripwire under it.
#
# Written as a list rather than into the awk program so that the next
# spelling is a line here. No backslash in any of them: the classes carry
# the word boundaries, because `\b` is a backspace to gawk and undefined to
# the others, and an apostrophe is spelled as its class for the reason
# `.claude/rules/shell.md` gives for a paren in a comment.
#
# The three lists reach awk through the environment. A `-v` assignment is
# lexed as a string literal, and the one true awk macOS ships refuses a
# newline inside one -- the stance walk exited 1 on every macOS run and
# graded no page at all. ENVIRON takes the bytes as they are, which also
# keeps a backslash written here from being read twice.
PROSE_CONTRAST='(, |; | -- )(not|never|rather than|instead of)([^[:alnum:]]|$)
(^|[^[:alnum:]])rather than([^[:alnum:]]|$)
(^|[^[:alnum:]])instead of([^[:alnum:]]|$)
(^|[^[:alnum:]])(not|never) (a|an|the|just|only|merely|simply) [^,;]+, (it|they|that|this|which) (is|are|was|were)([^[:alnum:]]|$)
(^|[^[:alnum:]])(not|never) [^,;]+ but[^[:alnum:]]
(isn|aren|wasn|doesn|didn)[^[:alnum:][:space:]]t (a|an|the)([^[:alnum:]]|$)'
# The tell words, whole and case-folded: each one is a sentence reaching
# for the reader who asked whether the page is telling the truth.
PROSE_TELLS='claim
claims
prove
proves
proven
honest
honestly
genuine
genuinely
deliberate
deliberately
fair
which is why
the reason it exists
so that nobody
on purpose'
# The conditions of fairness. A comparison that belongs on a page is a
# table with the other column beside the one for view; how the numbers
# were taken lives in docs/benchmarking.md.
PROSE_FAIRNESS='in the same run
on the same host
on the same machine
samples interleaved
paired against
under a real config'
# The words of the mechanism. README.md is read by a person deciding
# whether to try view, and each of these names how a feature is built
# where the page owes what the person sees; the pages under docs/ are
# where the mechanism is described.
PROSE_MECHANISM='round trip
round-trip
round trips
paint
painted
painting
repaint
osc 52
rpc
multigrid
ext_popupmenu
ext_cmdline
ext_messages
passthrough
capability tier
capability tiers
keystream
composited
cdp
chrome
surface
surfaces'
export PROSE_CONTRAST PROSE_TELLS PROSE_FAIRNESS PROSE_MECHANISM
# The program the walk runs, held in a variable so that no line of it
# is read inside an open command substitution: the population's
# heaviest carried-line count is this file, and the portability case
# that grades the reader's nesting state is a ratio over those lines.
PROSE_FRAMES_AWK='
    # what a backticked span holds is a sample of code or of a file, so it
    # is taken out before the line is read. A run of backticks closes on a
    # run of its own length, and a span the line leaves open takes the
    # rest of the line with it -- the split-span rule reports that
    function strip_spans(l,   i, n, out, run, open, ch) {
      out = ""; i = 1; n = length(l); open = 0
      while (i <= n) {
        ch = substr(l, i, 1)
        if (ch != "`") {
          if (!open) { out = out ch }
          i += 1
          continue
        }
        run = 0
        while (i + run <= n && substr(l, i + run, 1) == "`") { run += 1 }
        i += run
        if (!open) { open = run; continue }
        if (open == run) { open = 0; out = out " " }
      }
      return out
    }
    # the separator row carries no words, so it is the one row with nothing
    # to grade
    function is_separator_row(l,   t) {
      t = l; gsub(/[[:space:]]/, "", t)
      return (t ~ /^[|:-]+$/ && index(t, "-") > 0)
    }
    # what a shape is matched in, case-folded and with the two spellings a
    # reader sees as one word blanked. A blank of the same width and never
    # a shorter string, so that a position in the folded text is the same
    # position in the text it was folded from
    function fold(t,   lc) {
      lc = tolower(t)
      # the idiom, which is one word to a reader and a frame to a pattern
      gsub(/whether or not/, "whether       ", lc)
      # the transport denial .claude/rules/bench.md requires of the
      # speculated-echo paragraph, and nothing wider
      if (index(lc, "not a network") > 0 &&
          (index(lc, "local") > 0 || index(lc, "reading") > 0)) {
        gsub(/not a network/, "             ", lc)
      }
      return lc
    }
    # a hit is reported where it starts. Reading a pair of lines joined at
    # a seam, only a hit carrying text from both sides is new: everything
    # inside one line was graded when that line was read alone
    function spans(p, len, seam) {
      if (p == 0) { return 0 }
      if (seam == 0) { return 1 }
      return (p <= seam && p + len - 1 >= seam)
    }
    # a dash joins two clauses, so it is the joiner only where a clause
    # stands on each side of it: a cell holding a placeholder such as the
    # one a generated table writes for an empty column is a value
    function joins(s, p, len) {
      return (substr(s, 1, p - 1) ~ /[^[:space:]]/ &&
              substr(s, p + len) ~ /[^[:space:]]/)
    }
    function whole(w) {
      return "(^|[^[:alnum:]_])" w "([^[:alnum:]_]|$)"
    }
    function hit(shape, what, ln) {
      printf "%s %s:%d: %s\n", shape, FILENAME, ln, what
    }
    # the two dash joiners are read off the text as written and everything
    # else off the folded text, which is the same width
    function grade(text, low, ln, seam,   i, p) {
      p = index(text, " -- ")
      if (spans(p, 4, seam) && joins(text, p, 4)) {
        hit("joiner", "a dash joins two clauses", ln)
      }
      p = index(text, EMDASH)
      if (spans(p, length(EMDASH), seam) && joins(text, p, length(EMDASH))) {
        hit("joiner", "an em dash joins two clauses", ln)
      }
      # a semicolon carrying a denial is the joiner in its quietest
      # spelling: the clause after it exists to say the alternative a
      # reader never proposed is absent
      if (match(low, /; (no|nothing|none|nor)([^[:alnum:]]|$)/) &&
          spans(RSTART, RLENGTH, seam)) {
        hit("joiner", "a semicolon joins a denial", ln)
      }
      for (i = 1; i <= nc; i++) {
        if (C[i] != "" && match(low, C[i]) && spans(RSTART, RLENGTH, seam)) {
          hit("frame", "a contrast frame", ln); break
        }
      }
      for (i = 1; i <= nt; i++) {
        if (T[i] != "" && match(low, whole(T[i])) &&
            spans(RSTART, RLENGTH, seam)) {
          hit("tell", T[i], ln); break
        }
      }
      for (i = 1; i <= nf; i++) {
        if (F[i] == "" || !match(low, whole(F[i]))) { continue }
        if (spans(RSTART, RLENGTH, seam)) { hit("fairness", F[i], ln); break }
      }
      if (FILENAME !~ /(^|\/)README\.md$/) { return }
      for (i = 1; i <= nm; i++) {
        if (M[i] == "" || !match(low, whole(M[i]))) { continue }
        if (spans(RSTART, RLENGTH, seam)) { hit("mechanism", M[i], ln); break }
      }
    }
    BEGIN {
      nc = split(ENVIRON["PROSE_CONTRAST"], C, "\n")
      nt = split(ENVIRON["PROSE_TELLS"], T, "\n")
      nf = split(ENVIRON["PROSE_FAIRNESS"], F, "\n")
      nm = split(ENVIRON["PROSE_MECHANISM"], M, "\n")
      EMDASH = sprintf("%c%c%c", 226, 128, 148)
    }
    FNR == 1 { fenced = 0; prev_no = 0 }
    /^[[:space:]]*(```|~~~)/ {
      match($0, /`+|~+/)
      ch = substr($0, RSTART, 1)
      run = RLENGTH
      if (!fenced) { fenced = 1; fence_ch = ch; fence_run = run }
      else if (ch == fence_ch && run >= fence_run) { fenced = 0 }
      prev_no = 0
      next
    }
    fenced { prev_no = 0; next }
    # a row is a column of cells and each cell is read on its own: the words
    # of one cell are a sentence, and two cells side by side are not
    is_table_row($0) {
      prev_no = 0
      if (is_separator_row($0)) { next }
      cells = split(strip_spans($0), cell, "|")
      for (ci = 1; ci <= cells; ci++) {
        grade(cell[ci], fold(cell[ci]), FNR, 0)
      }
      next
    }
    # a blockquote is quoted upstream text
    /^[[:space:]]*>/ { prev_no = 0; next }
    # an nvim message is quoted as nvim writes it
    /(^|[^[:alnum:]])E[0-9]+:/ { prev_no = 0; next }
    {
      text = strip_spans($0)
      grade(text, fold(text), FNR, 0)
      # these pages wrap at 80 characters, so a frame or a tell word sits
      # across the margin as readily as inside a line, and a walk reading
      # one line at a time is a bypass one wrap wide
      if (prev_no == FNR - 1 && prev_text != "" && text != "") {
        joined = prev_text " " text
        grade(joined, fold(joined), prev_no, length(prev_text) + 1)
      }
      prev_text = text; prev_no = FNR
    }'
check_prose_frames() {
  local pages found rc
  pages=$(find "$@" -name '*.md' | LC_ALL=C sort)
  if [ -z "$pages" ]; then
    echo "STYLE FAIL: no markdown page found to grade for the stance"
    echo "  A walk handed an empty list reports nothing and reads like a"
    echo "  tree whose pages say what is true and stop."
    return 1
  fi
  rc=0
  # the program text comes after every option, because awk reads the first
  # non-option word as the program and every word past it as a file: handed
  # the fragment first, awk took the option words as filenames, found no
  # main rule to read them with, and exited 0 having graded nothing
  found=$(printf '%s\n' "$pages" | LC_ALL=C xargs awk \
    "$AWK_TABLE_ROW$PROSE_FRAMES_AWK") || rc=$?
  if [ "$rc" -ne 0 ]; then
    printf '%s\n' "$found"
    echo "STYLE FAIL: the stance walk exited $rc instead of grading the pages"
    echo "  awk names the page it could not read on stderr above."
    return 1
  fi
  if [ -z "$found" ]; then
    return 0
  fi
  printf '%s\n' "$found"
  echo "STYLE FAIL: a page a person reads argues for itself, or the README"
  echo "  names a mechanism. Remove the sentence, the clause or the word;"
  echo "  a mechanism word on the README is replaced by what the person sees."
  echo "  See \"A page is written for a reader who has not doubted anything\""
  echo "  and \"The README lists what a person gets\" in .claude/rules/docs.md."
  return 1
}

# The population every rule over scripts/ grades, read through the helper
# scripts/lib/script-population.sh so that this gate, check-portability.sh
# and check-budget-drift-cases.sh answer one list rather than three. Read
# once and handed to the temp-file walk, the width walk and both bans.
SCRIPT_POPULATION=""
# the verdict is memoized beside the list because four rules ask for it: a
# second read would name the same unreadable file a second time
SCRIPT_POPULATION_RC=""
read_script_population() {
  if [ -n "$SCRIPT_POPULATION_RC" ]; then
    return "$SCRIPT_POPULATION_RC"
  fi
  SCRIPT_POPULATION_RC=1
  if ! script_population_read; then
    echo "STYLE FAIL: a file under scripts/ cannot be read"
    return 1
  fi
  if [ -z "$SCRIPT_POPULATION" ]; then
    echo "STYLE FAIL: no script found to grade under $(pwd)"
    echo "  A walk handed an empty list reports nothing and reads like a"
    echo "  tree whose comments are inside the limit."
    return 1
  fi
  SCRIPT_POPULATION_RC=0
  return 0
}

# A ban over that population reports its hits and its own failure apart:
# grep answers 1 for a tree with no hits and 2 for a file it could not read,
# and a walk that reads both as clean passes a tree it never finished
# grading. The list is split into operands rather than piped through xargs,
# which folds those two statuses into one 123; a path carrying a blank
# splits into operands that do not exist, so grep answers 2 and the ban
# reddens naming them rather than grading a list one file short.
script_comment_ban() {
  local verdict="$1" files="$2" hits rc
  shift 2
  rc=0
  # shellcheck disable=SC2086
  hits=$(LC_ALL=C grep -n "$@" $files) || rc=$?
  if [ "$rc" -gt 1 ]; then
    echo "STYLE FAIL: the $verdict ban exited $rc instead of grading the scripts"
    return 1
  fi
  if [ -z "$hits" ]; then
    return 0
  fi
  printf '%s\n' "$hits"
  echo "STYLE FAIL: $verdict in script comment"
  return 1
}

# The three comment rules over scripts/, run together because they grade one
# population: two bans on what a comment may cite, and the width walk.
check_script_comment_rules() {
  local other rulefail
  if ! read_script_population; then
    return 1
  fi
  rulefail=0
  # this script is out of its own population: it names the banned phrases
  # literally to define the patterns below, which would otherwise
  # self-match. Filtered with awk rather than grep -v because a filter that
  # removes every line exits non-zero, which set -e reads as a failed walk
  other=$(printf '%s\n' "$SCRIPT_POPULATION" \
    | awk -v self="scripts/$(basename "$0")" '$0 != self')
  # net of this file the population can be empty, and both bans then report
  # ok having graded nothing -- the shape a pipeline stage reading an empty
  # list has, and the one the guard above cannot see because the population
  # it checked was not empty
  if [ -z "$other" ]; then
    echo "STYLE FAIL: scripts/ under $(pwd) holds no script but this one"
    echo "  Both citation bans and the width walk would report ok having"
    echo "  graded nothing."
    return 1
  fi
  script_comment_ban 'review-finding reference' "$other" \
    -E '\bFinding [0-9]|\btest gap [0-9]|found in review|\bAudit [A-Z]?[0-9]' || rulefail=1
  # the charter ban elsewhere reaches sources and docs; scripts carry the
  # same comments and are walked here instead
  script_comment_ban 'planning-charter reference' "$other" -iE '\bcharter' || rulefail=1
  check_script_comment_width || rulefail=1
  return $rulefail
}

# A script comment wraps at the width a page does, over that population,
# because a rule kept by hand over the five gate scripts left an
# 85-character comment standing in a sixth.
check_script_comment_width() {
  local wide rc
  if ! read_script_population; then
    return 1
  fi
  rc=0
  wide=$(printf '%s\n' "$SCRIPT_POPULATION" | LC_ALL=C xargs awk -v limit="$PROSE_WIDTH" "$AWK_COLS"'
    FNR == 1 { next }
    $0 !~ /^[[:space:]]*#/ { next }
    cols($0) <= limit { next }
    {
      # the run longer than the limit is what cannot wrap, so the comment is
      # measured without it, the way the page walk measures prose
      rest = cols($0)
      n = split($0, word, /[[:space:]]+/)
      for (i = 1; i <= n; i++) {
        if (cols(word[i]) > limit) { rest -= cols(word[i]) }
      }
      if (rest <= limit) { next }
      printf "%s:%d: %d characters\n", FILENAME, FNR, cols($0)
    }') || rc=$?
  if [ "$rc" -ne 0 ]; then
    printf '%s\n' "$wide"
    echo "STYLE FAIL: the comment width walk exited $rc instead of grading"
    echo "  awk names the script it could not read on stderr above."
    return 1
  fi
  if [ -z "$wide" ]; then
    return 0
  fi
  printf '%s\n' "$wide"
  echo "STYLE FAIL: a script comment runs past $PROSE_WIDTH characters"
  echo "  Re-wrap it. A run longer than the limit is taken out before the"
  echo "  line is measured, so a line reported here has a space in it."
  return 1
}

# The two width walks alone, run against a scratch crate root rather than
# this tree: the walks read only crates/view-engine, so grading them does
# not need the README, the scripts directory or the god-file classifier the
# full run below reads. The root is a positional argument, the form
# scripts/check-portability.sh takes for the same purpose.
if [ "${1:-}" = "--widths" ]; then
  ROOT="${2:-}"
  if [ -z "$ROOT" ]; then
    echo "usage: $0 --widths ROOT" >&2
    exit 2
  fi
  cd "$ROOT" || exit 2
  widthfail=0
  check_lua_chunk_width || widthfail=1
  check_string_literal_width || widthfail=1
  exit $widthfail
fi
# The prose width walk alone, graded the same way: a walk that stops
# reporting reads exactly like a tree whose pages are inside the limit.
if [ "${1:-}" = "--prose-width" ]; then
  ROOT="${2:-}"
  if [ -z "$ROOT" ]; then
    echo "usage: $0 --prose-width ROOT" >&2
    exit 2
  fi
  cd "$ROOT" || exit 2
  targets=""
  if [ -f README.md ]; then targets="README.md"; fi
  if [ -d docs ]; then targets="$targets docs"; fi
  if [ -d .claude/rules ]; then targets="$targets .claude/rules"; fi
  if [ -z "$targets" ]; then
    check_prose_width /dev/null
    exit $?
  fi
  # shellcheck disable=SC2086
  check_prose_width $targets
  exit $?
fi
# The stance walk alone, graded the same way. Its population is the three
# kinds of page someone reads: README.md, docs/ and the convention pages
# under .claude/rules/, which is the population the width walk grades.
if [ "${1:-}" = "--prose-frames" ]; then
  ROOT="${2:-}"
  if [ -z "$ROOT" ]; then
    echo "usage: $0 --prose-frames ROOT" >&2
    exit 2
  fi
  cd "$ROOT" || exit 2
  targets=""
  if [ -f README.md ]; then targets="README.md"; fi
  if [ -d docs ]; then targets="$targets docs"; fi
  if [ -d .claude/rules ]; then targets="$targets .claude/rules"; fi
  if [ -z "$targets" ]; then
    check_prose_frames /dev/null
    exit $?
  fi
  # shellcheck disable=SC2086
  check_prose_frames $targets
  exit $?
fi
# The comment rules over scripts/ alone, graded the same way: a walk that
# stops reporting reads exactly like a tree whose comments are inside the
# limit, and a ban handed a short population reads the same.
if [ "${1:-}" = "--script-comments" ]; then
  ROOT="${2:-}"
  if [ -z "$ROOT" ]; then
    echo "usage: $0 --script-comments ROOT" >&2
    exit 2
  fi
  cd "$ROOT" || exit 2
  check_script_comment_rules
  exit $?
fi
# The acceptance-expectation ban alone, graded the same way and for the same
# reason: a rule with no case matrix is a rule nobody has watched fail.
if [ "${1:-}" = "--acceptance" ]; then
  ROOT="${2:-}"
  if [ -z "$ROOT" ]; then
    echo "usage: $0 --acceptance ROOT" >&2
    exit 2
  fi
  cd "$ROOT" || exit 2
  check_acceptance_expectations
  exit $?
fi
# The written-program pin alone, graded the same way: its own blind spot is
# a walk that stops matching the spelling a site uses, which reads exactly
# like a clean tree.
if [ "${1:-}" = "--written-programs" ]; then
  ROOT="${2:-}"
  if [ -z "$ROOT" ]; then
    echo "usage: $0 --written-programs ROOT" >&2
    exit 2
  fi
  cd "$ROOT" || exit 2
  check_written_programs
  exit $?
fi
# The tied-spawn pin alone, graded the same way: a walk that stops matching
# the spelling a spawn uses reads exactly like a tree with no new spawns.
if [ "${1:-}" = "--tied-spawns" ]; then
  ROOT="${2:-}"
  if [ -z "$ROOT" ]; then
    echo "usage: $0 --tied-spawns ROOT" >&2
    exit 2
  fi
  cd "$ROOT" || exit 2
  check_tied_spawns
  exit $?
fi
# The geometry pin alone, graded the same way: a walk that stops matching
# the spelling a site uses reads exactly like a tree whose every attach is
# still sized by grid_target_for.
if [ "${1:-}" = "--geometry-sites" ]; then
  ROOT="${2:-}"
  if [ -z "$ROOT" ]; then
    echo "usage: $0 --geometry-sites ROOT" >&2
    exit 2
  fi
  cd "$ROOT" || exit 2
  check_geometry_sites
  exit $?
fi
# The doc-figure walk alone, graded the same way: a walk that stops matching
# the shape a reading is written in reads exactly like a tree whose doc
# comments quote nothing.
if [ "${1:-}" = "--doc-figures" ]; then
  ROOT="${2:-}"
  if [ -z "$ROOT" ]; then
    echo "usage: $0 --doc-figures ROOT" >&2
    exit 2
  fi
  cd "$ROOT" || exit 2
  check_doc_figures
  exit $?
fi
# The doc-width walk alone, graded the same way.
if [ "${1:-}" = "--doc-width" ]; then
  ROOT="${2:-}"
  if [ -z "$ROOT" ]; then
    echo "usage: $0 --doc-width ROOT" >&2
    exit 2
  fi
  cd "$ROOT" || exit 2
  check_doc_width
  exit $?
fi
# The temp-file trap walk alone, graded the same way.
if [ "${1:-}" = "--temp-traps" ]; then
  ROOT="${2:-}"
  if [ -z "$ROOT" ]; then
    echo "usage: $0 --temp-traps ROOT" >&2
    exit 2
  fi
  cd "$ROOT" || exit 2
  check_temp_traps
  exit $?
fi
# The notice joiner walk alone, graded the same way.
if [ "${1:-}" = "--notice-joiners" ]; then
  ROOT="${2:-}"
  if [ -z "$ROOT" ]; then
    echo "usage: $0 --notice-joiners ROOT" >&2
    exit 2
  fi
  cd "$ROOT" || exit 2
  check_notice_joiners
  exit $?
fi

# The temp-root walk alone, graded the same way.
if [ "${1:-}" = "--temp-roots" ]; then
  ROOT="${2:-}"
  if [ -z "$ROOT" ]; then
    echo "usage: $0 --temp-roots ROOT" >&2
    exit 2
  fi
  cd "$ROOT" || exit 2
  check_temp_roots
  exit $?
fi

fail=0
# Every directory a walk below is guarded on, named once and required here:
# a guard with no else reads as a pass when the tree it grades has moved, so
# the run says nothing about the rules it never reached. The guards stay --
# they keep a walk from being handed a root that is not there -- and this
# loop is what makes their absence loud.
for required in crates scripts scripts/acceptance compat corpus docs .claude/rules; do
  if [ ! -d "$required" ]; then
    echo "STYLE FAIL: $required/ directory missing"; fail=1
  fi
done
# The same rule where the guard is on a file rather than a directory: the
# emdash ban, the narrative-marker scan and the prose-width walk all sit
# behind `[ -f README.md ]`, so a root without it would pass having run none
# of the three. A second list because the test differs, and because the
# directory verdicts read `x/ directory missing`.
for required_file in README.md; do
  if [ ! -f "$required_file" ]; then
    echo "STYLE FAIL: $required_file missing"; fail=1
  fi
done
if [ -d scripts ]; then
  check_temp_traps || fail=1
  check_temp_roots || fail=1
fi
if [ -d crates ]; then
  check_content crates '//|#' --include='*.rs' || fail=1
  check_lua_chunk_width || fail=1
  check_string_literal_width || fail=1
  check_written_programs || fail=1
  check_tied_spawns || fail=1
  check_geometry_sites || fail=1
  check_doc_figures || fail=1
  check_doc_width || fail=1
fi
# One fold, and only one, may raise the single locally-raised condition
# notice. `Messages::set_native_condition` shows at most one such notice and
# re-asserts it on every pass -- a notice raised once on the transition would
# be dropped for good by the next `msg_clear` -- so a second production
# caller does not add a second notice: the two overwrite each other every
# pass and the banner flaps between them, which is a bug no test of either
# caller alone can see. Pinned rather than left to review, because the tree
# has already had to delete a second caller for exactly this reason.
CONDITION_OWNER="crates/view-core/src/update/supervision.rs"
CONDITION_CALLS=2
# production call sites only, via the god-file scanner's own classifier --
# the same read the two pins above make, so the gate pays for one scan and
# keeps one spelling of "which lines are production". It must not trip on
# the doc comments naming the function, nor on the unit tests that
# legitimately drive it directly. Fail closed: a renamed or missing
# classifier must not silently drop the ownership pin while the rest of the
# style gate stays green.
if ! read_prod_lines; then
  echo "STYLE FAIL: could not read production lines to check condition-notice ownership${PROD_LINES_WHY:+ -- $PROD_LINES_WHY}"
  fail=1
else
  # any non-identifier char before the name, so UFCS calls
  # (`Messages::set_native_condition(...)`) cannot walk past the pin; the
  # definition itself is the one legitimate non-call mention
  src=0
  sites=$(printf '%s\n' "$PROD_LINES_CACHE" \
    | grep -E '[^A-Za-z0-9_]set_native_condition\(' \
    | grep -v 'fn set_native_condition') || src=$?
  # counted with awk rather than grep -c, which answers 1 for an empty list
  # and would need a status thrown away to be read at all
  found=$(printf '%s' "$sites" | awk 'END { print NR }')
  strangers=""
  if [ "$src" -le 1 ]; then
    strangers=$(printf '%s' "$sites" | grep -v "^$CONDITION_OWNER:") || src=$?
  fi
  # an errored filter reports no stranger, which is what a clean tree
  # reports, and the pin still matches -- so the status is the verdict
  if [ "$src" -gt 1 ]; then
    echo "STYLE FAIL: the condition-notice filter exited $src instead of grading"
    echo "  A filter that could not run reports no stranger, which is what a"
    echo "  tree that owns its condition notice reports."
    fail=1
  elif [ -n "$strangers" ]; then
    printf '%s\n' "$strangers"
    echo "STYLE FAIL: set_native_condition called outside $CONDITION_OWNER"
    echo "  The one visible condition notice is owned by a single fold that"
    echo "  re-asserts or retracts it every pass. Route the new condition"
    echo "  through that fold instead of raising it here."
    fail=1
  elif [ "$found" != "$CONDITION_CALLS" ]; then
    printf '%s\n' "$sites"
    echo "STYLE FAIL: $CONDITION_OWNER makes $found set_native_condition calls, pinned at $CONDITION_CALLS"
    echo "  The pin is the retract and the assert of one fold. If a third is"
    echo "  genuinely one fold's business, move the pin and say why here."
    fail=1
  fi
fi
# Every path to a binary cargo built goes through `view_oracle::target_root`
# (re-exported as `view_harness::fixture::target_root`), which honours
# `CARGO_TARGET_DIR`. One spelled from `workspace_root` instead is invisible
# on a normal checkout and fails every leg of a run made from an isolated
# export, for a reason that has nothing to do with the code under test.
# `workspace_root` stays correct for source files and scratch roots -- what
# it must never reach is a `release`/`debug` profile directory.
# Keyed on the profile join itself rather than on what sits near a
# `workspace_root` call: any distance rule is one refactor away from a
# locator that spells the root and the profile far enough apart to pass.
# A join is answered for by its own statement -- `target_root()` in the
# chain, or a name the file bound to one earlier.
if [ -d crates ]; then
  # the pipeline's own status is checked rather than discarded: an awk with
  # a syntax error prints nothing and would otherwise read exactly like a
  # clean tree, passing the gate on a pin that never ran. `pipefail` is what
  # carries that status out of the pipe. No `-r` is owed for the empty case:
  # xargs runs its utility with stdin on /dev/null, so an awk reached with
  # no file operands reads nothing and prints nothing rather than hanging on
  # the gate's own stdin.
  if ! built=$(find crates -name '*.rs' -print0 | xargs -0 awk '
    function check(  name) {
      # a binding belongs to the function it was made in: bash-style file
      # scope would let one function name a root and whitelist the same
      # identifier for every sibling that never bound one
      if (stmt ~ /(^|[^A-Za-z0-9_])fn[[:space:]]/) {
        delete rooted
      }
      if (stmt ~ /target_root\(\)/ &&
          match(stmt, /let[[:space:]]+(mut[[:space:]]+)?[A-Za-z_][A-Za-z0-9_]*/)) {
        name = substr(stmt, RSTART, RLENGTH)
        sub(/^let[[:space:]]+/, "", name)
        sub(/^mut[[:space:]]+/, "", name)
        rooted[name] = 1
      }
      if (stmt !~ /\.join\("(release|debug)"\)/ && stmt !~ /\.join\(profile/) {
        return
      }
      if (stmt ~ /target_root\(\)/) {
        return
      }
      for (name in rooted) {
        if (stmt ~ ("(^|[^A-Za-z0-9_])" name "[^A-Za-z0-9_]")) {
          return
        }
      }
      printf "%s:%d:%s\n", FILENAME, FNR, stmt
    }
    FNR == 1 { delete rooted; stmt = "" }
    {
      line = $0
      sub(/\/\/.*$/, "", line)
      n = split(line, part, /[;{}]/)
      for (i = 1; i <= n; i++) {
        stmt = stmt " " part[i]
        if (i < n) { check(); stmt = "" }
      }
    }
  '); then
    echo "STYLE FAIL: the target_root pin could not be evaluated (find or awk failed)"
    fail=1
    built=""
  fi
  if [ -n "$built" ]; then
    printf '%s\n' "$built"
    echo "STYLE FAIL: a built binary resolved from something other than target_root"
    echo "  A profile directory belongs to cargo, and CARGO_TARGET_DIR moves it."
    fail=1
  fi
fi
for dir in compat corpus; do
  if [ -d "$dir" ]; then
    # --exclude-dir=.cache: compat/.cache/ is the gitignored, populated-at-
    # test-time plugin install cache (lazy.nvim plus every plugin it
    # clones, each with its own .git/ and third-party comments) -- scanning
    # it would make this check's outcome depend on whatever happens to be
    # cached locally rather than on committed source, and would be slow.
    check_content "$dir" '#|--' --exclude-dir=.cache --include='*.toml' --include='*.lua' || fail=1
  fi
done
if [ -d scripts ]; then
  check_script_comment_rules || fail=1
fi
check_acceptance_expectations || fail=1
check_notice_joiners || fail=1
if [ -f README.md ]; then
  doc_targets=(README.md)
  if [ -d docs ]; then doc_targets+=(docs); fi
  if grep -rn -- '—' "${doc_targets[@]}"; then
    echo "STYLE FAIL: emdash in user docs"; fail=1
  fi
  # narrative markers, unanchored (README/docs prose carries no comment
  # prefix to anchor on). `§` is deliberately not run here, unlike the
  # source-side ban in check_content: a doc legitimately cites a spec
  # section (e.g. docs/statusline-wire-capture.md's "spec §9"), where
  # source code never has occasion to.
  check_narrative_markers "" "${doc_targets[@]}" || fail=1
  # the convention pages are prose someone reads, so the width walk and the
  # stance walk both grade them. An agent reads them before it edits this
  # tree and writes in the voice they carry. The two bans above stay off
  # them -- a rules page cites a spec section and quotes the markers it
  # bans, where a doc never does
  page_targets=("${doc_targets[@]}")
  if [ -d .claude/rules ]; then page_targets+=(.claude/rules); fi
  check_prose_width "${page_targets[@]}" || fail=1
  check_prose_frames "${page_targets[@]}" || fail=1
fi
exit $fail
