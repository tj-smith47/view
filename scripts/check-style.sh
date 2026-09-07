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
# A character is not a terminal column, and every message this feeds says
# characters because that is what it counts: a double-width glyph counts
# one and paints two, a combining mark counts one and paints none, so the
# same measure passes a comment of 61 characters that fills 91 columns and
# reddens a decomposed one of 92 that fills 62. Both limits are stated in
# .claude/rules/shell.md and cased beside the width cases; a number carrying
# a unit it is not in is worse than no number at all.
AWK_COLS='function cols(s,   t) { t = s; gsub(/[\200-\277]/, "", t); return length(t) }
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
crates/view-ai/src/acp/session.rs 5 the agent adapter, tied on unix; the windows arm keeps tokio own spawn, which no parent-death signal covers there; plus the thread that waits out a signalled adapter and the task that drives one
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
crates/view-proc/src/lib.rs 1 the anchor thread every tied spawn forks from, which lives as long as the process does
crates/view-test-support/src/lib.rs 1 a sysctl read, waited on to completion
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
  local scanner
  if [ -n "$PROD_LINES_CACHE" ]; then
    return 0
  fi
  scanner="$(cd "$(dirname "$0")" && pwd)/audit-god-files.sh"
  if ! PROD_LINES_ERR=$(mktemp "${TMPDIR:-/tmp}/check-style-prod-lines.XXXXXX"); then
    PROD_LINES_WHY="mktemp under ${TMPDIR:-/tmp} failed"
    return 1
  fi
  # the scan is the slowest step in the gate, so the window in which a
  # Ctrl-C or a set -e abort would strand this file is the whole of it; the
  # straight-line remove below still covers the ordinary path
  trap 'rm -f "$PROD_LINES_ERR"' EXIT
  PROD_LINES_CACHE=$(bash "$scanner" --prod-lines . 2> "$PROD_LINES_ERR") || PROD_LINES_CACHE=""
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
# Each row is a path, its pinned number of production lines, and where the
# geometry those lines spend came from.
GEOMETRY_CALLS='ui_attach|UiAttach|ui_try_resize|try_resize\(|TryResize|(^|[^A-Za-z0-9_])attach\(|late_attach'
GEOMETRY_ATTACH_CRATES='^crates/(view|view-core|view-engine|view-oracle)/'
GEOMETRY_SITES='
crates/view-core/src/model.rs 1 the one RpcCall::UiAttach production builds, from Model::grid_target -- grid_target_for over the model own terminal size
crates/view-core/src/msg.rs 2 the UiAttach and TryResize variant declarations, whose fields wrap onto the lines below them; the pair each variant carries is what its builder put in it
crates/view-core/src/update/ai_fs.rs 4 an AI filesystem lock release and its own helper, no geometry anywhere
crates/view-core/src/update/mod.rs 1 the fold resizing the grid when the paint area moves, spending Model::grid_target
crates/view-core/src/update/ui_event.rs 1 the tabline fold resizing the grid when the chrome row count moves, spending Model::grid_target
crates/view-engine/src/nvim_api.rs 10 the handle own attach and resize entry points, the private attach both public ones funnel through, and the nvim_ui_attach and nvim_ui_try_resize method names they send; each spends what its caller hands it, except the private attach declaration line, whose parameters wrap onto the lines below it and which names no pair
crates/view-engine/src/process.rs 12 the spawn own geometry seed: the late_attach field, the builder that stores a pair, the getter, and the two argv paths that render one into --cmd, each spending what main or recovery handed the config
crates/view-oracle/src/hang.rs 4 the adversarial harness attaching and resizing its own engine at the fixture size it opened the session with, and the TryResize effect it forwards
crates/view-oracle/src/lib.rs 2 the oracle driver attaching at the size its caller opened the session with, and the TryResize effect it forwards
crates/view-oracle/src/reference.rs 2 the second applier attaching and resizing at the size the session under comparison is held at
crates/view-oracle/src/speculate.rs 2 the speculative-echo battery attaching and resizing at its own fixture geometry
crates/view/src/engine_ops.rs 14 the EngineOps attach and resize surface: one declaration and the forwarding impls behind it, each spending the pair it was handed, except the four ui_attach signature lines, whose parameters wrap onto the lines below them
crates/view/src/main.rs 2 the attach guard release and the spawn own geometry seed, both spending spawn_size -- what grid_target_for answered the terminal reading with
crates/view/src/native.rs 1 the native session resizing the grid for the row the statusline claims, spending Model::grid_target
crates/view/src/recovery.rs 1 the replacement engine own geometry seed, spending Model::grid_target
crates/view/src/runtime/executor.rs 3 the executor spending the pair the UiAttach and TryResize effects carry, which update() built from the model; the UiAttach arm own pattern opens on a line naming no pair
crates/view/src/startup.rs 7 the attach guard release and the attaches it feeds, all spending the pair main released rather than a reading of their own, plus the restart own zero-argument attach() closure call and its read-back of the config late_attach seed, neither of which carries a pair
'
check_geometry_sites() {
  local expected actual
  if ! read_prod_lines; then
    echo "STYLE FAIL: could not read production lines to check geometry sites${PROD_LINES_WHY:+ -- $PROD_LINES_WHY}"
    return 1
  fi
  expected=$(printf '%s\n' "$GEOMETRY_SITES" | awk 'NF { print $1, $2 }' | LC_ALL=C sort)
  # keyed to the path and line number the scanner emits rather than to the
  # match, so a line carrying both a call and a release counts once
  actual=$({
    printf '%s\n' "$PROD_LINES_CACHE" | grep -E "$GEOMETRY_CALLS" || true
    printf '%s\n' "$PROD_LINES_CACHE" | grep -E "$GEOMETRY_ATTACH_CRATES" \
      | grep -E '(^|[^A-Za-z0-9_])release\(' || true
  } | cut -d: -f1,2 | LC_ALL=C sort -u | sed 's/:[0-9]*$//' | uniq -c \
    | awk '{ print $2, $1 }' | LC_ALL=C sort) || actual=""
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

# A script that makes a temp file removes it under a trap. A straight-line
# `rm` covers the ordinary path and nothing else: the gates here run for
# seconds over a whole tree, and a Ctrl-C or a `set -e` abort inside that
# window strands the file under `${TMPDIR:-/tmp}`. Keyed on the script
# rather than on the statement, because the removal legitimately sits far
# from the `mktemp` -- what matters is that one exists.
check_temp_traps() {
  local fail=0 f
  for f in scripts/*.sh; do
    if grep -q 'mktemp' "$f" && ! grep -qE '^[[:space:]]*trap .*EXIT' "$f"; then
      echo "$f: makes a temp file with no EXIT trap to remove it"
      fail=1
    fi
  done
  if [ "$fail" -eq 0 ]; then
    return 0
  fi
  echo "STYLE FAIL: a temp file with no trap to remove it"
  echo "  A straight-line rm covers the ordinary path alone: a signal or a"
  echo "  set -e abort inside the window leaves the file behind. Remove it"
  echo "  under trap ... EXIT beside the mktemp."
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
check_prose_width() {
  local pages wide unreadable rc
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
  wide=$(printf '%s\n' "$pages" | LC_ALL=C xargs awk -v limit="$PROSE_WIDTH" "$AWK_COLS"'
    FNR == 1 { fenced = 0 }
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
      next
    }
    fenced { next }
    /^[[:space:]]*[|#]/ { next }
    cols($0) <= limit { next }
    /^[[:space:]]*(<[^ >]+>|[^ ]+:\/\/[^ ]+)[.,]?[[:space:]]*$/ { next }
    /^[[:space:]]*!?\[[^]]*\](\([^)]*\))?[.,]?[[:space:]]*$/ { next }
    /^[[:space:]]*\[!?\[[^]]*\]\([^)]*\)\]\([^)]*\)[.,]?[[:space:]]*$/ { next }
    {
      # the run longer than the limit is what cannot wrap, so it is what is
      # exempt: the rest of the line is prose and is measured without it
      rest = cols($0)
      n = split($0, word, /[ \t]+/)
      for (i = 1; i <= n; i++) {
        if (cols(word[i]) > limit) { rest -= cols(word[i]) }
      }
      if (rest <= limit) { next }
      printf "%s:%d: %d characters\n", FILENAME, FNR, cols($0)
    }') || rc=$?
  if [ "$rc" -ne 0 ]; then
    printf '%s\n' "$wide"
    echo "STYLE FAIL: the width walk exited $rc instead of grading the pages"
    echo "  awk names the page it could not read on stderr above."
    return 1
  fi
  if [ -z "$wide" ]; then
    return 0
  fi
  printf '%s\n' "$wide"
  echo "STYLE FAIL: a doc line runs past $PROSE_WIDTH characters"
  echo "  Re-wrap the paragraph. A line that cannot wrap -- fenced, a table"
  echo "  row, a heading, or one link -- is already exempt, and a run longer"
  echo "  than the limit is taken out before the line is measured, so a line"
  echo "  reported here is prose with a space in it."
  return 1
}

# The population every comment rule over scripts/ grades: a file whose first
# line names bash or sh, which is what makes a file a script here and what
# the portability legs select on. Read once and handed to the width walk and
# to both bans below, because the eight remote-test fixtures carry no suffix
# and a rule spelled over *.sh graded 26 of the 35 while its sibling graded
# all of them.
SCRIPT_POPULATION=""
read_script_population() {
  local entries skipped graded unreadable first
  if [ -n "$SCRIPT_POPULATION" ]; then
    return 0
  fi
  # symlinks are listed beside regular files so a dangling one is named by
  # the loop below rather than dropped by the selection: a selection that
  # skips what it cannot open leaves the population short, and a short
  # population grades its survivors and reads exactly like a tree with
  # nothing to report
  entries=$(find scripts \( -type f -o -type l \) | LC_ALL=C sort)
  # a fifo, socket or device node is readable and could never carry a
  # shebang, so it is named and passed over: reddening a gate for an entry
  # nobody can rewrap is a false red nobody can act on
  skipped=$(find scripts ! -type d ! -type f ! -type l | LC_ALL=C sort)
  if [ -n "$skipped" ]; then
    printf '%s\n' "$skipped" |
      sed 's/$/: not a regular file or a symlink, the comment rules skip it/'
  fi
  # selected by a loop rather than by an xargs awk: xargs splits a path on a
  # blank and awk takes a fatal on the fragment, which drops the file from
  # all three rules with the run still green. Read and select in the one
  # pass, so no entry can pass the readability vet and then be lost by the
  # selection below it
  graded=$(printf '%s\n' "$entries" | while IFS= read -r entry; do
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
    printf '%s\n' "$unreadable" | sed 's/$/: the comment rules cannot read it/'
    echo "STYLE FAIL: a file under scripts/ cannot be read"
    return 1
  fi
  SCRIPT_POPULATION=$(printf '%s\n' "$graded" | sed -n 's/^take //p')
  if [ -z "$SCRIPT_POPULATION" ]; then
    echo "STYLE FAIL: no script found to grade for comment width under $(pwd)"
    echo "  A walk handed an empty list reports nothing and reads like a"
    echo "  tree whose comments are inside the limit."
    return 1
  fi
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
      n = split($0, word, /[ \t]+/)
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
  [ -f README.md ] && targets="README.md"
  [ -d docs ] && targets="$targets docs"
  if [ -z "$targets" ]; then
    check_prose_width /dev/null
    exit $?
  fi
  # shellcheck disable=SC2086
  check_prose_width $targets
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

fail=0
if [ -d scripts ]; then
  check_temp_traps || fail=1
fi
if [ -d crates ]; then
  check_content crates '//|#' --include='*.rs' || fail=1
  check_lua_chunk_width || fail=1
  check_string_literal_width || fail=1
  check_written_programs || fail=1
  check_tied_spawns || fail=1
  check_geometry_sites || fail=1
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
  check_script_comment_rules || fail=1
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
  check_prose_width "${doc_targets[@]}" || fail=1
else
  echo "STYLE FAIL: README.md missing"; fail=1
fi
exit $fail
