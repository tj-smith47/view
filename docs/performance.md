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

The engine is pinned (`.engine-pin`, currently Neovim `v0.12.4`) and the
harness verifies the binary on `PATH` actually reports that version before
recording anything. Plugin-heavy cells use a committed lazy.nvim fixture
with 14 plugins pinned by its `lazy-lock.json`, driven through a real pty.

## Current numbers

Recorded baselines on a shared Linux dev host:

| What | view | bare Neovim | |
|---|---|---|---|
| UI shell painted, engine still loading (p99) | **4.1 ms** | n/a | budget 50 ms |
| First paint, cold, no plugins (p99) | **26.5 ms** | 131.4 ms | **5x faster** |
| First paint, cold, 14-plugin lazy.nvim stack (p99) | **120.5 ms** | 199.8 ms | **1.7x faster** |
| Resident memory (PSS) | **3.4 MB** | n/a | budget was 150 MB |
| Redraw parsed to terminal write (p99) | **0.12 ms** | n/a | budget 1 ms |
| Keystroke to cell change, steady typing (p99) | 0.90 ms | 0.81 ms | ~1.12x slower |
| Sustained scroll, 100k lines (p99 staleness) | 1.40 ms | n/a | budget 16 ms |
| Sustained scroll, versus Neovim | | | ~1.8 to 2.0x slower |

The first row is unpaired on purpose: view paints its shell before it has
even started the Neovim child, so bare Neovim has no comparable event. It
shows nothing until your config finishes loading. The 4.1 ms figure is
identical on a bare config and on the 14-plugin stack, because none of your
config has run yet at that point.

## The typing gap

Steady typing is currently about 17% slower than bare Neovim, and sustained
scrolling about 2x. Both are sub-millisecond and far inside their budgets,
so neither is perceptible. The goal is to beat Neovim, though, not to tie
it, so the gap gets tracked down rather than shrugged off.

An obvious suspect was the architecture itself: maybe an out-of-process UI
speaking Neovim's RPC protocol just costs this much. Neovim ships its own
out-of-process TUI, which makes that theory testable. Measured under the
identical protocol on the same host:

| steady typing, dev-linux | vs bare Neovim |
|---|---|
| Neovim's own TUI driving a headless Neovim over the UI protocol | **1.02x** |
| view, at the time of that measurement | 1.22x |

Speaking the protocol from another process costs about 2%. Roughly nine
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
| Steady typing vs Neovim, 14 plugins | 1.244x | **1.184x** |
| Tail (p99) typing ratio, 14 plugins | 1.142x | **1.010x** |

What remains is measured, not guessed. Of the ~644 µs from keypress to
glyph, 366 are spent inside Neovim itself, 80 in the OS's terminal plumbing
before view sees anything, and 36 in the terminal emulator drawing the
result. view's own share is 139 µs: 71 carrying the keystroke in, 68
painting the answer. About half of the 71 is one structural cost, handing
the keystroke from the thread that reads the terminal to the thread that
owns editor state, which has to happen because view must decide whether a
key belongs to Neovim or to view's own UI. No other stage on either path
exceeds 21 µs.

## How budgets are enforced

Budgets are recorded per machine class and regression-gated: a change that
makes any tracked metric worse fails the build. Each metric is also checked
against the design spec's own budget, not just the last recorded value.

Where a metric does not yet meet its spec budget, it is listed in
`crates/view-bench/budgets.toml` with the value it was accepted at and a
written reason. The build fails if a new shortfall appears, if a listed one
gets worse, or if a listed one is fixed but left on the list.
