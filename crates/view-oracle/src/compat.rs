//! The compat harness's pty-level driver: domain types for a scenario's
//! steps ([`Step`], [`PluginClass`], [`ScenarioState`]) plus [`CompatSession`],
//! which drives them against a real `view` process over a pty (via
//! [`PtySession`]) and answers `probe`/zero-error queries over a *second*,
//! independent channel.
//!
//! Serde-free by design, matching this crate's own `rmpv`-free rule (see the
//! crate root's module docs): `view-harness`'s `scenario` module owns the
//! TOML schema and hands this module already-validated [`Step`] values, the
//! same layering `corpus::CorpusEntry` uses for the differential oracle.
//!
//! # The probe channel
//!
//! A pty's rendered screen (what [`PtySession::screen`] exposes) is not a
//! channel a `probe` step or the implicit zero-error epilogue can query: it
//! is a human-shaped rendering, not a value. Scraping it with `:echo
//! luaeval(...)` and pattern-matching the redrawn text was considered and
//! rejected -- it collides with whatever plugin UI a scenario is actually
//! testing, and breaks on overlays, truncation, and timing, which is
//! exactly the fragility this harness exists to catch, not to inherit.
//!
//! Instead, each compat fixture's `init.lua` calls
//! `vim.fn.serverstart(vim.env.VIEW_COMPAT_SOCK)`, opening a second RPC
//! channel nvim itself exposes; [`CompatSession::probe`] and
//! [`CompatSession::zero_error_check`] act as the *client* of that channel
//! by shelling out to the pinned `nvim` binary itself: `nvim --server $SOCK
//! --remote-expr '<expr>'`. This is zero new `view` surface and zero new RPC
//! code -- nvim is both the server (already required) and the client
//! (already pinned) -- at the cost of one subprocess per probe. A
//! fixture-less scenario (no committed `init.lua` to carry the
//! `serverstart` call) has [`CompatSession::prime_probe_channel`] type the
//! equivalent command into the pty itself instead.
//!
//! Every probe subprocess is spawned with an explicit bounded wait
//! ([`wait_with_timeout`]), not `Child::wait`'s unbounded block: nvim
//! defers non-fast requests while blocked in a key-wait (the embed-channel
//! `nvim_eval` wedge `corpus/fuzz-42-6.toml` once reproduced, fixed in the
//! corpus stack by [`crate::snapshot`]'s blocked-state dismissal), and this
//! channel takes a different RPC path but the same underlying nvim process
//! can still leave an `--remote-expr` client's request unanswered while it
//! is showing a blocking prompt -- this channel existing at all does not
//! itself immunize a probe call from that, so it must fail loud on its own
//! deadline rather than hang the whole compat run.
//!
//! The probe subprocess is also the one `nvim` this tree starts through
//! neither hermetic spawn funnel ([`PtySession::spawn_configured`] and
//! `EngineConfig::isolated`, which between them clear the host's editor
//! configuration from every editor *session* the harness runs). A
//! `--remote-expr` client is not such a session: it hands an expression to
//! an already-running server and exits without performing an editor's
//! startup initialization, so the host's startup commands never execute in
//! it and there is nothing for a funnel to neutralize. Confirmed against the
//! pinned binary and pinned by
//! `the_probe_client_runs_none_of_the_hosts_startup_commands` below, since
//! that is a claim about the binary's behavior rather than about this code.

use std::io::Read;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use portable_pty::CommandBuilder;

use crate::pty::PtySession;
use crate::OracleError;

pub use view_engine::env::reset_hermetic_home;

/// Bound on how long a single probe subprocess (`nvim --server ... --remote-expr`)
/// is allowed to run before [`wait_with_timeout`] kills it. Generous
/// relative to a normal probe's near-instant reply (confirmed live: a clean
/// `nvim --server $SOCK --remote-expr` round-trip completes in well under a
/// second), short enough that a wedged target still fails a scenario
/// promptly instead of hanging the whole compat run.
const PROBE_TIMEOUT: Duration = Duration::from_secs(10);

/// Silence window a [`Step::Send`] with no [`SendConfirm`] requires the
/// pty's rendered screen to hold, *after* it has first been observed to
/// differ from its pre-send baseline, before treating the keys as
/// delivered. Matches the differential oracle's own quiesce convention
/// (`corpus::DEFAULT_QUIESCE_SILENCE_MS`): long enough that ordinary
/// key-processing latency never trips it, short enough that a step's own
/// deadline (below) is reached in a handful of polls once the screen has
/// genuinely gone quiet.
const SEND_SETTLE_SILENCE: Duration = Duration::from_millis(200);

/// Bound on how long an unconfirmed [`Step::Send`] waits for a
/// delivered-and-settled screen before giving up on confirming delivery and
/// letting the step return anyway (see its own doc comment for why giving
/// up is safe). A plugin that keeps a floating notification animating
/// indefinitely (fidget's progress
/// spinner, live-confirmed) never holds the whole screen still for
/// [`SEND_SETTLE_SILENCE`] once it has changed, so this bounds the wasted
/// wait at a small multiple of the forwarding latency this step actually
/// needs to outrun -- live-confirmed at zero failures across 200 bounded
/// repetitions of the scenario that first exposed the race, both idle and
/// under host load.
const SEND_SETTLE_DEADLINE: Duration = Duration::from_millis(1_000);

/// Bound on how long a [`Step::Send`] with a [`SendConfirm`] retries its
/// probe before failing with [`CompatError::SendConfirmTimedOut`]. A real
/// confirmation typically lands within one or two probe round-trips (the
/// effect a `:silent` ex command produces is applied synchronously, well
/// before the command line that triggered it returns control to nvim's
/// main loop), so this bounds the pathological case -- a genuinely stalled
/// or wrong expression -- without padding the ordinary one; matches
/// [`SEND_SETTLE_DEADLINE`] so a confirmed send never waits longer than an
/// unconfirmed one would have.
const SEND_CONFIRM_DEADLINE: Duration = Duration::from_millis(1_000);

/// Interval between retries of a [`SendConfirm`] probe. Matches
/// [`Step::WaitForProbe`]'s own retry cadence: each attempt is a real probe
/// subprocess (fork/exec/connect/eval), so spacing retries avoids hammering
/// the target with spawns for a value that has not had time to change yet.
const SEND_CONFIRM_POLL: Duration = Duration::from_millis(200);

/// Which of the design spec's three config-reconciliation classes a
/// scenario's plugin belongs to. Recorded per scenario since the
/// compat-evidence page's own row schema reports it, and a future coverage
/// model (top-N by class) groups by it; [`Self::UiOwning`] additionally
/// drives the loader's own three-state completeness check (see
/// [`ScenarioState`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PluginClass {
    /// No UI ownership (treesitter, LSP servers, cmp sources, surround,
    /// gitsigns data): by construction, compat by definition.
    Semantic,
    /// Draws inside the grid but owns no surface `view` itself renders
    /// natively (telescope, which-key, floating plugins): must coexist
    /// untouched.
    UiAdjacent,
    /// Occupies a surface `view` renders natively (lualine/statusline,
    /// noice, nvim-notify, tree sidebars): native wins by default, per
    /// the design spec's supersession policy.
    UiOwning,
}

/// One state named by a scenario's `[[states]]` entry (the design spec's
/// config-reconciliation states). A semantic/ui-adjacent scenario, whose
/// plugin coexists untouched
/// regardless of `[native]`, has exactly one entry ([`Self::Present`]); a
/// ui-owning scenario's plugin occupies a surface the engine can supersede,
/// so its coverage is only complete once all three reconciliation states are
/// asserted -- [`view_harness::scenario`]'s loader enforces that completeness
/// at load time, not here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScenarioState {
    /// The plugin is present and loaded; no supersession assertion is made
    /// (a semantic/ui-adjacent scenario's sole state).
    Present,
    /// `[native]` leaves the feature on (its default): the engine takes the
    /// surface over and the plugin's own rendering of it must not appear.
    Superseded,
    /// `[native]` turns the corresponding feature off with the plugin still
    /// present: the plugin's own rendering returns.
    Deferred,
    /// No plugin loaded at all (a fixture with none installed): proves the
    /// engine's native surface stands on its own, not merely "not
    /// overridden by a plugin that happens to also be absent".
    NativeOnly,
}

/// The set of TOML state names the loader recognizes, paired with
/// [`state_name`] so a scenario file's `states[].name` and the loader's own
/// error/report text never drift into two independently-maintained spellings
/// of the same four states.
#[must_use]
pub fn parse_state_name(s: &str) -> Option<ScenarioState> {
    match s {
        "present" => Some(ScenarioState::Present),
        "superseded" => Some(ScenarioState::Superseded),
        "deferred" => Some(ScenarioState::Deferred),
        "native-only" => Some(ScenarioState::NativeOnly),
        _ => None,
    }
}

/// The TOML spelling a [`ScenarioState`] round-trips through -- the inverse
/// of [`parse_state_name`].
#[must_use]
pub fn state_name(state: ScenarioState) -> &'static str {
    match state {
        ScenarioState::Present => "present",
        ScenarioState::Superseded => "superseded",
        ScenarioState::Deferred => "deferred",
        ScenarioState::NativeOnly => "native-only",
    }
}

