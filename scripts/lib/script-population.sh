#!/usr/bin/env bash
# The population every gate over scripts/ grades: a file whose first line
# names bash or `sh`. Sourced, never run, and shared by check-style.sh's
# comment rules and temp-file walk, check-portability.sh's userland scan and
# check-budget-drift-cases.sh's bash-3.2 parse leg, because four spellings of
# "the scripts" grade four different sets: a rule written over `*.sh` misses
# the eight remote-test fixtures that carry no suffix, one written over
# `scripts/*.sh` misses scripts/acceptance/ as well, and the difference is
# where a finding sits unread.
#
# Sets SCRIPT_POPULATION to the selected paths, one per line, relative to the
# root it is handed. Names on stdout what it passed over, returns 1 naming
# what it could not open, and leaves emptiness to the caller: each gate says
# something different about a tree with nothing to grade.
script_population_read() {
  local root="${1:-.}" entries skipped graded unreadable first
  SCRIPT_POPULATION=""
  # symlinks are listed beside regular files so a dangling one is named by
  # the loop below rather than dropped by the selection: a selection that
  # skips what it cannot open leaves the population short, and a short
  # population grades its survivors and reads exactly like a tree with
  # nothing to report
  entries=$(cd "$root" && find scripts \( -type f -o -type l \) | LC_ALL=C sort)
  # a fifo, socket or device node is readable and could never carry a
  # shebang, so it is named and passed over: reddening a gate for an entry
  # nobody can rewrap is a false red nobody can act on
  skipped=$(cd "$root" && find scripts ! -type d ! -type f ! -type l | LC_ALL=C sort)
  if [ -n "$skipped" ]; then
    printf '%s\n' "$skipped" |
      sed 's/$/: not a regular file or a symlink, the shebang selection passes it over/'
  fi
  # selected by a loop rather than by an xargs awk: xargs splits a path on a
  # blank and awk takes a fatal on the fragment, which drops the file from
  # every rule reading this list with the run still green. Read and select in
  # the one pass, so no entry can pass the readability vet and then be lost
  # by the selection below it
  graded=$(cd "$root" && printf '%s\n' "$entries" | while IFS= read -r entry; do
    [ -n "$entry" ] || continue
    if { [ -f "$entry" ] && [ -r "$entry" ]; } &&
      first=$(head -n 1 "$entry"); then
      case "$first" in ('#!'*bash | '#!'*/sh) printf 'take %s\n' "$entry" ;; esac
    else
      printf 'drop %s\n' "$entry"
    fi
  done)
  unreadable=$(printf '%s\n' "$graded" | sed -n 's/^drop //p')
  if [ -n "$unreadable" ]; then
    printf '%s\n' "$unreadable" | sed 's/$/: the shebang selection cannot read it/'
    return 1
  fi
  # every reader of this name is a sourcing script, which shellcheck does
  # not follow from here
  # shellcheck disable=SC2034
  SCRIPT_POPULATION=$(printf '%s\n' "$graded" | sed -n 's/^take //p')
  return 0
}
