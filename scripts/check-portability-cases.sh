#!/usr/bin/env bash
# Case matrix for check-portability.sh. Every case builds a scratch tree,
# points the scanner at it, and asserts BOTH the exit status and the exact
# set of file:line:category tokens reported: a case expecting one finding has to fail
# when a second one is reported, and a case expecting silence has to fail
# when the scan narrows itself and reports nothing for the wrong reason.
#
#   bash scripts/check-portability-cases.sh
#   bash scripts/check-portability-cases.sh --scanner /path/to/copy
#
# Written to stock POSIX-ish bash: macOS ships /bin/bash 3.2, and the
# scanner's own portability (BSD awk, BSD sed, BSD grep) is only proven by
# running this there. Every blind spot this scanner has had was found with
# throwaway fixtures; the cases below are those fixtures frozen, so the next
# one trips here instead of being found by hand again.
set -uo pipefail

SCANNER=""
while [ $# -gt 0 ]; do
  case "$1" in
    --scanner)
      SCANNER="${2:-}"
      shift 2
      ;;
    -h | --help)
      printf 'usage: %s [--scanner PATH]\n' "$0"
      exit 0
      ;;
    *)
      printf 'unknown argument: %s\n' "$1" >&2
      exit 2
      ;;
  esac
done
# Resolved from this file's own directory rather than $PWD, so a run started
# from anywhere grades the scanner that ships beside these cases.
if [ -z "$SCANNER" ]; then
  SCANNER="$(cd "$(dirname "$0")" && pwd)/check-portability.sh"
fi
if [ ! -f "$SCANNER" ]; then
  printf 'scanner not found: %s\n' "$SCANNER" >&2
  exit 2
fi

printf 'scanner under test: %s\n' "$SCANNER"

WORK=$(mktemp -d "${TMPDIR:-/tmp}/check-portability-cases.XXXXXX")
trap 'rm -rf "$WORK"' EXIT

n=0
failures=0
CASE=""

# The scan set is `.claude/hooks` plus `scripts` plus Taskfile.yml, so every
# scratch tree carries all three whether or not the case populates them.
new_case() {
  n=$((n + 1))
  CASE="$WORK/case$n"
  mkdir -p "$CASE/.claude/hooks" "$CASE/scripts"
  : > "$CASE/Taskfile.yml"
}

write() { cat > "$CASE/$1"; }

# A refusal prints one header naming the file and then the offending lines,
# numbered by the pass that found them: the two content passes emit a bare
# `N:` prefix under their header, the call-site pass emits `file:N:`. Both
# collapse to a file:line:category token so a case states one expected set.
# The category comes from the header's message, because a location alone does
# not say WHY the line was refused: a regression that reports the right line
# under the wrong check would otherwise grade as a pass. The message text
# itself is not compared -- rewording a diagnostic is not a regression, so the
# four messages collapse to the four checks that emit them.
findings() {
  awk '
    /^PORTABILITY-SELF-FAIL: / { print $2 ":selffail"; next }
    /^PORTABILITY FAIL: / {
      h = $0
      sub(/^PORTABILITY FAIL: /, "", h)
      f = h
      sub(/ -- .*$/, "", f)
      m = h
      sub(/^[^ ]* -- /, "", m)
      cat = "other"
      if (index(m, "a [[ =~ ]] pattern") == 1) cat = "inline"
      else if (substr(m, 1, 1) == "$") cat = "var"
      else if (index(m, "a GNU-only utility") == 1) cat = "util"
      else if (index(m, "a literal argument to") == 1) cat = "literal"
      next
    }
    /^[0-9]+:/ { k = $0; sub(/:.*$/, "", k); print f ":" k ":" cat; next }
    /^[^:]+:[0-9]+:/ { split($0, a, ":"); print a[1] ":" a[2] ":" cat; next }
  ' | sort -u | tr '\n' ' ' | sed 's/ *$//'
}

