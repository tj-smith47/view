#!/usr/bin/env bash
# Enforces the pairing the compat suite's migration evidence rests on: every
# adjustment a compat fixture makes on view's behalf sits behind the
# `accommodate` switch a scenario state can clear, and every reach into a
# plugin's private state is declared as one such adjustment.
#
#   bash scripts/check-compat-accommodations.sh            # scan + self-check
#   bash scripts/check-compat-accommodations.sh --root DIR # scan DIR only
#
# Two rules, both falsifiable by the fixtures under
# scripts/test-fixtures/compat-accommodations/ that the default run drives:
#
#   1. a `-- view-compat-accommodation:` marker's next code line must gate on
#      `accommodate`, so a marker cannot label a block that always runs.
#   2. a line reaching a plugin private (`_once`, a `__`-prefixed name, an
#      assignment into `package.loaded[...]`) must sit inside such a gated
#      block, so a suppression cannot arrive unlabelled.
#
# The gated region is the run of lines indented deeper than the gate line
# itself. Lua has no significant whitespace, so this is a convention rather
# than a parse -- but it is the convention every fixture here is written in,
# and a fixture that breaks it fails loudly instead of being read wrong.
# Written to stock POSIX-ish bash (macOS ships /bin/bash 3.2) and BSD awk,
# the same floor the other check-*.sh gates hold.
set -uo pipefail

ROOT=""
SELF_TEST=1
while [ $# -gt 0 ]; do
  case "$1" in
    --root)
      ROOT="${2:-}"
      SELF_TEST=0
      shift 2
      ;;
    -h | --help)
      printf 'usage: %s [--root DIR]\n' "$0"
      exit 0
      ;;
    *)
      printf 'unknown argument: %s\n' "$1" >&2
      exit 2
      ;;
  esac
done

REPO_ROOT=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
[ -n "$ROOT" ] || ROOT="$REPO_ROOT/compat/fixtures"

# reads from a redirect rather than a pipeline so a second offending file is
# still reported: a `while` on the right of a `|` runs in a subshell whose
# accumulated status cannot outlive it
scan() {
  scan_status=0
  while read -r file; do
    awk -v file="$file" '
      function indent_of(line,   n) {
        n = match(line, /[^ \t]/)
        return n == 0 ? 0 : n - 1
      }
      # a gate opened on an earlier line covers every line indented deeper
      # than it; anything at or left of that indent has closed it
      gated && indent_of($0) <= gate_indent && $0 ~ /[^ \t]/ { gated = 0 }
      /--[ \t]*view-compat-accommodation:/ {
        pending = NR
        next
      }
      /^[ \t]*$/ { next }
      /^[ \t]*--/ { next }
      {
        if (pending) {
          if ($0 !~ /accommodate/) {
            printf "%s:%d: accommodation marker is not followed by an accommodate gate\n", file, pending
            bad = 1
          } else {
            gated = 1
            gate_indent = indent_of($0)
          }
          pending = 0
        }
        if (!gated && ($0 ~ /_once/ || $0 ~ /__/ || $0 ~ /package\.loaded\[[^]]*\][ \t]*=/)) {
          printf "%s:%d: reaches a plugin private outside an accommodate gate (add a view-compat-accommodation marker)\n", file, NR
          bad = 1
        }
      }
      END {
        if (pending) {
          printf "%s:%d: accommodation marker has no code line after it\n", file, pending
          bad = 1
        }
        exit bad ? 1 : 0
      }
    ' "$file" || scan_status=1
  done < <(find "$1" -name '*.lua' -type f | sort)
  return "$scan_status"
}

if ! scan "$ROOT"; then
  printf 'ACCOMMODATION FAIL: see the lines above (scripts/check-compat-accommodations.sh)\n' >&2
  exit 1
fi

[ "$SELF_TEST" -eq 1 ] || exit 0

# The gate's own case matrix. Each committed fixture is scanned in isolation:
# a `bad-` case that stops failing means a rule has quietly narrowed, which is
# the failure mode a checker cannot report about itself.
CASES="$REPO_ROOT/scripts/test-fixtures/compat-accommodations"
status=0
for case_file in "$CASES"/*.lua; do
  [ -e "$case_file" ] || {
    printf 'ACCOMMODATION FAIL: no case fixtures under %s\n' "$CASES" >&2
    exit 1
  }
  case_dir=$(dirname -- "$case_file")
  name=$(basename -- "$case_file")
  # one file per scan, so a case cannot pass on another case's output
  work=$(mktemp -d) || exit 1
  cp -- "$case_file" "$work/"
  bash "$0" --root "$work" >/dev/null 2>&1
  rc=$?
  rm -rf -- "$work"
  case "$name" in
    bad-*)
      if [ "$rc" -eq 0 ]; then
        printf 'ACCOMMODATION FAIL: %s must be rejected, but the check passed it\n' "$name" >&2
        status=1
      fi
      ;;
    good-*)
      if [ "$rc" -ne 0 ]; then
        printf 'ACCOMMODATION FAIL: %s must be accepted, but the check rejected it\n' "$name" >&2
        status=1
      fi
      ;;
    *)
      printf 'ACCOMMODATION FAIL: %s must be named bad-* or good-*\n' "$name" >&2
      status=1
      ;;
  esac
done
exit "$status"
