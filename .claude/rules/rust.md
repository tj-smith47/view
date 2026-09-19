---
paths: ["**/*.rs"] template-source: "rules/rust.md.tmpl"
---
# Rust conventions (edition 2021)

## Errors

- **No `unwrap()` or `expect()` in library code.** Use `?` with proper error
  types.
- **`thiserror` for module-level error enums** in `errors/`. Public functions
  return `Result<T, ModuleError>`.
- **`anyhow::Result` only at the CLI/`main.rs` boundary.** Library crates carry
  their own error types.
- **Panics are reserved for invariant violations** (impossible states). A
  user-input error returns an error.

## API design

- **`#[must_use]`** on functions whose return values must be checked (especially
  `Result` returners that build state).
- **`#[non_exhaustive]`** on public enums / structs that may grow.
- **Accept `&str` / `&[T]` / `impl AsRef<...>`**; minimize forced ownership at
  call sites.
- **Internal mutability via `Cell`/`RefCell`/`Mutex`** only when the API
  contract demands it; prefer immutable data + builders.

## Lints

- **`clippy::all` is the floor.** Fix, or silence with a rationale comment.
- **`clippy::pedantic`** allowed selectively per crate.
- **`#![deny(unsafe_code)]`** at the crate root unless the crate needs `unsafe`
  (FFI, concurrency primitives).

## Tests

- **`#[cfg(test)] mod tests` colocated** with the code under test.
- **Integration tests in `tests/`** for cross-crate behavior.
- **`cargo nextest run`** if available; it is faster than `cargo test`.

## Tooling floor

- `cargo fmt --check` clean.
- `cargo clippy --all-targets --all-features -- -D warnings` clean.

## view specifics

- Workspace lints already deny unwrap/expect/panic/todo/unimplemented/dbg. Test
  modules may open with `#![allow(clippy::unwrap_used, clippy::expect_used)]`.
  Lib code never does.
- Typed errors per crate via `thiserror`; the bin crate `view` renders them for
  the user.
- Only `view-engine` speaks RPC; only `view-tui` touches the terminal;
  `view-core` is pure (no I/O, no tokio). `scripts/audit-deps.sh` enforces this.
  Run `task audit`.
- **A `docs/*-wire-capture.md` fence that publishes a chunk verbatim carries the
  marker `verbatim `NAME_CHUNK`:` on the line above it.** `nvim_api.rs`'s
  `every_wire_capture_fence_matches_its_chunk_byte_for_byte` walks every capture
  doc, compares each marked fence to the const it names, and fails on a marker
  naming a const it does not carry. A new doc joins the pin by writing the
  marker.
- **A test that asserts a floor ("nothing arrives for at least N") times it from
  the instant the work was dispatched, and discards a sample its own thread
  slept through.** `recv_timeout(floor).is_err()` reads the wrong window: a
  loaded host can leave the test thread off-CPU for longer than the floor
  between the dispatch and the call, and the reply sent on time is already
  queued when the window finally opens, so the assertion reports a punctual
  reply as an instant one (gh-macos,
  `the_re_probe_waits_out_the_save_before_looking_again`). The shape that holds:
  take an `Instant` before dispatching, receive with the full host deadline,
  then assert `dispatched.elapsed() >= floor`, so a stall moves both ends
  together. That same stall lands inside the measured span, where it makes a
  floor that has gone *pass*, so sample the instant the wait was entered too and
  repeat the dispatch. Assert on the received value: `is_err()` alone names no
  message. `view-oracle/tests/timing_bounds.rs`'s
  `no_timing_test_bounds_a_measured_span_with_an_undeclared_absolute` reads a
  comparison from both sides, so a floor against an unscaled duration must earn
  a `DECLARED_ABSOLUTES` entry with grounds, and only when the span carries one
  of the names in that file's `MEASURED_SPANS`. That is the walk's stated limit:
  a span called anything else is invisible to it, and the fix is to name it out
  of that list or add its name.