expect() {
  want_rc="$1"
  want="$2"
  desc="$3"
  out=$(bash "$SCANNER" "$CASE" 2>&1)
  rc=$?
  got=$(printf '%s\n' "$out" | findings)
  if [ "$rc" = "$want_rc" ] && [ "$got" = "$want" ]; then
    printf 'ok %s - %s\n' "$n" "$desc"
    return
  fi
  failures=$((failures + 1))
  printf 'not ok %s - %s\n  want rc=%s findings [%s]\n  got  rc=%s findings [%s]\n' \
    "$n" "$desc" "$want_rc" "$want" "$rc" "$got"
  printf '%s\n' "$out" | sed 's/^/  | /'
}

# The reader enumeration is a fixed point over the whole tree, so the cases
# that exercise it plant the same two-file definition every time.
plant_readers() {
  write scripts/aa-readers.sh <<'READERS'
#!/usr/bin/env bash
matches() { [[ $2 =~ $1 ]]; }

wait_for_re() {
  local pattern="$1"
  matches "$pattern" "$(pane)"
}

anchored() {
  local pattern="$1"
  matches "^$pattern" "$(pane)"
}
READERS
}

# ---------------------------------------------------------------------------
# the three ways a pattern reaches [[ =~ ]]
# ---------------------------------------------------------------------------
new_case
write scripts/case.sh <<'SH'
#!/usr/bin/env bash
x=$1
if [[ $x =~ \bfoo ]]; then :; fi
SH
expect 1 'scripts/case.sh:3:inline' 'a glibc word class written inline on the match line'

new_case
write scripts/case.sh <<'SH'
#!/usr/bin/env bash
PAT='\bfoo'
if [[ $x =~ $PAT ]]; then :; fi
SH
expect 1 'scripts/case.sh:2:var' 'a named variable carrying the word class into the match'

new_case
plant_readers
write scripts/case.sh <<'SH'
#!/usr/bin/env bash
matches '\bfoo' "$CAP"
SH
expect 1 'scripts/case.sh:2:literal' 'a literal handed to a reader that puts its own positional into the match'

new_case
plant_readers
write scripts/case.sh <<'SH'
#!/usr/bin/env bash
wait_for_re '\sbar' 5
SH
expect 1 'scripts/case.sh:2:literal' 'a literal handed to a forwarder, one hop from the reader'

new_case
write scripts/case.sh <<'SH'
#!/usr/bin/env bash
stat -c %Y f
sed -z s/a/b/ f
grep -P foo f
readlink -f f
date -d yesterday
SH
expect 1 'scripts/case.sh:2:util scripts/case.sh:3:util scripts/case.sh:4:util scripts/case.sh:5:util scripts/case.sh:6:util' 'all five GNU-only utility spellings'

new_case
write scripts/case.sh <<'SH'
#!/usr/bin/env bash
pgrep -P 1
SH
expect 0 '' 'a different program whose flag only looks like the GNU one'

new_case
write scripts/case.sh <<'SH'
#!/usr/bin/env bash
# prose about \bfoo on a match line, and about stat -c too
#   even indented, and even mentioning grep -P
echo ok
SH
expect 0 '' 'whole-line comments naming the banned spellings'

new_case
write scripts/case.sh <<'SH'
#!/usr/bin/env bash
case "$1" in
  a) [[ $x =~ \bfoo ]] ;;
esac
SH
expect 1 'scripts/case.sh:3:inline' 'a real match hidden inside a case arm'

new_case
plant_readers
write scripts/case.sh <<'SH'
#!/usr/bin/env bash
matches '^foo$' "$CAP"
SH
expect 0 '' 'a literal caller whose pattern uses no banned class'

# ---------------------------------------------------------------------------
# the stated ceiling: a forwarder that splices its positional into a larger
# pattern is not followed, so its call sites are not reached. Expected clean
# today; the day the closure grows to cover splicing, this case flips to a
# finding and says so out loud.
# ---------------------------------------------------------------------------
new_case
plant_readers
write scripts/case.sh <<'SH'
#!/usr/bin/env bash
anchored '\bfoo' "$CAP"
SH
expect 0 '' 'ceiling: a positional spliced into a larger pattern is not followed'

