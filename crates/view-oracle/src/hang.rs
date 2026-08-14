//! Reproduced hang schedules: the two shapes an engine actually stops
//! serving in, driven against a real pinned nvim and folded through the
//! same supervision pieces the runtime loop folds.
//!
//! Two schedules, because there are two failures with two recoveries and
//! nothing else in between (`view_core::native::supervision`'s own module
//! doc states the distinction the affordance rests on):
//!
//! - [`HangSchedule::ReadSideWedge`] runs a synchronous Lua loop inside the
//!   engine. Nothing in that loop pumps nvim's event loop, so the engine
//!   drains view's output and answers none of it -- the failure
//!   [`view_engine::heartbeat`] exists to notice, and the one no redraw
//!   traffic could ever wake a loop to discover.
//! - [`HangSchedule::DeadConnection`] kills the child out of band, which is
//!   what a crash looks like from here: no `VimLeavePre` announcement, no
//!   exit the session asked for, and a swap file left behind for the
//!   replacement to recover from.
//!
//! A third, [`HangSchedule::BlockedOnKey`], is the control the other two
//! are only meaningful against. It parks the engine in a key-wait, which is
//! *not* a hang: nvim answers `nvim_get_mode` on receipt in exactly that
//! state (`:help api-fast`), so the liveness verdict must stay
//! [`Liveness::Alive`] right through it. That wire fact is the entire basis
//! on which a wedged engine is told apart from a busy one, and it is
//! re-verified live here rather than assumed.
//!
//! # What this module observes, and what it does not
//!
//! Every verdict below comes from production code:
//! [`HeartbeatWatch::observe`](view_engine::heartbeat::HeartbeatWatch::observe)
//! for the read side,
//! [`OutboxStallWatch::observe`] for the write side,
//! [`view_engine::heartbeat::wedge_kind`] for the classification, and
//! `view_core::update::update` for the banner, the modal and the restart
//! request. What this module owns is the *schedule* (when the engine stops
//! serving), the *clock* (how long the wedge has been observed) and the
//! *bounds* (how long a verdict may take to arrive).
//!
//! The clock is deliberately the oracle's own rather than a copy of the
//! loop's episode fold: an oracle that re-implemented the measurement it is
//! checking would agree with itself and prove nothing, so the elapsed time
//! carried into `Msg::EngineLiveness` here is measured from the first pass
//! that saw a wedge at all, independently of how the loop measures its own.
//!
//! # Safety net
//!
//! Each schedule is bounded twice. The Lua loop ends on its own after
//! [`WEDGE_LOOP`], counted on `vim.uv.hrtime` -- a real clock, read afresh
//! on every iteration -- and never on `vim.uv.now`, whose value is cached
//! once per libuv loop iteration and therefore never advances at all inside
//! a loop that returns to no iteration. And every session force-kills its
//! child on the way out: `Engine`'s teardown sends `qa!`, waits at most the
//! configured shutdown timeout for an engine that by construction is not
//! reading it, and then `SIGKILL`s.
//!
//! Both halves are load-bearing rather than belt-and-braces. A harness that
//! dies without running its own teardown -- an out-of-memory kill, a
//! `SIGKILL` from an impatient runner -- leaves the self-bound as the only
//! thing that ever ends the loop, and an engine inside one serves no signal
//! short of `SIGKILL` either, since it can only act on a `SIGTERM` from the
//! event loop it is not returning to. A loop whose bound did not really
//! count would leave an nvim spinning a core for as long as the host stayed
//! up.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{sync_channel, Receiver, SyncSender};
use std::time::{Duration, Instant};

use view_core::model::Model;
use view_core::msg::{Effect, Key, Msg, RpcCall};
use view_core::native::supervision::{WedgeKind, RESTART_NOTATION};
use view_core::update::update;
use view_engine::heartbeat::{
    wedge_kind, Liveness, HEARTBEAT_PROBE_INTERVAL, HEARTBEAT_WEDGE_THRESHOLD,
};
use view_engine::process::{Engine, EngineConfig};
use view_engine::{DamagePump, OutboxStallWatch};

use crate::OracleError;

/// How long the engine's own side of a verdict may take: one
/// [`HEARTBEAT_PROBE_INTERVAL`], because a schedule landing just after a
/// tick leaves the next probe -- the first one the engine can fail to answer
/// -- a whole interval away, plus the [`HEARTBEAT_WEDGE_THRESHOLD`] that
/// probe must then go unanswered for.
///
/// Reached rather than approached: every schedule here fires immediately
/// after a spawn whose prober sleeps a full interval before its first tick,
/// so the phase is always the worst-case one and a measured detection sits
/// on this bound rather than under it.
pub const DETECTION_BOUND: Duration = Duration::from_millis(
    HEARTBEAT_WEDGE_THRESHOLD.as_millis() as u64 + HEARTBEAT_PROBE_INTERVAL.as_millis() as u64,
);

