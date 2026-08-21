# Migration Integrity Charters -- capability probing, surface ownership, compat evidence, notification surface

Boundary-level charters, deliberately free of signatures and task
decomposition, in the `2026-07-18-p3-p6-charters.md` format. Each gets its
full task-level plan (bite-sized TDD tasks, complete code, no placeholders)
authored when its start gate opens, against the real interfaces the
prerequisite work produced, under the planning protocol in
`2026-07-18-p3-p6-charters.md` (binding: coverage walk, wire-facts captured
never recalled). Spec of record: `.claude/specs/2026-07-17-view-design.md`;
on conflict the spec wins.

A charter item is a commitment; dropping one at planning time requires user
approval (ruled 2026-08-14).

**Start gate, all four charters:** P5 AI complete (its exit checklist closed
and the whole-branch final review folded). Ruled by the user 2026-08-20:
finish P5 first, then these as sub-phases.

**Origin:** the first real dogfood of a release binary by the user, over
Termius SSH from an iPad against a headless host, 2026-08-20. Every finding
below came from that one session. That is the finding of record in its own
right: 16 compat scenarios, a green suite, and CI-gated budgets did not
surface any of it, because none of them run what a user's own machine and
config actually present. The charters are ordered by leverage, not by the
order the symptoms appeared.

---

## C1. Capability probing tells the truth, and says it in the right place

**Start gate:** none beyond the shared P5 gate. Touches no AI surface.

**Charter:** what view detects about a terminal must be detected, not
inferred from a proxy that a normal connection drops -- and a diagnostic must
appear when it is useful rather than when it is confusing.

Three faults, one theme, all observed live:

