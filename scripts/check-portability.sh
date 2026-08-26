#!/usr/bin/env bash
# The shell surfaces whose behaviour differs between the GNU userland CI's
# ubuntu runner has and the BSD one its macOS runner has.
#
# Two of them have already shipped a guard that failed OPEN on macOS:
#
#   `[[ =~ ]]` compiles its pattern with the platform libc's
#     `regcomp(REG_EXTENDED)`, where `\b`, `\s`, `\w` and `\d` are glibc
#     extensions. On BSD libc such a pattern simply never matches, so a
#     check written with one answers "clean" for every input.
#   `stat -c`, `sed -z`, `grep -P`, `readlink -f` and `date -d` are GNU
#     spellings whose BSD counterpart takes a different flag entirely. The
#     BSD side errors out, and an error inside a hook is a non-blocking
#     exit rather than a refusal.
#
# Both are invisible to a linux-only test run, which is why they are pinned
# here rather than left to the cases that exercise the behaviour.
#
# Scope: whole-line comments are dropped before scanning, so prose naming a
# banned spelling costs nothing. A pattern reaching `[[ =~ ]]` as a
# positional parameter (`matches "$pattern"`) is not resolved -- the callers
# passing those live in the acceptance legs, and following them would take a
# dataflow pass this does not have. A pattern named by a variable IS
# followed, to its assignments in the same file.
set -euo pipefail

ROOT=$(cd -- "$(dirname -- "$0")/.." && pwd)
cd "$ROOT"
SELF=$(basename -- "$0")

ESCAPES='\\[bswd]'
# anchored on a non-word character so that `pgrep -P`, a different program
# with a different flag, is not read as `grep -P`
UTILITIES='(^|[^[:alnum:]_])(stat[[:space:]]+-c|sed[[:space:]]+-z|grep[[:space:]]+-[A-Za-z]*P|readlink[[:space:]]+-f|date[[:space:]]+-d)'

fail=0

# every line that is not wholly a comment, numbered
code() { grep -nvE '^[[:space:]]*#' "$1" || true; }

report() {
  printf 'PORTABILITY FAIL: %s -- %s\n' "$1" "$2"
  fail=1
}

targets=()
while IFS= read -r file; do
  targets+=("$file")
done < <(find .claude/hooks scripts -type f -name '*.sh' ! -name "$SELF" | sort)
targets+=(Taskfile.yml)

for file in "${targets[@]}"; do
  body=$(code "$file")

  matches=$(printf '%s\n' "$body" | grep -E '=~' | grep -E "$ESCAPES" || true)
  if [ -n "$matches" ]; then
    report "$file" "a [[ =~ ]] pattern spells a word class the BSD regex engine has no form of; write it as a character class instead:
$matches"
  fi

  names=$(printf '%s\n' "$body" |
    grep -oE '=~[[:space:]]*"?\$\{?[A-Za-z_][A-Za-z0-9_]*' |
    grep -oE '[A-Za-z_][A-Za-z0-9_]*$' | sort -u || true)
  for name in $names; do
    assigned=$(printf '%s\n' "$body" |
      grep -E "^[0-9]+:[[:space:]]*(local[[:space:]]+|readonly[[:space:]]+|export[[:space:]]+)?${name}=" |
      grep -E "$ESCAPES" || true)
    if [ -n "$assigned" ]; then
      report "$file" "\$$name reaches [[ =~ ]] carrying a glibc-only word class; write it as a character class instead:
$assigned"
    fi
  done

  used=$(printf '%s\n' "$body" | grep -E "$UTILITIES" || true)
  if [ -n "$used" ]; then
    report "$file" "a GNU-only utility spelling; both userlands must accept it:
$used"
  fi
done

if [ "$fail" -ne 0 ]; then
  exit 1
fi
printf 'portability: %s files clean\n' "${#targets[@]}"
