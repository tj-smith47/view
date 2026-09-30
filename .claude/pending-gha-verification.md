# Pending GHA verification

These are NOT bugs and NOT deferrable defects — they are verifications that
structurally need a GitHub Actions run to produce their evidence: a real
runner, published Actions, uploaded artifacts. Nothing local can stand in
for them, so they live here rather than in `known-bugs.md`, which tracks
findings that must be drained before work is called done.

One line per item, keyed to the run that proves it. A drained item is
deleted, with the run id going into the commit that deletes it; the history
of how a red run was fixed lives in the commit subjects.

## Proven by Bench 36733287414 on master (1a222a46)

- [ ] Bench: both gh legs `gate OK` on the seats hand-committed after run
      36665386464 (1ea7d5ee); `startup.user server_delta_ms` printed as
      recorded and not gated (a0ca3d4c); `picker.minimal` green on gh-macos
      with `echo_control` inside its bar (tripwire #28, a run whose control
      breaches is discarded); the WB-E relay cells at or under seat.

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
