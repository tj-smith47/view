# Roadmap restructure — one release, dependency-grouped streaks (2026-09-04)

User rulings this file encodes (2026-09-04):

- **There is no v0.1.0 goalpost.** Every known feature goes out in the initial
  release: everything under README "Landing before v0.1", everything under
  "After v0.1", spec §15.1's workspace arc, the four post-v0.1 charters, and
  every open dogfood/audit finding. "Post-v0.1", "v0.2" and "pre-v0.1.0
  perf session" stop being scheduling words. (The strangler direction, §15,
  is a direction, not a feature; it stays a direction.)
- **Work runs in focused streaks**, one concern per session, grouped by
  dependency; the harness task list holds only the active streak.
- **Briefs are strict**, implementers are held to them, deviation escalates
  the model rather than spending a fix round.

## Why the previous ordering failed

Four tasks landed in a day because the list interleaved five unrelated
concerns (terminal correctness, process lifecycle, UI identity, Windows,
docs), each dispatch re-learned the tree, and a UI task was built on a
screen that still painted wrong, so its evidence was worthless. The order
below puts *the screen is right on the user's terminal* first because
nothing later can be dogfooded until it is, and puts the UI identity next
because it changes what statusline/tabline/tree/separator tasks even mean.

## The Hyprland / omarchy fundamentals (binding design input for S3)

What a tiling desktop actually is, and what view takes from it:

| Hyprland / omarchy fact | view mapping |
|---|---|
| **Tiles never overlap.** The screen is fully partitioned into tiles (dwindle = recursive binary splits; master = one main + a stack). Resizing one tile resizes its neighbours. | nvim windows are the tiles; the window tree *is* the layout tree. No separator column: tiles are set apart by gaps. |
| **Gaps and frames, no title bars.** `gaps_in` between tiles, `gaps_out` at the screen edge, rounded corners, a border whose colour marks the *active* tile. Identity lives in the bar, not on the window. | Each window gets a rounded frame with a gap; the active window's frame is the accent colour. The frame's top edge carries the buffer name, the bottom edge carries the status segments (mode, git, diagnostics, position). That replaces the per-window statusline: the frame *is* the status — laststatus/statusline plugins are superseded through the §5.5 registry, never drawn twice. |
| **Floating is the exception, and transient.** The launcher (walker) floats centred over the tiles; notifications (mako) anchor to a corner; dialogs float. None of them are in the layout. | Pickers and the palette float centred; notifications anchor top-right; conflict notices float once (#22). Structural surfaces (tree, palette, agent panel, notifications) are **overlays by default and windowed by config**, per surface, with one key that cycles the whole set (user ruling 2026-09-14, supersedes the earlier "tree is a tile, never a float" wording; #32 becomes the windowed placement of the tree). |
| **Workspaces**, one visible set of tiles at a time, switched from the bar. | tabpages are workspaces. |
| **omarchy's top bar**: a thin bar with a centred workspace *pill* (the active workspace highlighted, siblings dimmed), clock/status at the edges, the launcher summoned by a key. | The tabline becomes a top pill: tabpages (or buffers when there is one tabpage — a config choice) centred as a pill, session identity (remote host, agent state) at the edges. Supersedes bufferline through the registry; `[native] tabline` chooses pill vs plugin (#25 folds into this). |
| **Theme cohesion** comes from a switcher rewriting each tool's config. | `theme = "auto"` derives from the live colourscheme (C3 is the evidence task). |
| **Menu**: one key opens a launcher listing everything the desktop can do. | the command palette, centred pill, same key convention as omarchy's (`Super+Alt+Space` → `<leader><leader>` default, configurable). |

Two look modes, both first-class, behind a knob that must always exist
(user ruling 2026-09-04: tiles inside an already-tiled desktop look awkward,
so the look is toggleable and defaults by detection):

```toml
[ui]
panes = "auto"   # "auto" | "tiles" | "nvim"
```

| `panes` | look |
|---|---|
| `"tiles"` | frames + gaps + accent, status in the frame edge, top pill |
| `"nvim"` | nvim's `│` separators and statusline, what T9 ships today |
| `"auto"` (default) | `"nvim"` only when a **tiling** window manager owns the layout (user ruling 2026-09-04: a floating/stacking desktop like GNOME, KDE, macOS or Windows still gets tiles). Detection from view's own environment: `HYPRLAND_INSTANCE_SIGNATURE`, `SWAYSOCK`, `I3SOCK`, `XDG_CURRENT_DESKTOP` ∈ {Hyprland, sway, i3, river, niri, bspwm, dwm, awesome, qtile, xmonad, herbstluftwm, leftwm}, or `_NET_WM_NAME`-class hints where a desktop name is absent; `"tiles"` everywhere else, including every non-tiling GUI desktop, a bare tty, and any ssh session (the client's environment never reaches the server). `doctor` reports the mode and the marker that chose it. |