/// One scripted action a scenario drives, in order. Each variant maps to
/// exactly one primitive on [`CompatSession`] or the underlying
/// [`PtySession`] -- there is no control flow (loop, conditional) a step can
/// express, by design: a fixed, introspectable record is reviewable and
/// diffable in a way a bespoke scripting language is not, and an imperative
/// Lua mini-language for scenario steps was considered and rejected for
/// exactly that reason.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Step {
    /// Types `keys` into the pty verbatim, as a user would, then confirms
    /// delivery before returning. With `confirm: None`, that means waiting
    /// (bounded) for the screen to actually show the keys' effect and
    /// settle, closing the ordinary-case gap between typing and forwarding
    /// without demanding a guarantee an animating target could never give.
    /// With `confirm: Some(_)`, the keys are known ahead of time to produce
    /// no screen delta (a `:silent` ex command is the idiom this exists
    /// for), so screen movement can never confirm them; [`SendConfirm`]'s
    /// expression is retried over the probe channel instead, the same
    /// positively-confirmed (never a fixed sleep) standard applied to a
    /// signal that can actually observe a silent effect.
    Send {
        keys: String,
        confirm: Option<SendConfirm>,
    },
    /// Blocks until the pty's rendered screen contains `needle`, or `timeout`
    /// elapses.
    WaitFor { needle: String, timeout: Duration },
    /// Blocks until the single cell at `(row, col)` holds exactly `expected`,
    /// or `timeout` elapses.
    WaitForCell {
        row: u16,
        col: u16,
        expected: String,
        timeout: Duration,
    },
    /// Fails the scenario if `needle` is present anywhere in the current
    /// screen content -- the inverse of `wait_for`, for asserting an error
    /// marker never appeared.
    AssertAbsent(String),
    /// Fails the scenario if the single cell at `(row, col)` holds exactly
    /// `glyph` right now -- a one-shot check, the discriminating negative
    /// [`Step::WaitForCell`] cannot express on its own: a superseded state's
    /// only proof that the plugin's own rendering lost the surface is a cell
    /// the plugin would have painted staying absent, not a cell the engine
    /// paints appearing (both states can share the latter).
    AssertCellNot { row: u16, col: u16, glyph: String },
    /// Evaluates `expr` over the probe channel once and fails the scenario
    /// unless the (trimmed) result equals `expect` exactly. A single check,
    /// not a wait: use [`Step::WaitForProbe`] for an expression whose value
    /// only becomes true asynchronously (a plugin still loading, an install
    /// still running).
    Probe { expr: String, expect: String },
    /// Retries `expr` over the probe channel until its (trimmed) result
    /// equals `expect`, or `timeout` elapses. Discovered live, not
    /// speculative: a bare [`Step::Probe`] taken immediately after sending
    /// keys raced a still-installing plugin (lazy.nvim's own install
    /// window still had input focus) and failed a scenario that was
    /// otherwise correct -- the async-load case a one-shot probe cannot
    /// express on its own.
    WaitForProbe {
        expr: String,
        expect: String,
        timeout: Duration,
    },
}

/// A [`Step::Send`]'s opt-in delivery confirmation for keys known to produce
/// no screen delta. `expr` is retried over the probe channel (the same
/// content-aware, positively-confirmed pattern [`Step::WaitForProbe`] uses)
/// until its trimmed result equals `expect` or [`SEND_CONFIRM_DEADLINE`]
/// elapses; unlike the screen-based path, reaching the deadline here is a
/// hard [`CompatError::SendConfirmTimedOut`], not a best-effort return --
/// the author asserted this expression proves delivery, so a probe that
/// never confirms it is an actual scenario failure, not merely an
/// unconfirmed animation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SendConfirm {
    pub expr: String,
    pub expect: String,
}

/// Errors driving a compat scenario. Never carries a scenario name or step
/// index itself -- see this module's own docs and `bin/oracle.rs`'s
/// `run_tokens`/`EntryOutcome` precedent: composing "which scenario, which
/// step" is the orchestrating loop's job, the same layering the differential
/// oracle already uses.
#[non_exhaustive]
#[derive(Debug, thiserror::Error)]
pub enum CompatError {
    /// The underlying pty could not be opened or driven.
    #[error(transparent)]
    Pty(#[from] OracleError),
    /// An I/O error spawning or reading from a probe subprocess.
    #[error(transparent)]
    Io(#[from] std::io::Error),
    /// A `wait_for` step's needle never appeared before its timeout.
    #[error("wait_for {needle:?} timed out after {timeout:?}")]
    WaitTimedOut { needle: String, timeout: Duration },
    /// A `wait_for_cell` step's target cell never matched before its
    /// timeout.
    #[error("wait_for_cell ({row},{col}) != {expected:?} timed out after {timeout:?}")]
    WaitForCellTimedOut {
        row: u16,
        col: u16,
        expected: String,
        timeout: Duration,
    },
    /// An `assert_absent` step's forbidden needle was present on screen.
    #[error("assert_absent violated: {needle:?} is present on screen")]
    ForbiddenTextPresent { needle: String },
    /// An `assert_cell_not` step's forbidden cell held exactly the glyph it
    /// must not.
    #[error("assert_cell_not violated: ({row},{col}) == {glyph:?}")]
    ForbiddenCellPresent { row: u16, col: u16, glyph: String },
    /// A `probe` step's expression evaluated to a value other than what was
    /// expected.
    #[error("probe {expr:?} returned {actual:?}, expected {expected:?}")]
    ProbeMismatch {
        expr: String,
        expected: String,
        actual: String,
    },
    /// A `wait_for_probe` step's expression never matched `expect` before
    /// its own timeout.
    #[error("wait_for_probe {expr:?} never returned {expect:?} within {timeout:?}")]
    WaitForProbeTimedOut {
        expr: String,
        expect: String,
        timeout: Duration,
    },
    /// A `send` step's `confirm` probe never returned its expected value
    /// before `timeout`: the keys were written, but the effect they were
    /// asserted to produce was never observed, so (unlike a plain `send`'s
    /// best-effort screen wait) this is a hard failure rather than a
    /// silent give-up.
    #[error("send confirm {expr:?} never returned {expect:?} within {timeout:?}")]
    SendConfirmTimedOut {
        expr: String,
        expect: String,
        timeout: Duration,
    },
    /// The probe subprocess (`nvim --server ... --remote-expr`) exited
    /// non-zero: the socket does not exist yet, the target rejected the
    /// expression, or the connection was refused.
    #[error("probe {expr:?} failed: {stderr}")]
    ProbeFailed { expr: String, stderr: String },
    /// The probe subprocess did not exit within [`PROBE_TIMEOUT`] and was
    /// killed -- the target is wedged (e.g. a blocking hit-enter prompt)
    /// rather than merely slow.
    #[error("probe {expr:?} did not respond within {timeout:?}; target may be wedged")]
    ProbeTimedOut { expr: String, timeout: Duration },
    /// [`CompatSession::prime_probe_channel`] typed the `serverstart` command
    /// into the pty, but no probe succeeded before its own deadline: the
    /// fixture-less priming path never actually opened the channel.
    #[error("probe channel never opened within {0:?} of priming")]
    ProbeChannelNeverOpened(Duration),
    /// A `send` step's key text contains a `<...>` token shaped like real
    /// vim key notation (a modifier prefix, an `<F\d+>` form) that
    /// [`resolve_key_token`] does not implement, rather than either a
    /// recognized token or an author's own literal `<...>` text. Caught at
    /// scenario load time too (`view_harness::scenario`'s `Send`-step
    /// validation calls this same translator), not first discovered
    /// mid-run.
    #[error("unsupported key notation <{token}>: {reason}")]
    UnsupportedKeyNotation { token: String, reason: &'static str },
    /// The implicit zero-error epilogue found an E-numbered error or a Lua
    /// traceback in `:messages` or `v:errmsg`.
    #[error("zero-error epilogue violated ({origin}): {detail:?}")]
    ZeroErrorViolation {
        /// Which probe surfaced it: `"messages"` or `"v:errmsg"`.
        origin: &'static str,
        detail: String,
    },
}

/// Runs `child` to completion, polling rather than blocking, killing (and
/// reaping, so it cannot become a zombie) it if `timeout` elapses first.
/// Mirrors [`PtySession::wait_for_exit`]'s own bounded-wait shape: a probe
/// subprocess reaching a genuinely wedged target must fail this call's own
/// deadline rather than hang the caller, the same property that method's
/// doc comment argues for.
///
/// `stdout`/`stderr` are drained on background threads from the moment
/// `child` is handed to this function, not read synchronously after it
/// exits: a child that writes more than one pipe buffer's worth of output
/// before exiting would otherwise block on a full pipe with nothing
/// draining it, so this function's own deadline would kill and report a
/// merely slow-draining child as wedged.
fn wait_with_timeout(
    mut child: Child,
    timeout: Duration,
) -> Result<std::process::Output, CompatError> {
    let stdout_reader = child.stdout.take().map(spawn_pipe_drain);
    let stderr_reader = child.stderr.take().map(spawn_pipe_drain);

    let deadline = Instant::now() + timeout;
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break Some(status);
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            break None;
        }
        std::thread::sleep(Duration::from_millis(20));
    };

    // joined only after the child has already exited or been killed, so
    // each reader thread is already at (or immediately reaches) EOF rather
    // than blocking this call past its own deadline
    let stdout = stdout_reader
        .and_then(|h| h.join().ok())
        .unwrap_or_default();
    let stderr = stderr_reader
        .and_then(|h| h.join().ok())
        .unwrap_or_default();

    match status {
        Some(status) => Ok(std::process::Output {
            status,
            stdout,
            stderr,
        }),
        None => Err(CompatError::ProbeTimedOut {
            expr: String::new(),
            timeout,
        }),
    }
}

/// Spawns a background thread that reads `pipe` to EOF and returns its full
/// contents, the concurrent counterpart [`wait_with_timeout`]'s own doc
/// comment explains the need for.
fn spawn_pipe_drain(mut pipe: impl Read + Send + 'static) -> std::thread::JoinHandle<Vec<u8>> {
    std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = pipe.read_to_end(&mut buf);
        buf
    })
}

/// Drives one compat scenario's steps against a real `view` process over a
/// pty, plus a second, independent probe channel to the embedded nvim (see
/// this module's own docs). Deliberately thin: [`spawn_configured`] mirrors
/// [`PtySession::spawn_configured`]'s own signature so a caller configuring
/// XDG isolation and `VIEW_COMPAT_SOCK` builds one familiar
/// [`CommandBuilder`], the same way `view-oracle`'s own smoke tests already
/// configure a `view` pty.
///
/// [`spawn_configured`]: Self::spawn_configured
pub struct CompatSession {
    pty: PtySession,
    nvim_bin: PathBuf,
    sock_path: PathBuf,
}

impl CompatSession {
    /// Opens a `cols`x`rows` pty and spawns `cmd` (an already-configured
    /// `view` invocation -- XDG isolation, `VIEW_COMPAT_SOCK`, any
    /// `--nvim-bin` override -- already set as env/args by the caller)
    /// inside it. `nvim_bin` is the pinned `nvim` binary this session's
    /// probe channel shells out to as a client; `sock_path` is the same
    /// path the spawned `view` process's `VIEW_COMPAT_SOCK` env var names.
    ///
    /// # Errors
    ///
    /// Returns [`CompatError::Pty`] if the pty cannot be opened or `cmd`
    /// fails to spawn.
    pub fn spawn_configured(
        cmd: CommandBuilder,
        cols: u16,
        rows: u16,
        nvim_bin: PathBuf,
        sock_path: PathBuf,
    ) -> Result<Self, CompatError> {
        let pty = PtySession::spawn_configured(cmd, cols, rows)?;
        Ok(Self {
            pty,
            nvim_bin,
            sock_path,
        })
    }

    /// Borrows the underlying [`PtySession`] for callers that need its own
    /// primitives directly (`wait`, `wait_for_exit`, `screen`, `kill`)
    /// beyond what [`drive_step`](Self::drive_step) covers.
    pub fn pty(&mut self) -> &mut PtySession {
        &mut self.pty
    }

