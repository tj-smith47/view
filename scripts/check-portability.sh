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
# A pattern reaches `[[ =~ ]]` three ways, and all three are followed: as a
# literal on the `=~` line, through a variable assigned in the same file,
# and through a positional parameter. The last one is why the readers are
# enumerated rather than named: `matches` takes its pattern as `$1`,
# `wait_for_re` forwards its own `$1` into `matches`, and a leg calling
# either with a literal is as exposed as one writing the pattern inline.
# The enumeration is a fixed point over "takes a positional into `=~`" and
# "calls something that does", so a reader added tomorrow joins it without
# being listed anywhere.
#
# Scope: whole-line comments and here-doc bodies are dropped before scanning,
# so prose naming a banned spelling costs nothing -- the shell hands a here-doc
# body to a program as data and never expands it as a command. `grep -E` is
# deliberately not in scope -- both greps take `\b` there, and
# `scripts/check-style.sh` relies on it.
set -euo pipefail

# Resolved before the cd below, because the helper it sources ships beside
# this script and not under the scan root.
HERE=$(cd -- "$(dirname -- "$0")" && pwd)
# shellcheck source=lib/script-population.sh
. "$HERE/lib/script-population.sh"

# A scan root handed in as the single argument replaces the tree this script
# lives in, which is how the case matrix points it at a fixture tree.
ROOT=${1:-$(cd -- "$HERE/.." && pwd)}
cd "$ROOT"
SELF=$(basename -- "$0")

ESCAPES='\\[bswd]'
# anchored on a non-word character so that `pgrep -P`, a different program
# with a different flag, is not read as `grep -P`
UTILITIES='(^|[^[:alnum:]_])(stat[[:space:]]+-c|sed[[:space:]]+-z|grep[[:space:]]+-[A-Za-z]*P|readlink[[:space:]]+-f|date[[:space:]]+-d)'
WORD='(^|[^[:alnum:]_-])'

fail=0

# Rules shared by both passes: drop whole-line comments, and drop the bodies
# of the here-docs a line opens. The tags a line opens come from the reader
# beside the population, which is where the one here-doc tokenizer in this
# tree lives; their bodies arrive in the order the tags were written, so a
# queue tracks them rather than a single tag. The introducing line still gets
# scanned; only the body and its terminator are dropped. A tag still open when
# a file ends means that tokenizer read something as an opener that the shell
# does not, so it stops the run loudly instead of narrowing the scan in
# silence.
SKIP="$SCRIPT_HEREDOC_AWK"'function unterminated() {
  printf("PORTABILITY-SELF-FAIL: %s opens a here-doc tagged %s that no later line closes; the scan would read the rest of that file as data\n", opened[qh], substr(queue[qh], 2)) > "/dev/stderr"
  exit 2
}
BEGIN { qh = 1; qn = 0 }
FNR == 1 { if (qn >= qh) unterminated(); qh = 1; qn = 0; fn = "" }
qn >= qh {
  line = $0
  cur = queue[qh]
  if (substr(cur, 1, 1) == "-") { sub(/^\t+/, "", line) }
  cur = substr(cur, 2)
  if (line == cur) qh += 1
  next
}
/^[[:space:]]*#/ { next }
{
  found = tags_of($0)
  if (found != "") {
    m = split(found, tags, "\n")
    for (k = 1; k <= m; k += 1) {
      if (tags[k] == "") continue
      qn += 1
      queue[qn] = tags[k]
      opened[qn] = FILENAME ":" FNR
    }
  }
}
END { if (qn >= qh) unterminated() }
'

# every line the shell would run, numbered
code() { awk -v SQ="'" "$SKIP"'{ print FNR ":" $0 }' "$1"; }

report() {
  printf 'PORTABILITY FAIL: %s -- %s\n' "$1" "$2"
  fail=1
}

# The shebang selection under scripts/, which is what check-style.sh and
# check-budget-drift-cases.sh grade, plus the hooks and the Taskfile: a
# `*.sh` spelling of the same set left the eight suffix-less remote-test
# fixtures unscanned, and they run on the macOS host this scan is about.
# Taskfile.yml stays last, so a here-doc tag one of the scripts leaves open
# is reported against the file that opened it.
if ! script_population_read; then
  echo "PORTABILITY FAIL: scripts -- a file under scripts/ cannot be read" >&2
  exit 1
