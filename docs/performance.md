# Performance

view's performance claims are measured, not asserted. This page explains how
the measurements work, what the current numbers are, and where view is still
slower than bare Neovim.

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
top-level plugins eagerly and both enable treesitter highlighting. What
`user` adds is what a config file carries and a compat fixture does not:
17 options set before the plugins load, five leader mappings, two
autocommands, the `habamax` colorscheme, the two plugins `heavy` carries
only as dependencies loaded as spec entries of their own, and `setup()` on
every plugin through one generic pass rather than per-entry options.
Whether that is also *slower* than `heavy` through a pty is what its
recorded row answers;
headless, the two start within a few milliseconds of each other. The
reason it exists is coverage rather than a bigger number: a real config is
what decides how long view's startup shell sits on screen saying it is
waiting for Neovim, and until this fixture existed no recorded bar moved
when that window got longer.

Two run-time notes for anyone recording it. The fixture is driven through
a real session first (`task user-fixture`, after `task compat` has filled
the plugin cache) as a precondition of a record run: a plugin that draws a
message where the content marker is read would otherwise land in the
number rather than in a failure. And a `first_paint` cell costs 100 warmup
plus 1000 measured cold spawns per side, so adding this one grows every
class's bench leg and every `task perf-audit` by roughly 2200 cold spawns
-- about one `first_paint/heavy` again.

## Current numbers

Recorded baselines on a shared Linux dev host:

| What | view | bare Neovim | |
|---|---|---|---|
| UI shell painted, engine still loading (p99) | **3.8-4.1 ms** | n/a | budget 50 ms |
| First paint, cold, no plugins, `minimal` (p99) | **25.2 ms** | 130.3 ms | **5.2x faster** |
| First paint, cold, 15-plugin lazy.nvim stack, `heavy` (p99) | **79.3 ms** | 164.3 ms | **2.1x faster** |
| First paint, cold, full login, `user` (p99) | not yet recorded | not yet recorded | |
| Resident memory (PSS), view process only, no plugins | **4.96 MB** | n/a | budget was 150 MB |
| Redraw parsed to terminal write (p99) | **0.08 ms** | n/a | budget 1 ms |
| Keystroke to cell change, steady typing (p99) | 0.73 ms | 0.67 ms | ~1.09x slower |
| Sustained scroll, 100k lines, no plugins (p99 staleness) | 1.07 ms | n/a | budget 16 ms |
| Sustained scroll, 100k lines, 15-plugin lazy.nvim stack (p99 staleness) | 1.23 ms | n/a | budget 16 ms |
| Sustained scroll, versus Neovim | | | ~1.6 to 1.9x slower |

The `user` row is measured by the same code as the two above it and lands
with the next recorded baseline; it is listed empty rather than omitted so
its absence is visible.

Until a class records that cell, a gate run against that class reports it
as uncovered and exits on it -- the designed signal for a measured row
with nothing to compare against, not a fault to work around. Every class
has to record it before its gate is green again: the two dev classes in
one batched quiet-host session, the CI classes on their own next
recording legs, which already run under `--record`.

The first row is unpaired on purpose: view paints its shell before it has
even started the Neovim child, so bare Neovim has no comparable event. It
shows nothing until your config finishes loading. The 3.8-4.1 ms range is
nearly identical on a bare config (4.1 ms) and on the 15-plugin stack
(3.8 ms), because none of your config has run yet at that point.

The no-plugins memory row is view's own process only: the embedded Neovim
engine is a separate process this budget deliberately excludes, so the
bare-Neovim column reads `n/a` rather than a real comparison.

### Memory equivalence, 15-plugin lazy.nvim stack

view embeds Neovim, so it can never be smaller than the Neovim it embeds --
no claim here says otherwise. Under the committed 15-plugin lazy.nvim
fixture, the same standard workload, recorded diagnostically (`task bench
-- --scenario memory --fixture heavy`, not CI-gated):

| reading | PSS |
|---|---|
| bare Neovim, whole process | 4.39 MB |
| view, own process only (excludes its Neovim child) | 5.00 MB |
| view, own process + embedded Neovim engine child (tree) | **27.96 MB** |