- **Truecolor is inferred from `$COLORTERM` alone**
  (`crates/view-tui/src/tiers.rs`, `truecolor_from_colorterm`, accepting
  exactly `truecolor` or `24bit`). `ssh` does not forward `COLORTERM` --
  it is in neither `sshd`'s default `AcceptEnv` nor `ssh`'s `SendEnv` --
  so every SSH session into a headless host probes `truecolor=false`.
  Because truecolor gates the tier (that module's own tests: "sync
  supported but no truecolor still yields basic"), one absent environment
  variable demotes a fully capable terminal to `Tier::Basic` and the whole
  theme renders degraded. view already escape-probes `sync` and
  `kitty_kbd` on a DA1 fence; truecolor belongs on that same path, with
  `COLORTERM` kept as a hint and never as the sole oracle. Widening the
  accepted values is not the fix.
- **Rounded borders are gated on `Tier::Full`**
  (`crates/view-surface/src/overlay.rs`: `Tier::Full => ROUNDED`,
  `Tier::Standard => PLAIN`), so every native overlay loses its corners
  unless the terminal reports sync *and* truecolor *and* kitty_kbd.
  U+256D-2570 has nothing to do with 24-bit color, synchronized output, or
  the kitty keyboard protocol; any terminal that draws `┌` draws `╭`. That
  file's own comment already notes the two sets differ only in the
  corners, which is the argument against the gate. The real question for a
  border set is whether the terminal draws box-drawing glyphs at all --
  a Unicode question, whose honest fallback is an ASCII set, not the
  square Unicode one.
- **The capability line prints on every launch**
  (`log_caps`, called unconditionally from the probe path): an `eprint!`
  in raw mode before the alternate screen is entered. The user never sees
  it at startup because the alt screen covers it; it surfaces on exit when
  the alt screen tears down, reading as debug spew on quit. A diagnostic
  that only ever appears at the wrong moment is worse than none. It
  belongs in `VIEW_LOG` with the other startup diagnostics, surfaced
  interactively only behind an explicit flag or on a `--tier` override.

**Why it is chartered and not a bug fix:** the three share one root shape --
a capability decision bound to a signal that does not carry the fact it is
being asked about. The plan must state, per capability view consumes, what
the authoritative probe is and what the fallback degrades to, so a fourth
instance is unrepresentable rather than merely absent.

---

## C2. Surface-ownership conflicts are detected, for plugins nobody tested

**Start gate:** none beyond the shared P5 gate. Highest leverage of the four;
supersedes fixing the two observed plugins one at a time.

**Charter:** view externalizes a fixed, small set of surfaces
(`crates/view-engine/src/nvim_api.rs`: `ext_linegrid`, `ext_cmdline`,
`ext_popupmenu`, `ext_messages`, `ext_tabline`, with no opt-out --
`ext_messages` is attach-level). Every plugin conflict is some plugin
claiming one of those five. The ecosystem is unbounded and an enumerated
per-plugin list can never deliver "painless migration" on its own; the set
view claims is fixed and enumerable, so the conflict *class* is detectable
generically.

Two shapes, both observed in one real config:

- **A -- the plugin inspects the attached UI's `ext_*` flags and refuses.**
  noice.nvim: `noice.health.check()` raises one ERROR notification per
  externalized ext, unconditionally, before its own `setup()` has parsed
  the options that would disable those components. No noice option gates
  it (upstream `folke/noice.nvim#1137`, unfixed). The user's first launch
  is three error popups.
- **B -- the plugin ignores the externalization and opens its own float**
  anchored at nvim's real cmdline/message position, which with no
  `ext_multigrid` composites into the base grid exactly where view has
  already drawn its own chrome. `cmp-cmdline` (nvim-cmp's cmdline source
  opens a float rather than driving nvim's popupmenu, so view never
  receives a `popupmenu_show` and its palette-absorption path is never
  even reached); very likely `nvim-notify` too.

Shape B is detectable from geometry view already receives: a float whose
anchor lands on the cmdline row or the message area is claiming a surface
view owns. On detection view says ONE clear thing naming the plugin, the
surface, and the remedy, instead of leaving the user to reverse-engineer
overlapping chrome.

Deliverable alongside the detection: a **surface-ownership matrix** -- per
externalized surface, the plugin classes that claim it, view's policy
(own / yield / absorb), and the scenario that proves that policy. Coverage
gaps become visible rather than implicit.

**Design fork to settle at plan time, not now:** for shape A, whether view
detects and disables the conflicting component with one explanatory notice,
or exposes an ext-set opt-out so the config runs unchanged. Read the
plugin's own guard before choosing. Note that view legitimately owns these
surfaces -- that IS the coherent-UI contract -- so dropping the ext set is
not on the table.

---

## C3. The compat suite proves the unaccommodated config

**Start gate:** none beyond the shared P5 gate. Should land with or before
C2, since C2's matrix asserts through this mechanism.

**Charter:** the compat suite is green while a real user's first launch
shows error popups, because the fixture configures the conflict away and no
state anywhere runs an unmodified config.

Evidence of record: `compat/fixtures/heavy/nvim/init.lua` reaches into
noice's private once-only dedup table (`require("noice.util")._once`) and
pre-seeds it with the three exact error strings `health.check()` emits,
marking them already-sent so they never render; the opts table then disables
noice's cmdline/messages/popupmenu components. Every step is individually
well-reasoned and documented, and it cites the unfixed upstream issue. The
defect is not any one step -- it is that the suite only ever tests the
adjusted config, so it structurally cannot fail on a migration defect.
Green means "view works with configs already adjusted for view", which is
the inverse of the contract.

Second instance, same shape: `compat/scenarios/nvim-cmp.toml` drives only
insert-mode buffer completion and never types at `:`, so the one path that
actually breaks is the one path untested.

Scope:

- Every ui-owning scenario gets a first-class state running the plugin's
  own default/documented config with zero accommodation, asserting what the
  user actually sees.
- The adjusted state remains as a second state, representing view's
  recommended configuration.
- **Binding rule, to be enforced in review:** a fixture accommodation with
  no paired unaccommodated state is a suppressed migration defect. It fails
  review. If the mechanism can be made a lint over the fixtures rather than
  a reviewer's habit, prefer the lint -- a promise needs a mechanism.

---

## C4. The notification surface is readable, pausable, and recoverable

**Start gate:** the shared P5 gate, plus C2 settled far enough to know which
toasts are view's own and which belong to a plugin float compositing into
the grid. Building motion into a surface the user is not actually looking at
would be wasted work.

**Charter:** user-requested, from the same dogfood session. Three wants,
one timer.

- **Motion model:** notifications stack and slide upward as the topmost
  expires, each notice's dismissal timer starting only once it reaches the
  top of the stack, then exiting to the right. Today `paint_messages` is
  static: `Messages::visible_lines` picks persistent error/warn lines plus
  the most recent transient ones, and they appear and vanish in place with
  expiry running from arrival regardless of position.
- **Pause:** a hotkey that freezes expiry, because a notice can vanish
  mid-read.
- **History and copy:** a scrollable modal of notification history where a
  keypress copies an entry's text -- the common case is a file path the
  user wants and cannot select in time. The history overlay itself already
  exists (titled "Messages", asserted by the compat scenarios' native-only
  states); what is missing is scrollback, copy, and the pause. Copy routes
  through the existing clipboard path, including its no-system-clipboard
  notice.

Design them as one unit: a position-aware timer and a pause control are the
same timer, and building them twice is the mistake this charter exists to
avoid.

**Performance obligation:** any animation is damage-driven, and the paint
loop still never awaits. The perf contract in the repo's hard rules applies
in full -- a motion model that costs frames is a regression against the
"objectively faster, smoother UX" shipping contract, not a decoration on it.
