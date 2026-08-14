# Post-v0.1 Charters -- reattach persistence, agent-fleet attention, theme-switcher interop

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

Origin: gap analysis of tmux/herdr multiplexers and Hyprland/omarchy-class
desktops against the planned surface (user-approved 2026-08-14). Finding of
record: no conflicts -- view-inside-tmux passthrough is first-class (spec
`#7` capability tiers, tmux/SSH detection), terminal-level tiling belongs
to the WM/nvim splits in v0.1 and to the `#15.1` pane compositor from v0.2,
and remote's spawn model is a design difference, not a gap. The three
genuine gaps are chartered below; C1 and C2 both serve the `#15.1`
terminal-workspace identity ("bridging a bare server and a minimal desktop
experience from one terminal binary").

---

## C1. Reattach persistence (detach / reconnect)

**Start gate:** P5.5 remote Part B shipped (the SSH transport and remote
spawn exist; this charter extends their lifecycle), and the engine pin
carries the lifecycle commands each half of the scope uses: `:detach`
shipped in nvim 0.11, the `:connect`/`:restart` commands and their UI
events in 0.12, and the detachable-UI semantics (`:detach!`, survival of
an *unannounced* UI channel death) exist only past 0.12 (0.13-dev/HEAD as
of 2026-08-14). The deliberate-detach half needs only the 0.12 pin; the
unexpected-disconnect half is additionally gated on a pin carrying
`:detach!`-class semantics, or on an alternate `--listen`-server
architecture decided at plan time.

**Charter:** an engine outliving its UI connection, and a view that can
reattach to it -- the one tmux/herdr capability the spawn-only remote model
does not cover (spec `#5.6`: attach was recorded post-v0.1 at charter time;
this charter schedules it). Scope, boundary-level:

- Deliberate detach: `:detach` (or a view-side command/flag) leaves the
  engine running. Covered by the 0.12 pin.
- Unexpected-disconnect survival: an SSH drop or closed terminal leaves a
  detachable-marked session's engine alive. On a 0.12 `--embed` server an
  unannounced UI channel death kills the engine, so this half rides the
  `:detach!`-semantics gate above -- it must not be committed to at plan
  time on a 0.12 pin. Local and remote engines both qualify; remote is the
  motivating case -- without it, remote users wrap view in tmux server-side
  and reintroduce exactly the remote-rendering latency P5.5 remote exists
  to eliminate.
- Reattach: `view --connect <addr>` (address syntax consistent with the
  remote plan's `[user@]host[:path]` CLI shape -- noting the positional
  slot diverges in meaning: there `path` is the file to edit, here it is a
  server socket address / `v:servername`; a named plan-time decision, not
  a silent reuse) attaches paint/input to the surviving engine; buffer
  text, jumplists, undo history arrive engine-side and are therefore
  already correct (nvim owns all buffer text). view-side
  native state (pickers, panels) re-derives from the engine snapshot; it is
  ephemeral by the same rule.
- view implements the nvim 0.12 `connect` and `restart` UI events. Wire
  facts observed in nvim's live gui.txt (2026-08-14; to be captured as a
  planning-protocol step-1 artifact at plan time, against the pinned
  engine's own docs): a UI
  that does not implement `connect` degrades `:connect` to plain
  `:detach` -- hot-swap silently lost; and `:restart` with no UI handling
  the event leaves a dangling server. Both are correctness obligations on
  view-engine, not optional polish.
- Supervision interplay: a detached engine must not be treated as a death.
  The supervision exit-announcement mechanism (the announcement, not exit
  shapes, separates a death from a quit) is the discriminator to extend;
  detach is a third deliberate state, never a restart trigger.

**Recorded design caveats (from nvim upstream, to re-verify against the
pinned engine at plan time):**

- `:connect!` also stops the detached server when no other UI is attached
  -- the view-side UX must make destructive vs non-destructive reattach
  unambiguous.
- The `--listen` socket handoff race: a new server cannot bind an address
  the old server still listens on, and the old server cannot be told to
  stop by a client whose channel just died with it. Address allocation
  strategy is a plan-time decision with a captured repro, not a charter
  guess.

**Exit gates:** oracle test proving detach -> reconnect content identity
(differential against a never-detached twin); reattach handshake inside the
startup budget class (spec `#8`); paint loop never awaits RPC holds across
detach/reattach; supervision conformance script (P5.5 T7) extended with the
detach-is-not-a-death case.

---

## C2. Agent-fleet attention (editor-integrated)

**Start gate:** P5 AI shipped (its session model defines what "an agent in
view" is -- an ACP session, a terminal job, a worktree, or all three).
Fresh-session feasibility assessment at the gate, in the spec `#15`
item-4 pattern (fresh-eyes evaluation, not foreclosed in advance): this
charter fixes intent and bar, not mechanism.

**Charter:** the herdr insight -- semantic agent state (working / blocked /
done / idle) turning N parallel agents into an attention queue -- carried
past the pane boundary, where a multiplexer cannot follow. The invented-
capability bar applies (spec `#9`): repackaged herdr fails it; the editor-
integrated half is the capability:

- An attention surface (view-native sub-component, Model/Msg/update,
  `Effect::Rpc` only) listing this machine's agent sessions with semantic
  state, ordered by "needs a decision first". Whether it renders as a `#9`
  -class overlay or as a `#15.1` compositor pane is a gate-time decision --
  the compositor, if landed by then, is the natural host.
- Jump-to-artifact: selecting a blocked agent lands in the exact buffer,
  diff, or worktree the agent is asking about -- not in a terminal pane.
  This is the differentiator herdr structurally cannot offer.
- Fleet sources are a plan-time decision at the gate: ACP sessions arrive
  free with P5; external processes (worktree multiplexing, non-ACP agents)
  only if the P5-built session model already carries them without a new
  daemon. view does not grow a multiplexer, socket orchestration API, or
  pane tree; herdr/tmux remain the right tool for hosting the processes
  themselves.

**Exit gates:** budget from spec `#3.1` for the surface class; oracle
non-interference (attention surface open != engine state drift); config
toggle per `#9` -- disabling here returns the surface to nothing, as no
plugin is being replaced; the feasibility assessment itself recorded in
the plan with a go/no-go the user rules on.

---

## C3. Theme-switcher interop (omarchy-class desktops)

**Start gate:** none -- verification-first, schedulable any time after P4
theming is exercised end-to-end; expected to be mostly evidence, little
code.

**Charter:** omarchy-style cohesion comes from an external switcher
rewriting each tool's config. view already derives its theme from the
engine's live colorscheme with `theme = "auto"` (spec `#7`), so a switcher
that retargets the nvim colorscheme should carry view for free. Chartered
obligations:

- Evidence that the free path is real: an external colorscheme change on a
  running instance re-derives the theme live via the `#7`
  ColorScheme-autocmd -> rpcnotify bridge, without restart. (The `#7`
  cache's no-flash guarantee covers cold start and is not this gate;
  live-switch smoothness is asserted on the bridge path itself.)
- Constraint (consumer surface, binding on future config work): theme
  overrides stay in plain TOML at stable key paths (`[ui.tokens]`,
  spec `#7.1`) in the one `view.toml` (spec `#11`) -- external switchers
  sed config files,
  and a key rename or a split into multiple files breaks every switcher
  integration silently.
- One integrator-facing doc page: how a theme switcher targets view
  (colorscheme path for `auto`, `[ui.tokens]` path for explicit overrides,
  reload semantics).

**Exit gates:** the live re-derive is oracle-tested or demo-scripted with
captured output; the doc page exists; the key-path stability constraint is
recorded in the config validation layer or its tests.

---

## Sequencing

| Charter | After | Why there |
|---|---|---|
| C1 reattach | P5.5 remote Part B | extends the transport it needs; highest retention value for remote users |
| C3 theme interop | any time post-P4 exercise | verification-mostly; cheap, independent |
| C2 fleet attention | P5 AI + feasibility gate | the session model decides the mechanism; earlier planning would invent it |