/// What an observer costs on top of [`DETECTION_BOUND`], before any host
/// scaling (see [`observation_slack`]).
///
/// A verdict becomes true at an instant and is read by a thread that polls,
/// so no observation can land on the bound itself; and since the bound is
/// reached exactly rather than approached, every measurement here would fail
/// a comparison that allowed nothing for the reading.
///
/// This is the entire headroom a gated run has, not a generous pad: a
/// detection measured at 12.0002s has spent under a millisecond of it, and
/// what the rest is for is the host rather than the engine. Half a second
/// covers a fold cadence two orders of magnitude below it, covers an
/// ordinary scheduling hiccup, and covers nothing a supervision regression
/// could do, since the next thing that can go wrong costs a whole probe
/// interval. What it does not cover is a host that leaves a runnable thread
/// off a core for longer than that, which is a documented condition on the
/// shared machine where foreign builds run beside a gated `task ci` -- and
/// widening it there without moving what an unloaded host asserts is what
/// [`SLACK_SCALE_VAR`] is for.
pub const OBSERVATION_SLACK: Duration = Duration::from_millis(500);

/// Environment variable multiplying [`OBSERVATION_SLACK`] on a host whose
/// deschedules are longer than an observation's own cost.
///
/// Read per call rather than captured once, so a caller can widen the bound
/// for a single run. A value that is absent, unparseable or zero leaves the
/// shipped bound exactly where it is: a scale that could disable the
/// deadline would turn every schedule here into a test that cannot fail,
/// which is the one outcome worse than a flaky one.
pub const SLACK_SCALE_VAR: &str = "VIEW_ORACLE_SLACK_SCALE";

/// [`OBSERVATION_SLACK`] as this host asks for it.
#[must_use]
pub fn observation_slack() -> Duration {
    OBSERVATION_SLACK * slack_scale(std::env::var(SLACK_SCALE_VAR).ok().as_deref())
}

/// What [`SLACK_SCALE_VAR`] reading `raw` multiplies the slack by.
///
/// Split from the environment it is normally read out of so the rule can be
/// asserted without a process-wide plant, which in a test binary running
/// several schedules at once is a plant on every one of them.
fn slack_scale(raw: Option<&str>) -> u32 {
    raw.and_then(|raw| raw.trim().parse::<u32>().ok())
        .filter(|scale| *scale > 0)
        .unwrap_or(1)
}

/// [`DETECTION_BOUND`] as an observer on this host may actually assert it.
#[must_use]
pub fn detection_deadline() -> Duration {
    DETECTION_BOUND + observation_slack()
}

/// How long a replacement engine may take to answer `nvim_get_mode` after a
/// restart is asked for.
///
/// Deliberately not the handshake timeout on its own, which bounds one
/// round trip (`nvim_get_api_info` inside the spawn) and would be
/// double-booked if it also had to bound everything around it: the
/// measurement starts before the dead engine is reaped and covers that
/// reaping, the spawn, the `ui_attach` and the probe after it. What is
/// added is exactly those parts -- one shutdown timeout for an engine that
/// answers nothing, and one observation's worth of host.
#[must_use]
pub fn restart_bound() -> Duration {
    EngineConfig::default().handshake_timeout + SHUTDOWN + observation_slack()
}

/// How long a run keeps folding after it has done everything else.
///
/// The survival evidence: a harness that hung alongside the engine it was
/// watching completes a handful of folds over this window, and one that did
/// not completes thousands.
const SURVIVAL_WINDOW: Duration = HEARTBEAT_PROBE_INTERVAL;

/// How long [`HangSchedule::ReadSideWedge`]'s Lua loop spins for.
///
/// Comfortably past every bound any schedule here waits out, so a verdict
/// this module reports is never the loop having finished on its own, and
/// short enough that an engine nobody is left to tear down stops burning a
/// core within the hour rather than at the next reboot.
pub const WEDGE_LOOP: Duration = Duration::from_secs(300);

/// How long the harness waits for a fresh reading between folds.
///
/// Short enough that the fold's own granularity is noise against
/// [`DETECTION_BOUND`], long enough that the observing thread is not a spin.
const FOLD_INTERVAL: Duration = Duration::from_millis(5);

/// Terminal size every schedule runs at, matching the corpus runner's own
/// fixed canvas rather than a per-schedule choice: nothing here asserts on
/// grid content, and one shared size keeps two schedules' report lines
/// directly comparable.
const COLS: u16 = 80;
const ROWS: u16 = 24;

/// How long a session's teardown waits for an engine that is not reading
/// `qa!` before force-killing it. Short on purpose: by construction every
/// schedule here leaves an engine that cannot answer, so the graceful half
/// is spent waiting for a reply that is never coming.
const SHUTDOWN: Duration = Duration::from_millis(500);

