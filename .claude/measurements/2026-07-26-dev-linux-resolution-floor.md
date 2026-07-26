# dev-linux measurement resolution floor (task #62)

Measured 2026-07-26. Engine pin v0.12.4, 12 cores. Method + pre-registration:
`~/.claude/tmp/floor/floor-prereg.md` (fixed before data); campaign scripts
`floor-run.sh` / `floor-analyze.py`; raw data `~/.claude/tmp/floor/dataA/campA.tsv`.

**The floor is a property of the measurement apparatus, not of any recorded bar.**
It answers: what is the smallest effect a SINGLE dev-linux measurement can resolve
above this host's own quiet replicate spread? Interpretation rule (pre-registered):
to distinguish two conditions from one measurement each, their true medians must
differ by more than **2x the half-width** (each measurement can sit a half-width
off its own centre, toward the other).

## Established floors (Campaign A, quiet host, loads 0.07-0.48)

Report-only runs, same release+taps binaries, per-run start/end load recorded,
quiet gate = both loads <= 0.60. Interleaved echo,input_path rounds x16;
14 (echo) and 13 (input_path) qualified.

| metric (cell)            | median | half-width (floor) | single-shot resolvable (2x) |
|--------------------------|--------|--------------------|-----------------------------|
| `ratio_p50` (echo/min)   | 1.28   | 0.034  (2.66%)     | **0.068**                   |
| `p99_us` (input_path/min)| 283 us | 15.7 us (5.53%)    | **31 us**                   |

### Consequences for the blocked tasks

- **HANDOFF headline confirmed.** The handoff's "ambient load alone moves
  ratio_p50 by ~0.06" is corroborated cleanly: the quiet single-shot resolution
  limit is 0.068. Task #57's 0.048 echo-inversion effect sits BELOW it, so
  dev-linux genuinely cannot resolve that effect single-shot -- the handoff's
  conclusion, now with numbers.
- **#50 (input_path) is NOT floor-limited, but IS architecture-limited (§5.6).**
  The gap (~270->100 us, ~170 us) is ~5x the 31 us single-shot threshold, so a
  real improvement WOULD be resolvable -- but the tap decomposition + a bare-metal
  mbp cross-check show there is no ~200 us of reducible view code to find: the
  budget is physically inconsistent with the hard-rule-mandated three-thread path.
  See `2026-07-26-input_path-floor-unreachable.md`. USER SPEC DECISION pending.
- **#61 (terminal-size cache)** does not depend on this floor: per the handoff it
  rests on the stage collapse (22.5->0.6 us, 18x its stage spread) and a
  sabotage-verified resize-invalidation test, and spec 3.1 is not being amended.
  The floor is context, not its gate.

## Established: first_paint `cold_ms` floor (Campaign B)

Measured self-load-inclusive (first_paint's own 1000 cold spawns/side drive the
host to ~4-5, so the floor MUST include that variance -- #49's own measurement
sees the same storm; gated on load_start only, excluding a heavy foreign build at
load_start > 8.0). 6 rounds, all 6 qualified. Raw: `~/.claude/tmp/floor/dataB/campB.tsv`.

| metric (cell)              | median   | half-width (floor) | single-shot resolvable (2x) |
|----------------------------|----------|--------------------|-----------------------------|
| `cold_ms` (first_paint/min)| 55.66 ms | 2.50 ms (4.48%)    | **4.99 ms**                 |

### Consequence for #49/#5 (first_paint 54.7 -> 50 ms)

The 4.7 ms effect-to-close sits JUST below the 4.99 ms single-shot resolution --
so a single measurement CANNOT distinguish 50 from 54.7. BUT the campaign is not
single-shot: all 6 qualifying replicates lie in [53.16, 56.70] ms, EVERY one above
the 50 ms budget (median 55.66, +5.66). The gap is REAL, not a noise artifact.
Therefore any #5 fix MUST be verified by an interleaved replicate campaign (median
of >=6), never one run. Unlike input_path (§5.6-unreachable), first_paint measures
view's own cold startup-to-shell-frame path, which is view-controllable -- #5 is a
genuine optimization, not (yet) a spec-amendment case; whether it is reachable is
open until the startup path is decomposed.

**RESOLVED 2026-07-26.** The startup path decomposed to a single cause: ~52 ms
of the 55.66 ms was view's capability probe waiting out its 50 ms fallback
deadline against a bench pty that never answered the DA1 fence -- an apparatus
defect, not view cost (the 55.66 ms was reproducible, so "real, not noise" held;
its source was the harness, not the startup path). With `PtySession` answering
DA1 the way every real terminal does, the interleaved replicate campaign
mandated above (6 rounds, all qualifying, [3.500, 3.607] ms) gives median
cold_ms **3.554 ms**, 14x under the 50 ms budget. Evidence + fix:
`2026-07-26-first_paint-probe-artifact.md`. Baselines re-recorded ~53.3 -> ~3.5 ms.
