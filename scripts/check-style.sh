#!/usr/bin/env bash
set -euo pipefail

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
  if grep -rniE "(${comment_prefix}).*\\b(used by|called from|called by|invoked by|invoked from) [\`\[]*[A-Za-z_][A-Za-z0-9_]*(::|\\.|\\(|\`|\\])" \
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

# Every embedded Lua chunk wraps at 80 columns. The chunks are read beside
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
  report=$(awk '
    function check_width(line) {
      if (length(line) > 80) {
        printf "%s:%d: %d columns\n", FILENAME, FNR, length(line)
        over++
      }
    }
    inchunk {
      check_width($0)
      # an escaped quote inside the Lua does not close the Rust literal, so
      # it must not end the walk either -- it would skip the rest silently
      if ($0 ~ /^\);$/ || $0 ~ /^";$/ || $0 ~ /[^\\]";$/) { inchunk = 0 }
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
    echo "STYLE FAIL: a Lua chunk line is over 80 columns"
    echo "  Wrap it the way the rest of nvim_api.rs's chunks are: break at"
    echo "  an operator or after a comma, keep the chunk's indentation."
    return 1
  fi
  return 0
}

# The other shape: a multi-line string literal, which is how every live test
# hands Lua to nvim and how the crate carries its longer messages. rustfmt
# holds the line it opens on and nothing else about it, so the same 80
# columns apply to every line of one -- no test of what the literal holds,
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
  report=$(awk '
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
      if (length(line) > 80) {
        printf "%s:%d: %d columns\n", FILENAME, FNR, length(line)
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
    echo "STYLE FAIL: a line inside a string literal is over 80 columns"
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

fail=0
if [ -d crates ]; then
  check_content crates '//|#' --include='*.rs' || fail=1
  check_lua_chunk_width || fail=1
  check_string_literal_width || fail=1
  check_written_programs || fail=1
else
  echo "STYLE FAIL: crates/ directory missing"; fail=1
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
if [ -f scripts/audit-god-files.sh ] && [ -d crates ]; then
  # production call sites only, via the god-file scanner's own classifier:
  # this must not trip on the doc comments naming the function, nor on the
  # unit tests that legitimately drive it directly
  prod_lines=$(bash scripts/audit-god-files.sh --prod-lines) || prod_lines=""
  if [ -z "$prod_lines" ]; then
    # the scanner's own reason, re-read only on this path: it is the whole
    # diagnosis (an untracked tree, a quoted path it cannot name) and
    # without it this line sends a reader to the wrong file
    why=$({ bash scripts/audit-god-files.sh --prod-lines 2>&1 >/dev/null || true; } | head -3)
    echo "STYLE FAIL: could not read production lines to check condition-notice ownership${why:+ -- $why}"
    fail=1
  else
    # any non-identifier char before the name, so UFCS calls
    # (`Messages::set_native_condition(...)`) cannot walk past the pin; the
    # definition itself is the one legitimate non-call mention
    sites=$(printf '%s\n' "$prod_lines" | grep -E '[^A-Za-z0-9_]set_native_condition\(' | grep -v 'fn set_native_condition' || true)
    found=$(printf '%s' "$sites" | grep -c . || true)
    strangers=$(printf '%s' "$sites" | grep -v "^$CONDITION_OWNER:" || true)
    if [ -n "$strangers" ]; then
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
else
  # fail closed: a renamed or missing classifier must not silently drop the
  # ownership pin while the rest of the style gate stays green
  echo "STYLE FAIL: scripts/audit-god-files.sh or crates/ missing; cannot check condition-notice ownership"
  fail=1
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
      if (stmt ~ /(^|[^A-Za-z0-9_])fn[ \t]/) {
        delete rooted
      }
      if (stmt ~ /target_root\(\)/ &&
          match(stmt, /let[ \t]+(mut[ \t]+)?[A-Za-z_][A-Za-z0-9_]*/)) {
        name = substr(stmt, RSTART, RLENGTH)
        sub(/^let[ \t]+/, "", name)
        sub(/^mut[ \t]+/, "", name)
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
  # file list built via find rather than a `grep --exclude` flag: exclude
  # syntax and behavior differ across grep implementations, and this script
  # must exclude itself since it names the banned phrases literally to
  # define the patterns above, which would otherwise self-match
  other_scripts=$(find scripts -name '*.sh' ! -name "$(basename "$0")")
  if [ -n "$other_scripts" ] && echo "$other_scripts" | xargs grep -nE '\bFinding [0-9]|\btest gap [0-9]|found in review|\bAudit [A-Z]?[0-9]'; then
    echo "STYLE FAIL: review-finding reference in script comment"; fail=1
  fi
fi
check_acceptance_expectations || fail=1
if [ -f README.md ]; then
  doc_targets=(README.md)
  [ -d docs ] && doc_targets+=(docs)
  if grep -rn -- '—' "${doc_targets[@]}"; then
    echo "STYLE FAIL: emdash in user docs"; fail=1
  fi
  # narrative markers, unanchored (README/docs prose carries no comment
  # prefix to anchor on). `§` is deliberately not run here, unlike the
  # source-side ban in check_content: a doc legitimately cites a spec
  # section (e.g. docs/statusline-wire-capture.md's "spec §9"), where
  # source code never has occasion to.
  check_narrative_markers "" "${doc_targets[@]}" || fail=1
else
  echo "STYLE FAIL: README.md missing"; fail=1
fi
exit $fail