    /// Evaluates `expr` against the probe channel via `nvim --server $SOCK
    /// --remote-expr '<expr>'`, returning its (trimmed) result.
    ///
    /// Live-verified reply shapes (`nvim` v0.12.4): a
    /// successful call exits 0 with the evaluated value on stdout and no
    /// trailing content worth preserving (trimmed here); a Lua/Vim error
    /// (e.g. an undefined variable) exits 2 with `"Lua: Vim:E121: ..."` on
    /// stderr; a not-yet-listening socket exits 2 with `"E247: Failed to
    /// connect ...: connection refused"` on stderr. Both error shapes surface
    /// as [`CompatError::ProbeFailed`] -- this method does not distinguish
    /// "channel not open yet" from "expression rejected" itself, since
    /// [`prime_probe_channel`](Self::prime_probe_channel) is the caller that
    /// cares about that distinction, via its own bounded retry.
    ///
    /// # Errors
    ///
    /// Returns [`CompatError::Io`] if the subprocess cannot be spawned,
    /// [`CompatError::ProbeTimedOut`] if it does not exit within
    /// [`PROBE_TIMEOUT`], or [`CompatError::ProbeFailed`] if it exits
    /// non-zero.
    pub fn probe(&self, expr: &str) -> Result<String, CompatError> {
        let child = Command::new(&self.nvim_bin)
            .arg("--server")
            .arg(&self.sock_path)
            .arg("--remote-expr")
            .arg(expr)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;

        let output = wait_with_timeout(child, PROBE_TIMEOUT).map_err(|err| match err {
            CompatError::ProbeTimedOut { timeout, .. } => CompatError::ProbeTimedOut {
                expr: expr.to_string(),
                timeout,
            },
            other => other,
        })?;

        if !output.status.success() {
            return Err(CompatError::ProbeFailed {
                expr: expr.to_string(),
                stderr: String::from_utf8_lossy(&output.stderr).trim().to_string(),
            });
        }
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
    }

    /// For a fixture-less scenario (no committed `init.lua` to carry its own
    /// `serverstart` call): types `:call serverstart($VIEW_COMPAT_SOCK)<CR>`
    /// into the pty, exactly as a user would at the command line, then
    /// confirms the channel actually opened via a bounded retry loop of real
    /// [`probe`](Self::probe) calls -- never a fixed sleep. Each retry is a
    /// genuine attempt at the real probe subprocess, so this loop cannot
    /// pass vacuously the way a bare "wait N ms then assume it worked" would:
    /// it only returns `Ok` once a probe has actually round-tripped.
    ///
    /// # Errors
    ///
    /// Returns [`CompatError::Pty`] if typing the command fails, or
    /// [`CompatError::ProbeChannelNeverOpened`] if no probe succeeds before
    /// `timeout` elapses.
    pub fn prime_probe_channel(&mut self, timeout: Duration) -> Result<(), CompatError> {
        self.pty.send(&resolve_send_keys(
            "<Esc>:call serverstart($VIEW_COMPAT_SOCK)<CR>",
        )?)?;
        self.await_probe_channel(timeout)
    }