/// The shape of failure a schedule produces.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HangSchedule {
    /// A synchronous Lua loop: the connection stays open and the engine
    /// answers nothing.
    ReadSideWedge,
    /// The child killed out of band: the connection closes and never
    /// reopens on its own.
    DeadConnection,
    /// A pending `r` replacement character: nvim's main loop is blocked
    /// waiting for a key and says so, while still answering the fast probe.
    /// The control, not a hang.
    BlockedOnKey,
}

impl HangSchedule {
    /// The report line's name for this schedule.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::ReadSideWedge => "read-side-wedge",
            Self::DeadConnection => "dead-connection",
            Self::BlockedOnKey => "blocked-on-key",
        }
    }

    /// The verdict a working supervision stack reaches for this schedule.
    #[must_use]
    pub const fn expected(self) -> Liveness {
        match self {
            Self::ReadSideWedge => Liveness::Wedged,
            Self::DeadConnection => Liveness::Dead,
            Self::BlockedOnKey => Liveness::Alive,
        }
    }
}

/// One schedule's run: which failure to produce, and how far past detection
/// to keep folding.
#[non_exhaustive]
#[derive(Debug, Clone, Copy)]
pub struct HangRun {
    /// The failure to produce.
    pub schedule: HangSchedule,
    /// Whether a connection observed dead is respawned without asking.
    pub auto_restart: bool,
    /// Whether to keep folding past detection until the sticky banner
    /// escalates into the modal.
    pub escalate: bool,
}

impl HangRun {
    /// A run of `schedule` with supervision's own shipped defaults and no
    /// escalation wait.
    #[must_use]
    pub const fn new(schedule: HangSchedule) -> Self {
        Self {
            schedule,
            auto_restart: true,
            escalate: false,
        }
    }

    /// The same run with automatic recovery turned off, which is the
    /// `[supervision] auto_restart = false` a user writes in `view.toml`.
    #[must_use]
    pub const fn attended(mut self) -> Self {
        self.auto_restart = false;
        self
    }

    /// The same run, folded on past detection until the modal opens.
    #[must_use]
    pub const fn escalating(mut self) -> Self {
        self.escalate = true;
        self
    }
}

/// What one schedule's run observed.
#[non_exhaustive]
#[derive(Debug, Clone)]
pub struct HangReport {
    /// The schedule that produced it.
    pub schedule: HangSchedule,
    /// The read-side verdict the run ended on.
    pub verdict: Liveness,
    /// The wedge that verdict classified to, if any.
    pub wedge: Option<WedgeKind>,
    /// How long after the schedule fired the expected verdict first
    /// appeared, or -- for a control whose verdict never changes -- how long
    /// that verdict was held for. `None` when the expected verdict was never
    /// reached within [`detection_deadline`], or when a control lost the one
    /// it started with.
    pub detected_after: Option<Duration>,
    /// How many folds the harness completed between the schedule firing and
    /// the run ending. Evidence that the observing side kept running while
    /// the engine did not: a harness that hung with the engine would report
    /// a handful, not thousands.
    pub folds: u64,
    /// The sticky banner's text at the end of the run, if one was raised.
    pub banner: Option<String>,
    /// The wedge the modal was opened for, if the escalation reached one.
    pub offered: Option<WedgeKind>,
    /// The whole seconds the modal was showing for that wedge, read at the
    /// same moment as [`offered`](Self::offered).
    pub offered_readout: Option<u64>,
    /// How long the oracle's own clock had been reading a wedge when
    /// [`offered_readout`](Self::offered_readout) was taken, so the two can
    /// be held against each other: the modal counts a duration the fold
    /// hands it, and a readout that stopped counting is a readout that
    /// stopped being true.
    pub wedged_for: Option<Duration>,
    /// Whether a restart was requested with nobody having answered a modal.
    pub unattended: bool,
    /// How long after the restart was requested the replacement answered
    /// `nvim_get_mode`, when a restart happened at all.
    pub restarted_after: Option<Duration>,
    /// The first line of the replacement engine's buffer, when the schedule
    /// left a swap file for it to recover.
    pub recovered_line: Option<String>,
    /// The read-side verdict over the replacement connection, folded through
    /// the same watch that condemned its predecessor. `None` when no restart
    /// happened.
    pub replacement_verdict: Option<Liveness>,
}

impl HangReport {
    /// Whether the run was what its schedule promised: the expected verdict,
    /// reached inside [`detection_deadline`], and -- where the schedule
    /// asked for a replacement -- a replacement the same watch reads as
    /// alive.
    ///
    /// The reading lives beside the report rather than in whatever runs it,
    /// so a manual reproduction and a gated assertion cannot disagree about
    /// what a passing schedule is.
    #[must_use]
    pub fn is_success(&self) -> bool {
        let timing = match self.schedule.expected() {
            // a control is proved by lasting, not by arriving: its verdict
            // never changes, so what it owes is having stayed `Alive` for at
            // least as long as a real wedge would have taken to report
            Liveness::Alive => self
                .detected_after
                .is_some_and(|held| held >= DETECTION_BOUND),
            _ => self
                .detected_after
                .is_some_and(|elapsed| elapsed <= detection_deadline()),
        };
        timing
            && self.verdict == self.schedule.expected()
            && self
                .replacement_verdict
                .is_none_or(|verdict| verdict == Liveness::Alive)
    }

