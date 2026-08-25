# The gh runners' own spread, measured: three statistics the compiled defaults cannot cover, and two findings they must keep reporting

Date: 2026-08-25
Classes: gh-linux (ubuntu-latest), gh-macos (macos-latest), both shared
hosted runners, ambient load uncontrolled and unreservable.
Method: no new runner time. Every bench leg CI has already run on these
classes was harvested from its job log — 10 legs on gh-linux, 11 on
gh-macos, 2026-08-03 to 2026-08-24, engine pin v0.12.4 throughout — and
each leg's printed `gated <metric> <value>` line is one draw. Sizing is the
2026-07-27 rule unchanged: median, half-width `(max-min)/2`, worst
excursion over the recorded value, and a factor clearing both the worst
excursion and `median + 2 x half-width` against the recorded value.

## What failed

Run 32784781768's gh-linux record leg exited 3:

```
RECORD REGRESSION MASKED [scroll.minimal] ratio_p50: measured 2.5372 would
breach the recorded bar 1.9669, held to keep the ratchet; a later --gate
will breach on it, so investigate first
```

`plan_record` marks a held metric masked when `value > headroom.bar(recorded)`.
Neither gh class shipped a `<class>.headroom.toml`, so every metric there
was judged by the compiled default: `1.9669 x 1.25 = 2.4586 < 2.5372`.

It was not view. The same commit measured `scroll` `ratio_p50` **1.620**
against a 1.605 bar on the coordinated quiet dev-linux host (+1%), and the
leg's own operands moved together — `scroll/minimal` view p50 1.783 ms and
nvim p50 0.697 ms were both the slowest readings of the ten legs:

| leg | date | view p50 | nvim p50 | ratio_p50 |
|---|---|---|---|---|
| 30874125937 | 08-04 | 0.793 | 0.383 | 2.086 |
| 30856641597 | 08-03 | 0.917 | 0.440 | 2.107 |
| 30879797693 | 08-04 | 1.046 | 0.590 | 1.809 |
| 32531092866 | 08-21 | 1.082 | 0.546 | 1.897 |
| 32619785446 | 08-23 | 1.162 | 0.588 | 1.987 |
| 32534348333 | 08-21 | 1.188 | 0.566 | 2.045 |
| 32537625267 | 08-21 | 1.251 | 0.636 | 1.967 |
| 32533279176 | 08-21 | 1.293 | 0.621 | 2.057 |
| 30861044267 | 08-03 | 1.376 | 0.600 | 2.295 |
| 32784781768 | 08-24 | 1.783 | 0.697 | **2.537** |

The runner's absolute speed swings 2.2x (view) and 1.8x (nvim) between
legs. Per-sample interleaving cancels the shared part; what it does not
cancel is view's scroll degrading faster than nvim's as the runner slows,
and that residual is the spread a factor has to cover.

## The harvest

Every statistic that gates on a *shared* class — no `p99` component, no
`cold` component, not a host-regime memory absolute — across every leg.
`2x rule` is `(median + 2 x half-width) / recorded`.

### gh-linux (10 legs, runner load 1.03 to 3.64)

