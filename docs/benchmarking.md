# Benchmarking

The maintainer-facing half of [performance](performance.md): how the numbers
on that page are produced, what every recorded cell is called, and how a bar
is re-seated. That page is written for someone deciding whether view is fast
enough to use; this one is written for whoever runs the harness.

## What a cell is allowed to claim

Every `[[budget]]` row in `crates/view-bench/budgets.toml` declares what it
means to a person, and the loader refuses a row that does not:

```toml
[[budget]]
spec_row = "Launch -> the settled screen, real config"
scenario = "startup"
metric   = "settled_ratio_p50"
max      = 1.0
kind     = "felt"            # felt | diagnostic | resource
felt     = "launch -> the screen you can start working in (tree, tabline, statusline settled), paired with nvim on the same host in the same run"
config   = "real"            # real (a plugin config a person runs) | fixture
```

- **felt** is a moment somebody lives through. It is measured under a real
  config -- the login-shaped `user` fixture -- paired against bare Neovim in
  the same run, and `bench.rs`'s
  `every_felt_bound_is_seated_on_the_config_it_claims` fails the build if the
  matrix seats it anywhere else. The exceptions are the rows with no
  bare-Neovim counterpart to pair against (view's own picker, the engine-wedge
  banner); each is listed in `view_harness::budgets::UNPAIRED_FELT` with its
  grounds, and a felt row that is neither is refused at load.
- **diagnostic** explains a felt row's number and never stands alone. It
  names the row it decomposes (`decomposes = "startup.settled_ratio_p50"`),
  the spec row says "Diagnostic", and it is never quoted as a win. A
  hundred microseconds inside the input path is not a moment: nobody
  perceives it, and a page that leads with it is measuring what was easy to
  measure.
- **resource** is a footprint. No speed language attaches to it at all.