    /// One run's report line, in the corpus runner's own report shape.
    #[must_use]
    pub fn report_line(&self) -> String {
        let status = if self.is_success() {
            "DETECTED"
        } else {
            "MISSED"
        };
        let restart = self.restarted_after.map_or(String::new(), |elapsed| {
            format!(", replacement answered after {elapsed:?}")
        });
        format!(
            "oracle: hang {} ... {status} ({:?} after {:?}, {} folds{restart})",
            self.schedule.label(),
            self.verdict,
            self.detected_after,
            self.folds,
        )
    }
}

/// The Lua chunk that wedges the read side for `budget`.
///
/// A Lua `while`, not a Vimscript one: Vimscript's break check pumps the
/// event loop, so an engine inside it goes on answering and there is no
/// wedge to detect. Typed as ordinary input rather than requested, because a
/// caller waiting on the reply to work that never returns could not go on to
/// observe anything (live-verified in `crates/view/tests/supervision_live.rs`,
/// which pins both halves of that distinction against the pinned engine).
///
/// `vim.uv.hrtime`, never `vim.uv.now`: the latter reports libuv's
/// loop-cached time, refreshed at the top of each loop iteration, so a loop
/// that reaches no iteration reads the same value forever and a budget
/// written against it never expires. `hrtime` reads the monotonic clock on
/// the spot, which is the only kind of self-bound a chunk like this one can
/// actually hold (nanoseconds, and a 64-bit count of them stays exact in a
/// Lua number for a century of uptime).
///
/// The comparison is written the long way round, subtracting the elapsed
/// time from the budget and testing `> 0`, because typed input carries no
/// `<`: a `<` opens a key-notation parse that runs to the next `>`, which
/// here is the one closing the `<CR>` that would have submitted the command
/// line. An engine sent a `<` in this position never runs the chunk at all;
/// it sits at a `:` prompt, answers everything, and reads perfectly alive.
fn wedge_chunk_for(budget: Duration) -> String {
    format!(
        ":lua local t=vim.uv.hrtime() while {} - (vim.uv.hrtime()-t) > 0 do end<CR>",
        budget.as_nanos()
    )
}

/// Kills `pid` out of band, the way a crash arrives: no shutdown sequence,
/// no `VimLeavePre` announcement, nothing for the session to have asked for.
///
/// Per platform, because a simulated crash has to be a real one on the host
/// actually running it and neither platform's mechanism exists on the other.
/// Deliberately not [`crate::kill_process_group`], whose contract is a pty
/// child spawned as a session leader: an `Engine`'s child is spawned with
/// no `setsid` at all, so its pid names no process group, and a group signal
/// aimed at it would either find nothing or land on an unrelated group that
/// happens to carry that id.
#[cfg(unix)]
fn kill_out_of_band(pid: u32) -> Result<(), OracleError> {
    let pid =
        i32::try_from(pid).map_err(|_| OracleError::Pty(format!("pid {pid} out of range")))?;
    nix::sys::signal::kill(
        nix::unistd::Pid::from_raw(pid),
        nix::sys::signal::Signal::SIGKILL,
    )
    .map_err(|e| OracleError::Pty(format!("SIGKILL to pid {pid}: {e}")))
}

/// See the Unix arm for the contract. Windows has no signals, so the
/// force-kill is `taskkill /F`, the same lever `view-engine`'s own crash
/// tests reach for there.
#[cfg(windows)]
fn kill_out_of_band(pid: u32) -> Result<(), OracleError> {
    let status = std::process::Command::new("taskkill")
        .args(["/F", "/PID", &pid.to_string()])
        .status()?;
    if status.success() {
        Ok(())
    } else {
        Err(OracleError::Pty(format!(
            "taskkill /F /PID {pid} failed: {status:?}"
        )))
    }
}

/// Disambiguates concurrently-generated scratch worlds, the same role the
/// corpus runner's own counter plays for its compat scratch paths.
static SCRATCH_COUNTER: AtomicU64 = AtomicU64::new(0);

