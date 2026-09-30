#!/bin/sh
# WHY: shared by the tapes that show the agent reviewing a change. The stub
# agent's own proposal is three lines of alpha/beta/gamma, a hunk nobody
# can read at 220 columns, so its `propose` is pointed at a real Rust file
# of this repo: `request_all` in look.rs rewritten as one iterator
# chain, with its doc comment reworded. The new text is generated from the
# file as it stands, so the hunk stays that one function whatever else the
# file has become, and the stub reads the old text off disk as it
# proposes. The panel is titled "Agent".
# The stub resolves the path against its cwd, which is view's, so the
# caller starts view from the repo root. view's agent inherits these
# variables through the tmux server the caller creates.
#
# Sourced, never executed.
# Usage: stub_proposal ROOT CACHEDIR (sets STUB_DIFF_FILE, repo-relative)
stub_proposal() {
  STUB_DIFF_FILE=crates/view-core/src/update/look.rs
  stub_new="$2/stub-proposal-look.rs"
  awk -v new='/// Every window grid'"'"'s owed inner size, in ascending grid order: one\n/// request for each grid whose slot, look or margin has moved.\npub(crate) fn request_all(model: &mut Model) -> Vec<Effect> {\n    let grids = model.engine.grids().window_grids();\n    grids\n        .into_iter()\n        .flat_map(|grid| request_for(model, grid))\n        .collect()\n}' '
    /^\/\/\/ What every window grid owes, in ascending grid order\.$/ {
      print new; skip = 1; found = 1; next
    }
    skip && /^}$/ { skip = 0; next }
    !skip { print }
    END { exit !found }
  ' "$1/$STUB_DIFF_FILE" >"$stub_new" || {
    echo "stub_proposal: request_all is gone from $STUB_DIFF_FILE; pick another function" >&2
    return 2
  }
  VIEW_AI_STUB_TITLE=Agent
  VIEW_AI_STUB_DIFF_PATH=$STUB_DIFF_FILE
  VIEW_AI_STUB_DIFF_NEW=$stub_new
  export VIEW_AI_STUB_TITLE VIEW_AI_STUB_DIFF_PATH VIEW_AI_STUB_DIFF_NEW
}
