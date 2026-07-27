# Pending first-push verification

These are NOT bugs and NOT deferrable defects — they are verifications that
structurally REQUIRE the repository's first push to exist (a real GitHub
runner, published Actions, uploaded artifacts). They cannot be drained before
the first push, so they live here rather than in `known-bugs.md`, which is a
push blocker: a post-push item logged there would deadlock the very push it
gates.

Drain these in the first green CI run after the initial push, not before it.

- [ ] **Add the CI status badge to the README header**: the header
  (`README.md`, centered block) carries license + status badges but not a CI
  badge, because a valid Actions badge URL needs the pushed `owner/repo` slug,
  unknown until the remote exists. Add
  `![CI](https://github.com/<owner>/view/actions/workflows/ci.yml/badge.svg)`
  once the repo is pushed, matching the other repos' badge convention.

- [ ] **GitHub Actions CI workflow verification**: unverified on real GitHub
  runners until first push. Local evidence: actionlint clean, YAML parses,
  `task ci` green. Step formatting follows `~/.claude/rules/github-actions.md`.
  First-push flow for the bench job (both legs run `--record`, because a
  `--gate` with no baseline compares nothing and would report success anyway):
  download the `bench-baseline-gh-linux` / `bench-baseline-gh-macos` artifacts
  from the first green run, commit them into `crates/view-bench/baselines/`,
  and switch `--record` to `--gate`, dropping the "Upload recorded baseline"
  step from ci.yml in that same commit. If the null-pair calibration refuses on a shared runner
  (ambient noise above the 1.15 floor in bench.rs), a per-class floor is the
  likely fix — decide from the observed calibration numbers in the run log,
  not speculatively. (Fixture tests are unix-gated by design; Windows runs the
  portable suite.)
