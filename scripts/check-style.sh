#!/usr/bin/env bash
set -euo pipefail
fail=0
if [ -d crates ]; then
  if grep -rnE '(//|#).*\b(Phase|Task|Step|Wave|Cycle|Session) [0-9]' crates --include='*.rs'; then
    echo "STYLE FAIL: session-narrative comment marker"; fail=1
  fi
  if grep -rn '§' crates --include='*.rs'; then
    echo "STYLE FAIL: section-symbol reference in code"; fail=1
  fi
  if grep -rnE '(//|#).*\b(we|I|Claude) (added|implemented|changed|fixed|removed)' crates --include='*.rs'; then
    echo "STYLE FAIL: assistant-citation comment"; fail=1
  fi
  # banned outright, not just in comments: no current .rs file has a string
  # literal that legitimately needs one, so this is a plain content scan
  # rather than a comment-only grep
  if grep -rn '—' crates --include='*.rs'; then
    echo "STYLE FAIL: emdash in Rust source"; fail=1
  fi
else
  echo "STYLE FAIL: crates/ directory missing"; fail=1
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
