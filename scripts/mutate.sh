#!/usr/bin/env bash
# One battery run per invocation: every row is applied, its named tests are
# run, and the file is restored from a byte copy taken before the edit --
# never from git, which cannot tell a mutation apart from uncommitted work.
set -uo pipefail

BATTERY="${1:-}"
if [ -z "$BATTERY" ] || [ ! -f "$BATTERY" ]; then
  echo "usage: task mutate -- <battery.tsv>" >&2
  echo "  columns, tab separated: file<TAB>old<TAB>new<TAB>cargo-test-args" >&2
  echo "  a row whose 'old' is absent from 'file' is a battery bug and fails" >&2
  echo "  \\n in 'old'/'new' is a line break: rows themselves are single lines" >&2
  exit 2
fi

BACKUP_DIR="$(mktemp -d "${TMPDIR:-$HOME/.cache}/view-mutate.XXXXXX")"
RESTORE_LIST="$BACKUP_DIR/restore.list"
: >"$RESTORE_LIST"

restore_all() {
  while IFS=$'\t' read -r saved original; do
    [ -n "$saved" ] && cp -- "$saved" "$original"
  done <"$RESTORE_LIST"
  rm -rf -- "$BACKUP_DIR"
}
# a battery that dies mid-row must still leave the tree as it found it
trap restore_all EXIT INT TERM

survivors=0
rows=0

while IFS=$'\t' read -r file old new test_args; do
  case "$file" in ''|'#'*) continue ;; esac
  rows=$((rows + 1))
  printf '\n=== %s\n    replacing: %.60s\n' "$file" "$old"

  if [ ! -f "$file" ]; then
    echo "    BATTERY BUG: no such file"
    survivors=$((survivors + 1))
    continue
  fi

  saved="$BACKUP_DIR/$(printf '%s' "$file" | tr / _)"
  cp -- "$file" "$saved"
  printf '%s\t%s\n' "$saved" "$file" >>"$RESTORE_LIST"

  if ! OLD="$old" NEW="$new" python3 - "$file" <<'PY'
import os, sys
path = sys.argv[1]
# a row is one line, so a mutation spanning lines writes `\n` for each
# break -- the only way to target one of two textually identical lines
old = os.environ["OLD"].replace("\\n", "\n")
new = os.environ["NEW"].replace("\\n", "\n")
text = open(path, encoding="utf-8").read()
n = text.count(old)
if n == 0:
    sys.exit("    BATTERY BUG: the text to mutate is not in the file")
if n > 1:
    sys.exit(f"    BATTERY BUG: the text to mutate appears {n} times; make it unique")
open(path, "w", encoding="utf-8").write(text.replace(old, new))
PY
  then
    survivors=$((survivors + 1))
    cp -- "$saved" "$file"
    continue
  fi

  # shellcheck disable=SC2086 -- the args column is deliberately word-split
  if cargo test $test_args >"$BACKUP_DIR/out" 2>&1; then
    echo "    SURVIVED -- no test failed with the guard removed"
    tail -5 "$BACKUP_DIR/out" | sed 's/^/      /'
    survivors=$((survivors + 1))
  else
    echo "    KILLED"
    grep -E '^(test .*FAILED|failures:|---- )' "$BACKUP_DIR/out" | head -6 | sed 's/^/      /'
  fi
  cp -- "$saved" "$file"
done <"$BATTERY"

printf '\n=== %d row(s), %d survivor(s)\n' "$rows" "$survivors"
[ "$survivors" -eq 0 ]