`scripts/check-budget-drift.sh` holds both halves: every number in the file
must appear in the spec row it names, every diagnostic and resource row's
spec text must say which it is, and neither `README.md` nor
`docs/performance.md` may name a metric identifier at all. Those two pages
state moments in words ("the key reaches Neovim in under a tenth of a
millisecond"); identifiers live here.

## How we measure

Every latency comparison is *paired*: view and bare Neovim run in the same
invocation, on the same host, with the same config, and their samples are
interleaved so background noise lands on both sides equally. 1000 samples
per cell. `task perf-audit` reproduces the full matrix; `task bench` runs
the gated subset CI uses.

On macOS every timed target holds a power assertion for the length of its
run (`scripts/hold-awake.sh`, fronting each harness in `Taskfile.yml`): the
host takes unattended maintenance sleeps, and a Mach monotonic clock does
not advance across one, so a cell measured through a sleep is a number
nothing produced. Reach a measurement through its `task` target and the
assertion comes with it.

The engine is pinned (`.engine-pin`, currently Neovim `v0.12.4`) and the
harness verifies the binary on `PATH` actually reports that version before
recording anything. Plugin-heavy cells use a committed lazy.nvim fixture
with 15 plugins pinned by its `lazy-lock.json`, driven through a real pty.

### The three first-paint configs

Cold start is measured against three named fixtures, and every first-paint
number below says which one it belongs to:

| fixture | what it is |
|---|---|
| `minimal` | no plugins at all: nvim's own startup and nothing else |
| `heavy` | the committed 15-plugin lazy.nvim stack, loaded as that fixture's spec asks |
| `user` | the same pinned plugin set arranged as a login: a `lua/config` module tree, a leader, a colorscheme, and `setup()` called on every plugin the cache holds |

The `user` fixture is generated at run time from the plugin cache the
harness already keeps, never committed: its plugin set *is* whatever that
cache holds, and it shares the cache with `heavy` by carrying the same
lockfile, so a run installs nothing.

Its delta over `heavy` is the shape, not the plugin count. Both load their
top-level plugins eagerly and both enable treesitter highlighting. What `user`
adds is what a config file carries and a compat fixture does not: 17 options set
before the plugins load, five leader mappings, two autocommands, the `habamax`
colorscheme, the two plugins `heavy` carries only as dependencies loaded as spec
entries of their own, and `setup()` on every plugin through one generic pass
rather than per-entry options. Whether that is also *slower* than `heavy`
through a pty is what its recorded row answers; headless, the two start within a
few milliseconds of each other. The reason it exists is coverage rather than a
bigger number: a real config is what decides how long view's startup shell sits
on screen saying it is waiting for Neovim, and until this fixture existed no
recorded bar moved when that window got longer.

Two run-time notes for anyone recording it. The fixture is driven through
a real session first (`task user-fixture`, after `task compat` has filled
the plugin cache) as a precondition of a record run: a plugin that draws a
message where the content marker is read would otherwise land in the
number rather than in a failure. And a `first_paint` cell costs 100 warmup
plus 1000 measured cold spawns per side, so adding this one grows every
class's bench leg and every `task perf-audit` by roughly 2200 cold spawns
-- about one `first_paint/heavy` again.

## Current numbers

Recorded baselines on a shared Linux dev host, whose `dev-linux` is the
default class of this page. A number here is that class's reading unless the
row or paragraph it stands in names another class, and it resolves against
the one fixture that unit names.


| What | view | bare Neovim | |
|---|---|---|---|
| UI shell painted, engine still loading, no plugins (p99) | `first_paint.shell_visible_cold_ms` **4.1 ms** | n/a | budget 50 ms |
| UI shell painted, engine still loading, 15-plugin lazy.nvim stack (p99) | `first_paint.shell_visible_cold_ms` **3.8 ms** | n/a | budget 50 ms |
| First paint, cold, no plugins, `minimal` (p99) | 27.4 ms | **25.4 ms** | ~1.08x slower -- `first_paint.marker_ratio_p99`, a diagnostic of the felt `startup.settled_ratio_p50` |
| First paint, cold, 15-plugin lazy.nvim stack, `heavy` (p99) | 104.2 ms | **99.7 ms** | ~1.05x slower -- `first_paint.marker_ratio_p99`, a diagnostic of the felt `startup.settled_ratio_p50` |
| First paint, cold, full login, `user` (p99) | `first_paint.marker_cold_ms` 80.5 ms | not recorded on its own | seated at `e9087db`; the ratio beside it was retaken 2026-09-06 (`first_paint.marker_ratio_p50` 1.084, `first_paint.marker_ratio_p99` 1.046) |
| Resident memory (PSS), view process only, no plugins | `memory.pss_mb` **4.96 MB** | n/a | budget was 150 MB |
| Redraw parsed to terminal write (p99) | `output_path.p99_ms` **0.11 ms** | n/a | budget 1 ms |
| Keystroke to cell change, steady typing, no plugins (p99) | `echo.view_p99_ms` 0.73 ms | 0.67 ms | `echo.ratio_p99` ~1.09x slower at the tail, where `echo.view_p99_ms` carries the bound; at the median `echo.ratio_p50` reads 1.130 |
| Keystroke to predicted glyph, no plugins, engine local (p99) | `echo_speculated.speculated_paint_p99_ms` **0.30 ms** | n/a | `echo_speculated.speculated_ratio_p50` reads 0.394 against the bare Neovim paired with it in the same run. The injected round trips are a separate leg (`scripts/acceptance/remote-rtt.sh`) |
| Sustained scroll, 100k lines, no plugins (p99 staleness) | `scroll.staleness_p99_ms` 1.07 ms | n/a | budget 16 ms |
| Sustained scroll, 100k lines, 15-plugin lazy.nvim stack (p99 staleness) | `scroll.staleness_p99_ms` 1.23 ms | n/a | budget 16 ms |
| Sustained scroll, no plugins, versus Neovim | | | ~1.6x slower (`scroll.ratio_p50`, the paired ratio beside the felt `scroll.staleness_p99_ms`) |
| Sustained scroll, 15-plugin lazy.nvim stack, versus Neovim | | | ~1.9x slower (`scroll.ratio_p50`, the paired ratio beside the felt `scroll.staleness_p99_ms`) |

The two bare-Neovim first-paint figures are the 2026-09-06 dev-linux
retake. The figures they replace were withdrawn: both sides are spawned on
a pty the harness owns, and until that pty answered the DSR that Neovim's
tty startup writes behind its background-colour query, the bare side waited
out its own `vim.wait(100, ...)` on every cold sample -- roughly 100 ms that
view's side never paid, because view's engine owns no tty and never asks.
The pty answers it now (`view_oracle::pty`, pinned by
`view-bench/tests/nvim_arm_startup.rs`).

Both first-paint columns are the retake's own interleaved pair; view's
recorded gate bar ratchets separately and came from a quieter run. One
paragraph per fixture below, since each of these numbers belongs to one of
them.

On `minimal` that bar is `first_paint.marker_cold_ms` 25.2 ms. The retake
pair reads 16.88 ms p50 against bare nvim's 15.43 ms, which is
`first_paint.marker_ratio_p50` 1.094 and `first_paint.marker_ratio_p99`
1.076.

On `heavy` the bar is `first_paint.marker_cold_ms` 79.3 ms. The retake pair
reads 55.08 ms p50 against 50.04 ms, which is `first_paint.marker_ratio_p50`
1.101 and `first_paint.marker_ratio_p99` 1.045.

On `user` the retake pair reads 58.73 ms p50 against 54.16 ms, which is
`first_paint.marker_ratio_p50` 1.084 and `first_paint.marker_ratio_p99`
1.046.

view trails bare Neovim by 8-10% on every paired cold cell, and on
`minimal` what it trails by is the post-VimEnter attach-plus-takeover round
trip the late-attach design pays serially, since Neovim's own TUI attaches
before init runs. Attribution past that outline is open work, not a claim
this page makes.

Both dev classes are re-seated: dev-linux from the retake above, dev-macos
from its own on mbp the same day. `gh-linux` and `gh-macos` carry their
first-paint ratios as `withdrawn` entries with the reason attached, so a
gate run on those classes fails loudly on the missing bars rather than
attesting to them, until each is re-seated from a run under the answering
pty.

The `user` row is recorded, and each class states its own reading on its own
line, since a number belongs to one class and one fixture.

dev-linux holds `first_paint.marker_cold_ms` 80.512 ms and
`first_paint.shell_visible_cold_ms` 4.543 ms on the `user` fixture, seated at
`e9087db`, and its two ratios come from the 2026-09-06 retake above
(`first_paint.marker_ratio_p50` 1.084, `first_paint.marker_ratio_p99` 1.046).

dev-macos holds `first_paint.marker_cold_ms` 87.563 ms and
`first_paint.shell_visible_cold_ms` 12.504 ms on the same `user` fixture,
with `first_paint.marker_ratio_p50` and `first_paint.marker_ratio_p99` both
reading 1.061 from its own retake.

The other two classes hold the absolute and owe the ratio, one paragraph
each, and both owe the same DSR re-seat on the plugin-free and 15-plugin
legs.

`gh-linux` holds `first_paint.marker_cold_ms` 96.326 ms on the `user`
fixture and carries `first_paint.marker_ratio_p50` and
`first_paint.marker_ratio_p99` on that cell as `withdrawn`.

`gh-macos` holds `first_paint.marker_cold_ms` 169.099 ms on the same `user`
fixture, with the same two ratios `withdrawn`.

Until a class records that cell, a gate run against that class reports it
as uncovered and exits on it -- the designed signal for a measured row
with nothing to compare against, not a fault to work around. Every class
has to record it before its gate is green again: the dev classes in a
quiet-host session each, the CI classes by re-seating from the
`bench-measured-<class>.toml` artifact their gate leg uploads (CI runs no
`--record` leg).

The first two rows are unpaired on purpose: view paints its shell before it has
even started the Neovim child, so bare Neovim has no comparable event. It
shows nothing until your config finishes loading.

The shell frame is nearly identical either way, because none of your config
has run yet at that point: `first_paint.shell_visible_cold_ms` reads 4.1 ms
on the plugin-free fixture.

On the 15-plugin stack the same `first_paint.shell_visible_cold_ms` reads
3.8 ms.

The no-plugins memory row is view's own process only: the embedded Neovim
engine is a separate process this budget deliberately excludes, so the
bare-Neovim column reads `n/a` rather than a real comparison.

### The real-config legs, and which classes still owe them

Every felt row is stated under a real config, and the matrix seats five
cells on the `user` fixture: `echo.user`, `echo_speculated.user`,
`scroll.user`, `flood.user` and `startup.user`. dev-linux records all five.
Four were taken 2026-09-06 in two quiet windows, each cell's null-pair
calibration inside 1.1%; `startup.user` was re-recorded 2026-09-15 in a quiet
window of its own (1-minute load 1.82 falling to 1.42 over the run), its
null-pair calibration 3.4% at the start of the run and 1.0% at the end, both
well inside the 15% floor that refuses a run:

| cell, `user` fixture | view | bare Neovim | reading |
|---|---|---|---|
| `startup.settled_ratio_p50` (`user`) | 53.929 ms p50 | 52.422 ms p50 | 1.029 against the 1.0 bar, unmet, re-recorded 2026-09-15 after the parked attach was fixed; the diagnostic `startup.server_delta_ms` reads 1.637 ms, the attach wait having moved inside the segment that metric measures |
| `echo.ratio_p50`, `echo.view_p99_ms` (`user`) | 0.920 ms p50, `echo.view_p99_ms` 1.583 ms p99 | 0.829 ms p50, 1.429 ms p99 | `echo.ratio_p50` 1.110 against the 1.10 bar, unmet; the tail is inside its 8 ms bar, `echo.paired_delta_p99_ms` 0.726 ms |
| `echo_speculated.speculated_ratio_p50` (`user`) | 0.200 ms p50, `echo_speculated.speculated_paint_p99_ms` 0.318 ms p99 | 0.603 ms p50, 1.250 ms p99 | 0.332 against the 1.0 bar, met; a prediction answered 99.9% of the samples and the rest can only understate it |
| `scroll.staleness_p99_ms` (`user`) | `scroll.staleness_p99_ms` 1.572 ms p99 | 0.951 ms p99 | inside the 16 ms bar; `scroll.ratio_p50` 1.717 and `scroll.ratio_p99` 1.664 are recorded on a shared class and not gated |
| `flood.cadence_p99_ms` (`user`) | `flood.cadence_p99_ms` 16.914 ms p99 | 17.480 ms p99 | 0.9 ms past the 16 ms frame, unmet on both sides under this stack; `flood.cadence_p99_ratio` 0.981, `flood.pace_ratio` 1.018, worst no-paint gap 48.9 ms reported and not gated |

The three unmet cells are `[[shortfall]]` entries in
`crates/view-bench/budgets.toml`, each accepted at its recorded value with
its bar untouched. Which class holds the five:

| class | the five `user` cells | how the rest get seated |
|---|---|---|
| `dev-linux` | all five recorded | -- |
| `dev-macos` | all five owed | a quiet-window session on mbp, `task user-fixture` first |
| `gh-linux` | all five owed | re-seat from the `bench-measured-gh-linux.toml` artifact its gate leg uploads |
| `gh-macos` | all five owed | the same re-seat from `bench-measured-gh-macos.toml` |
| `controlled-linux` | all five unseated | the matrix runs all five there and nothing scopes them away; its baseline holds no cell for any of them today, and it is the one class that loads the budget table, so a quiet-window recording there is what would attest the felt bars instead of ratcheting against a recorded value |

An owed cell is committed empty with a `[withdrawn.<scenario>.user]` reason
beside it, which is the state a gate run reports loudly (`GATE COVERAGE FAIL`)
rather than the state it passes over quietly. Filling one from the plugin-free
leg's number is the substitution this whole vocabulary exists to refuse.
`controlled-linux` carries no withdrawal for them because it carries no cell
either: it is recorded on demand rather than gated, and a run of `--all` there
selects the same five.

Recording them is one quiet-host session per class:

```
$ task user-fixture     # after task compat has filled the plugin cache
$ CLASS=controlled-linux task bench -- --scenario startup --fixture user --record
```

What the five cost a gate leg, per class, from the harness's own sample counts
(`--warmup 100 --samples 1000 --trials 3`): `startup.user` is a cold scenario,
so it spawns both editors once per sample -- 2200 process spawns, the same shape
and cost as one `first_paint` cell. The other four spawn a session and drive it:
`echo.user`, `echo_speculated.user` and `scroll.user` spawn one session per side
(2 spawns) and drive `trials x (warmup + samples)` = 3300 samples per side,
paced by the driver's own inter-sample sleep (5 ms on echo), so roughly 30 s
each; `flood.user` spawns a session per side per trial (6 spawns) and runs the
fixed 15 s flood window in each, so 1.5 min. The floor for a spawn is the
class's own recorded `first_paint.marker_cold_ms`, and `startup.user` alone pays
2200 of them:

| leg | cold spawn | `startup.user` floor |
|---|---|---|
| `gh-linux` | `first_paint.marker_cold_ms` on `user` 96.3 ms | >= 3.5 min |
| `gh-macos` | `first_paint.marker_cold_ms` on `user` 169.1 ms | >= 6.2 min |

The four session cells add ~3 min on top of that, both legs.

### Memory equivalence, 15-plugin lazy.nvim stack

view embeds Neovim, so it can never be smaller than the Neovim it embeds -- no
claim here says otherwise. Under the committed 15-plugin lazy.nvim fixture, the
same standard workload, recorded diagnostically
(`task bench -- --scenario memory --fixture heavy`, not CI-gated):

| reading | PSS |
|---|---|
| bare Neovim, whole process | 4.39 MB |
| view, own process only (excludes its Neovim child) | 5.00 MB |
| view, own process + embedded Neovim engine child (tree) | **27.96 MB** |

The first two rows are not a fair comparison: view's own-process number
deliberately excludes the Neovim child it spawns, the same exclusion the
no-plugins row above documents. The tree row is the honest one, summing
view's process and its embedded engine's, and it is what a bare-Neovim
comparison must be read against: 27.96 MB of resident memory for view and
its engine together, where bare Neovim's whole process reads 4.39 MB. A
footprint is a resource row and states no speed.

Neither side changes much between this reading and the plugin-free one for
view's own-process number. Plugin-free, `memory.pss_mb` reads 4.96 MB. Under
this stack the same process reads 5.00 MB, because lazy.nvim
defers most of the 15 plugins until their trigger event fires,
and the standard workload (opening and paging through plain text buffers)
never fires one -- this reading is each side settled after that workload,
not a ceiling on what a plugin stack can cost once its triggers do fire.

## The typing gap

Steady typing trails bare Neovim on every leg, and sustained scrolling
trails it further. One line per leg, because each number is one fixture's:

| leg | steady typing | sustained scroll |
|---|---|---|
| plugin-free (`minimal`) | `echo.ratio_p50` 1.130, about 13% | `scroll.ratio_p50` 1.605, the paired figure beside the felt `scroll.staleness_p99_ms` |
| 15-plugin stack (`heavy`) | -- | `scroll.ratio_p50` 1.891, the same paired figure |
| login-shaped (`user`) | `echo.ratio_p50` 1.110, about 11% | `scroll.ratio_p50` 1.717 |

Both are sub-millisecond and far inside their budgets, so neither is
perceptible. The goal is to beat Neovim, though, not to tie it, so the gap gets
tracked down rather than shrugged off.

An obvious suspect was the architecture itself: maybe an out-of-process UI
speaking Neovim's RPC protocol just costs this much. Neovim ships its own
out-of-process TUI, which makes that theory testable, and the control leg
measures it under the identical protocol on the same host, in the same
interleaved run as the typing cell above:

| leg | Neovim's own TUI driving a headless Neovim |
|---|---|
| plugin-free (`minimal`) | `echo_control.control_ratio_p50` 0.994 |
| 15-plugin stack (`heavy`) | `echo_control.control_ratio_p50` 1.009 |

Speaking the protocol from another process costs nothing this class can
measure, so the gap is view's own code. (Three earlier theories, a
thread-hop cost floor, the pty transport, and the measurement
instrumentation itself, also failed to survive measurement; each retraction
is recorded in the design spec.)

