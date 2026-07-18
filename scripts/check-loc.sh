#!/usr/bin/env bash
# Gate: no production .rs file may exceed MAX_PROD_LOC lines of production
# code. Oversized files must split into modules, and their tests must move
# to a separate tests.rs / *_tests.rs file: inline #[cfg(test)] blocks in
# large files inflate cargo-llvm-cov coverage (test lines count as covered
# production lines), while separate test files are correctly excluded from
# coverage.
#
# Production LOC = non-blank, non-comment-only lines BEFORE the first
# top-level `#[cfg(test)]` marker. Convention (enforced by review) keeps
# all test-only items at the end of the file, so everything from the first
# unindented #[cfg(test)] to EOF is test code.
set -euo pipefail
MAX_PROD_LOC=1000
fail=0
while IFS= read -r f; do
  case "$f" in
    */tests/*|*/benches/*|*/tests.rs|*_test.rs|*_tests.rs) continue ;;
  esac
  prod_loc="$(awk '
    /^#\[cfg\(test\)\]/ { exit }
    !/^[[:space:]]*$/ && !/^[[:space:]]*\/\// { n++ }
    END { print n+0 }
  ' "$f")"
  if [ "$prod_loc" -gt "$MAX_PROD_LOC" ]; then
    echo "LOC FAIL: $f has $prod_loc production LOC (max $MAX_PROD_LOC)."
    echo "  Split it into focused modules, and move any inline #[cfg(test)]"
    echo "  tests to a separate tests.rs or *_tests.rs file (inline tests"
    echo "  inflate cargo-llvm-cov coverage)."
    fail=1
  fi
done < <(find crates -path '*/src/*' -name '*.rs' | sort -u)
exit $fail
