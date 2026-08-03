#!/usr/bin/env bash
set -euo pipefail
FILE="${1:-}"
[[ -z "$FILE" || ! -f "$FILE" ]] && exit 0
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
problems=""
if command -v rustfmt >/dev/null 2>&1 && ! rustfmt --edition 2021 --check "$FILE" >/dev/null 2>&1; then
  problems+="rustfmt: $FILE needs formatting (run: cargo fmt)"$'\n'
fi
# delegates to check-style.sh's --file mode rather than carrying its own
# copy of the pattern list: the two ran the same session-narrative/§ checks
# out of sync with each other before, and every future pattern addition to
# check-style.sh (e.g. the reviewer/coordinator check) would otherwise need
# a second hand-applied edit here to actually gate on save
if ! style_out=$("$REPO_ROOT/scripts/check-style.sh" --file "$FILE" 2>&1); then
  problems+="comment style: $FILE failed check-style.sh (comments are WHY-only)"$'\n'
  problems+="$style_out"$'\n'
fi
case "$FILE" in
  */crates/*/src/*.rs)
    if ! loc_out=$("$REPO_ROOT/scripts/audit-god-files.sh" "$FILE" 2>&1); then
      problems+="$loc_out"$'\n'
    fi
    ;;
esac
if [[ -n "$problems" ]]; then printf '%s' "$problems" >&2; exit 2; fi
exit 0
