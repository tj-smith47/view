# Open questions — migration integrity planning

Only genuine forks: two architecturally sound designs where the choice needs
the user's authority. Design decisions that the request, the code, or a
sensible default settles are made in `plan-draft.md` and are not listed here.

---

## Q1. `<leader>fp` claims a key from a switching user's namespace

**The fork.** C4's pause control needs a global key, and view's key model is
"a real nvim mapping, registered after your config, reported when it
displaces yours" (§5.3, `mappings::DEFAULT_MAPS`). Two defensible shapes:

- **A default map (`<leader>fp`), like every other native feature.**
  Consistent with the six keys view already ships (`DEFAULT_MAPS:
  [MappingSpec; 6]` — `<leader>ff`, `fb`, `fg`, `e`, `fm`, `<leader>ai`),
  discoverable in `docs/keymaps.md` and which-key, reversible with the
  documented `[native] notifications = false` line. Cost: a seventh key,
  and a fifth on the `<leader>f` prefix that telescope users already
  populate — this is the only new *global* key in the whole phase.
- **Overlay-local only: pause lives inside the history overlay
  (`p`), with no global map.** Takes nothing, and the history overlay is
  already where a user goes to re-read a notice. Cost: it does not solve
  the stated problem — a toast vanishing *mid-read* needs a key that works
  while the toast is on screen, not one that works after opening a modal.

`plan-draft.md` Task 17 builds the first (it is the one that satisfies the
user's stated want) and this question is raised only because the key itself
is the user's namespace, not the assistant's: `<leader>fp` is a concrete
claim on their muscle memory and a different letter costs nothing to pick
now and a deprecation later.

**Recommendation:** ship `<leader>fp`. Overrule with a preferred key if
`<leader>f` is already crowded in the user's own config.

**Not a blocker.** This is an FYI with a recommendation, not a fork that
holds execution: the repo's own conventions decide it (view already claims
four `<leader>f` keys — `ff`, `fb`, `fg`, `fm`, the last for this very
surface — `fp` is unclaimed, and §5.3's displacement reporting is the
designed mitigation for a crowded prefix). Task 17 proceeds on the
recommendation unless the user says otherwise.

---

Nothing else reached this file. In particular, these were decided rather
than asked, with the reasoning recorded in `plan-draft.md`:

- C2's charter fork (detect-and-disable vs ext-set opt-out) — settled
  against detect-and-disable by noice's own source: `setup()` runs its ext
  check *before* it parses any user option (in the pinned `lua/noice/init.lua`), and
  that check's ext loop is unconditional, so no options-level disable can
  suppress a first launch's errors — only reaching into plugin privates
  could. Deviation 2 in the plan carries the evidence, including the part
  the charter states imprecisely (the 1 s re-check *is* gated, by
  `health.checker`, default on).
- What a default first launch shows when the user does *not* opt out —
  decided, not asked: view keeps its surfaces (they are the product) and
  owns the explanation, emitting one notice with the exact `[native]` line
  and holding the plugin's own startup errors in the history, which the
  notice itself points at. Task 19 in the plan builds it.
- Where the ext opt-out lives — no new config key: the `[native]` switches
  already exist and §5.5/§9 already promise they return the surface.
- Sticky notices and the slot queue — sticky entries stay out of the
  expiry queue, or one parked error freezes every transient behind it.
- `ClipboardWrite.token` becoming `Option` rather than a second copy
  effect — one copy path keeps one notice and one OSC 52 companion.
- `ext_tabline` staying unconditional — no native feature owns the
  tabline, so there is no switch to follow; recorded honestly as a `none`
  row in the surface matrix rather than invented.
