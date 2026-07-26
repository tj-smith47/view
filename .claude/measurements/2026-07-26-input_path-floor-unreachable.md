# input_path budget is architecture-unreachable (task #50/#6) — §5.6 finding

Measured 2026-07-26. Engine pin v0.12.4. Report-only tap decomposition, measure-
first per HANDOFF §5.4. Raw: `~/.claude/tmp/floor/ip-decomp.log` (dev-linux, quiet
load 0.24), `~/.claude/tmp/floor/ip-decomp-mbp.log` (bare-metal M1 Max), hot-path
map `~/.claude/tmp/floor/input_path-hotpath-map.md`. Resolution floor (task #1):
input_path p99 single-shot half-width 15.7 us, so 302 us is a solid number.

## The measurement

input_path measures `key at pty -> RPC bytes written` (t0 = harness pty write,
end = first `TAG_RPC_WRITTEN`). Budget: p99 <= 100 us (spec 3.1). Measured p99:
dev-linux 302 us, mbp 372 us (mbp at load 1.2-2.1, tail-inflated — use its p50).

Per-segment p50, dev-linux vs bare-metal mbp:

| segment | dev-linux p50 | mbp p50 | nature |
|---|---|---|---|
| pty->key-decoded        | 97.8 us | 36.0 us | OS pty deliver + crossterm read+parse (input thread) — host-bound |
| key-decoded->loop-wake  | 57.7 us | 29.0 us | cross-thread wake #1 (input thread -> paint loop recv) — host-bound |
| loop-wake->rpc-handoff  | 16.3 us | 16.0 us | update() + msgpack encode — **view CPU, host-INDEPENDENT** |
| rpc-handoff->rpc-written| 34.2 us | 30.0 us | cross-thread wake #2 + write()/flush to nvim — host-bound |
| **total p50**           | **206 us** | **111 us** | |

## Why it is unreachable

The ONLY host-independent segment — the sole genuine reducible view-code cost — is
`loop-wake->rpc-handoff` at ~16 us on BOTH hosts (pure CPU: map Msg::Key ->
Effect::Rpc(RpcCall::Input), then msgpack-encode one `nvim_input`). Every other
segment is dominated by cross-thread scheduling + the OS pty read. Those three
thread transitions (input-read thread, paint loop, RPC writer thread) are MANDATED
by the hard rules: "the paint loop never awaits RPC" forces the writer-thread hop,
and immediate keystroke wake forces the input-thread hop. Collapsing either
violates the architecture (a keystroke would either be dropped or the loop could
block on a full pipe).

Even bare-metal p50 (111 us) exceeds the 100 us budget, and p99 exceeds it on both
classes. There is no ~200 us of reducible view code; the 100 us p99 target is
physically inconsistent with the mandated three-transition path. This is the same
shape as the ratio target already amended per-class (HANDOFF §5.6).

Attack on this conclusion (§7): "did you try to reduce the two big cross-thread
wakes?" — they contain zero view code between the send and the wake tap; the
dev-linux/mbp delta (57.7->29, 97.8->36) proves the cost is host thread-scheduling
+ pty, not view logic. A ~8 us win exists by micro-optimizing the msgpack encode,
but it closes <5% of the gap.

## Options for the user (§5.6 step 2 — the user amends the spec)

- **(A) Per-class p99 budget** re-derived from the measured floors (dev-linux VM
  vs bare metal), exactly as the ratio target was handled.
- **(B) Redefine the measured boundary** to view's controllable domain,
  `key-decoded -> rpc-written`, excluding the OS pty read view does not own — the
  spec already flags `pty->key-decoded` as "ahead of that boundary."
- **(C) Drop the absolute us bar** for this row and gate the paired ratio only
  (view is already at/better than bare nvim on this path).
- **(D) Re-derive the budget** from the three-transition floor + margin.

RECOMMEND (A)+(B): per-class budgets measured on the view-controllable boundary.
Do NOT amend the spec unilaterally — this is the user's call.
