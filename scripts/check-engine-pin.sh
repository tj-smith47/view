#!/usr/bin/env bash
# Gate: CI must install the engine version .engine-pin names -- three OS
# legs that each resolve "latest stable" independently can and did drift.
set -euo pipefail
pin="$(tr -d '[:space:]' < .engine-pin)"
[ -n "$pin" ] || { echo "PIN FAIL: .engine-pin is empty"; exit 1; }
case "$pin" in
  v[0-9]*.[0-9]*.[0-9]*) ;;
  *) echo "PIN FAIL: '$pin' is not a vX.Y.Z tag"; exit 1 ;;
esac
fail=0
# every OS leg must read the pin file itself: one read per install step,
# or a leg can hardcode a version that silently drifts from the pin.
# The floor is a ratchet pinned PER FILE to the current read count -- bump
# it when adding jobs, never lower it. A total across both workflows cannot
# notice every bench leg losing its pin reads while ci.yml keeps its own.
check_workflow() {
  local file="$1" floor="$2" reads floating
  # an absent/renamed workflow file must not silently pass: grep's exit 2
  # on missing files was previously swallowed by `|| true`, turning every
  # check below into a no-op that still exited 0
  [ -f "$file" ] || { echo "PIN FAIL: $file not found"; fail=1; return; }
  # shellcheck disable=SC2016 # literal grep pattern, not a shell expansion
  reads="$(grep -cF 'ENGINE_PIN=$(cat .engine-pin)' "$file" || true)"
  if ! [ "$reads" -ge "$floor" ]; then
    echo "PIN FAIL: $file: expected >=$floor install steps reading .engine-pin, found $reads"
    fail=1
  fi
  if grep -nE 'neovim/releases/download/v[0-9]' "$file"; then
    echo "PIN FAIL: $file: hardcoded nvim version literal above (must derive from .engine-pin)"
    fail=1
  fi
  for floating in 'download/stable/' 'brew install neovim' 'choco install neovim' \
    'apt install neovim' 'apt-get install neovim'; do
    if grep -qF "$floating" "$file"; then
      echo "PIN FAIL: $file: floating install remains: $floating"; fail=1
    fi
  done
  # marketplace setup actions (rhysd/action-setup-vim, */setup-neovim, ...)
  # resolve their own nvim version internally, bypassing the pin entirely
  if grep -nE 'uses:.*setup-(neo)?vim' "$file"; then
    echo "PIN FAIL: $file: nvim setup action above resolves its own version (install from .engine-pin instead)"
    fail=1
  fi
}
# The read floor for one workflow, empty for a file nobody pinned one to.
# A ratchet per file: bump it when a workflow gains an install step, never
# lower it.
floor_for() {
  case "$1" in
    .github/workflows/ci.yml) echo 16 ;;
    .github/workflows/bench.yml) echo 3 ;;
    *) echo "" ;;
  esac
}
# Which workflows to check is derived, not listed: a third one that installs
# the engine is checked the moment it exists, rather than the next time
# somebody remembers this file. The markers are every shape an install can
# take -- the pin read a correct one performs, and the download, package
# manager and setup action a wrong one would.
# `tr`: one space-separated line, so the membership test below can match a
# name in it
installers="$(grep -lE 'ENGINE_PIN=\$\(cat \.engine-pin\)|neovim/releases/download|(brew|choco|apt|apt-get) install neovim|uses:.*setup-(neo)?vim' \
  .github/workflows/*.yml | tr '\n' ' ' || true)"
for workflow in $installers; do
  floor="$(floor_for "$workflow")"
  if [ -z "$floor" ]; then
    echo "PIN FAIL: $workflow installs nvim with no read floor of its own (add one to floor_for)"
    fail=1
    floor=1
  fi
  check_workflow "$workflow" "$floor"
done
# and the other direction: a workflow that carried a floor and no longer
# installs anything (renamed, deleted, or quietly stripped of its install
# steps) is a gate that stopped gating
for workflow in .github/workflows/ci.yml .github/workflows/bench.yml; do
  case " $installers " in
    *" $workflow "*) ;;
    *) echo "PIN FAIL: $workflow not found, or it no longer installs nvim"; fail=1 ;;
  esac
done
exit $fail
