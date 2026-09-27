#!/usr/bin/env bash
# Lints every shell script this tree carries at warning level: the shebang
# population under scripts/ and every other `.sh` git lists, untracked ones
# included, since `task commit` runs the lint before it stages a new file.
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
listed=$(git ls-files --cached --others --exclude-standard -- '*.sh') || {
  echo "lint:shell: git ls-files failed, so the scripts outside scripts/ cannot be listed" >&2
  exit 1
}
scripts=$({ printf '%s\n' "$SCRIPT_POPULATION"; printf '%s\n' "$listed"; } |
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
# reader. It sits directly on the command it covers, where a reader sees
# both. One ahead of the file's first command covers the whole file, so it
# goes on the command that needs it. shellcheck applies stacked directives
# alike, and one directive carrying the codes joined by a comma keeps the
# reader comment above the command. A line inside a here-doc body or a
# quoted string is text no shell reads as a comment, and each directive gets
# one message naming everything wrong with it.
bare=$(awk -v SQ="'" "$SCRIPT_CODE_AWK"'
  function directive_codes(line,    c) {
    c = line
    sub(/^.*disable=/, "", c)
    sub(/[[:space:]].*$/, "", c)
    return c
  }
  function settle(next_line) {
    if (site == "") { return }
    if (next_line == "last") {
      why = why "; is on the last line of the file and covers no command"
    } else if (next_line ~ /^[[:space:]]*#[[:space:]]*shellcheck[[:space:]]+disable=/) {
      why = why "; is stacked on another, and the codes join with a comma in one directive (disable=" codes "," directive_codes(next_line) ")"
    } else if (next_line ~ /^[[:space:]]*(#|$)/) {
      why = why "; has a blank or a comment under it, and belongs directly above the command it covers"
    }
    if (why != "") { print site ": a shellcheck disable" substr(why, 2) }
    site = ""; why = ""
  }
  FNR == 1 { settle("last"); above = ""; commands = 0 }
  {
    top = script_code_top()
    text = (HD != "" || top == SQ || top == "\"")
    script_code_scan($0)
    if (text) { next }
  }
  { settle($0) }
  /^[[:space:]]*#[[:space:]]*shellcheck[[:space:]]+disable=/ {
    if (above !~ /^[[:space:]]*#/ || above ~ /^[[:space:]]*#[[:space:]]*shellcheck[[:space:]]/ || above !~ /(^|[^[:alnum:]])read/) {
      why = why "; has no comment above it naming the reader shellcheck cannot see"
    }
    if (commands == 0) {
      why = why "; sits ahead of the first command of the file, where it covers the whole file"
    }
    site = FILENAME ":" FNR
    codes = directive_codes($0)
    next
  }
  $0 !~ /^[[:space:]]*(#|$)/ { commands++ }
  { above = $0 }
  END { settle("last") }
' "${files[@]}")
if [ -n "$bare" ]; then
  printf '%s\n' "$bare" >&2
  exit 1
fi

shellcheck -S warning -- "${files[@]}"
echo "lint:shell: ${#files[@]} scripts clean"
