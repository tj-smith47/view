# Spec amendments draft — migration integrity (C1–C4)

Paste-ready amendments to `.claude/specs/2026-07-17-view-design.md`. Each
entry names the section, gives the exact replacement or addition in the
spec's own voice and formatting, and states in one line why the amendment
is owed. Nothing here is applied — this file is the source the amendment
commit copies from.

Convention followed throughout: the spec records amendments inline, dated,
with the superseded text struck or explicitly named rather than silently
rewritten (see §3.1's "Amendment 2026-07-26, first-paint split" and §7.1's
FloatTitle amendment for the house style).

---

## A1 — §7, tier table preamble: the capability register (C1)

**Why:** the tier bullet says capabilities are "auto-detected via terminfo +
capability queries" and names SSH as a first-class detection case, while
truecolor is in fact read from `$COLORTERM` alone, which ssh does not
forward. The register makes "what is the authoritative probe for this
capability" a table the code is checked against, so a fourth instance of
the same defect is unrepresentable rather than merely absent.

**Replace** the tier bullet's parenthetical (currently: "(auto-detected via
terminfo + capability queries with timeouts; tmux and SSH are first-class
detection cases — query through rather than trusting `TERM=screen-256color`,
with `doctor` guidance on tmux `allow-passthrough`; overridable)") **with:**

> - **Terminal capability tiers** (derived from the capability register
>   below, never from a tier guess; tmux and SSH are first-class detection
>   cases — query through rather than trusting `TERM=screen-256color` or a
>   forwarded environment variable, with `doctor` guidance on tmux
>   `allow-passthrough`; overridable via `--tier`/`[ui] tier`):

**Insert**, immediately after the tier table and before the "Degradation is
a first-class tested surface" sentence:

