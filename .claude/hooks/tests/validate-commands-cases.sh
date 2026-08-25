#!/usr/bin/env bash
# Case matrix for validate-commands.sh. Every case is a PreToolUse payload the
# guard must classify, asserted on BOTH the exit status and which refusal was
# printed: a push case that only trips the commit grep is still rc=2, so a
# status-only assertion would grade a swallowed push as a pass.
#
#   bash .claude/hooks/tests/validate-commands-cases.sh
#   bash .claude/hooks/tests/validate-commands-cases.sh --guard /path/to/copy
#
# Written to stock POSIX-ish bash: macOS ships /bin/bash 3.2, and the guard's
# own portability (BSD sed, BSD awk) is only proven by running this there.
set -uo pipefail

GUARD=""
while [ $# -gt 0 ]; do
  case "$1" in
    --guard)
      GUARD="${2:-}"
      shift 2
      ;;
    -h | --help)
      printf 'usage: %s [--guard PATH]\n' "$0"
      exit 0
      ;;
    *)
      printf 'unknown argument: %s\n' "$1" >&2
      exit 2
      ;;
  esac
done
# Resolved from this file's own directory, never from $PWD or a hook search
# path: the machine running these cases also has the guard installed in its
# settings, and a run that silently graded the installed copy would report
# green for a tree whose committed guard is broken.
if [ -z "$GUARD" ]; then
  GUARD="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)/validate-commands.sh"
fi
if [ ! -f "$GUARD" ]; then
  printf 'guard not found: %s\n' "$GUARD" >&2
  exit 2
fi
if ! command -v jq >/dev/null 2>&1; then
  printf 'jq is required (the guard parses its payload with it)\n' >&2
  exit 2
fi

printf 'guard under test: %s\n' "$GUARD"

n=0
failures=0

check() {
  want_rc="$1"
  want_msg="$2"
  desc="$3"
  cmd="$4"
  n=$((n + 1))
  err="$(jq -n --arg c "$cmd" '{tool_input:{command:$c}}' | bash "$GUARD" 2>&1 >/dev/null)"
  rc=$?
  if [ "$rc" != "$want_rc" ]; then
    failures=$((failures + 1))
    printf 'not ok %s - %s (want rc=%s, got rc=%s)\n' "$n" "$desc" "$want_rc" "$rc"
    return
  fi
  case "$err" in
    *"$want_msg"*)
      printf 'ok %s - %s\n' "$n" "$desc"
      ;;
    *)
      failures=$((failures + 1))
      printf 'not ok %s - %s (want refusal matching %s, got: %s)\n' \
        "$n" "$desc" "$want_msg" "$err"
      ;;
  esac
}

# The two refusals are distinguished so that a push case can never be scored
# green by the commit grep catching the wreckage of a mis-parsed push.
block_push() { check 2 'push requires' "$1" "$2"; }
block_commit() { check 2 'commit via' "$1" "$2"; }
allow() { check 0 '' "$1" "$2"; }
# the quiet-host guard reads its lock under $HOME, so an empty HOME is a host
# with no coordination receipt
block_bench() { HOME="$NOLOCK_HOME" check 2 'quiet-host' "$1" "$2"; }
NOLOCK_HOME="$(mktemp -d "${TMPDIR:-/tmp}/validate-commands-home.XXXXXX")"
trap 'rm -rf "$NOLOCK_HOME"' EXIT

# ---------------------------------------------------------------------------
# must pass through to the interactive ask: a singular, standalone push.
# rc=0 here does NOT mean the push runs -- it means the guard steps aside so
# the permission layer's ask-rule can surface the exact command for approval.
# That rule prefix-matches the raw command string, which is why only commands
# literally starting `git push`, on one line, with no shell metacharacters,
# may reach it: anything else would bypass the prompt instead of landing on it.
# ---------------------------------------------------------------------------
allow 'bare push' 'git push'
allow 'push with remote and branch' 'git push origin main'
allow 'force-with-lease push' 'git push --force-with-lease'
allow 'push with upstream flag' 'git push -u origin HEAD'
allow 'push with a refspec argument' 'git push origin HEAD:refs/heads/main'
allow 'push with a flag resembling a message' 'git push -m x'
allow 'push with a quoted message-looking flag' 'git push -m "notes"'