# ---------------------------------------------------------------------------
# here-doc bodies are data the shell never runs
# ---------------------------------------------------------------------------
new_case
write scripts/case.sh <<'SH'
#!/usr/bin/env bash
cat <<EOF
a word class \bfoo on a =~ line, and stat -c beside it
EOF
cat <<'EOF'
another \sbar on a =~ line, and sed -z beside it
EOF
cat <<"EOF"
a third \wbaz on a =~ line, and grep -P beside it
EOF
SH
expect 0 '' 'bodies of the bare, single-quoted and double-quoted tag forms'

# the tabs below are literal: `<<-` strips tabs and nothing else, so a run
# whose fixture lost them reports the terminator unclosed rather than passing
new_case
write scripts/case.sh <<'SH'
#!/usr/bin/env bash
cat <<-EOF
	a word class \bfoo on a =~ line inside the body
	EOF
	[[ $x =~ \bfoo ]]
SH
expect 1 'scripts/case.sh:5:inline' 'a tab-indented body is skipped while a tab-indented call site is not'

new_case
write scripts/case.sh <<'SH'
#!/usr/bin/env bash
read -r v <<<"literal" ; [[ $v =~ \bfoo ]]
[[ $y =~ \sbar ]]
SH
expect 1 'scripts/case.sh:2:inline scripts/case.sh:3:inline' 'a here-string opens no body, so its own line and the next stay scanned'

new_case
write scripts/case.sh <<'SH'
#!/usr/bin/env bash
cmd <<A <<B
body of A carries \bfoo on a =~ line
A
body of B carries \sbar on a =~ line
B
[[ $x =~ \wbaz ]]
SH
expect 1 'scripts/case.sh:7:inline' 'two tags on one line consume two bodies, in the order written'

new_case
write scripts/case.sh <<'SH'
#!/usr/bin/env bash
cat <<\EOF
a word class \bfoo on a =~ line inside a backslash-tagged body
EOF
cat <<-\EOF
	a word class \sbar on a =~ line inside a dashed backslash-tagged body
	EOF
[[ $x =~ \wbaz ]]
SH
expect 1 'scripts/case.sh:8:inline' 'the backslash-escaped tag form is a here-doc like the other three'

# ---------------------------------------------------------------------------
# a `<<` the shell does not read as an opener must not be read as one here:
# treating it as one swallows every line after it, which turns a live finding
# into silence -- the one direction this scan may never fail in
# ---------------------------------------------------------------------------
new_case
write scripts/case.sh <<'SH'
#!/usr/bin/env bash
msg="the operator is <<EOF, see the manual"
PAT='\bshould_not_be_hidden'
[[ $x =~ $PAT ]]
SH
expect 1 'scripts/case.sh:3:var' 'a tag spelling inside a quoted string opens nothing'

new_case
write scripts/case.sh <<'SH'
#!/usr/bin/env bash
echo hi # see <<EOF for the syntax
PAT='\bshould_not_be_hidden'
[[ $x =~ $PAT ]]
SH
expect 1 'scripts/case.sh:3:var' 'a tag spelling inside a trailing comment opens nothing'

# an ANSI-C string keeps its own escapes: `\'` inside `$'...'` is a literal
# quote that does not end the string, so a tag spelling after one is still
# inside the string
new_case
write scripts/case.sh <<'SH'
#!/usr/bin/env bash
msg=$'don\'t<<EOF really'
[[ $x =~ \bfoo ]]
SH
expect 1 'scripts/case.sh:3:inline' 'a tag spelling inside an ANSI-C string opens nothing, escaped quote and all'

# ---------------------------------------------------------------------------
# `<<` inside an arithmetic context is a left shift, and the operand after it
# is a number rather than a tag
# ---------------------------------------------------------------------------
new_case
write scripts/case.sh <<'SH'
#!/usr/bin/env bash
n=$((a<<3))
SH
expect 0 '' 'an arithmetic left shift opens no body and does not stop the run'

new_case
write scripts/case.sh <<'SH'
#!/usr/bin/env bash
n=$((a<<3))
[[ $x =~ \bfoo ]]
3
[[ $y =~ \sbar ]]
SH
expect 1 'scripts/case.sh:3:inline scripts/case.sh:5:inline' 'a later line spelling the shift operand terminates nothing, so both call sites stay scanned'

