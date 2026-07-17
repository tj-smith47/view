#!/usr/bin/env bash
set -euo pipefail
fail=0
check_absent() { # usage: check_absent <crate> <forbidden-dep>
  local pattern="^$2[[:space:]]*=|^$2\\.|^\\[(target\\.[^]]+\\.)?(dependencies|dev-dependencies|build-dependencies)\\.$2\\]|package[[:space:]]*=[[:space:]]*\"$2\""
  if grep -Eq "$pattern" "crates/$1/Cargo.toml" 2>/dev/null; then
    echo "AUDIT FAIL: $1 must not depend on $2"; fail=1
  fi
}
for dep in view-engine view-tui view-surface view-native view-ai view-oracle view-bench rmpv crossterm ratatui tokio; do
  check_absent view-core "$dep"
done
for dep in view-engine view-tui view-native view-ai view-oracle view-bench tokio; do
  check_absent view-surface "$dep"
done
for crate in view-native view-ai; do
  for dep in view-engine view-tui view-oracle view-bench tokio; do
    check_absent "$crate" "$dep"
  done
done
for crate in view-core view-engine view-surface view-native view-ai view-oracle view-bench view; do
  for dep in crossterm ratatui; do
    check_absent "$crate" "$dep"
  done
done
for crate in view-core view-surface view-native view-ai view-tui view-oracle view-bench view; do
  check_absent "$crate" rmpv
done
exit $fail
