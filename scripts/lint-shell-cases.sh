#!/usr/bin/env bash
# Case matrix for scripts/lint-shell.sh: every shape of `shellcheck
# disable` directive it refuses, the ones it passes, and a failing
# `git ls-files`. The tree carries no refused directive, so without these a
# directive walk that stopped matching would still read "scripts clean".
# Each case builds a throwaway git repo holding the checker, the population
# lib it sources and the planted scripts, and runs the checker there.
#
#   bash scripts/lint-shell-cases.sh
#   bash scripts/lint-shell-cases.sh --checker /path/to/copy
set -uo pipefail

CHECKER=""
while [ $# -gt 0 ]; do
  case "$1" in
    --checker)
      CHECKER="${2:-}"
      shift 2
      ;;
    -h | --help)
      printf 'usage: %s [--checker PATH]\n' "$0"
      exit 0
      ;;
    *)
      printf 'unknown argument: %s\n' "$1" >&2
      exit 2
      ;;
  esac
done
HERE="$(cd "$(dirname "$0")" && pwd)"
if [ -z "$CHECKER" ]; then
  CHECKER="$HERE/lint-shell.sh"
fi
if [ ! -f "$CHECKER" ]; then
  printf 'checker not found: %s\n' "$CHECKER" >&2
  exit 2
fi
printf 'checker under test: %s\n' "$CHECKER"

# shellcheck source=lib/scratch.sh
. "$HERE/lib/scratch.sh"
WORK=$(mktemp -d "$(scratch_root)/lint-shell-cases-XXXXXX")
trap 'rm -rf "$WORK"' EXIT

n=0
failures=0

new_case() {
  n=$((n + 1))
  CASE="$WORK/case$n"
  mkdir -p "$CASE/scripts/lib"
  cp "$CHECKER" "$CASE/scripts/lint-shell.sh"
  cp "$HERE/lib/script-population.sh" "$CASE/scripts/lib/script-population.sh"
  (cd "$CASE" && git init -q && git config user.email t@t.t && git config user.name t)
}

# a planted script, its body read from stdin under a bash shebang
plant() {
  { printf '#!/usr/bin/env bash\n'; cat; } >"$CASE/scripts/$1"
}

commit_case() {
  (cd "$CASE" && git add -A && git commit -q -m case)
}

# want_lines counts the output lines naming the planted file, so one
# directive refused on several grounds is seen to give one message
expect() {
  local want_rc="$1" want="$2" want_lines="$3" desc="$4" out rc lines
  out=$(cd "$CASE" && PATH="${CASE_PATH:-$PATH}" bash scripts/lint-shell.sh 2>&1)
  rc=$?
  lines=$(printf '%s\n' "$out" | grep -c '^scripts/[pqr]\.sh:' || true)
  case "$out" in
    *"$want"*)
      if [ "$rc" = "$want_rc" ] && [ "$lines" = "$want_lines" ]; then
        printf 'ok %s - %s\n' "$n" "$desc"
        return
      fi
      ;;
  esac
  failures=$((failures + 1))
  printf 'not ok %s - %s\n  want rc=%s lines=%s text=%s\n  got  rc=%s lines=%s\n' \
    "$n" "$desc" "$want_rc" "$want_lines" "$want" "$rc" "$lines"
  printf '%s\n' "$out" | sed 's/^/  | /'
}

new_case
plant p.sh <<'EOF'
set -eu
echo clean
EOF
commit_case
expect 0 'scripts clean' 0 'a script with no directive passes'

new_case
plant p.sh <<'EOF'
set -eu
# read by the sourcing scripts, which shellcheck does not follow from here
# shellcheck disable=SC2034
UNUSED=1
EOF
commit_case
expect 0 'scripts clean' 0 'a directive under a reader comment, directly above its command, passes'

new_case
plant p.sh <<'EOF'
set -eu
# shellcheck disable=SC2034
UNUSED=1
EOF
commit_case
expect 1 'scripts/p.sh:3: a shellcheck disable has no comment above it naming the reader' 1 \
  'a directive with no comment above it is refused'

new_case
plant p.sh <<'EOF'
set -eu
# already fine, thread safe, see README
# shellcheck disable=SC2034
UNUSED=1
EOF
commit_case
expect 1 'scripts/p.sh:4: a shellcheck disable has no comment above it naming the reader' 1 \
  'a comment carrying read only inside another word names no reader'

new_case
plant p.sh <<'EOF'
set -eu
# shellcheck source=/dev/null
# shellcheck disable=SC2034
UNUSED=1
EOF
commit_case
expect 1 'scripts/p.sh:4: a shellcheck disable has no comment above it naming the reader' 1 \
  'a source directive above a disable names no reader'

new_case
plant p.sh <<'EOF'
set -eu
# read by the sourcing scripts, which shellcheck does not follow from here
# shellcheck disable=SC2034