> **Capability register (amended 2026-08-21, C1).** Every capability view
> consumes is decided by a probe that carries the fact being asked about.
> An environment variable may be a hint that shortens a probe, never the
> sole oracle: `ssh` forwards neither `COLORTERM` (it is in neither sshd's
> default `AcceptEnv` nor ssh's `SendEnv`) nor most of what a terminal
> knows about itself, so a capability bound to one is a capability that
> silently degrades on every remote session. A capability with no row here
> does not exist: adding one to `TermCaps` without its row is a build
> failure, not a review note.
>
> | Capability | Authoritative probe | Hint (never sole oracle) | Absent/unanswered ⇒ | Gates |
> |---|---|---|---|---|
> | `sync` | DECRQM mode 2026 (`ESC [ ? 2026 $ p`); the DECRPM reply `CSI ? 2026 ; Pm $ y` with `Pm` `1` or `2` | — | `false` | BSU/ESU bracketing |
> | `truecolor` | DECRQSS SGR readback: set a known 24-bit color, read `ESC P $ q m ESC \` back, reply preserves the RGB triple | `COLORTERM=truecolor\|24bit` (skips nothing; corroborates) | `false` | tier, color derivation |
> | `kitty_kbd` | Kitty keyboard progressive-enhancement query (`ESC [ ? u`) | — | `false` | key encoding |
> | `unicode_boxes` | Cursor-position readback (CPR) after writing one box-drawing glyph: advanced exactly one column ⇒ the terminal treats the sequence as one cell (a terminal that is not decoding UTF-8 advances three) | `LANG`/`LC_ALL` naming UTF-8 | `false` | border charset (§7.1) |
>
> A probe answers the question it can actually ask. The `unicode_boxes`
> readback proves the terminal's cell accounting, not the font's coverage:
> a terminal whose font lacks `╭` still advances one column and renders
> tofu. That is the best evidence obtainable from the wire, it is strictly
> better than an environment variable, and the honest reading is "this
> terminal treats a box glyph as one cell" — not "this glyph is legible".
>
> All four ride the one batched startup probe, fenced by DA1 (`ESC [ c`) —
> one write, replies read until the fence answers or the probe deadline
> expires, never repeated and never on the key-dispatch path. The probe's
> cost is inside the §3.1 `shell_visible_ms` budget, which is where a
> regression in it must show up.
>
> **Tier is derived from the register, and gates nothing on its own.** Tier
> is coarse UX vocabulary for the experience table above; behavior gates on
> the individual capability (`sync` for BSU/ESU, `unicode_boxes` for the
> border charset, `truecolor` for color derivation). A capability that a
> tier merely correlates with is not a probe for it.
>
> **The capability line is a diagnostic, not startup output.** The resolved
> register is written to `VIEW_LOG` with the other startup diagnostics on
> every launch. It is printed to the terminal only when the user asked for
> it — `--print-caps`, or a `--tier` override, which is a claim about the
> terminal that deserves an acknowledgement. A line printed unconditionally
> in raw mode before the alternate screen is entered is invisible at
> startup and reappears as debug spew on quit, which is worse than no
> diagnostic at all.

---

## A2 — §7, tier table `basic` row: borders are not a color capability (C1)

**Why:** the `basic` row promises "plain borders" as a property of a
16-color terminal, which binds a Unicode question to a color one. §7.1
already states the correct rule ("corner glyphs are font coverage, not a
terminal capability"); this makes the tier table agree with it.

**Replace** the `basic` row of the tier table:

> | `basic` | 16-color, ASCII-safe | Correct and complete, plain borders, no color derivation |

**with:**

> | `basic` | 16-color | Correct and complete, no color derivation; border charset follows `unicode_boxes` (§7.1), not this tier |

---

## A3 — §7.1, "Borders and spacing" (C1)

**Why:** the shipped code gates rounded corners on `Tier::Full` and gives
`Tier::Standard` square Unicode corners, which contradicts this paragraph
and demotes every SSH session's chrome for want of a forwarded variable.
The paragraph also names no probe for the property it says the choice
depends on; the register (A1) supplies one, and the square-Unicode set has
no home in the design language once the choice is "does this terminal draw
box glyphs".

**Replace** the paragraph's first sentence:

> **Borders and spacing.** Rounded corners `╭ ╮ ╰ ╯` on `full` and
> `standard` — corner glyphs are font coverage, not a terminal capability;
> `basic` falls back to ASCII `+ - |`.

**with:**

> **Borders and spacing.** Rounded corners `╭ ╮ ╰ ╯` whenever the terminal
> draws box-drawing glyphs, on every tier — corner glyphs are font
> coverage, not a color, synchronization, or keyboard-protocol capability,
> so the register's `unicode_boxes` row (§7) is the only input to this
> choice. A terminal that does not draw them falls back to ASCII
> `+ - |`, which is the honest degradation: square Unicode corners
> (`┌ ┐ └ ┘`) are not a fallback for rounded ones, since any terminal that
> draws `┌` draws `╭`, and view ships no third border set (amended
> 2026-08-21, C1).

---

## A4 — §5.5, reconciliation table + supersession bullets (C2)

**Why:** view externalizes five surfaces with no opt-out, so §5.5's
"per-feature opt-out returns it" is not true today for the two surfaces the
ext set owns: turning `notifications` off leaves `ext_messages` attached
and the surface unreturned. The spec also promises a `vim.notify`
re-point that no takeover row implements. Both gaps are what a real user's
first launch hit.

**Replace** the third bullet under the class table (currently: "**Supersession
is runtime-only and reversible.** Applied post-`VimEnter`, only while the
native feature is enabled: statusline → `laststatus=0` …") **with:**

> - **Supersession is runtime-only and reversible.** Applied
>   post-`VimEnter`, only while the native feature is enabled: statusline →
>   `laststatus=0` (lualine still loads; its surface goes unused);
>   notifications → `vim.notify` re-pointed at the engine default, held
>   against a plugin re-patching it later, so messages flow through
>   `ext_messages` into view's toasts; tree/picker → view claims its
>   default keys (§5.3). **Nothing in the user's config files is ever
>   edited, and nothing needs to be removed or disabled in `init.lua` for
>   native features to win.** Superseded plugins keep loading; their cost
>   is memory, not conflict.

**Insert** after that bullet:

> - **Externalization follows the `[native]` switches (amended 2026-08-21,
>   C2).** The `ext_*` set view requests at `nvim_ui_attach` is not a
>   constant: `ext_cmdline`/`ext_popupmenu` are attached only with
>   `palette` enabled, `ext_messages` only with `notifications` enabled.
>   `ext_linegrid` is unconditional (it is the grid protocol, not a
>   surface) and `ext_tabline` is attached unconditionally today, with no
>   native feature of its own to switch it — recorded in the surface
>   matrix rather than left implicit. This is what makes "disabling
>   returns that surface to the user's plugins" (§9) literally true: a
>   plugin that inspects the attached UI's `ext_*` flags and refuses to run
>   sees a UI it supports, and the user's config runs unchanged. Reading
>   `[native]` therefore happens before attach, not after it. The opt-out
>   has to be view's, because it cannot be the plugin's: noice raises one
>   ERROR per externalized ext from a health check that `setup()` runs
>   *before* it parses the user's options (in the pinned `lua/noice/init.lua`), and
>   that check's ext loop is unconditional, so no noice option can suppress
>   a first launch's errors (upstream `folke/noice.nvim#1137`, unfixed).
> - **Surface-ownership conflicts are detected, not left to the user.** The
>   set view claims is fixed and small, so the conflict *class* is
>   detectable generically even for plugins nobody has tested: a floating
>   window whose geometry lands on a surface view owns (the cmdline row,
>   the message area) is claiming that surface, whatever plugin opened it.
>   On detection view says one thing, naming the surface, the plugin as far
>   as the window identifies it, and the exact line that resolves it —
>   never a second, silently overlapping chrome. **The default first launch
>   is where this contract is worth the most.** view's defaults keep the
>   surfaces, so a config carrying a plugin that refuses to coexist with
>   them starts with a real conflict, and that conflict is view's to
>   explain: a first launch that hits a surface conflict shows **one**
>   notice per claiming plugin, naming the surfaces it took and carrying
>   the `[native]` lines that yield them. It stands for the session — the
>   conflict is true until the remedy is applied and view restarts — and
>   the user takes it down from the notification history (§9), which is
>   where the notice itself points. The plugin's own startup complaints are the same finding
>   in the plugin's voice, so they are recorded to the notification history
>   rather than stacked as toasts beside it, and the notice says where they
>   are. Nothing is discarded and the history is one key away — what a
>   first launch must never be is a wall of somebody else's errors with no
>   remedy in any of them. Per-surface policy is
>   recorded in the surface-ownership matrix (`docs/surface-ownership.md`),
>   which names, for each externalized surface, the plugin classes that
>   claim it, view's policy (own / yield / absorb), and the compat scenario
>   that proves that policy. A surface with no proving scenario is a
>   coverage gap the matrix shows rather than hides.

**Replace** the last bullet of §5.5 (currently: "Every UI-owning plugin in
the §13.3 matrix is asserted in all three states: superseded (default),
deferred (`feature = false` with the plugin present), and
native-without-plugin.") **with:**

> - Every UI-owning plugin in the §13.3 matrix is asserted in all four
>   states: **unaccommodated** (the plugin's own default/documented config,
>   with no fixture adjustment of any kind), superseded (default),
>   deferred (`feature = false` with the plugin present), and
>   native-without-plugin. Amended 2026-08-21 (C3): three states all ran
>   configs already adjusted for view, which is the inverse of the compat
>   contract.

---

## A5 — §9, Notifications row and the `ext_messages` routing table (C2, C4)

**Why:** the Notifications row's v0.1 scope predates the notification
surface the user actually asked for (position-aware expiry, pause,
recoverable history with copy) and predates the `vim.notify` takeover A4
now states. The routing table's "transient toast with timeout" says nothing
about *when* the timeout starts, which is the whole of the C4 motion model.

**Replace** the Notifications row of the feature table:

> | Notifications | nvim-notify / noice messages | `ext_messages` with kind-aware routing (table below); kills "Press ENTER" without ever eating a prompt |

**with:**

> | Notifications | nvim-notify / noice messages | `ext_messages` with kind-aware routing (table below), `vim.notify` re-pointed at the engine default (§5.5) so a plugin's own float never composites over view's chrome; a slot-timed toast stack, a pause key, and a scrollable history with per-entry copy and dismissal (§7.1 motion, table below); kills "Press ENTER" without ever eating a prompt |

**Replace** the last two rows of the `ext_messages` routing table:

> | `msg_showmode` / `msg_showcmd` / `msg_ruler` / `search_count` | Statusline segments (macro recording must always be visible) |
> | everything else | Transient toast with timeout + scrollback history |

**with:**

> | `msg_showmode` / `msg_showcmd` / `msg_ruler` / `search_count` | Statusline segments (macro recording must always be visible) |
> | everything else | Transient toast, stacked; its dismissal timer runs **only while it occupies the top slot** — a notice that arrived behind others has not been read yet, and a timer that starts on arrival retires it before it was ever visible. Captured in scrollback history (amended 2026-08-21, C4). One exception, at startup only: while a surface conflict is being resolved, a transient goes to the history instead of the stack, per §5.5's first-launch contract — it is captured either way, and the exception ends at the first keystroke |
>
> **View's own notices choose their lifetime (amended 2026-08-21, C2).** A
> notice view raises about itself is transient like any other message
> unless it asserts a condition the user must act on — a surface conflict
> and its remedy line, which is worthless if it expires before it can be
> read. Those stand for as long as the condition does, and are retired from
> the history overlay, which lists what is standing and takes an entry down
> on request. Being persistent they take no stack slot, so a standing
> notice never freezes the transients behind it.
>
> **Reading a notice is a first-class operation (amended 2026-08-21, C4).**
> A notice can vanish mid-read, and the most common thing a user wants out
> of one is a path they cannot select in time. So: a pause key toggles
> expiry off for the whole stack, and it stays off while pause is on; a
> notice that stands until its condition ends is taken down from the same
> overlay that lists it; the history overlay is
> scrollable rather than a single screenful; and a per-entry copy key
> routes through the same clipboard path a `"+y` takes (system clipboard
> plus OSC 52, so a remote session copies to the local machine), reporting
> when no system clipboard is reachable rather than silently copying
> nowhere.

---

## A6 — §7.1, Motion catalogue and rules (C4)

**Why:** the catalogue's toast rows describe a fade-and-collapse exit that
the user has ruled against (exit right, stack slides up), and the rules
block says nothing about a stack whose timers depend on position — the one
property that makes the motion honest rather than decorative.

**Replace** the two toast rows of the motion table:

> | Toast enter | slides 3 cells in from the right edge, `fast` ease-out |
> | Toast exit | fades to backdrop over `slow`, then the row collapses |

**with:**

> | Toast enter | slides 3 cells in from the right edge, `fast` ease-out; enters at the bottom of the stack |
> | Toast exit | the top slot's toast slides out to the right edge, `slow` ease-in, and the toasts beneath it slide up one slot over the same interval — one motion, not two (amended 2026-08-21, C4) |

**Insert** as rule 5 of the Motion rules block (after "4. **Two durations.**"):

> 5. **Position-aware, and pausable.** A stacked surface's dismissal timer
>    belongs to the *slot*, not to the notice: only the top slot's timer
>    runs, and a notice's timer starts when it arrives there, never when it
>    was created. The pause key stops every slot timer at once and the
>    stack holds for as long as pause is on; turning it off arms the top
>    slot for a full timeout rather than the remainder, because a notice
>    that was paused mid-read has not been read yet — pause is the same
>    timer, not a second mechanism. Below the `full` tier there is no interpolation
>    (rule 1's state-first frame is the only frame), and the slot timers
>    and the pause key behave identically: motion is presentation, timing
>    is behavior.

---

## A7 — §11, config block (C2)

**Why:** A4 makes the `[native]` switches govern the attached ext set. The
config block is the surface a user reads to find that out, and the comments
there are the only place the coupling can be stated without inventing a new
key (there is none to invent: the switches already exist).

**Replace** the `[native]` block of the `view.toml` listing:

```toml
[native]
picker = true
tree = true
statusline = true
notifications = true
palette = true
```

**with:**

```toml
[native]
picker = true
tree = true
statusline = true
notifications = true      # false also detaches ext_messages (§5.5)
palette = true            # false also detaches ext_cmdline/ext_popupmenu (§5.5)
```

---

## A8 — §13.3, compat suite (C3)

**Why:** the suite is green while a real user's first launch shows three
error popups, because the heavy fixture pre-seeds noice's private
once-only dedup table with the exact strings its health check emits and
disables the components that would complain. Every step is individually
well-reasoned; the defect is that no state anywhere runs an unmodified
config, so the suite structurally cannot fail on a migration defect.

**Replace** the sentence "The matrix must include the UI-owning class (§5.5)
— lualine, noice, nvim-notify, nvim-tree/neo-tree, dressing, fidget — each
asserted in all three §5.5 states, alongside the semantic class (telescope,
nvim-treesitter, nvim-cmp, mini.nvim, which-key)." **with:**

> The matrix must include the UI-owning class (§5.5) — lualine, noice,
> nvim-notify, nvim-tree/neo-tree, dressing, fidget — each asserted in all
> four §5.5 states, alongside the semantic class (telescope,
> nvim-treesitter, nvim-cmp, mini.nvim, which-key). **The unaccommodated
> state is first-class and load-bearing (amended 2026-08-21, C3):** it runs
> the plugin's own default or documented configuration with zero fixture
> adjustment, **under view's own default `[native]` settings**, and asserts
> what the user actually sees on a first launch — including, where the
> plugin claims a surface view attached, view's single conflict notice and
> the absence of the plugin's own complaints from the toast stack.
> The adjusted configuration remains as a second state, representing view's
> *recommended* setup — it is evidence about a config already adapted to
> view, which is the inverse of the migration contract, and it may never be
> the only state. **A fixture accommodation with no paired unaccommodated
> state is a suppressed migration defect.** The suite enforces this rather
> than trusting review: a scenario state declares whether it wants
> accommodations, every accommodation in a fixture must sit behind that
> declaration, and a ui-owning scenario that declares no unaccommodated
> state fails to load. A semantic-class scenario must additionally exercise the surfaces
> view externalizes wherever the plugin touches them (a completion plugin
> asserted only in insert mode leaves the cmdline path — the one that
> breaks — untested).

---

## A9 — §13.3, the compat-evidence row schema (C3, small)

**Why:** the published evidence page's row schema carries `state`, which
now has five values; a page that cannot distinguish accommodated from
unaccommodated evidence republishes the same defect the suite just closed.
The enumeration must also carry `present` — the state every semantic-class
scenario declares — or the page's own rows would name a value its schema
cannot describe.

**Replace** "has a defined row schema — plugin, version, engine pin,
scenario, state, result, date" **with:**

> has a defined row schema — plugin, version, engine pin, scenario, state
> (one of present / unaccommodated / superseded / deferred / native-only),
> result, date

---

## Amendment order and dependency

A1–A3 (C1) are independent of the rest and can land with the first C1
commit. A4/A7 (C2 externalization) must land with or before the task that
makes the ext set follow `[native]`, because the compat scenarios' expected
behavior changes with it; A4 also carries the default-first-launch contract
that the one-notice task builds against, so it lands whole rather than in
two passes. A8/A9 (C3) land with the unaccommodated-state
mechanism, which the C2 tasks then assert through. A5/A6 (C4) land with the
notification-surface work, after C2 has settled which toasts are view's own.
