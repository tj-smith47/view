#!/usr/bin/env bash
set -euo pipefail

# All content-pattern checks against Rust source. Parameterized on a target
# path so the same checks run tree-wide (default, scoped to crates/) or
# against a single file (--file, used by the post-edit-rs.sh hook so an
# editor-triggered check costs one grep pass per pattern instead of a
# full-tree walk on every keystroke-adjacent save).
check_rs_content() {
  local target="$1"
  local fail=0
  if grep -rnE '(//|#).*\b(Phase|Task|Step|Wave|Cycle|Session) [0-9]' "$target" --include='*.rs'; then
    echo "STYLE FAIL: session-narrative comment marker"; fail=1
  fi
  if grep -rn '§' "$target" --include='*.rs'; then
    echo "STYLE FAIL: section-symbol reference in code"; fail=1
  fi
  if grep -rnE '(//|#).*\b(we|I|Claude) (added|implemented|changed|fixed|removed)' "$target" --include='*.rs'; then
    echo "STYLE FAIL: assistant-citation comment"; fail=1
  fi
  if grep -rnE '\bFinding [0-9]|\btest gap [0-9]|found in review|\bAudit [A-Z]?[0-9]' "$target" --include='*.rs'; then
    echo "STYLE FAIL: review-finding reference in comment"; fail=1
  fi
  # narrative/roadmap pointers: comments must state what the code does now,
  # never when it changes. P[0-9] is intentionally case-sensitive (not -i):
  # a lowercase p0/p1 reads as a coordinate or point variable, not a phase
  # tag, and the tree has no such roadmap-tagged identifiers to catch.
  if grep -rniE '\bthis phase\b|\ba later (phase|task|session)\b|\bin a later\b' "$target" --include='*.rs'; then
    echo "STYLE FAIL: roadmap-phase comment marker"; fail=1
  fi
  # session-narrative markers the phase/task-number check above doesn't
  # catch: "this task" (no number attached, unlike "Task 10") and
  # "the RED/GREEN test" (TDD-status narration, not a fact about the code)
  if grep -rniE '\bthis task\b|\bthe (red|green) test\b' "$target" --include='*.rs'; then
    echo "STYLE FAIL: task/TDD-status comment marker"; fail=1
  fi
  if grep -rnE '\bP[0-9]\b' "$target" --include='*.rs'; then
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
  if grep -rnE "\b[CIM][0-9]+\`?'s?\b" "$target" --include='*.rs'; then
    echo "STYLE FAIL: review-finding tag in comment"; fail=1
  fi
  # spec-task tags (T4/T5/T6): a comment must state what the code does,
  # never which spec task produced it. Two shapes: a slash-joined sequence
  # (T4/T5/T6, T10/T11), which has no legitimate non-task-tag reading
  # anywhere in Rust syntax or prose, and a single tag standing alone in a
  # comment surrounded by whitespace ("the T4 brief", "done in T7."). Not a
  # blanket \bT[0-9]+\b ban, which would flag far more. Backtick-wrapped
  # type parameters never match (a backtick, not whitespace, precedes the
  # T); a BARE prose mention of a T1-style name in a comment still trips
  # the standalone pattern, so backtick type params in rustdoc prose.
  if grep -rnE '(//|#).*\bT[0-9]+/T[0-9]+' "$target" --include='*.rs'; then
    echo "STYLE FAIL: spec-task tag sequence in comment"; fail=1
  fi
  if grep -rnE '(//|#).*[[:space:]]T[0-9]+[.,:]?([[:space:]]|$)' "$target" --include='*.rs'; then
    echo "STYLE FAIL: spec-task tag in comment"; fail=1
  fi
  # TDD/session-narrative markers one synonym past the existing "this task"/
  # "the red/green test" check: "the RED/GREEN half" (a paired-test label),
  # "this fix"/"the unfixed" (fix-narrative instead of a code fact), and
  # "pre-image" (git-diff jargon for "the code before this change")
  if grep -rniE '\bthe (red|green) half\b|\bthis fix\b|\bthe unfixed\b|\bpre-image\b' "$target" --include='*.rs'; then
    echo "STYLE FAIL: fix-narrative comment marker"; fail=1
  fi
  # bare git-style commit hashes cited in prose ("fa54c7c's replay", "by
  # eae8542"): a plain \b[0-9a-f]{7}\b would also match a real 7-hex-digit
  # constant (a color, a checksum, a magic number) with no possessive or
  # "by"-prefix reading, so this scopes to the two prose shapes this tree's
  # actual violations used instead of a blanket hex-token ban
  if grep -rnE "\b[0-9a-f]{7}\`?'s\b|\bby [0-9a-f]{7}\b" "$target" --include='*.rs'; then
    echo "STYLE FAIL: commit-hash reference in comment"; fail=1
  fi
  # a comment must state what the code does, never who found it lacking or
  # what body prescribed it: "the reviewer flagged" / "coordinator
  # requirement" name a person or process, not a fact about the code
  if grep -rniE '\bthe (reviewer|coordinator|auditor)\b|\bcoordinator requirement\b' "$target" --include='*.rs'; then
    echo "STYLE FAIL: reviewer/coordinator attribution in comment"; fail=1
  fi
  # banned outright, not just in comments: no current .rs file has a string
  # literal that legitimately needs one, so this is a plain content scan
  # rather than a comment-only grep
  if grep -rn '—' "$target" --include='*.rs'; then
    echo "STYLE FAIL: emdash in Rust source"; fail=1
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
  check_rs_content "$FILE" || exit 1
  exit 0
fi

fail=0
if [ -d crates ]; then
  check_rs_content crates || fail=1
else
  echo "STYLE FAIL: crates/ directory missing"; fail=1
fi
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
  targets="README.md"
  [ -d docs ] && targets="$targets docs"
  if grep -rn -- '—' $targets; then
    echo "STYLE FAIL: emdash in user docs"; fail=1
  fi
else
  echo "STYLE FAIL: README.md missing"; fail=1
fi
exit $fail