- **A count asserted as a floor ("the harness completed at least N of
  something") counts only what happened inside a window whose length is fixed,
  and its bar is computed from that window and a stated per-cycle allowance.**
  `view-oracle`'s survival evidence read `report.folds`, a total spanning phases
  as long as the engine's own death and restart made them, against a bare `100`:
  a 3-core macOS runner folded 68 times over the 2s survival window, a cadence
  stretched by the `sleep(5ms)` between folds costing ~29ms there, against
  ~7.4ms on an idle 10-core macOS host and 5ms nominal, with the fold's own work
  8.6us of it, and failed a bar it had satisfied (gh-macos,
  `an_unattended_session_replaces_a_dead_engine_and_recovers_its_swap`). The
  shape that holds: `fold_for` returns the window's own count,
  `HangReport::survival_folds` carries it beside the run total, and
  `SURVIVAL_FOLD_FLOOR` is `SURVIVAL_WINDOW / OBSERVATION_SLACK`, the folds a
  thread still being scheduled fits into the window even when every cycle costs
  the whole observation headroom the module allows.
  `the_survival_bar_sits_between_a_stopped_observer_and_a_stretched_one` pins
  both sides, so a constant that drifts toward either one fails at compile time.
  The scope is a floor on the cycles a clock-bounded stretch produced. A
  workload size, how many samples a run is asked to take, is a different shape.
- **A test that shows a background walk was cancelled *mid-walk* holds the
  walker on a gate.** Sizing a fixture so the cancel "probably" lands in time is
  a race against the walker, and the walker wins it on a small loaded host whose
  metadata cache still holds the tree the test just wrote: the picker's close
  test walked all 20,000 entries in ~83 ms, less than the time a descheduled
  test thread took to flip the flag, and read a correct cancellation as a
  missing one (gh-macos, `closing_the_picker_cancels_its_files_scan_in_flight`,
  reproduced 8/30 on a loaded 10-core macOS host, 0/30 idle). The shape that
  holds: the walked function takes a `pace: impl Fn()` run ahead of every entry,
  immediately before its own `cancel` check. Production passes `|| {}`, which
  monomorphises away and leaves the per-entry cost the atomic it already paid,
  so no `#[cfg(test)]` branch lives in the walk. The test installs
  `view_test_support::ScanGate`, waits for the walker to park with entries still
  ahead of it, performs the cancelling act, releases, joins, and asserts
  `steps_after_release() == 0`, an exact count of entries taken after the
  release. The fixture then only has to hold more entries than the gate's park
  point, and disabling the `cancel` check makes every such test fail by name
  with the remaining entry count. `spawn_file_scan`, `spawn_live_grep_scan` and
  `tree::fs::scan_paced` are the members today; a new walk with a cancel flag
  joins by taking the same parameter. `ScanGate` lives in `view-test-support`.
  Nothing fails a *new* walker that ships a sized-fixture test, so this entry is
  the check.
- **A test that needs one event to land before another orders them by
  construction: the write made inline before the call that must observe it, or a
  hook the earlier event fires from.** Two threads asleep for 60ms and 500ms
  wake in either order on a loaded host. gh-macos parked a rewrite thread past
  the main thread's whole grace sleep, and
  `a_save_that_unlinks_before_rewriting_never_says_anything` announced a removal
  whose rewrite was still pending (run 33624302587). The grace's floor on the
  constants (`the_grace_outlasts_the_absence_a_save_can_leave`) and the arm
  spending it measured from dispatch
  (`the_re_probe_waits_out_the_save_before_looking_again`) already pin the
  timing, so the row's job is the ordering: write the file, then look. Where the
  earlier event must land *mid-retry*, the retried function takes a hook run at
  the point the test needs (`spawn_past_busy_text`'s `refused`, production
  `|| {}`, the same shape as the walks' `pace`), and the test asserts the hook's
  count so the retry being exercised is a fact. A sleep that only bounds a wait
  (a watchdog, a `host_deadline` ceiling) is a different shape, since a stall
  moves both of its ends together. The shape is any assertion that reads the
  *relative order* of two independently sleeping threads. Nothing fails a new
  one mechanically, so this entry is the check.
- **Every line of a multi-line string literal under `crates/view-engine/` wraps
  at 80 characters**, along with every `const ..._CHUNK: &str` in `nvim_api.rs`.
  rustfmt holds the line a literal opens on and nothing else about it, so
  nothing in the toolchain catches a line past that width, and nothing checks
  what the literal holds: a Lua chunk broken with a trailing `\`, a fixture row
  and an assert message are one shape, and a rule that tried to tell them apart
  read two live Lua violations as prose. `scripts/check-style.sh` fails a longer
  line, and fails when the number of literals and chunks its walks reach stops
  matching what a plain grep for the same declarations counts. No number is
  written down on either side, so a new literal costs nothing and a shape that
  drifts out of a walk still parts the two counts. Both run in `task ci`.
- **A rendering decision reads the probed capability bit that answers it, and
  every fixture that builds a `TermCaps` states the bit its test is about.**
  `caps.tier` is a summary of three probe answers (`sync`, `truecolor`,
  `kitty_kbd`) and derives from none of the others, so keying a fourth decision
  on it is an inference: the border charset keyed on the tier drew ASCII at a
  16-color terminal that renders `╭` perfectly, and rounded corners at a
  truecolor one that cannot (`BorderSet::for_caps` now reads
  `caps.unicode_boxes` alone). Each row of `view_tui::tiers::REGISTER` names the
  consumer that reads it: `sync` gates BSU/ESU, `unicode_boxes` gates the
  charset, `kitty_kbd` gates the keyboard push. A decision with no row of its
  own has no capability to key on. Two consequences a new bit inherits. Anything
  that caches or clips a painted frame compares the whole `TermCaps`
  (`cache::Inputs`, `Term::adopt_caps`), because a probe reply landing after the
  first paint flips a bit without moving the tier and a frame keyed on the tier
  is then reused in a charset the session no longer draws in; that generalises
  past `TermCaps` to every state struct a painter reads, per the entry below.
  And a fixture spells the bit it means (`DRAWS_BOX_GLYPHS` / `NO_BOX_GLYPHS` in
  `paint.rs` and `native_overlay_goldens.rs`), which is how 14 paint fixtures
  came to assert a charset none of them had chosen while they inherited
  `from_probe`'s all-false floor.
  `the_box_glyph_bit_alone_maps_to_a_border_charset` walks every tier against
  both answers and `a_terminals_tier_never_reaches_its_frame` holds both
  crossings to the committed goldens, so re-pointing the charset at the tier
  fails by name. Nothing mechanically fails a *new* decision that keys on the
  tier, so this entry and the register's consumer column are the check.
- **A test never writes the program it runs.** A sibling test's `fork` landing
  inside the write's open-descriptor window inherits the writable descriptor
  into its child, and Linux refuses the exec with `ETXTBSY` (`#!` scripts
  included; macOS does not, and the gh-macos leg never saw it). Measured on
  dev-linux, the window is 5-12 us wide and the odds run with the test binary's
  own fork rate: at view-native's 61 forks/s that is p ~ 3e-4 per suite run, so
  it fails once in thousands of runs and never on demand, and it surfaces as
  whatever the spawn's caller degrades a failed spawn to. `tree::git`'s
  wedged-deadline test wrote its own fake `git` and failed on
  `assert!(timed_out)` naming nothing, because `run_git_status` dropped the
  `io::Error`. Both halves are the rule: programs a test runs are committed
  fixtures under `scripts/test-fixtures/` (`fake-git-wedged`, the `fake-ssh*`
  family, `remote-probe`, `delay-relay`), and a spawn in lib code carries its
  cause out (`std::io::Result`, degraded to the module's empty answer at the
  public boundary) so a failure can be asked what happened.
  `scripts/check-style.sh`'s `check_written_programs` pins the whole population
  of sources that write a `0o755` mode, per file and with grounds, and
  `scripts/check-style-cases.sh` grades that walk. It is keyed on the mode
  literal, since the site this pin exists for spelled it
  `set_mode(&mut perms, 0o755)` and a call-anchored pattern reads straight past
  that. A new site trips `task ci` and must either become a fixture or write its
  mitigation into the pin.
- **A frame cache holds the whole struct a painter reads, and every model field
  is either captured or classified where the cache declares its inputs.** A
  projection silently drops the next field added behind it: `cache::Inputs` held
  `messages.entries` and so compared no pause flag, and the ⏸ mark never reached
  the terminal because the unmarked frame was reused. That is the same shape as
  `absorbed` one task earlier, and as the `TermCaps` tier one before that. Both
  halves are mechanical now: `Inputs` holds `Messages`, `TermCaps` and
  `AbsorbedRows` whole, so a field added to any of them joins the comparison for
  free, and `every_model_field_is_a_paint_input_or_named_here`
  (`view-surface/src/cache.rs`) walks every field `Model` and `EngineModel`
  declare and fails unless it is either captured by `Inputs` or named in the
  classification above `Inputs`. Three groups there, and a field in none of them
  has no third answer. `assert_equivalent` needs a debug build and a test that
  drives the field through the reuse path, and an exhaustive destructure is
  unavailable on the `#[non_exhaustive]` `Model` and `EngineModel`.
- **A hermetic plan names every path root a child resolves; it never leaves one
  to be *derived* from a variable it moved.** Redirecting `HOME` moves all four
  `stdpath()` roots on Unix, because each `XDG_*_HOME` defaults under it, and
  moves none of them on Windows, where `stdpath('state')`/`stdpath('data')` come
  from `%LOCALAPPDATA%` and `stdpath('cache')` from `%TEMP%`, both hermetic
  passthrough, with `-data` appended to the appname for state and data
  (`get_xdg_home`, the engine's `os/stdpaths.c`). Every "isolated" child on one
  Windows account therefore shared the operator's own configuration and state
  trees, and concurrent children raced each other's `mkdir` of the swap
  directory below the shared state root, which nvim reports as `E303` and then
  refuses the buffer: red only under concurrency, on one platform, with the
  panic naming a directory no plan of this tree had chosen (gh-windows, run
  33634619480). `view_engine::env::HERMETIC_STDPATH_VARS` names all four with
  their positions under `hermetic_home()`; `EngineConfig::env_plan` and
  `pty::make_hermetic` apply them as defaults a caller's own entry outranks,
  because delivering a fixture config through `XDG_CONFIG_HOME` to an otherwise
  isolated child is how the matrix measures a configuration at all.
  `env_isolation.rs`'s
  `an_isolated_childs_standard_paths_all_resolve_under_the_hermetic_directories`
  reads the accepted roots off the pinned engine's own `doc/vimfn.txt` and fails
  unless each sits in exactly one of that file's three groups (under the home,
  under the empty search path, left to the host), so a root an engine-pin bump
  adds trips a test before it reaches children unclassified. The same test
  compares `stdpath('state')` exactly against `engine_state_dir_name()`, since
  the swap directory `prepare_hermetic_home` pre-creates has to be the one the
  child mkdirs, and a stale shared home makes the `is_dir` half of that pass on
  its own.
- **A production site that names a `(width, height)` to the engine spends
  `view_core::model::grid_target_for`'s answer, and every such site is pinned
  per file with the ground its pair came from.** An attach is refused outright
  below `view_core::model::ENGINE_MIN_SIZE`, and a spawn seeded at a size its
  attach does not repeat relayouts every window on screen, so the raw reading,
  the pair a caller has in hand, must not reach either call. Nothing at a call
  site says which of the two it holds (`startup.rs` legitimately spends a
  `width, height` bound off a channel, so an identifier walk either accepts the
  reading everywhere or rejects the one correct site). `check_geometry_sites` in
  `scripts/check-style.sh` is the pin, the same shape `check_tied_spawns` uses
  for spawns: every spelling a geometry can reach the engine by, counted per
  production file against a grounds row. The attach as a method, as an effect
  variant and as the wire method name (`ui_attach`, `UiAttach`,
  `nvim_ui_attach`); the resize the same three ways (`try_resize(`, `TryResize`,
  `nvim_ui_try_resize`); the bare `attach(` that both public attach methods
  funnel through, bounded on the left so the `ui_attach` spelling does not
  answer for the private `fn attach` where the pair actually reaches the wire;
  the bare `late_attach`, which is how a geometry reaches the engine without
  going over the wire at all (`with_late_attach` stores the pair and
  `late_attach_cmd` renders it into the child's `--cmd` argument, the spawn seed
  the rationale above turns on); and the attach guard's `release(` hand-off in
  the four crates that own an engine attach, since elsewhere that word reaches a
  lock or a scan gate. It reads the god-file scanner's own `--prod-lines`
  classifier, so a `//` inside a string and an item closed by `} // end` cannot
  hide a site the way they did while this walk was a Rust test with a classifier
  of its own. A new site fails `task ci` by name until its row exists, and
  `scripts/check-style-cases.sh` grades the walk. Keying on spellings is the
  stated limit: a geometry that reaches the engine under a ninth name is
  invisible, and the first version of this pin, keyed on `try_resize(` alone,
  counted none of the three folds that build `RpcCall::TryResize` from
  `Model::grid_target`. The walk is fail-closed the other way, on a spelling
  named only by a trailing comment: `--prod-lines` emits the raw line, and
  `.claude/rules/shell.md` says why comments are left in.
- **A child that can outlive the process that spawned it goes out through
  `view_proc::spawn_tied_to_this_process`, and every other spawn writes down in
  one row why it cannot.** `Drop` covers the exits a process chooses and none of
  the ones it does not: a `SIGKILL`, a harness timeout or a restarted session
  runs no destructor, and a child wedged past noticing its closed pipes never
  leaves on its own. Three of them were found reparented to init at 100% CPU for
  days: two wedged `nvim --embed` children of a test binary that was gone, and
  one `view` whose driver had been restarted. On Linux the tie is
  `PR_SET_PDEATHSIG`, armed in the child between `fork` and `exec` with a
  `getppid` re-check closing the race, and a `prctl` the host refuses leaves the
  child unarmed. macOS and Windows have nothing armed and say so (`view-proc`'s
  own doc carries the platform table). The signal names the forking **thread**,
  so the primitive is a *spawn* and not an arming step a caller applies to its
  own `Command`: armed from view's short-lived attach thread, every engine died
  the instant that thread returned and the restart painted `E305` off the dead
  child's swap (`an_engine_spawned_from_a_thread_outlives_that_thread` is the
  pin for that alone). Some spawns owe no tie: a child waited on to completion
  inside the call that made it, one bounded by its own deadline and killed on
  it, or a pty child spawned as a session leader that the kernel ends when the
  master closes. The answer is written once by whoever adds the spawn.
  `check_tied_spawns` in `scripts/check-style.sh` pins
  `Command::new`/`CommandBuilder::new` **and** the calls that start one
  (`.spawn(`, `.spawn_command(`) per production file with grounds, graded by
  `scripts/check-style-cases.sh`, so a new spawn site fails `task ci` by name
  until its row exists. The call spellings are what reach a helper handed an
  already-built `Command`, at the price of pinning thread and task spawns, whose
  rows say they are threads. The crate that spawns declares `view-proc` and
  names it (`view_proc::spawn_tied_to_this_process`). There is no re-export
  chain to reach it through. A reaping pin adopts the pid its own child reports
  (`orphan_reaping.rs`'s stderr marker), and never one recognised by `comm` off
  the process table: the first version of the bench pin adopted an
  `nvim --version` capability probe and passed identically against a plain
  spawn.
- **A module-scope item whose only use sits inside a
  `#[cfg(target_os = "linux")]` block is dead code on the other two legs, and
  nothing on a Linux host says so.** `task lint` denies `dead_code` and
  `unused_imports`, but it lints the host's own source selection: a `const`, a
  `use` or a helper `fn` left outside a fence its only reader is inside compiles
  clean here and fails `clippy -D warnings` on the macOS and Windows legs, after
  a push, which is how five such items reached one branch at once, and how
  `engine_child_of` came to be called from unfenced code in
  `terminal_hangup.rs`. `task lint:cross` is the gate: `--all-features` clippy
  over `x86_64-unknown-freebsd`, which is unix-and-not-Linux, so it selects the
  same source the macOS leg does for every fence in the tree, and unlike darwin
  and windows-msvc it cross-compiles with no foreign C toolchain (criterion's
  `alloca` and ureq's `ring` both build for it, and both refuse the other two).
  It runs inside `task ci`, gated `platforms: [linux]` because on the other
  hosts it is no cross check at all. The shape that holds: the fenced block owns
  its own imports and consts (`use std::io::BufRead;` inside the block,
  `#[cfg(target_os = "linux")] const REAPED`), and a helper called from unfenced
  code gets a `#[cfg(not(target_os = "linux"))]` twin returning the empty
  answer.
- **A paired measurement's pty answers what a real terminal answers, for both
  arms through one policy, and a query that resolves no capability is answered
  under every answering policy.** A terminal question left unanswered costs
  time: the child waits out whatever deadline it was given and the wait lands
  inside the figure. The pinned engine's tty startup writes an OSC 11 background
  query with a DSR behind it and blocks in `vim.wait(100, ...)` until the DSR is
  answered (`runtime/lua/vim/_core/defaults.lua`), so an unanswering harness pty
  put a fixed ~100 ms into the bare-nvim arm of every cold cell and none into
  view's `--embed` arm; every recorded `first_paint.marker_ratio_*` was
  withdrawn for it. A ConPTY master is the harder case: it releases nothing a
  child writes until a bare `\x1b[6n` is answered, measured against
  `cmd.exe /c echo hi` as well as the engine
  (`docs/conpty-harness-wire-capture.md`), so there the unanswered query costs
  the whole stream. The members of `view_oracle::pty`'s table today are DA1,
  DECRQM 2026, the kitty keyboard query, the SGR readback, the box-glyph probe,
  OSC 11, DSR and a bare cursor report. The last three resolve no capability
  tier and therefore sit in `BASE_ANSWERS` as well as `FULL_TIER`, so the policy
  a session picked for its *tier* never decides whether its child *waits*. Where
  one query is nested inside a longer one (the cursor report inside the
  box-glyph probe), longest-match wins and a chunk boundary cutting the longer
  query holds the bytes, since answering the shorter one spends the probe's own
  position-dependent answer on a capability the child never asked about.
  `every_answering_policy_answers_the_queries_no_tier_depends_on` and
  `a_glyph_probe_cut_anywhere_still_gets_its_own_answer` (both in `pty.rs`) fail
  a policy or an ordering that regresses, and `nvim_arm_startup.rs`'s tool-free
  contrast leg fails on every host if the responder stops answering at all. The
  stated limit: nothing mechanical stops a baseline being re-seated from a draw
  taken before such a fix, so a re-seat cites the run its numbers came from and
  a draw from before the fix is inadmissible.
- **`-n` buys an embedded child nothing until a UI attaches, so no live-nvim
  test's isolation may rest on it.** `-n` only sets `'updatecount'` to 0, and it
  does that at `main.c`'s `if (params.no_swap_file)`, which runs *after*
  `remote_ui_wait_for_attach()`, where an `--embed` child parks servicing RPC. A
  test that spawns `EngineConfig::isolated()` and never attaches a UI runs every
  command it sends before that line, with `updatecount` still 200 and
  `'swapfile'` on; nvim 0.12's `ml_open_file` consults neither, since
  `b_may_swap` was decided in `ml_open` from the `p_uc` of that moment. Measured
  on the pinned engine: `nvim --embed --clean -n` answers `&updatecount` 200 and
  writes a real swapfile on `:edit`, while `nvim --embed --headless --clean -n`
  answers 0 and reports "No swap file". What keeps concurrent isolated spawns
  off each other is `prepare_hermetic_home` owning the swap directory before any
  child asks for it. A comment claiming `-n` suppresses the swapfile is wrong
  wherever no UI attaches.
- **A figure in a doc comment states a measurement nobody re-takes, so the
  mechanism goes in words and the reading stays in the commit that took it.**
  `check_doc_figures` in `scripts/check-style.sh` reads every `///` and `//!`
  line under `crates/` and fails one that carries a millisecond, microsecond,
  nanosecond, second, percentage or multiplier figure.

  What counts as a figure has two forms. A decimal carrying one of those units
  is a reading wherever it stands, since nothing in this tree passes 0.37 us to
  anything. An integer carrying one is a reading only inside the sentence a
  reading word opened, because an integer with a unit is far more often a
  constant the code passes (a 150 ms throttle, a 20 ms cadence) and sweeping
  those deletes the WHY the comment is there for.

  The reading words are the stems the shipped regex spells, read in any case:
  `measure`, `observ`, `record`, `spen[dt]`, `cost`, `took`, `tak(e[sn]|ing)`
  (`takes`/`taken`/`taking`), `walk(ed|s)`, `clocked`, `timed`, `appear`,
  `pays`, `paid`, `land(s|ed)`, `need(ed|s)`, `runs`, `ran`, and the noun
  `reading`. Every stem but `ran` is read as a bare substring (`runs` inside
  `reruns`, `lands` inside `islands`, `taking` inside `undertaking`): a
  substring false positive costs a reword, and a stem narrowed to dodge one
  risks a reading the check reads past. Six of them were added after readings
  shipped behind the gap (`pays ~13s`, `lands ~250us later`, a `~2 ms` figure
  mixed into `~50 ms` ones, and `needed`/`needs`, `taking`, `walks`, `paid` and
  `landed` reaching no case of their own), so a verb a reading can be written
  with belongs on that list before the reading does. `ran` is the one read with
  a non-letter on each side, because `range`, `transient` and `guarantee` all
  carry it and every one of them would otherwise open a sentence the walk then
  grades.

  A figure carries its sign and a spread is written as a range, so `+1.23%` is
  one figure and `0.62ms..92.5ms`, `1..=5 ms`, `8-10ms` and `+/-20%` are each
  two: the four separators become blanks before the line is read, except the
  hyphen, which becomes the sign of the figure after it. The inclusive range
  takes its `=` with it, because blanking the two dots alone left `=5 ms`, which
  no token shape reads. A token shape reading neither a sign nor a range passed
  ten measured figures on the shipped tree. A finding prints the token as the
  file spells it (`8-10ms`, and never the `-10` the range was cut into), so the
  reader lands on what the walk read.

  Three shapes take a line out of the grading: a word that makes the number
  something other than a reading, a cell id the drift check knows, and a fenced
  block, which is a sample of what something prints and which rewording would
  destroy. The first of those is `bar`, `budget`, `bound`, `band`, `tolerance`,
  `deadline`, `throttle`, `debounce`, `ceiling`, `cap`, `tier`, `pace` and
  `derive`, each with `s`, `d`, `ed` or `ped` allowed after it: `(s|d)?` gave
  `cap`, `caps` and `capd`, so `capped`, the spelling the rule named, was
  refused along with `tiered` and `bounded`. Twelve of the thirteen say the tree
  chose the number and can be read back off the constant holding it. `derive`
  says the code computes it from one the tree chose, which is what none of the
  others could say: `2 x` trials and the 31s five doubled waits come to are
  neither readings nor values written down. Each of the three exempts the
  figures on its own line and never the sentence the words on that line opened:
  skipping the line outright left a reading word beside a bound to open no
  sentence at all, and the figure rustfmt wrapped below it went ungraded.

  The cell ids are the drift check's own vocabulary, read at each run through
  `scripts/check-budget-drift.sh --cell-ids`, and a run that reads none of them
  fails closed: an empty vocabulary grades every figure as anchored, which reads
  exactly like a tree with nothing to report. A reading therefore goes to prose,
  or to `docs/benchmarking.md` beside the cell that records it, where the drift
  check grades it against that cell.

  Two limits are stated. The sentence bounding a figure ends at the first `.`
  before a blank or a line end on either side of it, and the word that grades
  the figure is as free to fall after it as before: rustfmt breaks a line
  wherever the width runs out, and a state that only ran forward graded
  `moved 6x cross-boot` as a constant for want of a `measured` that sat on the
  next line. So an integer two sentences from the word that took it, in either
  direction, is graded as a constant; and a figure written with no unit at all
  is graded by nothing here.

  Cased in `scripts/check-style-cases.sh` over thirty-four shapes: the tree that
  passes on all five escapes, the decimal, the integer inside a reading
  sentence, the same integer wrapped onto the line below the word that
  introduced it and the figure standing on the line above the word instead, the
  constant in the sentence after one a reading closed and the constant in the
  sentence before one a reading opens, the reading on a line naming a cell, the
  budgets file that declares no cell, one red per range and sign spelling, a
  reading word beside a bound and beside a cell id whose figure is wrapped onto
  the line below, the finding that names a range as the file spells it, one red
  per whole-unit verb the vocabulary reaches (`spends`, `walked`, `walks`,
  `pays`, `paid`, `lands`, `landed`, `needed`, `needs`, `runs`, `taking`,
  `ran`), the constant whose only reading verb is a word that contains one, and
  one green per escape the figure rests on (`throttle`, `tier`, `capped`,
  `derived`).

  A green case that would be green without the rule it names grades nothing: the
  two that first cased the escape carried no reading verb at all, so they passed
  under a checker with no escape list. Each case here reddens when the one rule
  it is about is reverted on a scratch copy of the checker, and that is what a
  new case owes before it ships.