| cell | metric | n | median | min..max | half-width | recorded | worst/rec | 2x rule | default | verdict |
|---|---|---|---|---|---|---|---|---|---|---|
| scroll.minimal | ratio_p50 | 10 | 2.051 | 1.809..2.537 | 17.7% | 1.9669 | 1.290 | **1.413** | 1.25 | **entry 1.42** |
| scroll.heavy | ratio_p50 | 10 | 2.188 | 2.067..2.553 | 11.1% | 2.0943 | 1.219 | 1.277 | 1.25 | covered by the same entry |
| picker.minimal | first_page_p50_ms | 6 | 6.321 | 2.739..14.702 | 94.6% | 11.0723 | 1.328 | **1.651** | 1.5 | **withdrawn, see below** |
| first_paint.minimal | marker_ratio_p50 | 10 | 0.203 | 0.126..0.216 | 22.2% | 0.1733 | 1.246 | 1.691 | 1.25 | finding, no entry |
| first_paint.heavy | marker_ratio_p50 | 10 | 0.4395 | 0.321..0.459 | 15.7% | 0.4262 | 1.077 | 1.355 | 1.25 | finding, no entry |
| echo.minimal | ratio_p50 | 10 | 1.162 | 1.113..1.209 | 4.1% | 1.1614 | 1.041 | 1.083 | 1.25 | inside the default |
| echo.heavy | ratio_p50 | 9 | 1.093 | 1.031..1.152 | 5.5% | 1.0631 | 1.084 | 1.142 | 1.25 | inside the default |
| echo_control.minimal | control_ratio_p50 | 10 | 1.024 | 0.986..1.039 | 2.6% | 0.9857 | 1.054 | 1.093 | 1.25 | inside the default |
| echo_control.heavy | control_ratio_p50 | 10 | 1.024 | 0.981..1.060 | 3.9% | 1.0084 | 1.051 | 1.094 | 1.25 | inside the default |
| echo_speculated.minimal | speculated_ratio_p50 | 6 | 0.366 | 0.355..0.379 | 3.3% | 0.3672 | 1.032 | 1.062 | 1.25 | inside the default |
| flood.minimal | pace_ratio | 10 | 1.014 | 0.987..1.044 | 2.8% | 0.9882 | 1.057 | 1.084 | 1.25 | inside the default |
| picker.minimal | match_paint_p50_ms | 6 | 3.256 | 3.192..3.704 | 7.9% | 3.2969 | 1.123 | 1.143 | 1.5 | inside the default |
| remote_memory.minimal | remote_local_ratio | 6 | 1.007 | 0.993..1.016 | 1.1% | 1.0066 | 1.009 | 1.024 | 1.25 | inside the default |
| first_paint.* | shell_visible_ms | 3 | 3.94/3.97 | 3.38..24.90 | — | not recorded | — | — | 1.5 | 3 draws: too few |

### gh-macos (11 legs, runner load 2.09 to 35.15)

| cell | metric | n | median | min..max | half-width | recorded | worst/rec | 2x rule | default | verdict |
|---|---|---|---|---|---|---|---|---|---|---|
| first_paint.minimal | marker_ratio_p50 | 10 | 0.2365 | 0.185..0.252 | 14.2% | 0.2053 | 1.227 | **1.478** | 1.25 | **entry 1.48** |
| first_paint.heavy | marker_ratio_p50 | 10 | 0.3855 | 0.359..0.415 | 7.3% | 0.3732 | 1.112 | 1.183 | 1.25 | covered by the same entry |
| echo.heavy | ratio_p50 | 10 | 1.055 | 0.798..1.503 | 33.4% | not recorded | — | — | 1.25 | finding, no entry |
| echo.minimal | ratio_p50 | 11 | 1.140 | 1.103..1.176 | 3.2% | 1.1027 | 1.066 | 1.100 | 1.25 | inside the default |
| echo_control.heavy | control_ratio_p50 | 11 | 1.027 | 0.739..1.353 | 29.9% | 1.3207 | 1.024 | 1.243 | 1.25 | inside the default, barely |
| echo_control.minimal | control_ratio_p50 | 11 | 1.013 | 1.005..1.022 | 0.8% | 1.0218 | 1.000 | 1.008 | 1.25 | inside the default |
| scroll.minimal | ratio_p50 | 11 | 1.870 | 1.730..1.990 | 7.0% | 1.9423 | 1.025 | 1.097 | 1.25 | inside the default |
| scroll.heavy | ratio_p50 | 10 | 1.958 | 1.882..2.248 | 9.3% | 2.2484 | 1.000 | 1.033 | 1.25 | inside the default |
| flood.minimal | pace_ratio | 10 | 0.9895 | 0.834..1.086 | 12.7% | 1.0310 | 1.053 | 1.204 | 1.25 | inside the default |
| echo_speculated.minimal | speculated_ratio_p50 | 6 | 0.293 | 0.288..0.310 | 3.8% | not recorded | — | — | 1.25 | nothing to gate |
| picker.minimal | first_page_p50_ms | 5 | 7.397 | 2.786..30.666 | 188.5% | not recorded | — | — | 1.5 | nothing to gate |
| picker.minimal | match_paint_p50_ms | 5 | 3.508 | 3.298..6.416 | 44.4% | not recorded | — | — | 1.5 | nothing to gate |
| remote_memory.minimal | remote_local_ratio | 5 | 1.008 | 0.989..1.027 | 1.9% | not recorded | — | — | 1.25 | nothing to gate |
| first_paint.* | shell_visible_ms | 4 | 31.1/32.0 | 27.27..33.91 | — | not recorded | — | — | 1.5 | 4 draws: too few |