UNUSED=1
EOF
commit_case
expect 1 'scripts/p.sh:4: a shellcheck disable has a blank or a comment under it' 1 \
  'a directive with a blank under it is refused'

new_case
plant p.sh <<'EOF'
set -eu
# read by the sourcing scripts, which shellcheck does not follow from here
# shellcheck disable=SC2034
# the value the caller reads
UNUSED=1
EOF
commit_case
expect 1 'scripts/p.sh:4: a shellcheck disable has a blank or a comment under it' 1 \
  'a directive with a comment under it is refused'

# the directive covers every later SC2034, the planted unused variable among
# them, which is how a file-wide directive hid one before this rule
new_case
plant p.sh <<'EOF'
# read by the sourcing scripts, which shellcheck does not follow from here
# shellcheck disable=SC2034
READ_ELSEWHERE=1
FORGOTTEN=2
EOF
commit_case
expect 1 'scripts/p.sh:3: a shellcheck disable sits ahead of the first command of the file, where it covers the whole file' 1 \
  'a directive ahead of the first command is refused'

new_case
plant p.sh <<'EOF'
set -eu
# read by the sourcing scripts, which shellcheck does not follow from here
# shellcheck disable=SC2034
# shellcheck disable=SC2086
UNUSED=$1
EOF
commit_case
expect 1 'scripts/p.sh:4: a shellcheck disable is stacked on another, and the codes join with a comma in one directive (disable=SC2034,SC2086)' 1 \
  'stacked directives give one message, on the first, naming the comma join'

new_case
plant p.sh <<'EOF'
set -eu
# read by the trap at exit, which shellcheck does not follow from here
# shellcheck disable=SC2016,SC2154
# shellcheck disable=SC2064 # expanded now
trap "echo $x" EXIT
EOF
commit_case
expect 1 'scripts/p.sh:4: a shellcheck disable is stacked on another, and the codes join with a comma in one directive (disable=SC2016,SC2154,SC2064)' 1 \
  'the stacked message names the codes the two directives carry'

new_case
plant p.sh <<'EOF'
# shellcheck disable=SC2034

UNUSED=1
EOF
commit_case
expect 1 'scripts/p.sh:2: a shellcheck disable has no comment above it naming the reader shellcheck cannot see; sits ahead of the first command of the file, where it covers the whole file; has a blank or a comment under it' 1 \
  'a directive refused on three grounds gives one message naming all three'

new_case
plant p.sh <<'EOF'
set -eu
# read by the sourcing scripts, which shellcheck does not follow from here
# shellcheck disable=SC2034
EOF
plant q.sh <<'EOF'
set -eu
echo clean
EOF
commit_case
expect 1 'scripts/p.sh:4: a shellcheck disable is on the last line of the file and covers no command' 1 \
  'a directive on the last line is reported before the next file starts'

new_case
plant q.sh <<'EOF'
set -eu
echo clean
EOF
plant r.sh <<'EOF'
set -eu
# read by the sourcing scripts, which shellcheck does not follow from here
# shellcheck disable=SC2034
EOF
commit_case
expect 1 'scripts/r.sh:4: a shellcheck disable is on the last line of the file and covers no command' 1 \
  'a directive on the last line of the last file is reported'

new_case
plant p.sh <<'OUTER'
set -eu
cat <<'EOF'
# shellcheck disable=SC2034
EOF
printf '%s\n' '
# shellcheck disable=SC2034
'
OUTER
commit_case
expect 0 'scripts clean' 0 'a directive in a here-doc body or a quoted string is text and passes'

# task commit runs the lint before it stages a new file, so a script git
# does not track yet is linted too
new_case
plant p.sh <<'EOF'
set -eu
echo clean
EOF
commit_case
mkdir -p "$CASE/compat"
printf '#!/usr/bin/env bash\nset -eu\n# shellcheck disable=SC2034\nUNUSED=1\n' >"$CASE/compat/u.sh"
expect 1 'compat/u.sh:3: a shellcheck disable has no comment above it naming the reader' 0 \
  'an untracked script outside scripts/ is linted'

new_case
plant p.sh <<'EOF'
set -eu
echo clean
EOF
commit_case
mkdir -p "$CASE/fakebin"
printf '#!/bin/sh\nexit 1\n' >"$CASE/fakebin/git"
chmod +x "$CASE/fakebin/git"
CASE_PATH="$CASE/fakebin:$PATH" expect 1 'lint:shell: git ls-files failed' 0 \
  'a failing git ls-files fails the lint'

[ "$failures" -eq 0 ] || {
  printf '%d/%d cases failed\n' "$failures" "$n"
  exit 1
}
printf 'lint-shell-cases.sh: %d cases ok\n' "$n"