`:View panes tiles|nvim` flips it live (a resize-class full repaint), so a
user on Omarchy who wants tiles anyway is one command or one config line
away. The
compositor built for tiles is the §15.1 pane compositor: a tile's content is
an nvim grid today and an image / media / browser / remote-tree surface in
S6, so the workspace arc is not a second compositor, it is more tile kinds.

## Streaks, in execution order

Each streak is one session (or a few, `/clear` between), owns one plan file
with strict briefs, and ends with the user dogfooding on Termius (263x88).
Task ids in parentheses are the harness ids from the 2026-09-04 list.

### S1 — Terminal truth: the screen is right on the user's terminal

Entry: now. Blocks everything (no UI evidence is valid until this holds).

1. Ambiguous-width cursor re-sync + neighbour repaint (#39). Root cause is
   established: ratatui-crossterm's `draw` emits `MoveTo` only when the next
   cell is not `x+1`; Termius draws EAW=Ambiguous glyphs (nerd icons in the
   private-use planes, and box-drawing U+2500…) two cells wide, so every run
   after such a glyph lands one cell right and the next diff misses the stale
   half. nvim's TUI does `grid->row = -1` after any `utf_ambiguous_width`
   glyph (tui.c `print_cell_at_pos`, v0.12.4) to force an absolute CUP. Fix:
   view's own `draw` loop (style diff identical to ratatui-crossterm's, pinned
   byte-for-byte on ambiguity-free frames) that forces `MoveTo` after a cell
   whose symbol is EAW=A (`width() != width_cjk()`) or carries VS16, and a
   diff adaptor that re-emits the right neighbour whenever such a cell
   changes. Cost: +8 bytes per ambiguous glyph, the same nvim pays.