# the hooks and the Taskfile alone would still report `1 files clean`, which
# is what a scan that reached every script also says; the two sibling gates
# over this population both redden here
elif [ -z "$SCRIPT_POPULATION" ]; then
  echo "PORTABILITY FAIL: scripts -- no file under $(pwd) whose first line names bash or sh" >&2
  exit 1
fi
targets=()
while IFS= read -r file; do
  [ -n "$file" ] || continue
  # this script names every banned spelling literally in order to define the
  # patterns above, so it is out of its own scan
  case "$file" in "scripts/$SELF") continue ;; esac
  targets+=("$file")
done <<EOF
$SCRIPT_POPULATION
$(find .claude/hooks -type f -name '*.sh' ! -name "$SELF" | LC_ALL=C sort)
EOF
targets+=(Taskfile.yml)

for file in "${targets[@]}"; do
  body=$(code "$file")

  matched=$(printf '%s\n' "$body" | grep -E '=~' | grep -E "$ESCAPES" || true)
  if [ -n "$matched" ]; then
    report "$file" "a [[ =~ ]] pattern spells a word class the BSD regex engine has no form of; write it as a character class instead:
$matched"
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

# Every non-comment line tagged with the function that encloses it, so a
# body can be asked what it does with its own arguments. A function runs
# from its `name() {` line to the first `}` in column one, which is how
# every function in this tree is written; a one-liner closes on its own
# line.
annotate() {
  awk -v SQ="'" -v S="$SCRIPT_FIELD_SEP" "$SKIP"'
    {
      if (fn == "" && match($0, /^[A-Za-z_][A-Za-z0-9_-]*\(\)[[:space:]]*\{/)) {
        name = $0; sub(/\(\).*/, "", name)
        print FILENAME S FNR S name S $0
        if ($0 !~ /\}[[:space:]]*$/) fn = name
        next
      }
      if (fn != "" && $0 ~ /^\}/) { print FILENAME S FNR S fn S $0; fn = ""; next }
      print FILENAME S FNR S (fn == "" ? "-" : fn) S $0
    }
  ' "$@"
}

ANNOTATED=$(annotate "${targets[@]}")

# seed: a body that puts one of its own positionals on the right of `=~`
readers=$(printf '%s\n' "$ANNOTATED" |
  awk -F"$SCRIPT_FIELD_SEP" '$3 != "-" && $4 ~ /=~[[:space:]]*"?[$][{]?[0-9]/ { print $3 }' | sort -u | grep . || true)
# closure: a body that hands a reader nothing but a variable is a forwarder,
# and the pattern that variable holds is whatever ITS caller wrote -- so the
# forwarder's own call sites are call sites of the reader. A body passing a
# literal instead is not a forwarder: that literal is already read where it
# is written, one loop below. A forwarder that splices a positional into a
# larger pattern is the case this does not reach.
FORWARDS='[[:space:]]+"?[$][{]?[A-Za-z_][A-Za-z0-9_]*[}]?"?([[:space:]]|$)'
while [ -n "$readers" ]; do
  alternation=$(printf '%s\n' "$readers" | tr '\n' '|' | sed 's/|$//')
  grown=$(printf '%s\n%s\n' "$readers" "$(printf '%s\n' "$ANNOTATED" |
    awk -F"$SCRIPT_FIELD_SEP" -v re="$WORD($alternation)$FORWARDS" '$3 != "-" && $4 ~ re { print $3 }')" |
    sort -u | grep . || true)
  [ "$grown" = "$readers" ] && break
  readers=$grown
done

for reader in $readers; do
  sites=$(printf '%s\n' "$ANNOTATED" |
    awk -F"$SCRIPT_FIELD_SEP" -v re="$WORD$reader[[:space:]]+[\"']" '$4 ~ re { print $1 ":" $2 ":" $4 }' || true)
  [ -n "$sites" ] || continue
  while IFS= read -r site; do
    [ -n "$site" ] || continue
    literal=$(printf '%s\n' "$site" | grep -oE "'[^']*'|\"[^\"]*\"" | grep -E "$ESCAPES" || true)
    if [ -n "$literal" ]; then
      report "${site%%:*}" "a literal argument to \`$reader\`, which reads it as a regex, carries a glibc-only word class:
$site"
    fi
  done <<EOF
$sites
EOF
done

if [ "$fail" -ne 0 ]; then
  exit 1
fi
printf 'portability: %s files clean, %s regex readers followed to their call sites (%s)\n' \
  "${#targets[@]}" "$(printf '%s\n' "$readers" | grep -c .)" "$(printf '%s\n' "$readers" | tr '\n' ' ' | sed 's/ $//')"