So we profiled it. A tapped build times every stage of a keystroke's round
trip, and the largest stage view owned was the handoff to a background
thread whose only job was writing bytes to a pipe. That thread exists so a
wedged Neovim stalls a background thread instead of the screen, but the
goal only requires the write to never *block*, not to always *defer*. The
main loop now writes the bytes itself whenever the pipe has signalled it
can accept them and nothing is queued ahead. Skipping the ~40 µs cost of
waking an idle core accounts for most of the improvement; the paired
before-and-after readings that recorded it are in the commit that landed
the change, where no re-record can leave them standing as current.

What remains is measured, not guessed. Of the ~644 µs from keypress to
glyph, 366 are spent inside Neovim itself, 80 in the OS's terminal plumbing
before view sees anything, and 36 in the terminal emulator drawing the
result. view's own share is 139 µs: 71 carrying the keystroke in, 68
painting the answer. About half of the 71 is one structural cost, handing
the keystroke from the thread that reads the terminal to the thread that
owns editor state, which has to happen because view must decide whether a
key belongs to Neovim or to view's own UI. No other stage on either path
exceeds 21 µs.

*Measured 2026-08-03 (`df411f19`). The largest item above, the
key-decoded->loop-wake hop (49.1 µs p50 in this decomposition; 52.4 µs
p50 in the reading taken immediately before the change), has since collapsed
to 13.9 µs p50 with the input-thread/runtime-loop unification
(spec:97-99's 2026-08-09 adjudication) -- this decomposition's ~644 µs
total predates that change and reads high.*

## Bisecting, and A/B on the quiet host

A paired measurement -- the same scenario against the binary from two
revisions -- is built by `scripts/ab-build.sh`, never by hand:

```
$ bash scripts/ab-build.sh 6ed8bc9 f63f7d0
before: ~/.cache/view-ab/before/target/release/view
after:  ~/.cache/view-ab/after/target/release/view
$ VIEW_BIN=~/.cache/view-ab/before/target/release/view task bench -- \
      --scenario echo --fixture minimal
```

Each side is exported with `git archive` into its own tree and built with
its own `CARGO_TARGET_DIR`, and the script refuses to hand back two
byte-identical binaries. Both rules come from one incident: a pair built
through a single shared target dir compiled nothing on the second build --
cargo's dep-info still named the first tree's files, all of them fresh --
and the run reported a null result that was really the same binary measured
twice.

A bisect is the same loop with one revision per step (`git archive <sha>`
into a scratch tree, one target dir of its own, then the bench cell), which
is how the gh-runner regression window was narrowed to a single pair of
revisions.

Everything under "How we measure" still applies: the host has to be quiet,
and its class has to be declared.

## How budgets are enforced

Budgets are recorded per machine class and regression-gated: a change that
makes any tracked metric worse fails the build. Each metric is also checked
against the design spec's own budget, not just the last recorded value.

A machine's class is declared, never detected:
`CLASS=controlled-linux task bench` (and the same word on `task perf-audit` and
`task acceptance`) says this host is quiet enough for the tail metrics and the
controlled-only budget rows. Undeclared, a host is `dev-<platform>`, and the
acceptance legs whose bound is armed on a controlled class alone -- today the
RTT-injection proof, `scripts/acceptance/remote-rtt.sh` -- announce a skip and
pass, rather than measuring against a bar nobody recorded for them.

Where a metric does not yet meet its spec budget, it is listed in
`crates/view-bench/budgets.toml` with the value it was accepted at and a
written reason. The build fails if a new shortfall appears, if a listed one
gets worse, or if a listed one is fixed but left on the list.

## Re-seating a bar, or re-sizing the spread it gates under

A recorded bar only ratchets down as far as the class's published spread
says honest runs move; a measurement further below it than that is one
lucky draw, and `--record` refuses it rather than pinning a bar most honest
runs would then fail. Moving such a bar takes a campaign -- N gated
replicates of the same cells on a quiet host -- and that campaign is a mode
of the tool that already measures:

```
$ task bench -- --scenario scroll --fixture minimal --class dev-macos --campaign 8
campaign on class dev-macos: 8 included replicate(s) wanted, at most 16 run(s) over 1 cell(s) = up to 16 cell measurement(s), excluding any replicate whose pre-run 1-min load exceeds 2
CAMPAIGN scroll/minimal dev-macos: replicate 1 (included 1/8)  load 1.42  brackets 1.0204/1.0311  ratio_p50 2.2810  INCLUDED
CAMPAIGN dev-macos: replicate 1 took 4m12s; 8 included ~= 33m36s, the 16-run cap ~= 1h07m
CAMPAIGN scroll/minimal dev-macos: replicate 2 (included 1/8)  load 2.31  brackets 1.0290/1.0402  ratio_p50 2.4020  EXCLUDED (load > 2), replacing
...
CAMPAIGN dev-macos: 8 included of 11 run
  scroll/minimal ratio_p50: median 2.2725  half-width 1.37%  worst 2.3094  proposes "scroll.minimal.ratio_p50" = 1.03
CAMPAIGN wrote crates/view-bench/baselines/dev-macos.campaign.toml (seats, factors, draws)
```

Each replicate is a full `--record`-grade measurement, null-pair calibration
brackets included -- those brackets are printed and recorded per replicate, so
an included draw that sat just under the floor is visible rather than
indistinguishable from a clean one. A replicate whose pre-run load exceeds
`--max-load` (2 by default, the value its own help prints) is published as an
excluded draw and replaced, and one that refuses its own measurement is replaced
too. Past twice the wanted replicates the campaign refuses, naming every load it
saw.

Nothing caps how many cells a campaign may span -- `--all` is a legal
request -- so the tool states the magnitude instead: the start line gives
the cell measurements the cap permits, and once the first replicate lands
it projects the wanted band and the cap from that replicate's own measured
cost.

The file it writes is a proposal and nothing reads it: it carries each
cell's proposed seat (its median), the headroom factor that seat and its
draws size under the same three-leg rule the characterization walk
re-checks a published factor with, and the `[draws]` tables that let the
walk do so. Committing a campaign means reviewing that file and moving its
contents into `baselines/<class>.toml` and `baselines/<class>.headroom.toml`
-- the tool proposes, the diff decides.
