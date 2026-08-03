# first_paint cold_ms gap was a bench-pty probe artifact, now fixed (task #49/#5)

Measured 2026-07-26. Engine pin v0.12.4. The 54.7 ms first_paint cold_ms
recorded against the 50 ms budget was not view startup cost: ~52 ms of it was
view's capability probe waiting out its full fallback deadline against a bench
pty that never answered. With the pty answering the probe the way every real
terminal does, view's true cold-start-to-shell-frame is **3.554 ms** (median of
6, dev-linux), 14x under budget.

## Root cause (re-derived from four observed facts)

1. `view-tui` `tiers::PROBE_DEADLINE = 50 ms` — the safety net for a terminal
   that ignores even the DA1 fence.
2. `tiers::detect` early-exits only when the DA1 reply arrives
   (`scan_csi_replies(&buf).2`); on no data `StdinReplySource::next_chunk`
   returns `Some(empty)` after a 1 ms sleep, never `None`, so the loop runs to
   the full deadline.
3. The bench pty (`view-oracle` `PtySession`) forwarded child output only into
   `vt100::Parser::process`; nothing wrote a DA1 reply back to the child.
4. The bench spawns `view` with no `--tier` override, so the real probe runs
   on every cold spawn.

Therefore every cold first_paint spawn paid the whole 50 ms deadline before
`paint_shell_frame`. `Term::init` (which runs the probe) precedes the shell
frame, so the probe blocks first paint. Measured cold_ms 55.66 ms (floor
Campaign B) = 50 ms probe + ~5.66 ms real startup. In production a real
terminal answers DA1 within milliseconds, the probe early-exits, and the 50 ms
is never paid.

## The fix

`PtySession`'s reader thread now answers a child's DA1 fence (`\x1b[c`) with
`\x1b[?1;2c`, a VT100-class private-CSI reply, standing in for the real
terminal a pty-driven child would otherwise probe in vain. The reply is
written through a writer shared with the caller (`Arc<Mutex>`), since a pty
master hands out its writer once.

DA1 only: the sync (mode-2026) and kitty-keyboard queries stay unanswered, so a
probe still derives `sync=false, kitty=false` — the same Basic tier every
scenario's baseline was recorded under. Answering the fence removes the timeout
without shifting the tier, so echo / input_path / flood / scroll baselines
(whose 50 ms startup stall sat in warmup, ahead of their measured windows) are
untouched. Widening the pty to advertise a fully-capable terminal is a separate
decision that would re-base every scenario, deliberately not bundled here.

## Before / after (dev-linux, first_paint/minimal)

| | cold_ms p99 | source |
|---|---|---|
| before | 55.66 ms (median of 6) | floor Campaign B, `2026-07-26-dev-linux-resolution-floor.md` |
| after  | **3.554 ms** (median of 6) | this campaign, `~/.claude/tmp/da1/campaign.tsv` |

After-campaign rounds all lie in [3.500, 3.607] ms (6/6 qualifying,
load_start 0.75-0.98) — a ~52 ms drop, ~10x the floor's 4.99 ms single-shot
resolution, so the effect is unambiguous even single-shot. view paints its
shell placeholder frame in 3% of bare nvim's 123 ms cold start
(ratio_vs_nvim 0.027); the engine attaches on a background thread, off the
first-paint path.

## Verification

- `PtySession` gains a runtime integration test (a raw-mode child emits the
  DA1 fence and records the exact reply bytes) plus `QueryResponder` unit
  tests (in-chunk, split-across-chunks, no-false-positive, repeated). Watched
  the integration test fail against the silent pty (child blocked its whole
  5 s deadline) before the fix, pass after.
- `task ci` green: view-oracle 98 lib tests (nvim-spawning pty tests included)
  + oracle bin 19, so answering DA1 to nvim as well as view regresses nothing.

Unlike input_path (§5.6-unreachable, a spec-amendment case), first_paint needed
no spec change: view's cold path was always well within budget; only the
measurement was wrong. Baselines re-recorded (ratchet improving-direction) on
both classes: dev-linux ~53.3 -> ~3.6 ms (minimal 3.641, heavy 3.583), dev-macos
~77.9 -> ~8.9 ms (minimal 9.094, heavy 8.608, M1 Max at load ~1.2-2.0). The
macOS cold spawn is slower than dev-linux but still 5-6x under the 50 ms budget,
and it carried the identical ~50 ms artifact before the fix (77.2 / 78.5 ms).