Both tables carry 14 rows, counted the same way (each folds the two
`first_paint` `shell_visible_ms` cells into one row). Five of gh-linux's 14
rows cross their default and one of gh-macos's 14 does: on gh-linux, one
entry covers the two crossing `scroll` rows, the two crossing
`first_paint.marker_ratio_p50` rows are a finding rather than a spread, and
`picker.first_page_p50_ms` is withdrawn; on gh-macos the single crossing row
takes the one entry. The rest keep their default, and that absence is the
file's statement, not a gap: gh-linux resolves `echo`'s `ratio_p50` to 4.1% and `remote_local_ratio`
to 1.1%, so a default of 1.25 there is loose, but tightening it on draws
taken from ten *different commits* would be the unverified-constant move
the 2026-07-27 campaign exists to end. Tightening needs an unchanged-pair
campaign, which a hosted runner cannot supply.

## The two entries

| class | key | factor | worst excursion | 2x rule | forward ratchet (lowest draw x factor vs worst draw) | replaces |
|---|---|---|---|---|---|---|
| gh-linux | `"scroll.ratio_p50"` | 1.42 | 1.290 | 1.413 | 1.809 x 1.42 = 2.5688 > 2.537 (+1.2%); heavy 2.067 x 1.42 = 2.935 > 2.553 | 1.25 |
| gh-macos | `"first_paint.marker_ratio_p50"` | 1.48 | 1.227 | 1.478 | 0.185 x 1.48 = 0.2738 > 0.252; heavy 0.359 x 1.48 = 0.5313 > 0.415 | 1.25 |

Scoping follows the spread. `scroll.ratio_p50` is scenario-scoped because
`echo`'s `ratio_p50` on the same legs holds inside 4.1% — the wide reading
is scroll's, not the host's ratio resolution — and no host-wide entry is
earned on either class.

The forward-ratchet column is the check that separates a factor which holds
from one that merely fits today's recorded value. The recorded bar is a
min-ratchet, so the lowest honest draw is the seat the class will settle
on; a factor holds only if that seat times the factor still clears the
band's worst reading. Both entries clear it, `scroll.minimal` by 1.2% —
thin, stated, and a later draw that turns it negative is a re-sizing rather
than a rounding.

### `picker.first_page_p50_ms` is withdrawn, not sized

Six draws (runs 32531092866, 32533279176, 32534348333, 32537625267,
32619785446, 32784781768; 2026-08-21/24): 14.702, 6.123, 6.519, 11.072,
2.739, 3.376. Median 6.321, half-width 5.9815 (94.6%), worst excursion over
the recorded 11.0723 only 1.328x, 2x rule 1.651x.

That band cannot satisfy both halves of the published-spread contract at
once. Refusing the 6.123 draw as a recorded seat needs
`11.0723 x (2 - f) > 6.123`, i.e. `f < 1.447`; the 2x rule asks
`f >= 1.651`. Any factor in between passes the forward-ratchet check only
until one honest improving draw seats a lower bar, and then the next honest
draw breaches it:

```
recorded 11.0723  bar 18.380  record_floor = 2(11.0723) - 18.380 = 3.7646
  draws 2.739, 3.376 -> below floor -> RefusedBelowSpread
  draw  6.123        -> above floor -> Improved, recorded becomes 6.123
recorded  6.123   bar = 6.123 x 1.66 = 10.164
  draws 11.072, 14.702 -> masked regression, leg exits 3
```

Resizing to `14.702 / 6.123 = 2.41` would clear that, but it sizes against
a seat the class has not taken and admits a 2.4x move on a statistic whose
worst observed excursion is 1.328x. The compiled absolute default (1.5) is
never crossed by that excursion, so absence masks nothing today. Six draws
is too few to resolve a 94.6% band, and that is the reason recorded here —
the same reason `shell_visible_ms` is named at 3 and 4 draws. The entry
waits for draws, not for a factor chosen to bridge the contradiction.

## Two findings the sidecars deliberately do not absorb