# ---------------------------------------------------------------------------
# must block as a push
# ---------------------------------------------------------------------------
block_push 'push with repeated spaces' 'git   push'
block_push 'push chained after with &&' 'git push && echo done'
block_push 'push chained before a second command' 'git push; echo done'
block_push 'push with a command substitution' 'git push $(echo origin)'
block_push 'push with a backtick substitution' 'git push `echo origin`'
block_push 'push with a variable expansion' 'git push origin $BRANCH'
block_push 'push with output redirected' 'git push >log'
block_push 'push followed by a second line' $'git push\necho done'
block_push 'double-quoted subcommand' 'git "push"'
block_push 'single-quoted subcommand' "git 'push'"
block_push 'quoted subcommand and remote' 'git "push" "origin" "main"'
block_push 'continuation split between words' $'git \\\npush'
block_push 'continuation split inside the subcommand' $'git pu\\\nsh'
block_push 'push behind a leading env assignment' 'GIT_DIR=/x git push'
block_push 'push after a directory change' 'cd /repo && git push'
block_push 'push inside a nested shell' 'bash -c "git push"'
block_push 'push with output redirected through a pipe' 'git push 2>&1 | tee log'
block_push 'push after a gated commit' 'git commit -m x && git push'
block_push 'push after a commit with a quoted message' 'git commit -m "wip" && git push origin main'
block_push 'push after a stash push' 'git stash push && git push'
block_push 'stash-looking push with a global option in between' 'git -C /x stash push'
block_push 'global config value containing a space' 'git -c user.name="a b" push'
block_push 'long message option before the subcommand' 'git --message=x push'
block_push 'global git-dir option before the subcommand' 'git --git-dir=/x push'
# The attached-value family: `-m` is the tail of a `-c key=value` token, not a
# flag, so nothing may be removed and the subcommand behind it must survive.
block_push 'attached config value ending in the short message flag' 'git -c foo=-m push'
block_push 'attached config value, dotted key, with arguments' 'git -c a.b=-m push origin main'
block_push 'attached config value on a real config key' 'git -c user.name=-m push'
block_push 'attached config value ending in the long message flag' 'git -c x=--message push'
block_push 'attached config value with a double-quoted subcommand' 'git -c a.b=-m "push"'
block_push 'attached config value with a single-quoted subcommand' "git -c a.b=-m 'push'"
# Unquoted message values end at a shell separator, not at the next space:
# consuming past one would swallow the command that follows it.
block_push 'unquoted message value abutting a semicolon' 'task commit -- -m fix;git push'
block_push 'unquoted message value abutting a pipe' 'task commit -- -m fix|git push'
block_push 'unquoted message value abutting an ampersand' 'task commit -- -m fix&git push'

# ---------------------------------------------------------------------------
# must block as a commit
# ---------------------------------------------------------------------------
block_commit 'bare commit' 'git commit'
block_commit 'commit with a quoted message' 'git commit -m "fix: something"'
block_commit 'commit with a single-quoted message' "git commit -m 'fix: something'"
block_commit 'commit with a combined flag cluster' 'git commit -am wip'
block_commit 'amend without an editor' 'git commit --amend --no-edit'
block_commit 'commit with an attached long message option' 'git commit --message=wip'
block_commit 'attached config value ending in the message flag, then commit' 'git -c x=-m commit'

# ---------------------------------------------------------------------------
# must allow
# ---------------------------------------------------------------------------
allow 'the gated commit target quoting this guard' 'task commit -- -m "fix: the git hook blocks push"'
allow 'the gated commit target, single-quoted' "task commit -- -m 'the push guard refuses its own documentation'"
allow 'a message describing the ship rule' 'task commit -- -m "docs: git push requires an explicit ship instruction"'
allow 'a message naming the commit target' 'task commit -- -m "chore: git commit goes through the task target"'
allow 'a message containing a shell separator' 'task commit -- -m "fix: a; git push is prose here"'
allow 'a bare single-word message value' 'task commit -- -m fix'
allow 'an attached long message option' 'task commit -- --message="fix: the push guard"'
allow 'a message across a line continuation' $'task commit -- \\\n  -m "feat: the push guard"'
allow 'a local stash push' 'git stash push'
allow 'a local stash push with a message' 'git stash push -m "wip on the push guard"'
allow 'a local stash push with a flag' 'git stash push --keep-index'
allow 'a stash pop' 'git stash pop'
allow 'status' 'git status'
allow 'log' 'git log --oneline -5'
allow 'diff' 'git diff HEAD'
allow 'staging' 'git add -A'
allow 'fetch' 'git fetch origin'
allow 'version' 'git --version'
allow 'a proxied read-only git call' 'rtk git status'
allow 'an unrelated command' 'cargo test --workspace'
allow 'the ci gate' 'task ci'
allow 'a commit naming a path under view-bench' 'task commit PATHS="crates/view-bench/baselines/gh-linux.headroom.toml" -- -m "fix: x"'
allow 'a quick commit naming a bench doc' 'task commit:quick PATHS="docs/bench-micro.md" -- -m "docs: x"'
block_bench 'a measurement without the quiet-host lock' 'task bench -- --scenario scroll'
block_bench 'a micro measurement without the lock' 'task bench-micro'
block_bench 'a perf audit chained after a build' 'task build && task perf-audit'

printf '\n%s cases, %s failures\n' "$n" "$failures"
[ "$failures" -eq 0 ]
