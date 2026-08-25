#!/usr/bin/env bash
# Recurring macOS build leg, run from a dev host against the mbp machine.
#
# CI's own macos legs (ci.yml's `ci`/`oracle`/`compat` jobs, plus bench.yml's
# `bench` job) only run on a push or a PR, so a macOS-only build break sits
# unseen on a branch nobody has pushed yet -- the gap that left macOS broken
# for days. This closes it independently of push timing: mirror the tracked
# tree to a scratch checkout on mbp and build the workspace there.
#
#   bash scripts/mbp-build-leg.sh
#
# Never touches ~/repos/view on mbp -- that is a live working repo, not this
# leg's to `rm -rf`. Writes a timestamped log under ~/.claude/tmp/mbp-leg/
# and exits nonzero on any failure: an unreachable host, a failed mirror, or
# a nonzero `cargo build --workspace` on the far end.
#
# Mirrors `git archive HEAD`: the last COMMIT, not the working tree. An
# uncommitted edit is invisible to this leg, so "OK" attests the committed
# state only -- commit first if you want an in-progress change checked.
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$root"

host="mbp"
log_dir="$HOME/.claude/tmp/mbp-leg"
mkdir -p "$log_dir"
log_file="$log_dir/$(date -u +%Y%m%dT%H%M%SZ).log"

echo "== mbp build leg, $(date -u +%Y-%m-%dT%H:%M:%SZ) ==" | tee "$log_file"
git log --oneline -1 | tee -a "$log_file"

echo "-- mirroring tracked tree to $host:~/repos/view-ci --" | tee -a "$log_file"
# `*(ND)`, not a bare `*`: zsh (mbp's login shell) aborts a glob with no
# matches by default, unlike bash, so a first run against a freshly
# `mkdir -p`'d, still-empty view-ci would fail here before ever reaching
# tar. The `(N)` qualifier is zsh's own nullglob-for-one-pattern -- an empty
# match expands to nothing instead of erroring. `(D)` includes dotfiles,
# which plain `*` skips -- without it a tracked dotfile removed from a later
# commit would silently survive the wipe and leak into subsequent builds
# (zsh globs never generate `.` or `..`, so the recursive rm stays safe).
if ! git archive HEAD |
  ssh "$host" 'zsh -lc "mkdir -p ~/repos/view-ci && rm -rf ~/repos/view-ci/*(ND) && tar -x -C ~/repos/view-ci"' \
    >>"$log_file" 2>&1; then
  echo "MBP BUILD LEG FAIL: could not mirror the tree to $host (host unreachable, or the remote mkdir/rm/tar failed)" |
    tee -a "$log_file" >&2
  exit 1
fi

echo "-- building on $host --" | tee -a "$log_file"
# zsh -lc, not a bare ssh command: mbp's non-interactive ssh session misses
# /opt/homebrew/bin, where cargo lives, and only a login shell picks it up.
#
# `${pipestatus[1]}`, not `$?`: after `cargo build ... | tail -30`, a bare
# `$?` names `tail`'s own exit status, which is 0 regardless of whether the
# build it truncated succeeded -- that would make every remote build report
# EXIT:0 and defeat the nonzero-on-failure contract this script exists to
# keep. zsh's `pipestatus` array holds each pipeline stage's own status,
# 1-indexed, so `[1]` is cargo's.
# `\${pipestatus[1]}`, escaped: the double-quoted zsh -lc string is expanded
# by the OUTER remote login shell first, whose own pipestatus is always 0 --
# unescaped, every build reported EXIT:0, success or not. The backslash
# defers expansion to the inner zsh that actually ran the pipeline.
if ! ssh "$host" 'zsh -lc "cd ~/repos/view-ci && cargo build --workspace 2>&1 | tail -30; echo EXIT:\${pipestatus[1]}"' \
  >>"$log_file" 2>&1; then
  echo "MBP BUILD LEG FAIL: the ssh session to $host itself failed" | tee -a "$log_file" >&2
  exit 1
fi

# `|| true`: when the marker is absent, grep exits 1 and (under pipefail)
# fails the whole substitution -- and a failing bare `var="$(...)"` is fatal
# under `set -e`, so without it the "marker missing" branch below could
# never run and the script would die with no diagnostic at all.
remote_exit="$(grep -oE 'EXIT:[0-9]+' "$log_file" | tail -1 | cut -d: -f2 || true)"
if [ -z "$remote_exit" ]; then
  echo "MBP BUILD LEG FAIL: no EXIT: marker in $host's output; the build's real result is unknown" |
    tee -a "$log_file" >&2
  exit 1
fi
if [ "$remote_exit" -ne 0 ]; then
  echo "MBP BUILD LEG FAIL: cargo build --workspace exited $remote_exit on $host" | tee -a "$log_file" >&2
  exit 1
fi

echo "MBP BUILD LEG OK: $host built the workspace clean" | tee -a "$log_file"
