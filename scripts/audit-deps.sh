#!/usr/bin/env bash
set -euo pipefail
for tool in cargo jq; do
  command -v "$tool" >/dev/null 2>&1 || { echo "AUDIT FAIL: $tool is required" >&2; exit 1; }
done
meta="$(cargo metadata --format-version 1 --no-deps)"
fail=0
check_absent() { # usage: check_absent <crate> <forbidden-dep>
  if jq -e --arg c "$1" --arg d "$2" \
    '.packages[] | select(.name == $c) | .dependencies[] | select(.name == $d)' \
    <<<"$meta" >/dev/null; then
    echo "AUDIT FAIL: $1 must not depend on $2"; fail=1
  fi
}
for dep in view view-engine view-tui view-surface view-native view-ai view-oracle view-bench rmpv crossterm ratatui tokio async-std smol; do
  check_absent view-core "$dep"
done
for dep in view view-engine view-tui view-native view-ai view-oracle view-bench tokio async-std smol; do
  check_absent view-surface "$dep"
done
for crate in view-native view-ai; do
  for dep in view view-engine view-tui view-oracle view-bench tokio async-std smol; do
    check_absent "$crate" "$dep"
  done
done
check_absent view-native view-ai
check_absent view-ai view-native
for crate in view-core view-engine view-surface view-native view-ai view-oracle view-bench view; do
  for dep in crossterm ratatui; do
    check_absent "$crate" "$dep"
  done
done
for crate in view-core view-surface view-native view-ai view-tui view-oracle view-bench view; do
  check_absent "$crate" rmpv
done
exit $fail
