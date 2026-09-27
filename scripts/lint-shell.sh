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
# its own capture, so a failing git stops the lint: the shebang population
# alone would otherwise read clean
tracked=$(git ls-files -- '*.sh') || {
  echo "lint:shell: git ls-files failed, so the tracked scripts cannot be listed" >&2
  exit 1
}
scripts=$({ printf '%s\n' "$SCRIPT_POPULATION"; printf '%s\n' "$tracked"; } |
  grep . | LC_ALL=C sort -u || true)
if [ -z "$scripts" ]; then
  echo "lint:shell: no shell script found to lint" >&2
  exit 1
fi

files=()
while IFS= read -r script; do
  files+=("$script")
done <<<"$scripts"

# A `disable` keeps a finding, which holds only where a reader shellcheck
# cannot see uses the construct, so the comment line above it names that
# reader, and it sits directly on the one command it covers after the
# file's first. shellcheck reads a directive with a blank or a comment under
# it, or one ahead of the first command, as covering the whole file.
bare=$(awk '
  FNR == 1 { prev = ""; site = ""; commands = 0 }
  site != "" {
    if ($0 ~ /^[[:space:]]*(#|$)/) {
      print site ": a shellcheck disable not directly above the command it covers"
    }
    site = ""
  }
  /^[[:space:]]*#[[:space:]]*shellcheck[[:space:]]+disable=/ {
    if (prev !~ /^[[:space:]]*#/ || prev ~ /^[[:space:]]*#[[:space:]]*shellcheck[[:space:]]/ || prev !~ /read/) {
      print FILENAME ":" FNR ": a shellcheck disable with no comment above it naming the reader shellcheck cannot see"
    }
    if (commands == 0) {
      print FILENAME ":" FNR ": a shellcheck disable ahead of the first command covers the whole file"
    }
    site = FILENAME ":" FNR
  }
  $0 !~ /^[[:space:]]*(#|$)/ { commands++ }
  { prev = $0 }
' "${files[@]}")
if [ -n "$bare" ]; then
  printf '%s\n' "$bare" >&2
  exit 1
fi

shellcheck -S warning -- "${files[@]}"
echo "lint:shell: ${#files[@]} scripts clean"
