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
# view-engine depends on view-core (it decodes redraw batches into
# view-core's UiEvent vocabulary) -- that edge is sanctioned and must stay
# legal. This loop only forbids the reverse: view-core must never depend on
# view-engine, so a later audit sweep must not "fix" the engine->core edge.
for dep in view view-engine view-tui view-surface view-native view-ai view-oracle view-bench rmpv crossterm ratatui tokio async-std smol; do
  check_absent view-core "$dep"
done
# view-bench depends on view-oracle (the latency bench drives the same
# PtySession the oracle's own tests use, instead of a second copy of the
# spawn/read/write scaffolding) -- that edge is sanctioned and must stay
# legal. This check only forbids the reverse: view-oracle must never depend
# on view-bench, so a later audit sweep must not "fix" the bench->oracle edge.
check_absent view-oracle view-bench
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
# serde + toml exist for the theme cache file and are confined to the bin
# crate; a lib crate gaining either would let wire/config concerns leak
# into the pure layers
for crate in view-core view-engine view-surface view-native view-ai view-tui view-oracle view-bench; do
  check_absent "$crate" serde
  check_absent "$crate" toml
done

# Resolved-graph check: cargo metadata --no-deps (used by check_absent above)
# only sees each package's own declared manifest edges, so a forbidden
# runtime crate pulled in transitively through some unrelated dependency
# would be invisible to it. `cargo tree -i` walks the fully resolved
# dependency graph in reverse from the forbidden crate, catching a leak
# check_absent cannot.
check_transitive_reach() { # usage: check_transitive_reach <forbidden-dep> [allowed-workspace-member ...]
  local dep="$1"
  shift
  local allowed=("$@")
  local out
  out="$(cargo tree -i "$dep" -e normal --prefix none 2>/dev/null)" || true
  # only local path+file packages are this workspace's members; an external
  # crates.io dependent elsewhere in the same reverse tree isn't ours to
  # gate
  local reachers
  # `|| true`: grep legitimately finds nothing when a forbidden crate is
  # unreachable from the workspace at all (the desired outcome for the
  # async-runtime crates), and pipefail must not treat that "no match" as a
  # script-ending failure
  reachers="$(grep -E ' \(/.*\)$' <<<"$out" | awk '{print $1}' | sort -u || true)"
  local member
  while IFS= read -r member; do
    [ -z "$member" ] && continue
    local ok=0
    for a in "${allowed[@]}"; do
      [ "$member" = "$a" ] && ok=1 && break
    done
    if [ "$ok" -eq 0 ]; then
      echo "AUDIT FAIL: $dep is transitively reachable from $member (resolved graph); only [${allowed[*]:-nothing}] may reach it"
      fail=1
    fi
  done <<<"$reachers"
}
# view-oracle reaches rmpv transitively through view-engine (Engine,
# EngineHandle, eval_str): a deliberate, named policy change -- the oracle's
# own API stays rmpv-free (typed probes only, see src/lib.rs's module
# docs), but its Cargo.toml now has a normal (not dev) dependency edge to
# view-engine, so the resolved graph legitimately reaches rmpv through it.
# view-bench reaches the same rmpv edge one hop further out, through its
# own sanctioned dependency on view-oracle; view-bench's own API stays
# rmpv-free the same way view-oracle's does.
check_transitive_reach rmpv view-engine view view-oracle view-bench
check_transitive_reach serde view
check_transitive_reach toml view
check_transitive_reach crossterm view-tui view
check_transitive_reach ratatui view-tui view
check_transitive_reach tokio
check_transitive_reach async-std
check_transitive_reach smol
exit $fail
