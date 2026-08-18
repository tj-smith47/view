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
# view-bench depends on view-oracle (the scenario drivers drive the same
# PtySession the oracle's own tests use, instead of a second copy of the
# spawn/read/write scaffolding) -- that edge is sanctioned and must stay
# legal. This check only forbids the reverse: view-oracle must never depend
# on view-bench, so a later audit sweep must not "fix" the bench->oracle edge.
check_absent view-oracle view-bench
for dep in view view-engine view-tui view-native view-ai view-oracle view-bench tokio async-std smol; do
  check_absent view-surface "$dep"
done
# the tokio ban keeps view-native and drops view-ai, whose runtime the
# spec's decision log sanctions; view-ai still may not reach the app,
# RPC, or measurement crates
for dep in view view-engine view-tui view-oracle view-bench tokio async-std smol; do
  check_absent view-native "$dep"
done
for dep in view view-engine view-tui view-oracle view-bench; do
  check_absent view-ai "$dep"
done
# the view-native <-> view-ai mutual forbid below is unrelated to tokio
# and stays
check_absent view-native view-ai
check_absent view-ai view-native
for crate in view-core view-engine view-surface view-native view-ai view-oracle view-bench view view-harness; do
  for dep in crossterm ratatui; do
    check_absent "$crate" "$dep"
  done
done
for crate in view-core view-surface view-native view-ai view-tui view-oracle view-bench view view-harness; do
  check_absent "$crate" rmpv
done
# arboard is the clipboard worker's local-backend dependency, confined to
# the view bin crate (see crates/view/src/clipboard.rs): every other crate,
# including view-native (which owns the clipboard line-shaping helpers, not
# the system-clipboard call) and view-tui (which owns the OSC 52 escape
# write, not the local read/write), must stay free of it.
for crate in view-core view-engine view-surface view-native view-ai view-tui view-oracle view-bench view-harness; do
  check_absent "$crate" arboard
done
# ureq (the checksummed adapter download, crates/view-ai/src/provision.rs)
# and sha2 (its checksum verification) are confined to view-ai the same way
# arboard is confined to the bin above: nothing else in the workspace
# fetches or verifies a downloaded binary, so neither dependency has any
# business appearing anywhere else.
for crate in view-core view-engine view-surface view-native view-tui view-oracle view-bench view view-harness; do
  check_absent "$crate" ureq
  check_absent "$crate" sha2
done
# nucleo (the picker's matcher engine, spec §18), ignore (the Files
# source's .gitignore-respecting walker), and grep-searcher/grep-regex (the
# live-grep source's in-process search, ripgrep's own constituent crates)
# are confined to view-native's own matcher worker thread
# (crates/view-native/src/picker/matcher.rs and sources.rs) on the same
# terms as arboard above: view-core stays free of a matching-library
# dependency (its PickerState only ever asks the worker, never matches
# itself), and no other crate needs any of them.
for crate in view-core view-engine view-surface view-ai view-tui view-oracle view-bench view view-harness; do
  check_absent "$crate" nucleo
  check_absent "$crate" ignore
  check_absent "$crate" grep-searcher
  check_absent "$crate" grep-regex
done
# serde + toml exist for the theme cache file, the corpus loader, and the
# [native] table of view.toml, confined to the crates that own a file
# format (view, view-harness, view-native); any other lib crate gaining
# either would let wire/config concerns leak into the pure layers.
# view-native is the one lib crate on that list: view.toml's [native]
# table is a shipping user surface, and parsing it in the bin instead
# would put the schema and its error messages where neither view-native's
# tests nor the oracle can reach them. view-core stays absent regardless,
# so the feature registry it owns remains pure &'static data.
# view-harness is deliberately absent from this loop: cargo
# dependencies are per-package, not per-target, so a bin target inside
# view-oracle (rather than a standalone package) would drag serde into
# the oracle lib's own manifest and fail its own absence check below --
# view-harness exists precisely to be the one non-view package allowed to
# parse TOML, not to be checked for its absence.
# view-ai owns a file format of its own now ([ai]'s table), so it leaves the
# serde/toml absence loop alongside view-native. serde_json gains the loop it
# never had: ACP is JSON-RPC, and the pure layers stay free of that wire
# format the same way they stay free of serde and toml.
for crate in view-core view-engine view-surface view-tui view-oracle view-bench; do
  check_absent "$crate" serde
  check_absent "$crate" toml
  check_absent "$crate" serde_json
