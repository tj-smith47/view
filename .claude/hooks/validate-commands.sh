#!/usr/bin/env bash
set -euo pipefail
INPUT=$(cat)
COMMAND=$(printf %s "$INPUT" | jq -r '.tool_input.command // empty' 2>/dev/null)
[[ -z "$COMMAND" ]] && exit 0
FLAT=$(printf %s "$COMMAND" | sed -z 's/\\\n//g' | tr -d '\\' | tr -d "'\"" | tr '\n' ' ')
if printf %s "$FLAT" | grep -qE '\bgit\b[^|;&]*\bpush\b'; then
  echo "BLOCKED: git push requires an explicit user ship instruction." >&2
  exit 2
fi
if printf %s "$FLAT" | grep -qE '\bgit\b[^|;&]*\bcommit\b'; then
  echo "BLOCKED: commit via: task commit -- -m \"<msg>\" (runs the ci gate)." >&2
  exit 2
fi
exit 0
