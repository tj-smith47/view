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

fail=0
if [ -d crates ]; then
  check_content crates '//|#' --include='*.rs' || fail=1
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
    echo "STYLE FAIL: could not read production lines to check condition-notice ownership"
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
