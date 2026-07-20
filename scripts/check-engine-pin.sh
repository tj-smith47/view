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
# an absent/renamed workflow file must not silently pass: grep's exit 2
# on missing files was previously swallowed by `|| true`, turning every
# check below into a no-op that still exited 0
[ -f .github/workflows/ci.yml ] || { echo "PIN FAIL: .github/workflows/ci.yml not found"; exit 1; }
# every OS leg must read the pin file itself: one read per install step,
# or a leg can hardcode a version that silently drifts from the pin.
# The floor is a ratchet pinned to the current read count -- bump it when
# adding jobs, never lower it. A loose generic floor cannot notice one
# job losing all of its pin reads while the others keep theirs.
# shellcheck disable=SC2016 # literal grep pattern, not a shell expansion
reads="$(grep -c 'ENGINE_PIN=$(cat .engine-pin)' .github/workflows/ci.yml || true)"
if ! [ "$reads" -ge 13 ]; then
  echo "PIN FAIL: expected >=13 install steps reading .engine-pin, found $reads"
  fail=1
fi
if grep -nE 'neovim/releases/download/v[0-9]' .github/workflows/ci.yml; then
  echo "PIN FAIL: hardcoded nvim version literal above (must derive from .engine-pin)"
  fail=1
fi
for floating in 'download/stable/' 'brew install neovim' 'choco install neovim' \
  'apt install neovim' 'apt-get install neovim'; do
  if grep -qF "$floating" .github/workflows/ci.yml; then
    echo "PIN FAIL: floating install remains: $floating"; fail=1
  fi
done
# marketplace setup actions (rhysd/action-setup-vim, */setup-neovim, ...)
# resolve their own nvim version internally, bypassing the pin entirely
if grep -nE 'uses:.*setup-(neo)?vim' .github/workflows/ci.yml; then
  echo "PIN FAIL: nvim setup action above resolves its own version (install from .engine-pin instead)"
  fail=1
fi
exit $fail
