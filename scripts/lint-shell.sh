#!/usr/bin/env bash
# Lints every shell script this tree carries at warning level: the shebang
# population under scripts/ and every other tracked `.sh`.
set -euo pipefail

cd "$(dirname "$0")/.."
# shellcheck source=scripts/lib/script-population.sh
. scripts/lib/script-population.sh

if ! command -v shellcheck >/dev/null 2>&1; then
  echo "lint:shell: shellcheck is not on PATH; install it (CI pins the version in .github/workflows/ci.yml)" >&2
  exit 1
fi

script_population_read .
scripts=$({ printf '%s\n' "$SCRIPT_POPULATION"; git ls-files -- '*.sh'; } |
  grep . | LC_ALL=C sort -u || true)
if [ -z "$scripts" ]; then
  echo "lint:shell: no shell script found to lint" >&2
  exit 1
fi

files=()
while IFS= read -r script; do
  files+=("$script")
done <<<"$scripts"
shellcheck -S warning -- "${files[@]}"
echo "lint:shell: ${#files[@]} scripts clean"
