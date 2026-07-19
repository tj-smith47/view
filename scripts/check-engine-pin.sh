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
# or a leg can hardcode a version that silently drifts from the pin
# shellcheck disable=SC2016 # literal grep pattern, not a shell expansion
reads="$(grep -c 'ENGINE_PIN=$(cat .engine-pin)' .github/workflows/ci.yml || true)"
if [ "$reads" -lt 3 ]; then
  echo "PIN FAIL: expected >=3 install steps reading .engine-pin, found $reads"
  fail=1
fi
if grep -nE 'neovim/releases/download/v[0-9]' .github/workflows/ci.yml; then
  echo "PIN FAIL: hardcoded nvim version literal above (must derive from .engine-pin)"
  fail=1
fi
for floating in 'download/stable/' 'brew install neovim' 'choco install neovim'; do
  if grep -qF "$floating" .github/workflows/ci.yml; then
    echo "PIN FAIL: floating install remains: $floating"; fail=1
  fi
done
exit $fail
