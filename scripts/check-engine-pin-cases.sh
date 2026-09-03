#!/usr/bin/env bash
# Case matrix for check-engine-pin.sh's single_grid re-evaluation weld
# (the charter's "re-evaluate the knob at every engine-pin bump" promise,
# welded so a bump with no re-evaluation fails the same gate that already
# refuses a hardcoded nvim version). Every case runs the checker against a
# scratch copy of the real repo's pin-relevant files with only the pin
# and/or the doc's re-evaluation line mutated, and asserts both the exit
# status and whether the re-evaluation failure was the one that fired --
# a case expecting the weld to fire must not pass because some other,
# unrelated PIN FAIL happened to make the run non-zero.
#
#   bash scripts/check-engine-pin-cases.sh
#   bash scripts/check-engine-pin-cases.sh --checker /path/to/copy
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
if [ -z "$CHECKER" ]; then
  CHECKER="$(cd "$(dirname "$0")" && pwd)/check-engine-pin.sh"
fi
if [ ! -f "$CHECKER" ]; then
  printf 'checker not found: %s\n' "$CHECKER" >&2
  exit 2
fi
REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"

printf 'checker under test: %s\n' "$CHECKER"

WORK=$(mktemp -d "${TMPDIR:-/tmp}/check-engine-pin-cases.XXXXXX")
trap 'rm -rf "$WORK"' EXIT

n=0
failures=0

# Every other file the checker reads, copied verbatim from the real repo so
# a case's edit to .engine-pin or docs/multigrid.md is the only variable --
# without this, every case would also trip the installer/floating/hardcoded
# checks this weld is not about.
new_case() {
  n=$((n + 1))
  CASE="$WORK/case$n"
  mkdir -p "$CASE/.github/workflows" "$CASE/docs" "$CASE/scripts"
  cp "$REPO_ROOT/.engine-pin" "$CASE/.engine-pin"
  cp "$REPO_ROOT/docs/multigrid.md" "$CASE/docs/multigrid.md"
  cp "$REPO_ROOT"/.github/workflows/*.yml "$CASE/.github/workflows/"
  cp "$REPO_ROOT/.anodizer.yaml" "$CASE/.anodizer.yaml"
  cp "$REPO_ROOT/scripts/package-bundle.sh" "$CASE/scripts/package-bundle.sh"
}

# want_rc: expected exit status. want_reevaluation: "fire" if the run must
# report a re-evaluation PIN FAIL, "silent" if it must not (whether or not
# the run passes overall on some unrelated ground, which none of these
# cases exercise).
expect() {
  want_rc="$1"
  want_reevaluation="$2"
  desc="$3"
  out=$(cd "$CASE" && bash "$CHECKER" 2>&1)
  rc=$?
  fired=silent
  case "$out" in
    *re-evaluat*) fired=fire ;;
  esac
  if [ "$rc" = "$want_rc" ] && [ "$fired" = "$want_reevaluation" ]; then
    printf 'ok %s - %s\n' "$n" "$desc"
    return
  fi
  failures=$((failures + 1))
  printf 'not ok %s - %s\n  want rc=%s reevaluation=%s\n  got  rc=%s reevaluation=%s\n' \
    "$n" "$desc" "$want_rc" "$want_reevaluation" "$rc" "$fired"
  printf '%s\n' "$out" | sed 's/^/  | /'
}

new_case
expect 0 silent 'an untouched pin and doc pass with no re-evaluation finding'

new_case
echo 'v99.0.0' > "$CASE/.engine-pin"
expect 1 fire 'a_pin_bump_without_a_reevaluation_fails'

new_case
grep -v 'Last re-evaluated against engine pin v[0-9]' "$REPO_ROOT/docs/multigrid.md" \
  > "$CASE/docs/multigrid.md"
expect 1 fire 'a missing re-evaluation line fails the same way as a stale one'

# the weld must also be satisfiable, not only trippable: a bump carrying its
# re-evaluation passes, and the header's promise is otherwise a claim that
# only ever fires one way.
new_case
echo 'v99.0.0' > "$CASE/.engine-pin"
sed 's/engine pin v[0-9][0-9.]*/engine pin v99.0.0/' "$REPO_ROOT/docs/multigrid.md" \
  > "$CASE/docs/multigrid.md"
expect 0 silent 'a pin bump whose re-evaluation line was updated with it passes'

# and the comparison is an equality, not a floor: a doc line naming a pin the
# tree has not reached is as wrong as one left behind.
new_case
sed 's/engine pin v[0-9][0-9.]*/engine pin v99.0.0/' "$REPO_ROOT/docs/multigrid.md" \
  > "$CASE/docs/multigrid.md"
expect 1 fire 'a re-evaluation line ahead of the pin fails'

[ "$failures" -eq 0 ] || {
  printf '%d/%d cases failed\n' "$failures" "$n"
  exit 1
}
printf 'check-engine-pin-cases.sh: %d cases ok\n' "$n"