/// A directory under the build tree -- never the system temp root, which is
/// world-writable -- for the file a recovery schedule edits and the swap
/// nvim writes beside it. Emptied first, so a directory an earlier run
/// leaked can never be read as this run's own state.
///
/// Named per run, not per schedule: several runs of one schedule execute
/// concurrently under `cargo test`, and a shared directory means each
/// emptying wipes the file and the swap another run is mid-recovery on --
/// which surfaces as an empty recovered buffer, indistinguishable from a
/// recovery that genuinely failed. Both the process id and the counter are
/// needed, since `cargo test` runs each integration binary as its own
/// process against the same build tree.
fn scratch(label: &str) -> Result<PathBuf, OracleError> {
    let mut root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    root.pop();
    root.pop();
    let run = SCRATCH_COUNTER.fetch_add(1, Ordering::Relaxed);
    let dir = root
        .join("target")
        .join("view-oracle-hang")
        .join(format!("{label}-{}-{run}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("swap"))?;
    Ok(dir)
}

/// The config a schedule that must leave a recoverable swap file spawns
/// with: swap enabled (unlike `EngineConfig::isolated`, which passes `-n`),
/// every swap file written under `dir`, and the file itself opened.
///
/// The `'directory'` pin rides a `--cmd` because nvim runs those before it
/// opens any file, and so before the first swap file is written; a `-c`
/// would arrive after the buffer that already made one. It is written as Lua
/// with a long-bracket string so a path carrying separators, spaces or
/// commas needs no `:set` escaping.
///
/// Isolation is spelled out here rather than borrowed from
/// `EngineConfig::isolated`'s hermetic funnel, which cannot be used at all:
/// that funnel passes `-n`, and a session with no swap file has nothing to
/// leave behind for a recovery to find. The names below are the ones this
/// crate's own tests plant process-wide (see `crate::testenv`), so a child
/// spawned inside a concurrent plant's window would otherwise inherit them
/// -- and inheriting them harmlessly under `--clean` is a coincidence
/// between two tests' flags rather than a property either one states.
fn recoverable(dir: &Path, file: &Path) -> EngineConfig {
    EngineConfig::default()
        .with_arg("--clean")
        .with_arg("--cmd")
        .with_arg(format!(
            "lua vim.o.directory = [[{}//]]",
            dir.join("swap").display()
        ))
        .with_arg(file)
        .with_env("HOME", dir)
        .with_env("XDG_CONFIG_HOME", dir.join("config"))
        .with_env("XDG_DATA_HOME", dir.join("data"))
        .with_env("XDG_STATE_HOME", dir.join("state"))
        .with_env_remove("VIMINIT")
        .with_env_remove("XDG_CONFIG_DIRS")
        .with_shutdown_timeout(SHUTDOWN)
}

/// A real engine, a real `Model`, and the supervision fold between them:
/// the smallest thing that can watch an engine stop serving and act on it
/// the way the runtime loop does.
pub struct HangSession {
    engine: Engine,
    pump: DamagePump,
    sink: SyncSender<Msg>,
    rx: Receiver<Msg>,
    model: Model,
    write: OutboxStallWatch,
    /// The loop's own resolution of a stop into a death worth recovering
    /// from, never the connection's closed flag alone: `:q` closes the
    /// connection exactly the way a crash does, and only a resolved exit
    /// tells the two apart.
    lost: bool,
    /// When the first pass that saw any wedge landed. The oracle's clock,
    /// independent of the loop's own episode fold (see the module doc).
    since: Option<Instant>,
    folds: u64,
    restart_requested: bool,
    /// What the most recent fold classified, so a caller can read the
    /// verdict a fold reached without folding again: a second fold over a
    /// dead connection would spend another of the session's own bounded
    /// automatic recoveries, which is a thing the runtime loop never does.
    last_wedge: Option<WedgeKind>,
}

impl HangSession {
    /// Spawns an engine from `cfg`, attaches a UI, and returns a session
    /// ready for a schedule to fire against.
    ///
    /// The pump is attached before the UI is, matching the runtime's own
    /// startup order: `start_pump` is what arms the heartbeat, and a probe
    /// issued before a consumer exists to fold its reply would be charged to
    /// the engine as silence it never owed.
    ///
    /// # Errors
    ///
    /// Returns [`OracleError::Engine`] if the process fails to spawn or the
    /// `ui_attach` handshake fails or times out.
    pub fn spawn(cfg: EngineConfig) -> Result<Self, OracleError> {
        let mut engine = Engine::spawn(cfg)?;
        let (sink, rx) = sync_channel::<Msg>(256);
        let (pump, _cutover) = engine.start_pump(sink.clone());
        engine.handle.ui_attach(COLS, ROWS)?;
        Ok(Self {
            engine,
            pump,
            sink,
            rx,
            model: Model::with_term_size(COLS, ROWS),
            write: OutboxStallWatch::default(),
            lost: false,
            since: None,
            folds: 0,
            restart_requested: false,
            last_wedge: None,
        })
    }

    /// The model this session's folds mutate, for a caller asserting on what
    /// a user would see.
    #[must_use]
    pub fn model(&self) -> &Model {
        &self.model
    }

    /// This session's engine, for a probe a schedule needs to make directly.
    #[must_use]
    pub fn engine(&self) -> &Engine {
        &self.engine
    }

    /// How many folds this session has completed.
    #[must_use]
    pub fn folds(&self) -> u64 {
        self.folds
    }

    /// How long the session has been reading some wedge, by the oracle's own
    /// clock rather than the model's, so a caller can hold the two against
    /// each other.
    #[must_use]
    pub fn wedged_for(&self) -> Option<Duration> {
        self.since.map(|opened| opened.elapsed())
    }

    /// De-wires the heartbeat: the prober stops issuing probes, so the read
    /// side has nothing outstanding to time and no silence to report.
    ///
    /// The lever a regression would pull by accident, offered on purpose, so
    /// that "these schedules detect because the heartbeat is wired" is a
    /// gated assertion rather than an experiment somebody once ran.
    pub fn pause_heartbeat(&self) {
        self.engine.heartbeat.pause();
    }

    /// Fires `schedule` and returns the instant it fired at.
    ///
    /// # Errors
    ///
    /// Returns [`OracleError::Engine`] if the engine cannot be written to,
    /// or [`OracleError::Pty`] if the out-of-band kill fails.
    pub fn fire(&mut self, schedule: HangSchedule) -> Result<Instant, OracleError> {
        match schedule {
            HangSchedule::ReadSideWedge => return self.fire_bounded_wedge(WEDGE_LOOP),
            HangSchedule::BlockedOnKey => self.engine.handle.input("r")?,
            HangSchedule::DeadConnection => kill_out_of_band(self.engine.pid())?,
        }
        Ok(Instant::now())
    }

    /// [`HangSchedule::ReadSideWedge`] with the loop's self-bound named by
    /// the caller, so the bound itself can be put under test: a budget short
    /// enough to sit out is the only way to observe that the loop ends on a
    /// clock rather than on a kill.
    ///
    /// # Errors
    ///
    /// Returns [`OracleError::Engine`] if the engine cannot be written to.
    pub fn fire_bounded_wedge(&mut self, budget: Duration) -> Result<Instant, OracleError> {
        self.engine.handle.input(&wedge_chunk_for(budget))?;
        Ok(Instant::now())
    }

    /// Drains everything the connection has posted, applying each message
    /// through the same `update()` production drives, and folds one
    /// supervision reading out of both watches.
    ///
    /// Never blocks on the engine: the drain is a `try_recv` loop and the
    /// two observations are atomic loads, which is the only reason a fold
    /// can be asked for while the engine answers nothing.
    pub fn fold(&mut self) -> Option<WedgeKind> {
        self.drain();
        self.folds = self.folds.saturating_add(1);
        let stalled = self.write.observe(&self.engine.handle);
        let closed = self.lost || self.engine.handle.is_closed();
        let observed = wedge_kind(stalled, self.engine.heartbeat.observe(closed), self.lost);
        let now = Instant::now();
        self.since = match (observed, self.since) {
            (None, _) => None,
            (Some(_), opened @ Some(_)) => opened,
            (Some(_), None) => Some(now),
        };
        let observed_for = self.since.map_or(Duration::ZERO, |opened| {
            now.saturating_duration_since(opened)
        });
        let effects = update(
            &mut self.model,
            Msg::EngineLiveness {
                wedge: observed,
                observed_for,
            },
        );
        self.apply(effects);
        self.last_wedge = observed;
        observed
    }

    /// What the most recent fold classified.
    #[must_use]
    pub fn last_wedge(&self) -> Option<WedgeKind> {
        self.last_wedge
    }

    /// The read side's verdict on its own, without the classification.
    #[must_use]
    pub fn liveness(&self) -> Liveness {
        self.engine
            .heartbeat
            .observe(self.lost || self.engine.handle.is_closed())
    }

    /// Folds until `want` is observed *and* folded through, or `bound`
    /// elapses from `fired`, returning how long it took. `None` means the
    /// bound ran out first.
    pub fn await_liveness(
        &mut self,
        want: Liveness,
        fired: Instant,
        bound: Duration,
    ) -> Option<Duration> {
        while fired.elapsed() < bound {
            let _ = self.fold();
            if self.settled_on(want) {
                return Some(fired.elapsed());
            }
            std::thread::sleep(FOLD_INTERVAL);
        }
        // one last fold, so a verdict that landed inside the final sleep is
        // still folded before the answer is given: a verdict past the bound
        // is a miss either way, and reporting how late it was beats
        // reporting that it never came
        let _ = self.fold();
        self.settled_on(want).then(|| fired.elapsed())
    }

    /// Whether the read side reads `want` *and* the fold that read it
    /// reached a decision, rather than the raw verdict alone.
    ///
    /// The distinction is a real race, not pedantry. The reader publishes a
    /// death in two steps -- a blocking `Msg::EngineStopped` send, then the
    /// handle's closed flag -- so a fold whose drain runs between them sees
    /// a closed connection with the stop still unresolved, and classifies
    /// nothing at all (a stop that has not been resolved may be a `:q`,
    /// which is not a death to recover from). A caller that returned on the
    /// raw verdict would take its one-shot decision inside exactly that
    /// window and find no restart to act on. The runtime never resolves a
    /// death from the first reading either; it folds until the facts arrive.
    fn settled_on(&self, want: Liveness) -> bool {
        if self.liveness() != want {
            return false;
        }
        want == Liveness::Alive || self.last_wedge.is_some()
    }

    /// Keeps folding for `window`, so a caller can prove a verdict *stays*
    /// what it was rather than catching one instant of it.
    pub fn fold_for(&mut self, window: Duration) {
        let until = Instant::now() + window;
        while Instant::now() < until {
            let _ = self.fold();
            std::thread::sleep(FOLD_INTERVAL);
        }
    }

    /// Whether a fold has produced `Effect::RestartEngine` since the last
    /// time this was asked. Deferred rather than acted on inside the effect
    /// batch, exactly as the runtime defers it: the replacement happens
    /// once, off any batch, with nothing borrowed from the engine it
    /// replaces.
    pub fn take_restart_request(&mut self) -> bool {
        std::mem::replace(&mut self.restart_requested, false)
    }

    /// Routes one keystroke the way the input reader delivers it, and
    /// returns whether it asked for a restart.
    pub fn press(&mut self, notation: &str) -> bool {
        let effects = update(
            &mut self.model,
            Msg::Key(Key {
                notation: notation.to_string(),
            }),
        );
        self.apply(effects);
        self.take_restart_request()
    }

    /// Replaces the engine per `cfg` and returns how long the replacement
    /// took to answer `nvim_get_mode`.
    ///
    /// The measurement starts before the teardown, not after it: what a
    /// caller is owed is the whole gap between asking for a restart and
    /// having an editor again, and the dead engine's reaping is part of it.
    ///
    /// # Errors
    ///
    /// The shapes `Engine::spawn` returns, for the same reasons: a restart
    /// that fails leaves no engine at all.
    pub fn restart(self, cfg: EngineConfig) -> Result<(Self, Duration), OracleError> {
        let Self {
            engine,
            pump: _,
            sink,
            rx,
            mut model,
            write,
            since,
            folds,
            ..
        } = self;
        let asked = Instant::now();
        let mut engine = engine.restart(cfg)?;
        let (pump, _cutover) = engine.start_pump(sink.clone());
        let (width, height) = model.grid_target();
        engine.handle.ui_attach(width, height)?;
        engine.handle.get_mode()?;
        let answered = asked.elapsed();
        // the replacement is a live connection again, so nothing about the
        // dead one's outage is still true: leaving the episode on record
        // would keep the banner re-asserting over an engine that answers
        model.supervision.forget_episode();
        Ok((
            Self {
                engine,
                pump,
                sink,
                rx,
                model,
                write,
                lost: false,
                since,
                folds,
                restart_requested: false,
                last_wedge: None,
            },
            answered,
        ))
    }

    /// The current buffer's first line, as nvim reports it.
    ///
    /// # Errors
    ///
    /// Returns [`OracleError::Engine`] if the engine does not answer.
    pub fn first_line(&self) -> Result<String, OracleError> {
        self.engine
            .handle
            .eval_str("getline(1)")
            .map_err(Into::into)
    }

    /// Replaces the buffer's text and flushes nvim's swap file without ever
    /// writing the file to disk, so a kill that follows leaves exactly what
    /// a crash leaves.
    ///
    /// `:preserve` is nvim's own request for that flush now, rather than on
    /// its `'updatetime'`/`'updatecount'` schedule, so a schedule built on
    /// this measures the recovery instead of the flush timer.
    ///
    /// # Errors
    ///
    /// Returns [`OracleError::Engine`] if the engine does not answer either
    /// call.
    pub fn write_unsaved(&self, text: &str) -> Result<(), OracleError> {
        self.engine
            .handle
            .eval_str(&format!("setline(1, '{text}')"))?;
        self.engine.handle.command("preserve")?;
        Ok(())
    }

    /// The sticky banner's text, if one is raised.
    #[must_use]
    pub fn banner(&self) -> Option<String> {
        self.model
            .engine
            .messages
            .visible_lines(4)
            .into_iter()
            .map(|spans| spans.into_iter().map(|s| s.text).collect::<String>())
            .find(|line| !line.is_empty())
    }

    /// Everything the connection has posted, applied the way the runtime's
    /// own intake applies it.
    fn drain(&mut self) {
        while let Ok(msg) = self.rx.try_recv() {
            let Some(msg) = self.intake(msg) else {
                continue;
            };
            let effects = update(&mut self.model, msg);
            self.apply(effects);
        }
    }

    /// One message, resolved the way the runtime's `intake` resolves it: a
    /// redraw token becomes the compacted batch behind it, a heartbeat reply
    /// is acknowledged on this same thread before the verdict that reads it
    /// is folded, and a stop belonging to a connection already replaced is
    /// dropped rather than acted on against the engine now running.
    fn intake(&mut self, msg: Msg) -> Option<Msg> {
        Some(match msg {
            Msg::RedrawReady => Msg::Redraw(self.pump.take_damage()),
            Msg::HeartbeatReply { generation } => {
                self.engine.heartbeat.record_ack(generation);
                Msg::HeartbeatReply { generation }
            }
            Msg::EngineStopped { generation, .. } if generation != self.engine.generation() => {
                return None
            }
            Msg::EngineStopped { generation, reason } => {
                let (exit, announced) = self.engine.stop_report();
                self.lost = self.model.supervision.note_engine_stop(exit, announced);
                Msg::EngineStopped { generation, reason }
            }
            other => other,
        })
    }

    /// Forwards the effects a fold produced.
    ///
    /// `Effect::Rpc` goes to the real engine for the narrow set of calls a
    /// supervision fold can produce; a write to a wedged or closed
    /// connection is dropped, because that failure is the very condition
    /// being observed and there is no second recovery to hand it to.
    /// `Effect::RestartEngine` is recorded rather than performed, so the
    /// replacement happens off the batch.
    fn apply(&mut self, effects: Vec<Effect>) {
        for effect in effects {
            match effect {
                Effect::RestartEngine => self.restart_requested = true,
                Effect::Rpc(RpcCall::Input { notation }) => {
                    let _ = self.engine.handle.input(&notation);
                }
                Effect::Rpc(RpcCall::TryResize { width, height }) => {
                    let _ = self.engine.handle.try_resize(width, height);
                }
                _ => {}
            }
        }
    }
}

/// Drives one schedule end to end and reports what it observed: the single
/// execution engine every test below and the `oracle hang` runner share, so
/// a manual reproduction and a gated assertion see identical semantics.
///
/// # Errors
///
/// Returns [`OracleError`] if the engine cannot be spawned, the schedule
/// cannot be fired, or a restart's replacement never comes up.
pub fn run_schedule(run: HangRun) -> Result<HangReport, OracleError> {
    let recovers = run.schedule == HangSchedule::DeadConnection;
    let dir = recovers
        .then(|| scratch(run.schedule.label()))
        .transpose()?;
    let file = dir.as_ref().map(|dir| dir.join("doc.txt"));
    if let Some(file) = &file {
        std::fs::write(file, "what is on disk\n")?;
    }

    let cfg = match (&dir, &file) {
        (Some(dir), Some(file)) => recoverable(dir, file),
        _ => EngineConfig::isolated().with_shutdown_timeout(SHUTDOWN),
    };
    let mut session = HangSession::spawn(cfg)?;
    session.model.supervision.auto_restart = run.auto_restart;
    if file.is_some() {
        session.write_unsaved("never written to disk")?;
    }

    let fired = session.fire(run.schedule)?;
    let want = run.schedule.expected();
    let detected_after = match want {
        // the control never changes verdict, so there is no arrival to wait
        // for: it is proved by staying Alive for as long as a wedge would
        // have taken to be reported
        Liveness::Alive => {
            session.fold_for(detection_deadline());
            (session.liveness() == Liveness::Alive).then(|| fired.elapsed())
        }
        _ => session.await_liveness(want, fired, detection_deadline()),
    };
    let verdict = session.liveness();
    let wedge = session.last_wedge();

    if run.escalate {
        session.fold_for(
            view_core::native::supervision::ENGINE_BUSY_MODAL_THRESHOLD + HEARTBEAT_PROBE_INTERVAL,
        );
    }

    let unattended = session.take_restart_request();
    let open = session.model.engine_busy();
    let offered = open.map(|open| open.kind);
    let offered_readout = open.map(|open| open.since.readout());
    // taken beside the readout rather than at the end, so the two describe
    // the same instant of the same wedge
    let wedged_for = offered.and(session.wedged_for());
    let banner = session.banner();
    // the manual half of the same recovery: with automatic restart off, the
    // modal stays up until the user picks it, and that keystroke is the only
    // thing that asks for a replacement
    let requested =
        unattended || (offered == Some(WedgeKind::Dead) && session.press(RESTART_NOTATION));

    let (mut session, restarted_after, recovered_line) = if requested {
        let cfg = match (&dir, &file) {
            (Some(dir), Some(file)) => recoverable(dir, file),
            _ => EngineConfig::isolated().with_shutdown_timeout(SHUTDOWN),
        };
        let (session, elapsed) = session.restart(cfg)?;
        let line = match &file {
            Some(_) => Some(session.first_line()?),
            None => None,
        };
        (session, Some(elapsed), line)
    } else {
        (session, None, None)
    };

    // read last, and only after the survival window: what makes the fold
    // count evidence is that it was still climbing after everything else the
    // run did, and what makes a replacement's verdict evidence is that the
    // same watch had time to probe it
    session.fold_for(SURVIVAL_WINDOW);
    let replacement_verdict = requested.then(|| session.liveness());

    Ok(HangReport {
        schedule: run.schedule,
        verdict,
        wedge,
        detected_after,
        folds: session.folds(),
        banner,
        offered,
        offered_readout,
        wedged_for,
        unattended,
        restarted_after,
        recovered_line,
        replacement_verdict,
    })
}

#[cfg(test)]
mod tests;