# ---------------------------------------------------------------------------
# a file ending inside an unclosed function must not carry that function name
# into the next file: the reader seed reads the name, so a leak renames the
# reader and every one of its call sites goes unchecked
# ---------------------------------------------------------------------------
new_case
write scripts/aa-open.sh <<'SH'
#!/usr/bin/env bash
leftopen() {
  :
SH
write scripts/ab-readers.sh <<'SH'
#!/usr/bin/env bash
matches() { [[ $2 =~ $1 ]]; }
SH
write scripts/case.sh <<'SH'
#!/usr/bin/env bash
matches '\bfoo' "$CAP"
SH
expect 1 'scripts/case.sh:2:literal' 'an unclosed function in one file does not retag the next file'

# ---------------------------------------------------------------------------
# a tag that no later line closes is this tokenizer reading an opener the
# shell would not, so the run stops loudly instead of scanning less
# ---------------------------------------------------------------------------
new_case
write scripts/case.sh <<'SH'
#!/usr/bin/env bash
cat <<NEVERCLOSED
still body
SH
expect 2 'scripts/case.sh:2:selffail' 'an unterminated tag stops the scan instead of narrowing it'

# the same refusal at the other end of the scan: a tag left open by the last
# file in scan order is caught when the scan runs out of input rather than
# when it reaches the next file
new_case
write Taskfile.yml <<'YML'
version: '3'
tasks:
  age:
    cmd: cat <<NEVERCLOSED
YML
expect 2 'Taskfile.yml:4:selffail' 'a tag left open by the last file scanned stops the run at the end of the scan'

# ---------------------------------------------------------------------------
# the whole scan set: Taskfile.yml is a target too, and the guard revision
# below is the one whose four findings this scanner was written for
# ---------------------------------------------------------------------------
new_case
write Taskfile.yml <<'YML'
version: '3'
tasks:
  age:
    cmd: stat -c %Y f
YML
expect 1 'Taskfile.yml:4:util' 'a banned spelling in a task command line'

new_case
write .claude/hooks/validate-commands.sh <<'HISTORIC_GUARD'
#!/usr/bin/env bash
set -euo pipefail
INPUT=$(cat)
COMMAND=$(printf %s "$INPUT" | jq -r '.tool_input.command // empty' 2>/dev/null)
[[ -z "$COMMAND" ]] && exit 0
# The command is flattened to one line, then quotes and backslashes are dropped,
# so that `git "push"` and a continuation split mid-word cannot hide the
# subcommand from the greps below. Two spans are removed before that flattening
# rather than after, because dropping the quotes around them is what let text
# that is never executed reach a grep looking for commands:
#
#   -m / --message arguments -- a message is data, so `task commit -- -m "fix:
#     the git hook blocks push"` was refused by the guard it describes, and a
#     guard that refuses the sentence explaining it is one people route around.
#     Nothing about a real push is expressed through -m: `git push -m x` still
#     reads as a push here, because the subcommand precedes the flag.
#   `git stash push` -- a local stash, not a remote one. Anchored on `git`
#     immediately preceding `stash` so that `git -C stash push`, where `push`
#     genuinely is the subcommand and `stash` is a directory, keeps being read
#     as a push.
#
# A message span is only recognised where a flag can actually begin: the start
# of the flattened line, or a space. A looser leading boundary that merely
# excluded word characters also matched `=`, so the tail of an attached value
# read as a flag and the word behind it was deleted as its argument -- in
# `git -c a.b=-m push` that word is the subcommand, and git accepts that line,
# so a real push reached the allow path. For the same reason an unquoted value
# stops at `;`, `&` and `|`: those end the command rather than the word, and
# consuming past one would swallow the command that follows it.
#
# Line continuations are joined with awk rather than `sed -z`, which is a GNU
# extension: BSD sed rejects the option outright, and under `set -o pipefail`
# that aborts the guard before either check runs, so the hook would fail OPEN
# on macOS and let an unrequested push through. For the same reason the
# expressions below spell their word boundaries as character classes: `\b` in
# sed is GNU-only and no-ops on BSD sed, which would silently restore both
# false positives on macOS only.
SQ=\'
FLAT=$(printf %s "$COMMAND" |
  awk '{ if (sub(/\\$/, "")) printf "%s", $0; else print }' |
  tr '\n' ' ' |
  tr -d '\\' |
  sed -E \
    -e "s/(^|[[:space:]])(-m|--message)[[:space:]=]*\"[^\"]*\"/\1/g" \
    -e "s/(^|[[:space:]])(-m|--message)[[:space:]=]*${SQ}[^${SQ}]*${SQ}/\1/g" \
    -e "s/(^|[[:space:]])(-m|--message)[[:space:]=]+[^-[:space:];&|][^[:space:];&|]*/\1/g" \
    -e "s/([^[:alnum:]_]|^)git[[:space:]]+stash[[:space:]]+push([^[:alnum:]_]|\$)/\1git stash\2/g" |
  tr -d "'\"")
# A singular, deliberate push is not refused here: it falls through to the
# permission layer, whose ask-rule surfaces the exact command for interactive
# approval. Only the standalone form qualifies -- the RAW command, one line,
# literally starting `git push`, with no chaining, substitution, redirection,
# expansion or escapes in its arguments -- so a push can never ride in on the
# tail of a compound command, hide behind quoting, or smuggle a second command
# past the approval prompt. Judged on $COMMAND, not $FLAT: the flattening
# exists to expose hidden subcommands to the greps below, and would also erase
# exactly the disguises (quotes, continuations) that disqualify a push from
# being called deliberate.
#
# Matched with `[[ =~ ]]` rather than a pipe into `grep -q`, here and below:
# a quiet grep exits at its first match and SIGPIPEs what feeds it, which
# `pipefail` turns into a failed pipeline -- a refusal read as a pass, in
# the one place that reads as an approval nobody gave.
STANDALONE='^git push([[:space:]]+[^;&|<>`$\\]*)?$'
case "$COMMAND" in
  *$'\n'*) : ;;
  *)
    if [[ $COMMAND =~ $STANDALONE ]]; then
      exit 0
    fi
    ;;