**gh-linux `first_paint.marker_ratio_p50` stepped, and it is view's own
cold start.** The pooled draws would ask 1.70 (minimal) and 1.36 (heavy),
both past the default, but the two groups do not overlap and nvim is flat:

| | 2026-08-03/04 | 2026-08-21/24 |
|---|---|---|
| view cold marker p50 | 15.2, 17.0, 18.1, 21.9 ms | 25.9, 26.6, 26.9, 27.3, 27.4 ms |
| nvim cold marker p50 | 121.1, 121.6, 122.7, 126.5 ms | 126.0, 127.1, 127.1, 127.4, 128.0 ms |
| `marker_ratio_p50` minimal | 0.126..0.173 | 0.202..0.216 |

view's cold marker p50 rose ~55% somewhere between 6ed8bc9 (08-04) and
f63f7d0 (08-21). The recorded 0.1733 predates the step, and today's
readings sit under its 1.25 bar (0.2166) by under 1%: the next gh-linux
leg may well exit 3 on this metric, and if it does, that is the gate
working. Same statistic on gh-macos is the opposite case — view's cold
marker p50 spans 40.1..57.6 ms with the 08-03/04 group already reaching
56.4 and nvim flat at 217..234 — overlapping in both directions, which is
spread, and that is the one that earned an entry.

**gh-macos `echo.heavy` `ratio_p50` needs a fixture-scoped key that the
grammar does not have.** Its stale 0.9316 bar exited the record leg 3 on
runs 32534348333 (measured 1.5031) and 32537625267 (measured 1.1726), and
741ad03 deleted the key so the next record reseats it honestly. That reseat
has not landed, so the cell records fresh today and masks nothing. But the
band is 0.798..1.503 (33.4% half-width) while `echo/minimal` on the same
runner is 1.103..1.176 (3.2%), and a headroom key scopes to a scenario,
never to one fixture: the factor honest for heavy (1.89 against a 0.9316
seat) would loosen minimal's bar from 1.378 to 2.095. Seating heavy from a
single draw of that band under the compiled default puts the class straight
back where those two runs were. The honest fix is an optional fixture level
in the key grammar (`"echo.heavy.ratio_p50"`), which `declared_factor`,
`headroom_for`, `declared_headroom` and `require_headroom_bound` would all
have to learn; it is not written here because an entry no recorded cell
binds is a load error, and heavy records nothing to bind to today.

## What changed

```toml
# crates/view-bench/baselines/gh-linux.headroom.toml
machine_class = "gh-linux"

[headroom]
"scroll.ratio_p50" = 1.42
```

```toml
# crates/view-bench/baselines/gh-macos.headroom.toml
machine_class = "gh-macos"

[headroom]
"first_paint.marker_ratio_p50" = 1.48
```

Both files are loaded by the run's own class:
`bench.rs:932-938` derives the sidecar path from `baseline_path(&cli.class)`,
and both legs invoke `task bench -- --all --class gh-{linux,macos}
--record` at `.github/workflows/bench.yml:96` — the not-yet-pushed workflow
that is nonetheless the only live invocation at this commit, since 7e4abf2
moved the two jobs out of `ci.yml`. Every leg harvested above predates that
split and ran as `ci.yml`'s `bench` job, which is why the runs are listed
under `--workflow=ci.yml` below. No wiring beyond the files is needed:
`every_shipped_headroom_sidecar_binds_to_its_baseline` already walks the
whole baselines directory, so both new files load and bind under
`task ci` today.

`the_gh_linux_sidecar_admits_the_runners_scroll_spread_and_nothing_past_it`
pins the case against the committed pair: `plan_record` over the shipped
gh-linux baseline reports no masked regression at 2.5372, reports one at
`bar + 0.001`, and asserts the compiled default still would have failed the
leg — so deleting the sidecar, or shrinking the factor below the campaign's
own worst excursion, fails the test rather than one CI bench run later.

## Reproducing

```bash
gh run list --workflow=ci.yml --limit 40 \
  --json databaseId,headSha,conclusion,createdAt
gh run view <run-id> --json jobs        # the bench (…, gh-linux) job id
gh api /repos/tj-smith47/view/actions/jobs/<job-id>/logs > leg.log
grep -E '^[a-z_]+/[a-z]+:|gated ' leg.log
```

`gh run view <run-id> --log --job <job-id>` returns empty for these runs;
the REST logs endpoint is what serves them.
