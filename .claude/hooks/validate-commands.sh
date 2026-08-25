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
case "$COMMAND" in
  *$'\n'*) : ;;
  *)
    if printf %s "$COMMAND" | grep -qE '^git push([[:space:]]+[^;&|<>`$\\]*)?$'; then
      exit 0
    fi
    ;;
esac
if printf %s "$FLAT" | grep -qE '\bgit\b[^|;&]*\bpush\b'; then
  echo "BLOCKED: git push requires interactive approval and must be a singular standalone command: git push <args>, nothing before or after it on the line." >&2
  exit 2
fi
if printf %s "$FLAT" | grep -qE '\bgit\b[^|;&]*\bcommit\b'; then
  echo "BLOCKED: commit via: task commit -- -m \"<msg>\" (runs the ci gate)." >&2
  exit 2
fi
# A measurement on this shared host is only evidence if the other sessions
# on it were asked to hold their cargo work first. The lock is the receipt
# for that exchange: created by hand after the peer session (cfgd-*) says
# "go", removed when it is told "released". Its age caps a forgotten lock.
QUIET_LOCK="$HOME/.cache/view-quiet-host.lock"
if printf %s "$FLAT" | grep -qE '\btask\b[^|;&]*\b(bench|bench-micro|perf-audit|heartbeat-ab)\b'; then
  if [[ ! -f "$QUIET_LOCK" ]] || (( $(date +%s) - $(stat -c %Y "$QUIET_LOCK") > 7200 )); then
    echo "BLOCKED: quiet-host measurement without coordination. Message the peer session (ListAgents → cfgd-*) to hold heavy cargo work, wait for its \"go\", then \`touch $QUIET_LOCK\` and re-run; \`rm\` the lock and tell the peer \"released\" when done. A lock older than 2h is stale." >&2
    exit 2
  fi
fi
exit 0
