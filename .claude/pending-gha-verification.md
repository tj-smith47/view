# Pending GHA verification

These are NOT bugs and NOT deferrable defects — they are verifications that
structurally need a GitHub Actions run to produce their evidence: a real
runner, published Actions, uploaded artifacts. Nothing local can stand in
for them, so they live here rather than in `known-bugs.md`, which tracks
findings that must be drained before work is called done.

Drain these against a green GHA run.

- [x] *(drained 2026-08-04: badge live in the README centered header block,
  renders against the real slug)* **Add the CI status badge to the README header**: the header
  (`README.md`, centered block) carries license + status badges but not a CI
  badge, because a valid Actions badge URL needs the real `owner/repo` slug,
  unknown until the remote exists. Add
  `![CI](https://github.com/<owner>/view/actions/workflows/ci.yml/badge.svg)`
  once the remote exists, matching the other repos' badge convention.

- [x] *(drained 2026-08-04: all 9 jobs verified green on real runners across
  runs 30861044267 + 30868862255; baselines committed from run 30861044267
  (gh-linux, pre-refusal harness 580429d — no measured-code delta to HEAD) and
  run 30868862255 (gh-macos, d196563, full clean matrix, no refusals);
  --record flipped to --gate and upload step removed in the same commit.
  Calibration DID refuse once on gh-linux (ratio_p50 1.1734 vs 1.15 floor,
  run 30868862255) but the same class passed cleanly on 30861044267, so the
  observed numbers say transient boot noise, not a noisier class — no
  per-class floor. Follow-up: the first gate run 30874125937 failed both gh
  legs by construction — class-unscoped budgets + cold-absolute ratchet;
  ratified fix in HANDOFF.md section 0)* **GitHub Actions CI workflow verification**: unverifiable off real GitHub
  runners. Local evidence: actionlint clean, YAML parses,
  `task ci` green. Step formatting follows `~/.claude/rules/github-actions.md`.
  Arming flow for the bench job (both legs run `--record`, because a
  `--gate` with no baseline compares nothing and would report success anyway):
  download the `bench-baseline-gh-linux` / `bench-baseline-gh-macos` artifacts
  from the first green run, commit them into `crates/view-bench/baselines/`,
  and switch `--record` to `--gate`, dropping the "Upload recorded baseline"
  step from ci.yml in that same commit. If the null-pair calibration refuses on a shared runner
  (ambient noise above the 1.15 floor in bench.rs), a per-class floor is the
  likely fix — decide from the observed calibration numbers in the run log,
  not speculatively. (Fixture tests are unix-gated by design; Windows runs the
  portable suite.)

- [x] *(drained 2026-08-29: Bench run 33204611952 at 68b06a4 green on both
  legs (gh-linux 42 min, gh-macos 100 min); its recorded artifacts carry all
  17 armed cells per leg and are committed byte-for-byte; gh-macos
  input_path.minimal refused (tap overhead p99 6.0us > 5us) and holds an
  empty cell the coverage pin reads as bar-less, not missing; legs flipped
  back to --gate, upload step removed)* gh-leg baseline coverage (T5 fix-round I4): gh-linux/gh-macos baselines record
  12 of 15 cells — picker, supervision, and echo_speculated are absent. Those
  cells can only be recorded on gh runners, so run the gh `--record`
  workflow on GHA to add them. Ruling 2026-08-15: gh legs are regression
  tripwires (gate-attestation split); documented absence does not breach, so
  this does not block Part A exit. The legs' pre-existing red predates T5.