The first two rows are not a fair comparison: view's own-process number
deliberately excludes the Neovim child it spawns, the same exclusion the
no-plugins row above documents. The tree row is the honest one, summing
view's process and its embedded engine's, and it is what a bare-Neovim
comparison must be read against: view's real footprint here is about 6.4x
bare Neovim's. Neither side changes much between this reading and the
no-plugins one for view's own-process number (4.96 MB vs 5.00 MB) because
lazy.nvim defers most of the 15 plugins until their trigger event fires,
and the standard workload (opening and paging through plain text buffers)
never fires one -- this reading is each side settled after that workload,
not a ceiling on what a plugin stack can cost once its triggers do fire.

## The typing gap

Steady typing is currently about 13% slower than bare Neovim, and sustained
scrolling about 1.6 to 1.9x. Both are sub-millisecond and far inside their budgets,
so neither is perceptible. The goal is to beat Neovim, though, not to tie
it, so the gap gets tracked down rather than shrugged off.

An obvious suspect was the architecture itself: maybe an out-of-process UI
speaking Neovim's RPC protocol just costs this much. Neovim ships its own
out-of-process TUI, which makes that theory testable. Measured under the
identical protocol on the same host:

| steady typing, dev-linux (no plugins / 15-plugin stack) | vs bare Neovim |
|---|---|
| Neovim's own TUI driving a headless Neovim over the UI protocol | **1.04x / 1.02x** |
| view, at the time of that measurement | 1.22x / 1.24x |

Speaking the protocol from another process costs about 2-4%. Roughly nine
tenths of the gap was view's own code. (Three earlier theories, a
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
waking an idle core accounts for most of the improvement:

| | before | after |
|---|---|---|
| RPC handoff to bytes written | 42.5 µs | **10.5 µs** |
| Keystroke to RPC bytes written (p99) | 154.7 µs | **117.7 µs** |
| Steady typing vs Neovim, no plugins | 1.354x | **1.172x** |
| Steady typing vs Neovim, 15 plugins | 1.244x | **1.184x** |
| Tail (p99) typing ratio, 15 plugins | 1.142x | **1.010x** |

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
key-decoded->loop-wake hop (49.1 µs p50 in this table; 52.4 µs p50 in
the reading taken immediately before the change), has since collapsed
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

A machine's class is declared, never detected: `CLASS=controlled-linux task
bench` (and the same word on `task perf-audit` and `task acceptance`) says
this host is quiet enough for the tail metrics and the controlled-only
budget rows. Undeclared, a host is `dev-<platform>`, and the acceptance
legs whose bound is armed on a controlled class alone -- today the
RTT-injection proof, `scripts/acceptance/remote-rtt.sh` -- announce a skip
and pass, rather than measuring against a bar nobody recorded for them.

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
CAMPAIGN scroll/minimal dev-macos: replicate 1/8  load 1.42  ratio_p50 2.2810  INCLUDED
CAMPAIGN scroll/minimal dev-macos: replicate 2/8  load 2.31  ratio_p50 2.4020  EXCLUDED (load > 2), replacing
...
CAMPAIGN dev-macos: 8 included of 11 run
  scroll/minimal ratio_p50: median 2.2725  half-width 1.37%  worst 2.3094  proposes "scroll.minimal.ratio_p50" = 1.03
CAMPAIGN wrote crates/view-bench/baselines/dev-macos.campaign.toml (seats, factors, draws)
```

Each replicate is a full `--record`-grade measurement, null-pair
calibration brackets included; a replicate whose pre-run load exceeds
`--max-load` (2.0 by default) is published as an excluded draw and
replaced, and one that refuses its own measurement is replaced too. Past
twice the wanted replicates the campaign refuses, naming every load it saw.

The file it writes is a proposal and nothing reads it: it carries each
cell's proposed seat (its median), the headroom factor that seat and its
draws size under the same three-leg rule the characterization walk
re-checks a published factor with, and the `[draws]` tables that let the
walk do so. Committing a campaign means reviewing that file and moving its
contents into `baselines/<class>.toml` and `baselines/<class>.headroom.toml`
-- the tool proposes, the diff decides.