    /// For a fixture whose own `init.lua` already calls `serverstart`
    /// (every committed compat fixture): confirms the channel is actually
    /// live via the same bounded, real-probe-retry loop
    /// [`prime_probe_channel`](Self::prime_probe_channel) uses, minus the
    /// pty typing step. Startup ordering (config sourced, `serverstart`
    /// called, socket file created) is otherwise unobserved from the
    /// caller's side, so a scenario's first real step must not race it.
    ///
    /// # Errors
    ///
    /// Returns [`CompatError::ProbeChannelNeverOpened`] if no probe
    /// succeeds before `timeout` elapses.
    pub fn await_probe_channel(&mut self, timeout: Duration) -> Result<(), CompatError> {
        let deadline = Instant::now() + timeout;
        loop {
            if self.probe("1").is_ok() {
                return Ok(());
            }
            if Instant::now() >= deadline {
                return Err(CompatError::ProbeChannelNeverOpened(timeout));
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    /// Blocks until the pty's rendered screen stops changing for `silence`,
    /// or `deadline` elapses -- a bounded, positively-confirmed settle
    /// check (each poll re-reads the real screen; two consecutive
    /// identical reads is the confirmation), never a fixed sleep. Used
    /// before [`prime_probe_channel`](Self::prime_probe_channel) on a
    /// fixture-less scenario: unlike every committed fixture (whose
    /// `init.lua` calls `serverstart` as its very first statement, well
    /// before any redraw), a daily config's own startup content is
    /// unknown to this harness, so there is no fixed needle to `wait_for`
    /// instead. An already-quiet screen at the moment this is called counts
    /// as settled here, which is the right reading for "has this session's
    /// startup content finished rendering" -- there is nothing upstream of
    /// this call whose delivery is still in flight, unlike
    /// [`wait_for_send_delivery`](Self::wait_for_send_delivery)'s narrower
    /// question.
    ///
    /// Returns whether the screen was observed to settle before `deadline`.
    #[must_use]
    pub fn wait_for_screen_quiescence(&mut self, silence: Duration, deadline: Duration) -> bool {
        self.poll_screen_settle(None, silence, deadline)
    }

    /// Blocks until the pty's rendered screen is observed to *change* from
    /// `baseline` and then hold unchanged for `silence`, or `deadline`
    /// elapses. Unlike [`wait_for_screen_quiescence`](Self::wait_for_screen_quiescence),
    /// a screen that still equals `baseline` at every poll can never satisfy
    /// this, however long it holds still: an unmoved screen is a delivery
    /// that has not landed yet, not one that settled instantly, so this is
    /// the check [`drive_step`](Self::drive_step)'s `Send` arm uses to
    /// confirm the keys it just wrote actually reached the screen before
    /// treating them as delivered. `baseline` must be captured by the
    /// caller *before* the keys are written -- capturing it here, after the
    /// write, would already have missed a change fast enough to land before
    /// the first poll, which is exactly the false "nothing happened"
    /// reading this exists to rule out.
    ///
    /// Returns whether a change-then-settle was observed before `deadline`.
    #[must_use]
    fn wait_for_send_delivery(
        &mut self,
        baseline: String,
        silence: Duration,
        deadline: Duration,
    ) -> bool {
        self.poll_screen_settle(Some(baseline), silence, deadline)
    }

    /// Shared polling loop behind [`wait_for_screen_quiescence`](Self::wait_for_screen_quiescence)
    /// and [`wait_for_send_delivery`](Self::wait_for_send_delivery): re-reads
    /// the real screen every 50ms and declares settled once it has held
    /// unchanged for `silence`, bounded by `deadline`. `require_change_from`
    /// is the two callers' one behavioral difference: `None` treats
    /// "unchanged since the first poll" as already settled; `Some(baseline)`
    /// refuses to settle until the screen has been observed to differ from
    /// `baseline` at least once first.
    fn poll_screen_settle(
        &mut self,
        require_change_from: Option<String>,
        silence: Duration,
        deadline: Duration,
    ) -> bool {
        let start = Instant::now();
        let mut changed = require_change_from.is_none();
        let mut last = require_change_from.unwrap_or_else(|| self.pty.screen());
        let mut quiet_since = Instant::now();
        loop {
            std::thread::sleep(Duration::from_millis(50));
            let current = self.pty.screen();
            if current == last {
                if changed && Instant::now().duration_since(quiet_since) >= silence {
                    return true;
                }
            } else {
                changed = true;
                last = current;
                quiet_since = Instant::now();
            }
            if start.elapsed() >= deadline {
                return false;
            }
        }
    }

    /// Drives one [`Step`]. Every `WaitFor`/`WaitForCell` step already
    /// carries its own resolved `timeout` (the scenario loader applies the
    /// schema default when a TOML step omits `timeout_ms`), so this method
    /// takes no separate timeout parameter of its own.
    ///
    /// # Errors
    ///
    /// Returns the [`CompatError`] variant matching whichever check failed.
    pub fn drive_step(&mut self, step: &Step) -> Result<(), CompatError> {
        match step {
            Step::Send { keys, confirm } => {
                // Captured before the write, not after: the pipeline from
                // "bytes on the pty" to "nvim has forwarded and applied
                // them" can take long enough that an effect landing between
                // the write and a post-write capture would already be
                // baked into the baseline, and wait_for_send_delivery would
                // then read a screen that never again differs from it as
                // "nothing happened" for the rest of the poll. Only the
                // screen-based path below reads this; a confirm probe
                // queries live nvim state instead, so it needs no baseline.
                let baseline = self.pty.screen();
                self.pty.send(&resolve_send_keys(keys)?)?;

                if let Some(SendConfirm { expr, expect }) = confirm {
                    // These keys are known ahead of time to produce no
                    // screen delta (why the author reached for `confirm`
                    // instead of the default path at all), so
                    // wait_for_send_delivery's "must observe a change"
                    // requirement can never be satisfied here -- it would
                    // just burn its full deadline every time. Retrying a
                    // live probe until it actually reports the effect
                    // (never a fixed sleep, never trusting a single
                    // best-effort read) is the same positively-confirmed
                    // standard: a probe result can only ever be "not yet"
                    // while the keys are still in flight, never a false
                    // "delivered" that ends the loop early, on a channel
                    // that can observe a silent effect the screen cannot.
                    let deadline = Instant::now() + SEND_CONFIRM_DEADLINE;
                    loop {
                        if self.probe(expr).is_ok_and(|actual| &actual == expect) {
                            return Ok(());
                        }
                        if Instant::now() >= deadline {
                            return Err(CompatError::SendConfirmTimedOut {
                                expr: expr.clone(),
                                expect: expect.clone(),
                                timeout: SEND_CONFIRM_DEADLINE,
                            });
                        }
                        std::thread::sleep(SEND_CONFIRM_POLL);
                    }
                }

                // Best-effort past this point: a plugin that keeps
                // something animating (a progress spinner) never lets the
                // screen re-settle once it has changed, and this step has
                // no way to tell that apart from a target that is
                // genuinely still forwarding the keys. Failing loudly here
                // would turn a legitimate animation into a hard error; the
                // steps that actually need delivery confirmed already poll
                // for it themselves (`wait_for`, `wait_for_probe`), so a
                // step that gives up after this bound loses only the
                // narrower race a single-shot `probe`/`assert_absent`
                // would otherwise be exposed to immediately afterward. What
                // giving up can no longer mean is a false "delivered": the
                // deadline can only be reached by a screen that has never
                // shown the keys' effect at all, never by one this call
                // mistook a stall for settling on.
                let _ = self.wait_for_send_delivery(
                    baseline,
                    SEND_SETTLE_SILENCE,
                    SEND_SETTLE_DEADLINE,
                );
                Ok(())
            }
            Step::WaitFor { needle, timeout } => {
                if self.pty.wait_for(needle, *timeout) {
                    Ok(())
                } else {
                    Err(CompatError::WaitTimedOut {
                        needle: needle.clone(),
                        timeout: *timeout,
                    })
                }
            }
            Step::WaitForCell {
                row,
                col,
                expected,
                timeout,
            } => {
                if self.pty.wait_for_cell(*row, *col, expected, *timeout) {
                    Ok(())
                } else {
                    Err(CompatError::WaitForCellTimedOut {
                        row: *row,
                        col: *col,
                        expected: expected.clone(),
                        timeout: *timeout,
                    })
                }
            }
            Step::AssertAbsent(needle) => {
                if self.pty.screen().contains(needle.as_str()) {
                    Err(CompatError::ForbiddenTextPresent {
                        needle: needle.clone(),
                    })
                } else {
                    Ok(())
                }
            }
            Step::AssertCellNot { row, col, glyph } => {
                let matches = self.pty.with_screen(|screen| {
                    screen
                        .cell(*row, *col)
                        .is_some_and(|c| c.contents() == *glyph)
                });
                if matches {
                    Err(CompatError::ForbiddenCellPresent {
                        row: *row,
                        col: *col,
                        glyph: glyph.clone(),
                    })
                } else {
                    Ok(())
                }
            }
            Step::Probe { expr, expect } => {
                let actual = self.probe(expr)?;
                if &actual == expect {
                    Ok(())
                } else {
                    Err(CompatError::ProbeMismatch {
                        expr: expr.clone(),
                        expected: expect.clone(),
                        actual,
                    })
                }
            }
            Step::WaitForProbe {
                expr,
                expect,
                timeout,
            } => {
                let deadline = Instant::now() + *timeout;
                loop {
                    if self.probe(expr).is_ok_and(|actual| &actual == expect) {
                        return Ok(());
                    }
                    if Instant::now() >= deadline {
                        return Err(CompatError::WaitForProbeTimedOut {
                            expr: expr.clone(),
                            expect: expect.clone(),
                            timeout: *timeout,
                        });
                    }
                    std::thread::sleep(Duration::from_millis(200));
                }
            }
        }
    }

    /// The implicit epilogue every scenario gets after its scripted steps:
    /// probes `:messages` and `v:errmsg` over the
    /// probe channel and fails if either carries an E-numbered error or a
    /// Lua traceback. Two separate probes (not one combined expression):
    /// `v:errmsg` only ever holds the *last* error, while `:messages`
    /// accumulates the whole session's log, so a scenario that triggers and
    /// then clears an error (a later successful command overwrites
    /// `v:errmsg`) would otherwise slip past a `v:errmsg`-only check with
    /// the error still sitting in `:messages`.
    ///
    /// # Errors
    ///
    /// Returns [`CompatError::ZeroErrorViolation`] if either probe's content
    /// contains an `E`-numbered error code or the literal string
    /// `"traceback"`, or any [`CompatError`] the two probes themselves can
    /// raise.
    pub fn zero_error_check(&self) -> Result<(), CompatError> {
        self.zero_error_check_since(&ErrorBaseline::default())
    }

    /// Snapshots the two error surfaces [`zero_error_check`]
    /// (`Self::zero_error_check`) probes, for a caller that will later hand
    /// the snapshot to [`zero_error_check_since`]
    /// (`Self::zero_error_check_since`).
    ///
    /// # Errors
    ///
    /// Returns any [`CompatError`] the two probes themselves can raise.
    pub fn error_baseline(&self) -> Result<ErrorBaseline, CompatError> {
        Ok(ErrorBaseline {
            messages: self.probe("execute('messages')")?,
            errmsg: self.probe("v:errmsg")?,
        })
    }

    /// [`zero_error_check`](Self::zero_error_check), but attributing only
    /// content that appeared *after* `baseline` to the scenario's steps.
    /// Exists for the fixture-less (daily-config) scenario: a maintainer's
    /// live config may set `v:errmsg` during its own startup (`nvim` under
    /// the same config sets the identical value, so it is the config's own
    /// property, not a compat divergence), and a committed fixture's
    /// guarantee that startup is error-free does not extend to a config
    /// this harness does not own.
    ///
    /// # Errors
    ///
    /// Returns [`CompatError::ZeroErrorViolation`] if `:messages` gained an
    /// error line `baseline` did not hold, or `v:errmsg` changed to an
    /// error value, or any [`CompatError`] the two probes themselves can
    /// raise.
    pub fn zero_error_check_since(&self, baseline: &ErrorBaseline) -> Result<(), CompatError> {
        let messages = self.probe("execute('messages')")?;
        if let Some(detail) = new_error_line(&messages, &baseline.messages) {
            return Err(CompatError::ZeroErrorViolation {
                origin: "messages",
                detail,
            });
        }
        let errmsg = self.probe("v:errmsg")?;
        if errmsg != baseline.errmsg {
            if let Some(detail) = error_marker(&errmsg) {
                return Err(CompatError::ZeroErrorViolation {
                    origin: "v:errmsg",
                    detail,
                });
            }
        }
        Ok(())
    }
}

/// A snapshot of the two error surfaces the zero-error epilogue probes
/// (`:messages` and `v:errmsg`), taken before a scenario's steps run so
/// [`CompatSession::zero_error_check_since`] can attribute only content
/// that appeared afterward to the steps. `Default` is the empty snapshot,
/// against which every error is new -- the strict form
/// [`CompatSession::zero_error_check`] applies to committed fixtures.
#[derive(Debug, Default)]
pub struct ErrorBaseline {
    messages: String,
    errmsg: String,
}

/// Scans `text` for an E-numbered Vim error (`E` followed by a digit,
/// nvim's own error-code convention) or a Lua traceback marker, returning
/// the offending line if found. A line-oriented scan (not a single
/// whole-text substring search) so [`CompatError::ZeroErrorViolation`]'s
/// `detail` names the actual offending line rather than the entire
/// `:messages` buffer, which can span an unrelated scenario's whole
/// scripted history.
fn error_marker(text: &str) -> Option<String> {
    text.lines()
        .find(|line| is_error_line(line))
        .map(str::to_string)
}

/// [`error_marker`] restricted to lines absent from `baseline`: the
/// line-delta scan behind [`CompatSession::zero_error_check_since`].
/// Whole-line set membership (not positional diffing) because `:messages`
/// only ever appends, and an error line the baseline already held is by
/// definition not the steps' doing wherever it now sits.
fn new_error_line(text: &str, baseline: &str) -> Option<String> {
    let seen: std::collections::HashSet<&str> = baseline.lines().collect();
    text.lines()
        .find(|line| is_error_line(line) && !seen.contains(line))
        .map(str::to_string)
}

/// The per-line predicate behind [`error_marker`] and [`new_error_line`]:
/// an E-numbered Vim error token or a Lua traceback marker.
fn is_error_line(line: &str) -> bool {
    line.contains("traceback")
        || line
            .as_bytes()
            .windows(2)
            .enumerate()
            .any(|(i, w)| w[0] == b'E' && w[1].is_ascii_digit() && starts_error_token(line, i))
}

/// True if the `E<digit>` found at byte offset `i` in `line` starts a token
/// (line start, or preceded by whitespace/`:`/`(`) rather than sitting
/// mid-word (e.g. the "E" in a plugin name or a file path), which would
/// otherwise false-positive [`error_marker`] on ordinary, error-free
/// content.
fn starts_error_token(line: &str, i: usize) -> bool {
    match line.as_bytes().get(i.wrapping_sub(1)) {
        None => true,
        Some(b) => !(b.is_ascii_alphanumeric() || *b == b'_'),
    }
}

/// Translates vim key-notation embedded in a [`Step::Send`] string (or
/// [`CompatSession::prime_probe_channel`]'s own typed command) into the
/// literal bytes a real keypress would send -- `"ihello<Esc>"` becomes the
/// text `ihello` followed by one real `0x1b` byte, not the four literal
/// characters `<Esc>`. Without this, a pty write of `keys.as_bytes()`
/// verbatim types `<Esc>`/`<CR>` as on-screen text instead of pressing the
/// key, which leaves the session stuck in whatever mode it started in --
/// silently, since most scenario assertions are substring checks that
/// still pass with the stray literal text present. The one case that
/// cannot silently tolerate it is `prime_probe_channel`'s own `<CR>`: an
/// ex command typed but never submitted never opens the probe channel,
/// which is how this bug was actually caught (a fixture-less scenario's
/// priming step timing out rather than any visible corruption).
///
/// `view_harness::scenario`'s own `Send`-step validation calls this same
/// function at scenario load time (discarding the bytes, keeping only the
/// `Result`), so an untranslatable token is caught before any pty is even
/// spawned, not only mid-run.
///
/// # Errors
///
/// Returns [`CompatError::UnsupportedKeyNotation`] if `text` contains a
/// `<...>` token shaped like real vim key notation that [`resolve_key_token`]
/// does not implement. A `<...>` token that is not notation-shaped at all
/// (an author's own literal angle-bracket text, e.g. `<Nonsense>`) is a
/// legitimate use of `send` and passes through unchanged rather than
/// erroring.
pub fn resolve_send_keys(text: &str) -> Result<Vec<u8>, CompatError> {
    let mut out = Vec::with_capacity(text.len());
    let bytes = text.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'<' {
            if let Some(rel_end) = text[i..].find('>') {
                let token = &text[i + 1..i + rel_end];
                if let Some(resolved) = resolve_key_token(token)? {
                    out.extend_from_slice(&resolved);
                    i += rel_end + 1;
                    continue;
                }
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    Ok(out)
}

/// The bare (no modifier prefix) named-key notation [`resolve_key_token`]
/// recognizes: the exact byte sequence a real terminal emits for that key
/// under the legacy VT100/xterm encoding `view-tui`'s own `crossterm`
/// dependency parses back into a `KeyCode` (see `view-tui/src/keys.rs`'s
/// `key_token`, whose `<Name>` output this table is the pty-input inverse
/// of), not vim's own `<Name>` notation string.
fn resolve_named_key(name: &str) -> Option<&'static [u8]> {
    Some(match name {
        "up" => b"\x1b[A",
        "down" => b"\x1b[B",
        "right" => b"\x1b[C",
        "left" => b"\x1b[D",
        "home" => b"\x1b[H",
        "end" => b"\x1b[F",
        "pageup" => b"\x1b[5~",
        "pagedown" => b"\x1b[6~",
        "del" | "delete" => b"\x1b[3~",
        "insert" => b"\x1b[2~",
        "f1" => b"\x1bOP",
        "f2" => b"\x1bOQ",
        "f3" => b"\x1bOR",
        "f4" => b"\x1bOS",
        "f5" => b"\x1b[15~",
        "f6" => b"\x1b[17~",
        "f7" => b"\x1b[18~",
        "f8" => b"\x1b[19~",
        "f9" => b"\x1b[20~",
        "f10" => b"\x1b[21~",
        "f11" => b"\x1b[23~",
        "f12" => b"\x1b[24~",
        _ => return None,
    })
}

/// The notation table [`resolve_send_keys`] recognizes, case-insensitive
/// (vim notation itself is case-insensitive for named keys: `<esc>` and
/// `<Esc>` are the same key) except for the single character wrapped by a
/// `<C-x>`/`<M-x>`/`<A-x>` modifier, whose own case is preserved rather than
/// folded (`<M-X>` is Alt+Shift+x, distinct from `<M-x>`, the same
/// distinction vim notation itself makes).
///
/// This table is the pty-input inverse of `view-tui/src/keys.rs`'s own
/// `encode_key`/`key_token` (nvim-notation-to-`KeyEvent`, in `view`'s own
/// forwarding direction): dependency direction forbids this crate from
/// importing that one to share the table directly, so the two are pinned
/// independently, and a divergence between them would silently change what
/// this harness types versus what `view` itself forwards to nvim, invisible
/// to either crate's own tests alone.
///
/// Returns `Ok(None)` for a token that is not shaped like real vim notation
/// at all (an author's own literal `<...>` text, e.g. `<Nonsense>`), which
/// [`resolve_send_keys`] then types as literal characters. Returns `Err`
/// for a token that *is* notation-shaped (a recognized modifier prefix, an
/// `<F\d+>` form, or a keypad name) but whose specific case this
/// translator does not implement (a stacked modifier combo, a modifier
/// wrapping a named key rather than a single character, an out-of-range
/// function key, a super-modifier or keypad form with no terminal byte
/// sequence to translate to): silently
/// typing that shape as literal text would leave a scenario's session
/// stuck in whatever mode it started in, exactly the failure this module's
/// own docs describe for a bare `<Esc>`, so an unimplemented-but-notation-
/// shaped token fails loud instead of passing through.
fn resolve_key_token(token: &str) -> Result<Option<Vec<u8>>, CompatError> {
    let lower = token.to_ascii_lowercase();
    match lower.as_str() {
        "esc" | "escape" => return Ok(Some(b"\x1b".to_vec())),
        "cr" | "enter" | "return" => return Ok(Some(b"\r".to_vec())),
        "tab" => return Ok(Some(b"\t".to_vec())),
        "bs" | "backspace" => return Ok(Some(b"\x08".to_vec())),
        "space" => return Ok(Some(b" ".to_vec())),
        "lt" => return Ok(Some(b"<".to_vec())),
        "bar" => return Ok(Some(b"|".to_vec())),
        "bslash" => return Ok(Some(b"\\".to_vec())),
        "nul" => return Ok(Some(b"\x00".to_vec())),
        "nl" | "linefeed" => return Ok(Some(b"\n".to_vec())),
        "ff" => return Ok(Some(b"\x0c".to_vec())),
        "s-tab" => return Ok(Some(b"\x1b[Z".to_vec())),
        _ => {}
    }
    if let Some(bytes) = resolve_named_key(&lower) {
        return Ok(Some(bytes.to_vec()));
    }
    if lower.starts_with("c-") {
        return resolve_ctrl_notation(&token[2..], token);
    }
    if lower.starts_with("m-") || lower.starts_with("a-") {
        return resolve_alt_notation(&token[2..], token);
    }
    if is_notation_shaped(&lower) {
        return Err(CompatError::UnsupportedKeyNotation {
            token: token.to_string(),
            reason: "not one of this translator's recognized tokens (named \
                      keys, <C-x>/<M-x>/<A-x> for a single character, \
                      <S-Tab>, or <F1>-<F12>)",
        });
    }
    Ok(None)
}

/// `<C-x>` for a single ASCII letter: a real terminal's Ctrl modifier
/// clears bits 6-7 of the letter's code point (`Ctrl-A` through `Ctrl-Z`
/// occupy `0x01`-`0x1A` regardless of the letter's own shift state, which
/// is why `<C-w>` and `<C-W>` are the identical keypress), matching
/// `crossterm`'s own inverse decode (`c @ b'\x01'..=b'\x1A'` in its unix
/// parser, which always produces a lowercase `Char`). Any other body -- a
/// named key, more than one character -- is notation-shaped but not a case
/// this translator implements.
fn resolve_ctrl_notation(body: &str, original: &str) -> Result<Option<Vec<u8>>, CompatError> {
    let mut chars = body.chars();
    match (chars.next(), chars.next()) {
        (Some(c), None) if c.is_ascii_alphabetic() => {
            Ok(Some(vec![c.to_ascii_lowercase() as u8 - b'a' + 1]))
        }
        _ => Err(CompatError::UnsupportedKeyNotation {
            token: original.to_string(),
            reason: "<C-...> is only translated for a single ASCII letter (e.g. <C-w>)",
        }),
    }
}

/// `<M-x>`/`<A-x>` for a single character: a real terminal's Alt modifier
/// sends a bare `ESC` immediately before the key's own unmodified bytes
/// (matching `crossterm`'s own inverse decode: its unix parser recurses on
/// the remaining buffer after a leading `ESC` that is not itself a CSI/SS3
/// lead-in and ORs in `KeyModifiers::ALT`), so the wrapped character's own
/// case is preserved rather than folded. Any other body is notation-shaped
/// but not a case this translator implements.
fn resolve_alt_notation(body: &str, original: &str) -> Result<Option<Vec<u8>>, CompatError> {
    let mut chars = body.chars();
    match (chars.next(), chars.next()) {
        (Some(c), None) if c.is_ascii_graphic() || c == ' ' => Ok(Some(vec![0x1b, c as u8])),
        _ => Err(CompatError::UnsupportedKeyNotation {
            token: original.to_string(),
            reason: "<M-...>/<A-...> is only translated for a single ASCII character",
        }),
    }
}

/// True if `lower` (already lowercased) matches the *shape* of real vim key
/// notation -- a modifier prefix, an `<F\d+>` function-key form, or a
/// keypad name -- even when the specific token is not one this translator
/// implements. Distinguishes that case from a token that merely looks
/// unfamiliar (an author's own literal `<...>` text): only the former must
/// hard-error rather than pass through as literal characters, per
/// [`resolve_key_token`]'s own doc. The `d-` (super/cmd) and `t-`
/// (termcap) prefixes are here permanently, not pending implementation:
/// the legacy VT100/xterm encoding this translator's byte sequences
/// target has no super-modifier representation at all, and a termcap
/// entry names a terminal capability rather than a keypress, so neither
/// token can ever be typed faithfully through the pty and both must
/// always fail loud.
fn is_notation_shaped(lower: &str) -> bool {
    lower.starts_with("c-")
        || lower.starts_with("s-")
        || lower.starts_with("m-")
        || lower.starts_with("a-")
        || lower.starts_with("d-")
        || lower.starts_with("t-")
        || is_keypad_name(lower)
        || is_untypeable_name(lower)
        || (lower.starts_with('f')
            && lower.len() > 1
            && lower[1..].bytes().all(|b| b.is_ascii_digit()))
}

/// True if `lower` (already lowercased) is a vim key-notation name with no
/// faithful pty byte sequence at all. `<Cmd>`, `<Ignore>`, and `<NOP>` are
/// mapping-side pseudo-keys nvim synthesizes internally -- no terminal ever
/// emits them as input bytes. `<Help>` and `<Undo>` are dedicated keys the
/// legacy VT100/xterm encoding never assigned sequences to. `<EOL>` is
/// platform-dependent (CR, LF, or CR-LF), so no single byte sequence types
/// it faithfully. `<CSI>` is the raw 8-bit 0x9b control, which is not
/// valid standalone UTF-8 and would corrupt the pty's input stream rather
/// than arrive as the key. All are notation-shaped and must fail loud
/// instead of passing through as literal text, per
/// [`is_notation_shaped`]'s contract.
fn is_untypeable_name(lower: &str) -> bool {
    matches!(
        lower,
        "cmd" | "ignore" | "nop" | "help" | "undo" | "eol" | "csi"
    )
}

/// True if `lower` (already lowercased) is one of vim's keypad key names
/// (`<kEnter>`, `<kPlus>`, `<k0>`-`<k9>`, ...). Matched as an explicit set
/// rather than a `k` prefix heuristic so an author's own literal `<...>`
/// text that merely starts with `k` (`<keys>`, `<kbd>`) still passes
/// through, per [`is_notation_shaped`]'s contract. Keypad keys are
/// notation-shaped but never translated: the legacy VT100/xterm encoding
/// has no keypad byte sequences distinct from the plain keys' own (a
/// terminal in numeric-keypad mode sends `kEnter` as a plain `\r`), so
/// typing one faithfully as *the keypad key* is not possible through the
/// pty and silently substituting the plain key would test the wrong
/// mapping.
fn is_keypad_name(lower: &str) -> bool {
    matches!(
        lower,
        "kenter"
            | "kplus"
            | "kminus"
            | "kmultiply"
            | "kdivide"
            | "kpoint"
            | "kcomma"
            | "kequal"
            | "khome"
            | "kend"
            | "korigin"
            | "kpageup"
            | "kpagedown"
            | "kup"
            | "kdown"
            | "kleft"
            | "kright"
            | "kdel"
            | "kinsert"
    ) || (lower.len() == 2 && lower.starts_with('k') && lower.as_bytes()[1].is_ascii_digit())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    use crate::testenv;

    #[test]
    fn state_name_and_parse_state_name_round_trip_every_variant() {
        for state in [
            ScenarioState::Present,
            ScenarioState::Superseded,
            ScenarioState::Deferred,
            ScenarioState::NativeOnly,
        ] {
            assert_eq!(parse_state_name(state_name(state)), Some(state));
        }
    }

    #[test]
    fn parse_state_name_rejects_an_unrecognized_spelling() {
        assert_eq!(parse_state_name("bogus"), None);
    }

    #[test]
    fn error_marker_finds_an_e_numbered_line() {
        let text = "Press ENTER\nE5108: Error executing lua ...\nsome other line";
        assert_eq!(
            error_marker(text),
            Some("E5108: Error executing lua ...".to_string())
        );
    }

    #[test]
    fn error_marker_finds_a_lua_traceback() {
        let text = "stack traceback:\n\t[C]: in function 'foo'";
        assert_eq!(error_marker(text), Some("stack traceback:".to_string()));
    }

    #[test]
    fn error_marker_ignores_clean_content() {
        assert_eq!(error_marker(""), None);
        assert_eq!(error_marker("-- INSERT --"), None);
    }

    #[test]
    fn error_marker_does_not_false_positive_on_a_word_containing_e_and_a_digit() {
        // "CVE2024" contains an upper-case "E" immediately followed by a
        // digit, the same shape a real E-numbered error starts with -- but
        // it sits mid-word (preceded by "V", alphanumeric), so it must not
        // be mistaken for one.
        assert_eq!(error_marker("see CVE2024 for details"), None);
    }

    #[test]
    fn error_marker_matches_e_number_at_start_of_line() {
        assert_eq!(
            error_marker("E121: Undefined variable: foo"),
            Some("E121: Undefined variable: foo".to_string())
        );
    }

    #[test]
    fn new_error_line_ignores_an_error_the_baseline_already_held() {
        let baseline = "startup chatter\nE216: No such group or event: FileExplorer *";
        assert_eq!(new_error_line(baseline, baseline), None);
    }

    #[test]
    fn new_error_line_reports_an_error_the_baseline_did_not_hold() {
        let baseline = "E216: No such group or event: FileExplorer *";
        let grown = "E216: No such group or event: FileExplorer *\nE121: Undefined variable: foo";
        assert_eq!(
            new_error_line(grown, baseline),
            Some("E121: Undefined variable: foo".to_string())
        );
    }

    #[test]
    fn new_error_line_against_an_empty_baseline_matches_error_marker() {
        let text = "ok line\nE121: Undefined variable: foo";
        assert_eq!(new_error_line(text, ""), error_marker(text));
    }

    #[test]
    fn resolve_send_keys_translates_esc_to_a_real_escape_byte() {
        assert_eq!(
            resolve_send_keys("ihello<Esc>").unwrap(),
            b"ihello\x1b".to_vec()
        );
    }

    #[test]
    fn resolve_send_keys_translates_cr_to_a_real_carriage_return() {
        assert_eq!(
            resolve_send_keys(":q<CR>").unwrap(),
            b":q\r".to_vec(),
            "a typed ex command with a literal '<CR>' suffix instead of a \
             real carriage-return byte never submits, which is exactly the \
             priming-channel bug this translator fixes"
        );
    }

    #[test]
    fn resolve_send_keys_is_case_insensitive_on_known_notation() {
        assert_eq!(resolve_send_keys("x<esc>").unwrap(), b"x\x1b".to_vec());
        assert_eq!(resolve_send_keys("x<ESC>").unwrap(), b"x\x1b".to_vec());
    }

    #[test]
    fn resolve_send_keys_passes_unrecognized_notation_through_literally() {
        assert_eq!(
            resolve_send_keys("a<Nonsense>b").unwrap(),
            b"a<Nonsense>b".to_vec()
        );
    }

    #[test]
    fn resolve_send_keys_passes_plain_text_through_unchanged() {
        assert_eq!(
            resolve_send_keys("plain text").unwrap(),
            b"plain text".to_vec()
        );
    }

    #[test]
    fn resolve_send_keys_handles_an_unterminated_angle_bracket() {
        assert_eq!(resolve_send_keys("a < b").unwrap(), b"a < b".to_vec());
    }

    #[test]
    fn resolve_send_keys_translates_lt_to_a_literal_angle_bracket() {
        assert_eq!(resolve_send_keys("<lt>").unwrap(), b"<".to_vec());
    }

    #[test]
    fn resolve_send_keys_translates_ctrl_w_to_its_control_byte() {
        assert_eq!(resolve_send_keys("<C-w>").unwrap(), vec![0x17]);
    }

    #[test]
    fn resolve_send_keys_translates_ctrl_w_uppercase_identically() {
        // a real terminal cannot distinguish Ctrl-w from Ctrl-W: both send
        // the same control byte, so the notation's own case must not
        // change the translated output
        assert_eq!(resolve_send_keys("<C-W>").unwrap(), vec![0x17]);
    }

    #[test]
    fn resolve_send_keys_translates_up_to_its_csi_escape_sequence() {
        assert_eq!(resolve_send_keys("<Up>").unwrap(), b"\x1b[A".to_vec());
    }

    #[test]
    fn resolve_send_keys_translates_named_nav_and_function_keys() {
        assert_eq!(resolve_send_keys("<Down>").unwrap(), b"\x1b[B".to_vec());
        assert_eq!(resolve_send_keys("<Left>").unwrap(), b"\x1b[D".to_vec());
        assert_eq!(resolve_send_keys("<Right>").unwrap(), b"\x1b[C".to_vec());
        assert_eq!(resolve_send_keys("<Home>").unwrap(), b"\x1b[H".to_vec());
        assert_eq!(resolve_send_keys("<End>").unwrap(), b"\x1b[F".to_vec());
        assert_eq!(resolve_send_keys("<PageUp>").unwrap(), b"\x1b[5~".to_vec());
        assert_eq!(
            resolve_send_keys("<PageDown>").unwrap(),
            b"\x1b[6~".to_vec()
        );
        assert_eq!(resolve_send_keys("<Del>").unwrap(), b"\x1b[3~".to_vec());
        assert_eq!(resolve_send_keys("<F1>").unwrap(), b"\x1bOP".to_vec());
        assert_eq!(resolve_send_keys("<F5>").unwrap(), b"\x1b[15~".to_vec());
        assert_eq!(resolve_send_keys("<F12>").unwrap(), b"\x1b[24~".to_vec());
    }

    #[test]
    fn resolve_send_keys_translates_shift_tab_to_the_backtab_csi_sequence() {
        assert_eq!(resolve_send_keys("<S-Tab>").unwrap(), b"\x1b[Z".to_vec());
    }

    #[test]
    fn resolve_send_keys_translates_alt_char_to_an_esc_prefixed_byte() {
        assert_eq!(resolve_send_keys("<M-x>").unwrap(), vec![0x1b, b'x']);
        assert_eq!(
            resolve_send_keys("<A-x>").unwrap(),
            vec![0x1b, b'x'],
            "<A-...> is vim's own alias for <M-...>"
        );
    }

    #[test]
    fn resolve_send_keys_alt_char_preserves_the_wrapped_characters_case() {
        // <M-X> is Alt+Shift+x, a distinct keypress from <M-x>; folding case
        // here would silently collapse the two
        assert_eq!(resolve_send_keys("<M-X>").unwrap(), vec![0x1b, b'X']);
    }

    #[test]
    fn resolve_send_keys_hard_errors_on_a_modifier_wrapping_a_named_key() {
        let err = resolve_send_keys("<C-Up>")
            .expect_err("a modifier wrapping a named key is notation-shaped but unimplemented");
        assert!(matches!(err, CompatError::UnsupportedKeyNotation { .. }));
    }

    #[test]
    fn resolve_send_keys_hard_errors_on_an_out_of_range_function_key() {
        let err = resolve_send_keys("<F99>")
            .expect_err("an out-of-range F-key is notation-shaped but unimplemented");
        assert!(matches!(err, CompatError::UnsupportedKeyNotation { .. }));
    }

    #[test]
    fn resolve_send_keys_hard_errors_on_a_stacked_modifier_combo() {
        let err = resolve_send_keys("<C-M-x>")
            .expect_err("a stacked modifier combo is notation-shaped but unimplemented");
        assert!(matches!(err, CompatError::UnsupportedKeyNotation { .. }));
    }

    #[test]
    fn resolve_send_keys_hard_errors_on_a_shift_wrapped_non_tab_key() {
        let err =
            resolve_send_keys("<S-Left>").expect_err("<S-...> is only implemented for <S-Tab>");
        assert!(matches!(err, CompatError::UnsupportedKeyNotation { .. }));
    }

    #[test]
    fn resolve_send_keys_translates_bar_and_bslash_to_their_literal_bytes() {
        assert_eq!(resolve_send_keys("<Bar>").unwrap(), b"|".to_vec());
        assert_eq!(resolve_send_keys("<Bslash>").unwrap(), b"\\".to_vec());
    }

    #[test]
    fn resolve_send_keys_translates_nul_to_a_zero_byte() {
        assert_eq!(resolve_send_keys("<Nul>").unwrap(), vec![0x00]);
    }

    #[test]
    fn resolve_send_keys_hard_errors_on_super_modifier_notation() {
        let err = resolve_send_keys("<D-w>")
            .expect_err("the super/cmd modifier has no terminal byte sequence");
        assert!(matches!(err, CompatError::UnsupportedKeyNotation { .. }));
    }

    #[test]
    fn resolve_send_keys_hard_errors_on_keypad_notation() {
        for token in ["<kEnter>", "<kPlus>", "<k0>", "<k9>", "<kPageUp>"] {
            let err = resolve_send_keys(token).expect_err(
                "a keypad key has no terminal byte sequence distinct from the plain key",
            );
            assert!(
                matches!(err, CompatError::UnsupportedKeyNotation { .. }),
                "{token} must hard-error, not pass through as literal text"
            );
        }
    }

    #[test]
    fn resolve_send_keys_translates_nl_and_ff_to_their_control_bytes() {
        assert_eq!(resolve_send_keys("<NL>").unwrap(), b"\n".to_vec());
        assert_eq!(resolve_send_keys("<FF>").unwrap(), vec![0x0c]);
    }

    #[test]
    fn resolve_send_keys_hard_errors_on_untypeable_named_keys() {
        for token in [
            "<Cmd>", "<Ignore>", "<NOP>", "<Help>", "<Undo>", "<EOL>", "<CSI>",
        ] {
            let err = resolve_send_keys(token)
                .expect_err("a named key with no faithful pty byte sequence");
            assert!(
                matches!(err, CompatError::UnsupportedKeyNotation { .. }),
                "{token} must hard-error, not pass through as literal text"
            );
        }
    }

    #[test]
    fn resolve_send_keys_hard_errors_on_termcap_notation() {
        let err = resolve_send_keys("<t-ku>")
            .expect_err("a termcap entry names a capability, not a typeable key");
        assert!(matches!(err, CompatError::UnsupportedKeyNotation { .. }));
    }

    #[test]
    fn resolve_send_keys_passes_literal_text_sharing_an_untypeable_prefix_through() {
        // <command> starts with the same letters as the <Cmd> pseudo-key but
        // is an author's own literal text; the untypeable-name reject set is
        // an exact match, not a prefix heuristic, and must not swallow it
        assert_eq!(
            resolve_send_keys("<command>").unwrap(),
            b"<command>".to_vec()
        );
    }

    #[test]
    fn resolve_send_keys_passes_literal_k_prefixed_text_through() {
        // <kbd> shares keypad notation's leading `k` but is an author's own
        // literal text, not a vim key name; the keypad reject set must not
        // swallow it
        assert_eq!(resolve_send_keys("<kbd>").unwrap(), b"<kbd>".to_vec());
    }

    #[test]
    fn resolve_send_keys_pins_the_same_byte_sequences_view_tuis_encode_key_table_expects() {
        // hardcoded independently of view-tui/src/keys.rs (dependency
        // direction forbids importing it here): each right-hand side is the
        // exact byte sequence a real terminal sends for the key crossterm's
        // own unix parser decodes back into the KeyCode that
        // view-tui/src/keys.rs's encode_key then renders as the notation
        // string on the left -- the duplication here *is* the pin against
        // the two tables drifting apart undetected.
        let pairs: &[(&str, &[u8])] = &[
            ("<Esc>", b"\x1b"),
            ("<CR>", b"\r"),
            ("<Tab>", b"\t"),
            ("<BS>", b"\x08"),
            ("<lt>", b"<"),
            ("<Up>", b"\x1b[A"),
            ("<Down>", b"\x1b[B"),
            ("<Left>", b"\x1b[D"),
            ("<Right>", b"\x1b[C"),
            ("<Home>", b"\x1b[H"),
            ("<End>", b"\x1b[F"),
            ("<Del>", b"\x1b[3~"),
            ("<C-w>", &[0x17]),
            ("<S-Tab>", b"\x1b[Z"),
            ("<F5>", b"\x1b[15~"),
        ];
        for (notation, expected_bytes) in pairs {
            assert_eq!(
                resolve_send_keys(notation).unwrap(),
                expected_bytes.to_vec(),
                "notation {notation} did not resolve to the expected pty bytes"
            );
        }
    }

    /// Selects which fixture behaviour a re-invocation of this test binary
    /// performs, and by its absence marks an ordinary test run.
    const FIXTURE_MODE: &str = "VIEW_ORACLE_COMPAT_FIXTURE";

    /// The fixture body the two `wait_with_timeout` tests below drive, run
    /// by re-invoking this very test binary rather than by spawning
    /// `/bin/sleep` and `/bin/sh -c 'head -c ... /dev/zero'`.
    ///
    /// Those two behaviours -- killing and reaping a child past its
    /// deadline, and draining a pipe concurrently instead of after exit --
    /// are platform-independent library code, so unix-only fixtures gated
    /// them off exactly the platform whose pipe buffering differs most from
    /// the 64 KB Linux default they were sized against, which is where a
    /// drain bug would first diverge. A test binary is the one child process
    /// guaranteed to exist and behave identically on every platform this
    /// crate builds for.
    ///
    /// A normal run reaches this with the variable unset and returns at
    /// once; it is a `#[test]` only because that is the entry point a
    /// libtest binary can be asked for by name.
    ///
    /// The payload goes to stderr, not stdout: libtest writes its own
    /// progress lines to the child's stdout even under `--nocapture`, so
    /// stderr is the one stream whose byte count can be asserted exactly.
    #[test]
    fn fixture_child_body() {
        let Ok(mode) = std::env::var(FIXTURE_MODE) else {
            return;
        };
        match mode.as_str() {
            // outlives any deadline its parent sets, so only being killed
            // can end it
            "sleep" => std::thread::sleep(Duration::from_secs(60)),
            "flood" => {
                let len: usize = FIXTURE_PAYLOAD_LEN;
                let payload = vec![b'z'; len];
                use std::io::Write as _;
                let mut err = std::io::stderr();
                err.write_all(&payload)
                    .expect("fixture stderr write failed");
                err.flush().expect("fixture stderr flush failed");
            }
            other => {
                // exits rather than panicking: a panic here would be
                // reported as a failure of the fixture test in the child's
                // own output, which the parent discards, leaving the parent
                // to fail on a confusing symptom instead of the typo'd mode
                eprintln!("unknown fixture mode {other:?}");
                std::process::exit(64);
            }
        }
    }

    /// 200 KB comfortably exceeds a 64 KB pipe buffer (the typical Linux
    /// default) and Windows' own default, so a `wait_with_timeout` that read
    /// its pipes only after the child exited would leave the child blocked
    /// writing into a full pipe until the deadline killed it -- reporting a
    /// merely slow-draining child as wedged.
    const FIXTURE_PAYLOAD_LEN: usize = 200_000;

    /// Spawns this test binary again in `mode`, with only the fixture test
    /// selected so nothing else in the suite runs inside the child.
    ///
    /// `--nocapture` is what lets the fixture's own writes reach the pipes
    /// this returns; without it libtest buffers them and prints them back
    /// through its own reporting.
    fn spawn_fixture(mode: &str, stdout: Stdio, stderr: Stdio) -> Child {
        let exe = std::env::current_exe().expect("the running test binary must be locatable");
        testenv::spawning(|| {
            Command::new(&exe)
                .args([
                    "--exact",
                    "compat::tests::fixture_child_body",
                    "--nocapture",
                    "--test-threads=1",
                ])
                .env(FIXTURE_MODE, mode)
                .stdout(stdout)
                .stderr(stderr)
                .spawn()
        })
        .expect("failed to re-invoke the test binary as a fixture")
    }

    #[test]
    fn wait_with_timeout_kills_and_reaps_a_process_that_outlives_its_deadline() {
        let child = spawn_fixture("sleep", Stdio::null(), Stdio::null());
        // captured before the child is moved into the call
        let pid = child.id();

        let result = wait_with_timeout(child, Duration::from_millis(100));

        assert!(
            matches!(result, Err(CompatError::ProbeTimedOut { .. })),
            "expected ProbeTimedOut, got {result:?}"
        );

        assert!(
            !process_table_entry_exists(pid),
            "child pid {pid} still has a process-table entry after \
             wait_with_timeout returned; it was killed but not reaped"
        );
    }

    /// Whether the OS still holds a process-table entry for `pid`.
    ///
    /// The distinction the reaping assertion needs: a killed-and-reaped
    /// child leaves no entry at all, while a killed-but-never-waited one
    /// lingers as a zombie ("Z" state) until some other process reaps it,
    /// so an entry that is still there says `kill` ran without `wait`.
    /// Reads `/proc` on Linux and asks `ps` for the same fact on macOS,
    /// which has no `/proc`; `ps` lists zombies exactly like `/proc` keeps
    /// their directories, so the two are the same evidence.
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    fn process_table_entry_exists(pid: u32) -> bool {
        #[cfg(target_os = "linux")]
        {
            std::path::Path::new(&format!("/proc/{pid}")).exists()
        }
        #[cfg(target_os = "macos")]
        {
            // a spawn failure means the probe never got to look, which must
            // not be reported as "no entry": that reads as a successfully
            // reaped child and hands the caller a silent pass from a broken
            // probe. An empty listing from a `ps` that DID run is the real
            // negative, and is the only one accepted here (`ps` also exits
            // nonzero for an unknown pid, so its status is not the signal)
            let listing = testenv::spawning(|| {
                Command::new("/bin/ps")
                    .args(["-o", "stat=", "-p", &pid.to_string()])
                    .output()
            })
            .expect("/bin/ps must run for the process table to be observable at all");
            !listing.stdout.is_empty()
        }
    }

    /// Windows has no zombie state -- a killed child's handle is released
    /// once nothing holds it -- so `tasklist` reporting the pid at all is
    /// the same evidence `/proc` and `ps` give elsewhere: the child was
    /// killed but never waited for.
    ///
    /// A `tasklist` that could not run is treated as "still there" for the
    /// same reason macOS treats an unrunnable `ps` as fatal: reporting no
    /// entry from a probe that never looked would hand the assertion a
    /// silent pass.
    #[cfg(windows)]
    fn process_table_entry_exists(pid: u32) -> bool {
        let listing = testenv::spawning(|| {
            Command::new("tasklist")
                .args(["/FI", &format!("PID eq {pid}"), "/NH", "/FO", "CSV"])
                .output()
        })
        .expect("tasklist must run for the process table to be observable at all");
        // the no-match case is an INFO line on stdout rather than a nonzero
        // exit, so the pid's own presence is the only usable signal
        String::from_utf8_lossy(&listing.stdout).contains(&format!("\"{pid}\""))
    }

    /// Reports no entry on a platform with neither `/proc`, a POSIX `ps`,
    /// nor `tasklist`, so the reaping assertion is inert there rather than
    /// failing on the absence of a way to look.
    #[cfg(all(unix, not(any(target_os = "linux", target_os = "macos"))))]
    fn process_table_entry_exists(_pid: u32) -> bool {
        false
    }

    /// A scratch world for the probe-client pin: private search paths and
    /// `XDG_*_HOME` directories, so an editor started here reads nothing of
    /// the host's, plus the marker file a planted `VIMINIT` writes when an
    /// editor actually executes it.
    ///
    /// Under the build tree rather than the system temp dir, matching
    /// `pty`'s own planted world: this stands in for host configuration, so
    /// it must not sit somewhere an unrelated process can write into.
    // Serves only the unix-gated probe-client test below; gated with it so it
    // is not dead code on Windows.
    #[cfg(unix)]
    struct ProbeInitWorld {
        empty: PathBuf,
        homes: PathBuf,
        marker: PathBuf,
        sock: PathBuf,
    }

    #[cfg(unix)]
    impl ProbeInitWorld {
        const XDG_HOMES: [&'static str; 4] = [
            "XDG_CONFIG_HOME",
            "XDG_DATA_HOME",
            "XDG_STATE_HOME",
            "XDG_CACHE_HOME",
        ];

        fn plant() -> Self {
            let mut root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
            root.pop(); // crates/
            root.pop(); // workspace root
            let root = root.join("target").join("view-compat-probe-init");
            let _ = std::fs::remove_dir_all(&root);
            let world = Self {
                empty: root.join("empty"),
                homes: root.join("homes"),
                marker: root.join("viminit-ran"),
                sock: root.join("probe.sock"),
            };
            std::fs::create_dir_all(&world.empty).unwrap();
            std::fs::create_dir_all(&world.homes).unwrap();
            world
        }

        /// The ex command line the planted `VIMINIT` carries. Writing a file
        /// is what makes this an oracle rather than a restatement of the
        /// code under test: it reports whether an editor *executed* the
        /// host's startup command, and says so without consulting the
        /// variable's name or anything the harness does with it.
        fn payload(&self) -> String {
            format!("call writefile(['ran'], '{}')", self.marker.display())
        }

        fn startup_command_ran(&self) -> bool {
            self.marker.exists()
        }

        fn forget_that_it_ran(&self) {
            let _ = std::fs::remove_file(&self.marker);
        }

        /// Points a raw (funnel-less) editor invocation at this world. The
        /// control below deliberately runs without `--clean`, which skips
        /// `VIMINIT` outright (confirmed against the pinned binary: with
        /// `--clean` the planted command never runs, so a control carrying
        /// it would pass while proving nothing), and these directories are
        /// what keep such a start from reading the host's own config
        /// instead.
        fn isolate(&self, cmd: &mut Command) {
            for name in Self::XDG_HOMES {
                cmd.env(name, self.homes.join(name.to_lowercase()));
            }
            cmd.env("XDG_CONFIG_DIRS", &self.empty);
            cmd.env("XDG_DATA_DIRS", &self.empty);
        }
    }

    // Spawns nvim as both server and --remote-expr client over a real socket.
    #[cfg(unix)]
    #[test]
    fn the_probe_client_runs_none_of_the_hosts_startup_commands() {
        let world = ProbeInitWorld::plant();

        // the probe target: a real server for the client to talk to, started
        // before the plant so its own startup cannot write the marker. A
        // successful round-trip is what makes the client's silence mean
        // "ran no startup command" rather than "exited early because it
        // could not connect".
        let mut session = testenv::spawning(|| {
            let mut cmd = CommandBuilder::new("nvim");
            for arg in ["--clean", "-n", "--headless", "--listen"] {
                cmd.arg(arg);
            }
            cmd.arg(&world.sock);
            CompatSession::spawn_configured(cmd, 80, 24, PathBuf::from("nvim"), world.sock.clone())
        })
        .expect("spawning the probe target");
        session
            .await_probe_channel(Duration::from_secs(30))
            .expect("the probe target never started listening");

        let planted = testenv::plant(&[("VIMINIT", &world.payload())]);

        // control first: the same planted command reaching an editor that
        // does start up. Without it a silent probe below would equally
        // describe a payload that never worked or a variable the pinned
        // binary stopped reading.
        let mut control = planted
            .spawning(|| {
                let mut cmd = Command::new("nvim");
                cmd.args(["-n", "--headless", "-c", "qa!"]);
                world.isolate(&mut cmd);
                cmd.spawn()
            })
            .expect("spawning the control editor");
        assert!(
            control
                .wait()
                .expect("waiting for the control editor")
                .success(),
            "the control editor failed to run"
        );
        assert!(
            world.startup_command_ran(),
            "the planted startup command never ran even in an editor that \
             starts up, so this test can prove nothing about the probe client"
        );
        world.forget_that_it_ran();

        let answer = planted
            .spawning(|| session.probe("1+1"))
            .expect("the probe round-trip failed");
        drop(planted);
        assert_eq!(answer, "2", "the probe channel answered something else");
        assert!(
            !world.startup_command_ran(),
            "the probe client executed the host's startup command, so it is \
             an editor session after all and belongs behind a hermetic spawn \
             funnel like every other one"
        );

        session.pty().kill();
        let _ = session.pty().wait_for_exit(Duration::from_secs(5));
    }

    // A freshly opened pty runs its child's `stty raw -echo` asynchronously
    // relative to the parent's first write: until that command actually
    // executes, the kernel's own line discipline is still echoing input
    // back onto the screen, so a `send()` landing in that window shows up
    // as a screen change indistinguishable from one the target itself
    // produced. Both fake targets below print a marker right after their
    // `stty raw -echo`, and the driver blocks on it before sending anything,
    // so the baseline `drive_step` captures is never mid-echo -- the same
    // "observe positive evidence before proceeding" standard the fix under
    // test applies to the real pty channel applies here to the test's own
    // synchronization with its fake target.
    // Gated with the tests that block on it: both fake targets are shell
    // scripts driven over a pty, which exists on unix alone, so off unix
    // this helper would have no caller and would fail the dead-code gate.
    #[cfg(unix)]
    fn wait_for_ready_marker(session: &mut CompatSession, marker: &str) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !session.pty().screen().contains(marker) {
            assert!(
                Instant::now() < deadline,
                "fake target never printed its readiness marker {marker:?}"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    // A `Step::Send` step's pty channel and a following `probe`/`assert_absent`
    // step's subprocess channel are independent (this module's own docs);
    // nothing else in the driver ties one to the other. The fake target below
    // reads exactly the bytes a real `send` step writes, then keeps changing
    // the screen every 50ms for five more polls before its one distinguishing
    // write -- long enough that a `drive_step` returning as soon as the bytes
    // are written (rather than once the screen has genuinely gone quiet)
    // reliably races ahead of that write. Covers the ongoing-animation case
    // (the target keeps moving right up to its distinguishing write); the
    // stalls-with-no-movement-at-all case below is a distinct failure mode
    // this one does not exercise, since the target here starts changing the
    // screen on essentially its first poll.
    #[cfg(unix)]
    #[test]
    fn send_step_blocks_until_the_pty_screen_reflects_its_keys() {
        let keys = "ix<Esc>";
        let payload = resolve_send_keys(keys).expect("resolving test keys");
        let script = format!(
            "stty raw -echo; printf 'READY\\r\\n'; head -c{} >/dev/null; \
             for _ in 1 2 3 4 5; do printf .; sleep 0.05; done; \
             printf 'SETTLED\\r\\n'",
            payload.len()
        );

        let mut session = testenv::spawning(|| {
            let mut cmd = CommandBuilder::new("/bin/sh");
            cmd.arg("-c");
            cmd.arg(&script);
            CompatSession::spawn_configured(
                cmd,
                80,
                24,
                PathBuf::from("nvim"),
                PathBuf::from("/dev/null"),
            )
        })
        .expect("spawning the fake pty target");
        wait_for_ready_marker(&mut session, "READY");

        session
            .drive_step(&Step::Send {
                keys: keys.to_string(),
                confirm: None,
            })
            .expect("the send step itself must report success");

        assert!(
            session.pty().screen().contains("SETTLED"),
            "drive_step returned before the target's delayed output reached \
             the screen -- the same race a probe step reads through when it \
             runs immediately after a send step; screen:\n{}",
            session.pty().screen()
        );

        session.pty().kill();
        let _ = session.pty().wait_for_exit(Duration::from_secs(5));
    }

    // A settle check that treats an unmoved screen as confirmation needs
    // only a stall, not an ongoing animation, to read early: this target
    // reads the sent bytes, holds the screen completely unchanged for
    // longer than SEND_SETTLE_SILENCE, then makes its one distinguishing
    // write. A baseline captured after the write (rather than before it) or
    // a settle rule satisfied by "no change since polling started" (rather
    // than requiring an observed change first) both return once the stall
    // alone has outlasted the silence window, before this write ever lands.
    #[cfg(unix)]
    #[test]
    fn send_step_blocks_through_a_stall_with_no_screen_movement_at_all() {
        let keys = "ix<Esc>";
        let payload = resolve_send_keys(keys).expect("resolving test keys");
        let script = format!(
            "stty raw -echo; printf 'READY\\r\\n'; head -c{} >/dev/null; \
             sleep 0.4; printf 'SETTLED\\r\\n'",
            payload.len()
        );

        let mut session = testenv::spawning(|| {
            let mut cmd = CommandBuilder::new("/bin/sh");
            cmd.arg("-c");
            cmd.arg(&script);
            CompatSession::spawn_configured(
                cmd,
                80,
                24,
                PathBuf::from("nvim"),
                PathBuf::from("/dev/null"),
            )
        })
        .expect("spawning the fake pty target");
        wait_for_ready_marker(&mut session, "READY");

        session
            .drive_step(&Step::Send {
                keys: keys.to_string(),
                confirm: None,
            })
            .expect("the send step itself must report success");

        assert!(
            session.pty().screen().contains("SETTLED"),
            "drive_step returned during a stall in which the screen never \
             moved at all -- a settle check satisfied by an unchanged \
             screen, rather than one requiring an observed change first, \
             cannot tell an unstarted delivery from a settled one; \
             screen:\n{}",
            session.pty().screen()
        );

        session.pty().kill();
        let _ = session.pty().wait_for_exit(Duration::from_secs(5));
    }

    // A confirm probe against a socket that will never exist fails every
    // single attempt (connection refused), which is exactly the "always
    // not-yet" condition a stalled forwarding path also produces -- so this
    // is the same shape of evidence the two mechanism tests above establish
    // for the screen channel, pinned here for the probe channel's own hard-
    // failure exit instead of its success exit.
    #[cfg(unix)]
    #[test]
    fn send_step_with_confirm_hard_fails_when_the_probe_channel_never_confirms() {
        let keys = "ix<Esc>";
        let script = "stty raw -echo; sleep 5";

        let mut session = testenv::spawning(|| {
            let mut cmd = CommandBuilder::new("/bin/sh");
            cmd.arg("-c");
            cmd.arg(script);
            CompatSession::spawn_configured(
                cmd,
                80,
                24,
                PathBuf::from("nvim"),
                PathBuf::from("/nonexistent/view-compat-test.sock"),
            )
        })
        .expect("spawning the fake pty target");

        let start = Instant::now();
        let err = session
            .drive_step(&Step::Send {
                keys: keys.to_string(),
                confirm: Some(SendConfirm {
                    expr: "1".to_string(),
                    expect: "1".to_string(),
                }),
            })
            .expect_err(
                "a confirm probe against a socket that never exists must hard-fail, not \
                 silently give up the way the unconfirmed screen-based path does",
            );
        let elapsed = start.elapsed();

        assert!(
            matches!(err, CompatError::SendConfirmTimedOut { .. }),
            "expected SendConfirmTimedOut, got {err:?}"
        );
        assert!(
            elapsed < SEND_CONFIRM_DEADLINE + Duration::from_secs(2),
            "confirm loop ran well past its own {SEND_CONFIRM_DEADLINE:?} deadline: {elapsed:?}"
        );

        session.pty().kill();
        let _ = session.pty().wait_for_exit(Duration::from_secs(5));
    }

    #[test]
    fn wait_with_timeout_drains_output_larger_than_a_pipe_buffer_without_blocking_the_child() {
        let child = spawn_fixture("flood", Stdio::null(), Stdio::piped());

        let start = Instant::now();
        let output = wait_with_timeout(child, Duration::from_secs(10))
            .expect("a large-but-finite payload must not time out");
        let elapsed = start.elapsed();

        assert!(output.status.success());
        assert_eq!(output.stderr.len(), FIXTURE_PAYLOAD_LEN);
        assert!(
            elapsed < Duration::from_secs(5),
            "took {elapsed:?}, which suggests the child blocked on an \
             undrained pipe rather than exiting promptly once its output \
             was consumed"
        );
    }
}