- [x] *(drained 2026-09-02: CI run 33648727384 at master 81aa630 green on
  all 10 jobs. The chain grew to five findings before the run came green —
  #27 sleep-race (9d62fb4), #28 stderr-on-PTY guard (2e12515/f569c7a/4f1cdaf),
  #29 headless-clipboard linux premise (a3b960a + 2ffef75 needle gate),
  #30 Windows stdpath hermeticity (037cdda; windows leg green from a2a6deb
  onward), #31 toast reap-wait host scaling (81aa630). Bench leg tracked to
  conclusion separately as a regression tripwire.)* **Findings #27 + #28
  verified on the gh-macos runner** (commits 9d62fb4,
  2e12515, f569c7a, 4f1cdaf): both defects were caught by conditions only that
  runner produces — a loaded 3-core host that reorders sleeping threads
  (#27, external_write_live sleep race) and a session whose pasteboard write
  is denied so AppKit logs to stderr mid-paint (#28, stderr shares the PTY).
  Local evidence is complete (task ci green ×3, red-first pins, windows-gnu
  clippy clean, sonnet review PASS); the drain is one green CI run at
  master 4f1cdaf, plus its Bench leg concluding.

- [ ] **Release workflow tag-time residue (T15, commits 24fceb8+6408a2f)**:
  the parts of `.github/workflows/release.yml` only a real GHA run can
  prove — the cosign OIDC identity string matching the workflow ref, the
  aarch64 cross-link leg, and the macOS runner legs. The workflow carries a
  `workflow_dispatch` dry-run path (build+stage+sign+verify, publish gated
  on tag push) built for exactly this drain: run the dispatch once the
  branch lands on master, read the verify step's count check. Local
  evidence complete: layout/pin/every-platform tests green in ci,
  stage+archive real on dev-linux and winserver, actionlint clean.
  Prerequisite (user): `gh variable set ANODIZER_VERSION` on view — floor
  is v0.23.0 (the capture's version); cfgd runs v0.25.2, re-verify the
  config against it before matching.

## Tripwire #28 — gh-macos picker.minimal bench leg (fix 767606b + 8f3c55e)
- Breach site: gh-macos first_page_p50 7.903 > bar 4.5053 at 81aa630 (run 33648727369).
- Fix landed locally: fixture surface re-attach (767606b) + compat repairs (8f3c55e); local evidence E=2.260 at tip / D falsifier 2.376-vs-2.682 at 45808c1.
- Drain: first bench run at the landed tree must show the gh-macos picker.minimal leg green at the standing seat (no re-seat, no --record). Draws taken between 45808c1 and the fix landing are inadmissible for that cell.

- 2026-09-06 `crates/view-bench/tests/swap_hygiene.rs` `#[cfg(windows)]` pin for `empty_swap_dir` against a held-open file (a2e7863): compiles for `x86_64-pc-windows-gnu`, unrun — winserver was unreachable ("no route to host"); verify on the gh-windows leg or on winserver when it is back.

- 2026-09-06 **Bench check goes red on the withdrawn first-paint ratios, by
  construction** (87e6c44): `first_paint.{minimal,heavy,user}`
  `marker_ratio_p50` and `marker_ratio_p99` were withdrawn from
  `gh-linux.toml` and `gh-macos.toml`, and a withdrawn metric gates as
  `Unbarred::Unrecorded` — a coverage failure — so the first Bench run on
  master after this fix fails both gh legs on those six cells. The two gh
  classes also carry `startup.minimal` `settled_ratio_p50` and
  `server_delta_ms` as withdrawn, which the same run fails on for the same
  reason. dev-linux is re-seated from the 2026-09-06 quiet-window retake
  (marker_ratio_p50 1.0937 minimal / 1.1008 heavy / 1.0843 user, p99 1.0762
  / 1.0454 / 1.0460, settled_ratio_p50 1.0955, server_delta_ms 0.682);
  dev-macos is withdrawn until its own mbp retake lands. The reasons live
  in each baseline's `[withdrawn.<scenario>.<fixture>]` table, so the
  re-seat has the fault and the admissibility rule beside the cell it
  clears. There is no
  `--record` leg in CI to re-arm them (`bench.yml:94` is the only bench
  invocation and it is `--gate`), so nothing heals this on its own. What
  clears it: that same run's `bench-measured-gh-linux` /
  `bench-measured-gh-macos` artifacts (`crates/view-bench/baselines/
  <class>.measured.toml`, uploaded `if: always()`) carry the cells measured
  under the answering pty; a human re-seats them into the class baselines and
  commits. Only draws from a run at or after 6b89c29 are admissible.

- 2026-09-06 **ConPTY no longer stalls on the cursor report** — unrun.
  `QueryPolicy`'s responder now answers a bare `\x1b[6n`, which
  `docs/conpty-harness-wire-capture.md` measured as the whole reason a
  Windows leg produced 4 bytes and never exited (`cmd.exe /c echo hi` as well
  as nvim). Expected change: a re-capture on Windows no longer hangs to its
  deadline and no longer reports an empty screen. Not observable on this
  host — the ConPTY arm is a winserver run and winserver is unreachable ("no
  route to host") — so verify on the gh-windows leg or on winserver when it
  is back.

- 2026-09-06 **dev-linux records all five real-config seats; the other
  three classes still owe them.** `echo.user`, `echo_speculated.user`,
  `scroll.user`, `flood.user` and `startup.user` were recorded on dev-linux
  in two quiet windows on 2026-09-06 (`crates/view-bench/baselines/
  dev-linux.toml`; the three over their bar are `[[shortfall]]` entries).
  `dev-macos` seats its five from a quiet-window session on mbp
  (`task user-fixture` first, then one `--record` run per scenario);
  `gh-linux` and `gh-macos` seat theirs the way their first-paint ratios
  are seated -- from the `bench-measured-<class>.toml` artifact their gate
  leg uploads `if: always()`, re-seated into the class baseline by hand and
  committed, since CI runs no `--record` leg. Until then those three keep
  their `[withdrawn.<scenario>.user]` cells and their bench legs report
  `GATE COVERAGE FAIL` on them by construction.

- 2026-09-07 **Eight bound cells sit over their bar on the two gh classes,
  ledgered nowhere and correctly so.** Neither gh class is `controlled-`,
  so neither loads the spec budget table at all; both ratchet against their
  own recorded values, which is the 2026-08-15 gate-attestation split, and
  a `[[shortfall]]` there would record debt against a bound that never
  executes. They are regression tripwires, and they are re-seated from the
  `bench-measured-<class>.toml` artifact of the first Bench run on master,
  the same way the withdrawn first-paint ratios are. Replayed against the
  committed baselines at be23a50 -- `find_budget`'s scenario + metric +
  `covers(class)` + `seats().contains(fixture)`, then the value against the
  bar:

  gh-linux, two cells:
  - `echo.minimal` `ratio_p50` 1.1614, bar 1.10
  - `first_paint.heavy` `marker_cold_ms` 87.805 ms, bar 30.0

  gh-macos, six cells:
  - `echo.heavy` `ratio_p50` 1.1490, bar 1.10
  - `echo.heavy` `view_p99_ms` 8.049 ms, bar 8.0
  - `echo.minimal` `ratio_p50` 1.1027, bar 1.10
  - `first_paint.heavy` `marker_cold_ms` 148.066 ms, bar 30.0
  - `first_paint.minimal` `marker_cold_ms` 71.101 ms, bar 30.0
  - `picker.minimal` `match_paint_p99_ms` 30.797 ms, bar 16.0

  The last one is the one to read twice: `picker.match_paint_p99_ms` is a
  felt row, and it is missed on both macOS classes -- dev-macos 19.479 ms
  after the 2026-09-06 retake (ledgered, `[[shortfall]]` at 19.4787) and
  gh-macos 30.797 ms, against a 16 ms bar. The same moment on the same
  platform, so the gh-macos twin belongs beside the dev-macos entry the
  moment a Bench artifact re-seats it.
