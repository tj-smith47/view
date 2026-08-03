# flood drain attribution: $SHELL and cross-host pty chunking (task #58, step 1)

Measured 2026-07-26. Engine pin v0.12.4. Two independent paths, agreeing:
**path A** = the real `flood` scenario through nvim's `:terminal` (report-only,
`--samples 1 --trials 1`); **path B** = a raw-pty mechanism harness driving the
same `seq -f 'L%.0f' 1 3000000; printf 'MECH''DONE\n'` under each shell and
reading the pty master directly (`~/.claude/tmp/flood58/{pty_mech,mac_mech}.py`).
Path B needs no view/nvim build, so it ran on mbp (macOS 26.2) as well as
dev-linux. This is HANDOFF #58 step 1: **attribute the drain difference before
pinning `$SHELL`** (pinning first would destroy the evidence).

## Finding 1 — `$SHELL` reaches the drain number enormously, via zsh's ZLE

Path A (dev-linux, through nvim, host contended so absolutes are load-inflated):

| `$SHELL`   | view drain | frame gaps | result                         |
|------------|-----------:|-----------:|--------------------------------|
| `/bin/bash`| 17-22 s    | 1301-1632  | completes                      |
| `/bin/dash`| 12.7-13.5 s| 1045-1089  | completes                      |
| `/bin/zsh` | —          | —          | **fails: done marker never appeared in 120 s** |

Path B isolates the mechanism (dev-linux, env held fixed, 25-30 s cap):

| zsh mode                    | drain_s      |
|-----------------------------|-------------:|
| `-c` (non-interactive)      | 1.18         |
| `-i` (interactive, ZLE on)  | 25.2 (cap)   |
| `-i` + `-o nomonitor`       | 25.0 (cap)   |
| `-i` + `-o nozle`           | **1.45**     |

The cause is **ZLE (the Zsh Line Editor)**: disabling it (`-o nozle`) restores
zsh to bash/dash speed; disabling job control (`nomonitor`) does not. Not byte
delivery: every shell emits a byte-identical stream (28,888,9xx bytes, the ~7 B
spread is prompt noise) with identical `OPOST=ONLCR=True`. `seq` execs with
stdout = the pty directly, so the shell is never in `seq`'s data path; ZLE's cost
is CPU/scheduling interference during the drain, not output reshaping.

## Finding 2 — the ZLE penalty is Linux-zsh-specific; macOS zsh is fast

Path B on mbp (macOS), ~2.1 s cap-free:

| macOS shell             | drain_s | chunks  |
|-------------------------|--------:|--------:|
| `zsh -i` (ZLE on)       | 2.178   | 784000  |
| `zsh -i -o nozle`       | 2.151   | 778643  |
| `zsh -c` (non-interactive)| 2.108 | 775410  |
| `bash -i`               | **> 60 s (cap)** | — |

macOS zsh drains fast **with or without ZLE** — the ZLE slowdown does not exist
on macOS zsh. This resolves the macOS-fast-despite-zsh puzzle: macOS's 845 ms
drain is not a shell effect. Symmetrically, macOS `bash -i` (bash 3.2.57) is
slow like Linux zsh. **Interactive shells are a cross-host minefield: bash is
slow-interactive on macOS, zsh is slow-interactive on Linux; non-interactive
`sh -c` is fast on both.**

## Finding 3 — the two hosts chunk bytes into the pty ~65x differently

Same `seq`, read off the pty master (path B):

| host       | bytes/read chunk | read chunks | total bytes |
|------------|-----------------:|------------:|------------:|
| dev-linux  | ~2400 (batched)  | ~12000      | 28.89 MB    |
| macOS (mbp)| ~37 (line-at-a-time)| ~780000  | 28.89 MB    |

Same total bytes, same `OPOST/ONLCR`; the granularity difference is the OS pty
buffer's, not the shell's or the producer's. This is exactly the case HANDOFF
#58 flagged: a wall-clock window equalizes measurement DURATION while the
STIMULUS (bytes/read into nvim, which drives redraw coalescing) still differs
65x, so the row would gate two different things under one name.

## Consequence for the remedy (answers step 1's OPEN question)

The wall-clock drain window is necessary but **not sufficient alone**. The
attribution says it needs two companion pins:

1. **`$SHELL` pin — run the producer non-interactively.** Pinning a shell *path*
   fails (no single fast interactive shell exists on both hosts). Running the
   flood through a fixed NON-interactive shell (`sh -c`, no ZLE, no readline) is
   fast on both hosts and removes interactive-shell variance entirely. This is
   the correct form of "pin `$SHELL` so the shell stops being a host input."
2. **Chunk-size pin.** Because the kernels chunk 65x differently, the producer
   must write fixed-size blocks into the pty so the stimulus is comparable
   across hosts, or the cadence metric measures the OS buffer, not view.

Step 2 (implement the pins + unbounded producer + window) and step 3 (re-record
both classes on quiet hosts; mbp needs the pinned nvim + a view build, neither
present yet) follow. The Linux `[flood.minimal]` bar in `dev-linux.toml`
(cadence_p99_ms 14.245, drain_ratio 0.970) was recorded under bash and must be
re-recorded after the pins land.