esac
CHAINED_PUSH='\bgit\b[^|;&]*\bpush\b'
if [[ $FLAT =~ $CHAINED_PUSH ]]; then
  echo "BLOCKED: git push requires interactive approval and must be a singular standalone command: git push <args>, nothing before or after it on the line." >&2
  exit 2
fi
CHAINED_COMMIT='\bgit\b[^|;&]*\bcommit\b'
if [[ $FLAT =~ $CHAINED_COMMIT ]]; then
  echo "BLOCKED: commit via: task commit -- -m \"<msg>\" (runs the ci gate)." >&2
  exit 2
fi
# A measurement on this shared host is only evidence if the other sessions
# on it were asked to hold their cargo work first. The lock is the receipt
# for that exchange: created by hand after the peer session (cfgd-*) says
# "go", removed when it is told "released". Its age caps a forgotten lock.
QUIET_LOCK="$HOME/.cache/view-quiet-host.lock"
# anchored to the task name: a path such as crates/view-bench/... inside a
# `task commit PATHS=` list is not a measurement
MEASUREMENT='\btask\b[[:space:]]+(bench|bench-micro|perf-audit|heartbeat-ab)([[:space:]]|$)'
if [[ $FLAT =~ $MEASUREMENT ]]; then
  if [[ ! -f "$QUIET_LOCK" ]] || (( $(date +%s) - $(stat -c %Y "$QUIET_LOCK") > 7200 )); then
    echo "BLOCKED: quiet-host measurement without coordination. Message the peer session (ListAgents → cfgd-*) to hold heavy cargo work, wait for its \"go\", then \`touch $QUIET_LOCK\` and re-run; \`rm\` the lock and tell the peer \"released\" when done. A lock older than 2h is stale." >&2
    exit 2
  fi
fi
exit 0
HISTORIC_GUARD
expect 1 '.claude/hooks/validate-commands.sh:73:var .claude/hooks/validate-commands.sh:78:var .claude/hooks/validate-commands.sh:90:var .claude/hooks/validate-commands.sh:92:util' 'the guard revision this scan was written for, reporting its four spellings and nothing else'

printf '\n%s cases, %s failures\n' "$n" "$failures"
[ "$failures" -eq 0 ]