done
check_absent view-native serde_json

# view-test-support (the ScratchDir fixture, see its own module doc) is
# dev-only by charter: it exists to be pulled in as a [dev-dependencies]
# entry by any crate's own tests, never as a normal dependency of anything
# that ships. check_absent's presence-only test can't express that -- it
# would also reject the legitimate dev-dependency edges view-native and
# view already declare -- so this reads cargo metadata's per-dependency
# `kind` field directly (null for a normal edge, "dev"/"build" otherwise)
# instead.
check_dev_only() { # usage: check_dev_only <crate>
  if jq -e --arg d "$1" \
    '.packages[].dependencies[] | select(.name == $d and .kind != "dev")' \
    <<<"$meta" >/dev/null; then
    echo "AUDIT FAIL: $1 must only appear as a dev-dependency, never a normal or build one"; fail=1
  fi
}
check_dev_only view-test-support

# view -> view-oracle exists so the bin crate's live supervision test
# (crates/view/tests/supervision_live.rs) asserts against the oracle's own
# DETECTION_BOUND instead of re-deriving it, and nothing in the shipped
# binary may reach the oracle. check_dev_only above cannot say that: it is a
# statement about every consumer of a crate, and view-bench and view-harness
# hold sanctioned normal edges to view-oracle. This is the same `kind` test
# narrowed to one edge, so the day that dev-dependency is promoted to a
# normal one the audit says so instead of shipping the oracle in the editor.
check_edge_dev_only() { # usage: check_edge_dev_only <crate> <dep>
  if jq -e --arg c "$1" --arg d "$2" \
    '.packages[] | select(.name == $c) | .dependencies[] | select(.name == $d and .kind != "dev")' \
    <<<"$meta" >/dev/null; then
    echo "AUDIT FAIL: $1 may depend on $2 only as a dev-dependency, never a normal or build one"; fail=1
  fi
}
check_edge_dev_only view view-oracle

# view-harness is bin-only (the corpus loader + oracle/bench runner CLIs);
# the dependency graph only ever points into it (view-harness ->
# view-oracle, view-harness -> view-bench -- the bench bin drives
# view-bench's scenario matrix while owning the TOML baseline I/O
# view-bench itself must stay free of), never out. Nothing else may
# depend on it.
for crate in view view-core view-engine view-tui view-surface view-native view-ai view-oracle view-bench; do
  check_absent "$crate" view-harness
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
# view-bench and view-harness reach the same rmpv edge one hop further out,
# through their own sanctioned dependency on view-oracle; neither one's own
# API exposes rmpv the same way view-oracle's does not.
check_transitive_reach rmpv view-engine view view-oracle view-bench view-harness
# view-harness is the corpus loader's sanctioned TOML/serde home (see the
# check_absent loop above): its dependency on view-oracle is what makes the
# resolved graph legitimately reach these two through it, same shape as the
# rmpv widening just above. serde/toml also gain view-ai here because its
# manifest now declares both. serde_json had no line at all; view-harness
# already reaches it through the corpus loader, and view reaches it through
# view-ai.
check_transitive_reach serde      view view-harness view-native view-ai
check_transitive_reach toml       view view-harness view-native view-ai
check_transitive_reach serde_json view view-harness view-ai
check_transitive_reach crossterm view-tui view
check_transitive_reach ratatui view-tui view
check_transitive_reach arboard view
# grep-searcher/grep-regex back the live-grep source's in-process search
# (see the nucleo/ignore confinement loop above); view depends on
# view-native (the Executor glue crate), so the resolved graph legitimately
# reaches them through that sanctioned edge.
check_transitive_reach grep-searcher view-native view
check_transitive_reach grep-regex view-native view
check_transitive_reach tokio view view-ai
check_transitive_reach async-std
check_transitive_reach smol
exit $fail
