# Pending GHA verification

These are NOT bugs and NOT deferrable defects — they are verifications that
structurally need a GitHub Actions run to produce their evidence: a real
runner, published Actions, uploaded artifacts. Nothing local can stand in
for them, so they live here rather than in `known-bugs.md`, which tracks
findings that must be drained before work is called done.

One line per item, keyed to the run that proves it. A drained item is
deleted, with the run id going into the commit that deletes it; the history
of how a red run was fixed lives in the commit subjects.

## Needs a manual action

- [ ] Release workflow dry run (T15, `.github/workflows/release.yml`
      `workflow_dispatch`): cosign OIDC identity against the workflow ref,
      the aarch64 cross-link leg, the macOS legs; read the verify step's
      count. User first: `gh variable set ANODIZER_VERSION` on view (floor
      v0.23.0; cfgd runs v0.25.2, re-verify the config against it).
- [ ] dev-macos owes its five real-config seats (`echo.user`,
      `echo_speculated.user`, `scroll.user`, `flood.user`, `startup.user`)
      from an mbp quiet window: `task user-fixture`, then one `--record` per
      scenario. Until then its bench leg reports `GATE COVERAGE FAIL` on
      them by construction.
