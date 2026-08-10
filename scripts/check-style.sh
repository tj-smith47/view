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
  # above missed. Requiring a directly adjacent number keeps this from
  # flagging the ordinary lowercase word "task" on its own, which -- unlike
  # "step" -- does have legitimate unrelated prose readings; the other four
  # words above ("phase"/"wave"/"cycle"/"session") stay case-sensitive since
  # no lowercase-numbered instance of any of them has been found here.
  if grep -rniE "${anchor}\\btask [0-9]" "${targets[@]}"; then
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
