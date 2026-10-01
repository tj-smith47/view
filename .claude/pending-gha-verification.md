# Pending GHA verification

These are NOT bugs and NOT deferrable defects — they are verifications that
structurally need a GitHub Actions run to produce their evidence: a real
runner, published Actions, uploaded artifacts. Nothing local can stand in
for them, so they live here rather than in `known-bugs.md`, which tracks
findings that must be drained before work is called done.

One line per item, keyed to the run that proves it. A drained item is
deleted, with the run id going into the commit that deletes it; the history
of how a red run was fixed lives in the commit subjects.