2. The close battery (#41): dir → file → splits → close one by one,
   `rec.py` at 263x88 between every step; duplicate status bars and residue
   fixed at root cause; the battery becomes a committed replay test.
3. Two ESC bytes in one read reach nvim as two `<Esc>` (#37).
4. nvim-tree scroll lag over ssh (#42): measure bytes/frame and frame count
   per scroll step against nvim under the same recorder; fix the emission
   (the no-change 32-byte frames and full-row repaints are the suspects).
5. Directory open latency on the user's link (#28): needs the user's
   `VIEW_LOG` run; the recorder above gives bytes-on-wire as a second signal.
6. Startup notice as a message (#21, landed 4f99837), alt-screen clear
   (#24, landed 1c526c4), separator under floats (#23, landed a85d2b7),
   conflict floats to history (#22, landed 12d1c25) — evidence re-taken on
   the recorder after item 1, since all four were captured on a screen that
   still shifted.

Exit: replay of the battery under a width-2-ambiguous emulator model is
cell-exact; the user's Termius pass shows no residue.

### S2 — Engine lifecycle: it starts, stops and dies exactly once

Landed 793197e..0acb3e6 on `dev/p6-polish`. The range also carries the CI
repairs that window needed (e319bde, 42306f8, 66dbc33, 3f46987, a8c10b8).

1. `:qa!` respawns the engine 1 in 6 (#30) — `announced_exit` race (landed
   af8121e: an exit nvim announced is never a fault, and a stop settles on
   the reader's completion; repro loop 535d951, hardening 46fea04, 2207e2e).
2. Stray reaping / busy loop after PTY death (#18) — the parent-death tie on
   all three platforms, with the signal shapes to measure it (landed cd2adf4;
   register ab138a4, hardening 3abb372, af462be, 7861cb2).
3. macOS: session whose pty master closed does not end (#38) — a Darwin poll
   with an empty mask registers no kqueue filter, so HUP never arrives
   (landed 2d06383, source pin 56b7b39).
4. Key-backlog spin root cause (T26, #15) — the paint happens once per
   drained input batch instead of once per key, capped at one frame of
   staleness (landed 0604ec8; Windows loop and cap 57d4834, hermetic data
   root 01c48d4, 7c211a7, 0acb3e6).

Exit: 30-run loops of each on dev-linux and mbp, zero strays in the
process table after. Met on dev-linux at 0acb3e6 from the installed binary
(390 quit and signal runs across ten shapes, 30 hangup runs, 30 paste runs,
zero strays). On mbp the pre-attach signal legs (20 runs) and the hangup
loop (30 runs) are recorded; the settled and editing signal legs are not
recordable there, because the login-shaped config's failing mason installs
repaint and the screen never settles.

### S3 — Tiled UI: the Hyprland / omarchy identity

Design doc first (the fundamentals table above expanded into config,
keymaps, goldens), fable-reviewed, then:

1. Frames + gaps + active accent, `[ui] panes = "tiles" | "nvim"` (#27).
2. Status segments in the frame's bottom edge; `[ui] panes = "nvim"` keeps a
   real bar (background, mode, filetype) (#26 re-scoped).
3. Top pill for tabpages/buffers; `[native] tabline` pill | plugin (#25 re-scoped).
4. Tree is a sidebar tile, `<leader>e` toggles from inside it (#32).
5. Pickers/palette centred floats, notifications corner-anchored, one
   conflict notice — the float class audited against the table.
5a. Gapped vs gapless, and overlay vs windowed per surface (user ruling
   2026-09-14). Both are runtime toggles as well as config:

   ```toml
   [ui]
   panes = "auto"          # "auto" | "tiles" | "nvim"
   gaps = true             # false = gapless: frames touch, no outer gap

   [ui.surfaces.tree]
   placement = "overlay"   # "overlay" | "windowed", each surface independent
   anchor    = "left"      # left | right              (sidebar shapes)
   size      = 30          # percent of columns; alias of [native] tree_width

   [ui.surfaces.agent]
   placement = "overlay"
   anchor    = "right"     # left | right
   size      = 30          # alias of [ai] panel_width

   [ui.surfaces.palette]
   placement = "overlay"
   anchor    = "center"    # overlay: center | top | bottom; windowed: top | bottom
   size      = 40          # percent of rows

   [ui.surfaces.notifications]
   placement = "overlay"
   anchor    = "top-right" # overlay: any corner; windowed: left | right (stream), top | bottom (ticker)
   size      = 30

   [keys]
   toggle_gaps = "<leader>ug"
   cycle_surfaces = "<leader>uw"   # config -> all windowed -> all overlay -> config
   ```

   `cycle_surfaces` never writes the config: the three positions are the
   user's `[ui.surfaces]` table, all four forced windowed, all four forced
   overlay, then back to the table. Windowed `tree` and `agent` are the
   sidebar tiles from items 4 and the fleet plan; windowed `palette` is a
   tile at the bottom edge. Windowed `notifications` is a slide-over stream
   on the right edge: every recent notification in one scrollable list,
   newest first, each row carrying its datetime; selecting the stream
   (focus into it) stops the 4 s transient timeout for as long as it holds
   focus, and `j`/`k`/`G`/`gg`/`y`/`d` behave as in the message history.
   Requires a timestamp on `MessageEntry` (none today), which the history
   overlay then shows too. `anchor` is one field with two readings: the
   float anchor in overlay mode, the tile edge in windowed mode (the
   existing `Anchor` enum in `native/geometry.rs`). The allowed set is per
   surface; a value outside it is a startup notice plus the default, never
   a rejected config. Two sidebars on one edge stack vertically when
   windowed (tree above agent) and open on top of each other when overlay.
   `size` means the same percent in both placements, so the cycling key
   never resizes. `[native] tree_width` and `[ai] panel_width` stay as
   aliases with a deprecation notice. Goldens for gapped/gapless and for every surface
   in both placements join item 6.
6. Tier goldens for every tile/pill/float surface at every tier (T24, #13).
7. README visuals (user ruling 2026-09-20): a tape or gif beside each
   feature block (agent panel, remote editing, engine restart, tiled panes)
   and a diagram for the surface ownership idea, once the tiled look
   exists to record. The user decides where tapes live (repo or release
   page). The structure pass is done (efc0a6d..e01a285); every Performance
   figure has to stay resolvable under `scripts/check-budget-drift.sh` and
   `task drift:sweep`.

Exit: goldens committed for both look modes; user's Termius pass; README
visuals recorded under the user's config.

### S4 — Native surfaces that were claimed and never seen

1. Buffers picker lists nothing (#31).
2. False conflict notice under `--clean` (#33).
3. History pause/copy/dismiss, live-grep results, diagnostics segment, key
   replay — each captured live under the user's config, fixed if absent (#35).
4. Real-config compat leg skipped by default (#34).
5. nvim-cmp's cmdline completion draws its own box at the bottom-left
   instead of landing in the palette's rows (user report 2026-09-15). The
   absorption path exists (`view-core::native::palette::AbsorbedRows`,
   `docs/palette-popupmenu-source-wire-capture.md`); prove under the user's
   config why cmp's float is not absorbed and fix so the candidates render
   inside the palette.

Exit: one capture per feature in the report, under the user's config.

### S5 — Invented capabilities (plans drafted, not built)

Order by dependency: key introspector → session DVR (both keystream-side),
media handoff → image viewing (image is start-gated on media's open
dispatch), then C2 agent-fleet attention, C1 reattach persistence, C3
theme-switcher evidence, C4 agent change gallery (the user judges C4 against
the dogfooded panel first — a ruling, not a build gate).

### S6 — Workspace arc: more tile kinds

Image tile and media tile on the S3 compositor, remote-tree tile, browser
tile over CDP (system Chromium, detected never bundled, `doctor` guides),
in-pane mpv. Servo stays a watch.

### S7 — Setup and support surface

T7 derived-defaults audit (#4), T13/T14 doctor (#2, #3), T16 adapter
packaging (#5), T17 install from artifact on three platforms (#6), T25 ACP
version honesty (#14).

### S8 — Windows tier-1

T18 platform-gate register (#7) → T19 memory metric (#8) → T20 oracle/compat
equivalents (#9) → T21 CI legs (#10) → T22 winserver evidence (#11). Last
among code streaks so every feature above is in the evidence it attests.

### S9 — Release evidence

first_paint regression check under a quiet window (#40), echo campaign
(#19), the performance-and-stability session over the whole population,
T23 docs (#12), final whole-branch review (#16). Docs are written once,
against the finished surface.

#### The performance-and-stability session's ledger

A task that has absorbed its hours leaves what it measured here rather than
running another loop.

**S1.12 cold start, the settled screen under a real config** (2026-09-14/15,
about 12 h of session time across six commits, 823b76c through b880b9b).

- *What remains.* `startup.settled_ratio_p50` on `dev-linux`/`user` reads
  1.0287 against a bar of 1.0, ledgered as a `[[shortfall]]`. About a
  millisecond of it is the attach's full-screen redraw travelling over RPC
  and being re-rendered by view, where bare nvim's own TUI reads that screen
  out of process memory; the rest is the attach-and-takeover round trip the
  startup pump now runs inside `VimEnter`.
- *What was tried and measured.* The parked attach was the cause: view's
  attach and takeover sat behind the `view_vim_enter` reply until nvim had
  drawn once, so the whole first draw was re-rendered afterwards. The fix
  pumps the loop inside the startup chunk (`vim.wait(200, attached)`) and
  fires one synthetic `UIEnter` on it. Round 1 A/B, three interleaved runs of
  the same driver with both binaries in each run: base 1.1065 p50 against fix
  1.0316 (n=22, load 7.0 to 4.5), and the same step on two earlier runs (base
  1.0888 / fix unavailable, base 1.1409 / fix 1.0735). The traced tail --
  marker buffer filled to the marker's bytes on the pty -- went from
  6.11-12.27 ms to 1.35-2.62 ms against bare nvim's 0.90-1.41 ms, and the
  attach moved from 4.33-9.96 ms after the marker buffer is filled to
  4.0-7.3 ms before the marker's `VimEnter` starts. Fix rounds 1-4 were
  review findings (the synthetic event's shape and ordering, the held reply's
  `EngineLost`, bounded oracle teardowns, the doc-figure walk), not further
  tuning of the number.
- *Next lever, as the report names it.* Two, and each is a design change
  rather than a tuning pass: removing the redraw needs the UI attached before
  `init.lua` runs, which is the whole of what the late attach exists to avoid
  (a plugin reading `nvim_list_uis` at configure time must see a plain TUI);
  removing the round trip needs the takeover to stop being one.
- *Carried with it.* `startup.server_delta_ms` on the same cell re-seats from
  just below zero to 1.637 ms, because the attach wait now runs inside the
  interval that metric measures. That is the same work on the other side of
  the mark, and the ratchet's next honest reading on this cell cannot fall
  more than the class's published spread below 1.637 without being refused as
  a lucky draw -- so a fix that removes the wait needs another hand re-seat.
  The same record run's tail moved too: `first_frame_ratio_p99` measured
  1.0715 (82.764 ms against 77.243 ms) against the 1.0124 seat the run held
  (tails are recorded, not gated, on this shared class) -- the first thing
  to check when this session revisits the path that produces the first
  frame.
- *Owed.* `startup.settled_ratio_p50` and `startup.server_delta_ms` are each
  one draw at load 1.8, not a replicate median; this session re-seats both
  from a replicate median (`--campaign 8`) on a quiet host.

**S1.13 flood cadence, the screen under a terminal storm** (2026-09-15, about
3 h of session time, no code change).

- *What remains.* `flood.cadence_p99_ms` on `dev-linux`/`user` reads 16.914 ms
  against a bar of 16.0, ledgered as a `[[shortfall]]`; `flood.cadence_p99_ratio`
  is 0.981, so view is the better of the two sides in every draw of the
  recording run and the absolute the bar refuses is the engine's own.
- *What was tried and measured.* No tuning: the cell was attributed by reading
  and by two measurements. The engine refreshes a terminal buffer on a 10 ms
  one-shot timer (`REFRESH_DELAY`, nvim v0.12.4 `terminal.c:132`, the pin in
  `.engine-pin`), so both sides' cadence is that timer plus one redraw. The
  login stack's share was measured with bare nvim over the generated `user`
  fixture at host load 0.7 to 1.1: lualine's `statusline()` was evaluated 918
  times in a 15 s flood for 167.6 ms in total, under a fifth of a millisecond
  each, which exonerates it and leaves the share with nvim's own redraw under
  the fixture's window options. `flood.minimal` was re-recorded the same day
  and re-seated by hand from 14.5657 to 16.0907, so the two legs sit within a
  millisecond of each other.
- *Next lever, with the file it lives in.* L1, the floor itself
  (`REFRESH_DELAY 10` in the pinned engine's `terminal.c:132`), is not view's
  to move and no view-side change reaches under it. L2, the heartbeat prober
  at `crates/view-engine/src/heartbeat.rs:63`, is the one flood cost only view
  pays -- about seven `nvim_get_mode` requests per window, each taking an
  engine main-loop turn -- and `task heartbeat-ab` is what decides whether it
  is worth removing rather than a guess. L3, the missing scroll-region escape
  (`crates/view-core/src/grid.rs:325-329` dirties every row of a scrolled
  region and `crates/view-tui/src/terminal.rs` emits no `CSI r`/`IL`/`DL`
  where the engine's own TUI does), buys nothing in this cell, where a
  ~3000-line scroll exceeds the window and both sides repaint everything --
  pull it for the `scroll` row. L5 is what the row records:
  `crates/view-bench/src/scenarios/flood.rs:207-211` already computes p50 and
  p90 per side and only the p99 is persisted at `flood.rs:416-418`, so
  persisting the p90 beside it would tell a tail that has detached from its
  bulk from a host that hiccupped a dozen times.
- *Owed.* The published spread `"flood.cadence_p99_ms" = 1.02` in
  `dev-linux.headroom.toml` was sized on the 14.566 level and the campaign
  behind it drew inside 0.6%; today's three draws span 15.61 to 17.79. The
  factor needs re-characterizing against the level the cell now holds.

## Execution discipline (binding from S1 on)

- **One streak per session.** The harness list carries only the active
  streak's tasks; the rest live here. `/clear` between streaks.
- **Briefs are the contract.** Every brief names: the files it may touch,
  the exact signatures, the pins by name with their assertions, the capture
  or replay battery that proves it, and the perf statement owed. An
  implementer that touches a file outside the list or changes a signature
  in place is stopped and re-dispatched, not asked to fix.
- **Models.** Implementers: opus, always, for anything under `view-tui`,
  `view-surface`, `view-native`, `view-engine` input/paint paths (never
  sonnet). Reviewers: Fable 5.1 for S1–S3 and S6 (screen-level work is
  where the misses were); opus elsewhere. Fix rounds: two on the same
  agent, then a fresh agent one tier up — not three, not five.
- **Evidence per task**: cap frames under the user's config *and*, for
  S1/S3, a 263x88 recorder replay; per streak: the user's Termius pass.
- **Install after every landed task** to `~/.local/bin/view` (ruled).
