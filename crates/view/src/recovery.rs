//! Bringing an engine back, and deciding when to.
//!
//! [`crate::runtime`]'s loop drives one engine at a time. Everything about
//! there being a *next* one lives here: the session bundle the loop is
//! handed, the resolution of a failed write into either a recovery or the
//! session's own ending, and the replacement procedure itself.
//!
//! What a restart recovers is whatever nvim's own swap files already hold,
//! and nothing else. No view subsystem holds buffer text, so there is
//! nothing here that reconciles one against a fresh engine, and no
//! keystroke is ever replayed into one.

use std::sync::mpsc;

use view_core::model::Model;
use view_core::msg::{ExitInfo, Msg};
use view_core::native::supervision::ReconnectProgress;
use view_engine::handle::EngineHandle;
use view_engine::process::Engine;

use crate::engine_ops::EngineOps;
use crate::osc52::Osc52Job;
use crate::runtime::{dispatch, Executor, Flow, FollowUps};

/// What the loop has resolved about its engine, carried between passes and
/// between the three sites that dispatch into it.
///
/// Both flags are answers the loop reached and no other, which is why they
/// live here rather than being re-derived from the connection: whether a
/// stop was a death or the session's own ending is a judgement made once
/// (see [`crate::runtime`]'s `intake`), and whether a restart is owed is a choice made by the
/// user or by the auto-recovery rule, never by a reading.
#[derive(Debug, Default)]
pub(crate) struct LoopState {
    /// The connection is gone and its stop was resolved as a death worth
    /// recovering from, so the supervision fold may finally reach
    /// [`WedgeKind::Dead`]. Cleared by the restart that answers it.
    pub(crate) connection_lost: bool,
    /// A restart is owed at the top of the next pass. Deferred rather than
    /// performed where it is asked for, so the replacement happens once,
    /// off any effect batch, with nothing borrowed from the engine it
    /// replaces.
    pub(crate) restart_requested: bool,
}

/// Whether replacing `engine` is a reconnect -- spaced by the backoff and
/// capped -- or an ordinary restart, which is due the moment it is asked for.
///
/// Both halves are load-bearing. A connection that is still open is being
/// replaced because the user asked, not because it went away, and making
/// them wait a second for a restart they pressed a key for would be a
/// regression they can see. A local engine that died has no far side to
/// become reachable again: its process is gone, the next `spawn` either
/// works or does not, and spacing the retries would only delay the answer.
pub(crate) fn reconnects(engine: &Engine, connection_lost: bool) -> bool {
    engine.is_remote() && connection_lost
}

/// When the loop's next replacement attempt is due, and how many of a
/// dropped remote connection's attempts are left.
///
/// A dropped ssh connection is the failure a remote session actually meets,
/// and it is the one where retrying at once is wrong: the host is often
/// briefly unreachable (a laptop asleep, a VPN blip, a bastion recycling),
/// and an unconditional retry spins the local client against it. So a dead
/// remote connection's attempts are spaced by
/// [`remote_reconnect_backoff`] and capped at
/// [`REMOTE_RECONNECT_MAX_ATTEMPTS`], while every other replacement -- a
/// local engine's, and a user's own restart of a connection that is still
/// open -- stays due the moment it is asked for. A crashed local process is
/// not going to become reachable by waiting.
///
/// Nothing here sleeps. The schedule answers *when*, and the loop's own
/// bounded wait is what gets there: the paint loop is exactly the thread
/// that must not be parked while a banner it owns is counting attempts.
pub(crate) struct ReconnectSchedule {
    /// The wait before the first attempt of a backoff sequence, doubled per
    /// attempt already spent.
    base: std::time::Duration,
    /// How many attempts one sequence is allowed.
    max_attempts: u32,
    /// When the next attempt is due, or `None` when none is owed.
    due: Option<std::time::Instant>,
    /// How many attempts the current backoff sequence has already spent, or
    /// `None` when the pending attempt is not part of one.
    spent: Option<u32>,
}

impl Default for ReconnectSchedule {
    /// The shipped backoff, with nothing scheduled.
    fn default() -> Self {
        Self::new(
            view_engine::REMOTE_RECONNECT_BACKOFF_BASE,
            view_engine::REMOTE_RECONNECT_MAX_ATTEMPTS,
        )
    }
}

impl ReconnectSchedule {
    /// A schedule with `base` and `max_attempts` in place of the shipped
    /// ones, so a test can prove the sequence against waits it can afford to
    /// actually wait out.
    pub(crate) fn new(base: std::time::Duration, max_attempts: u32) -> Self {
        Self {
            base,
            max_attempts,
            due: None,
            spent: None,
        }
    }

    /// Records that a replacement has been asked for, and decides when it
    /// happens: at once, or on the backoff a dropped remote connection is
    /// owed.
    ///
    /// A request arriving while a sequence is still counting down changes
    /// nothing -- the attempt it would ask for is already scheduled. One
    /// arriving after a sequence has run out is the user's own restart from
    /// the modal, and it is due immediately: they have waited out the whole
    /// sequence already.
    pub(crate) fn request(&mut self, backoff: bool, now: std::time::Instant) {
        if !backoff {
            self.spent = None;
            self.due = Some(now);
            return;
        }
        match self.spent {
            None => {
                self.spent = Some(0);
                self.due = Some(now + self.wait_before(1));
            }
            Some(_) if self.due.is_none() => self.due = Some(now),
            Some(_) => {}
        }
    }

    /// Whether an attempt is due now, taking it: a caller that is told
    /// `true` owes the attempt, and the sequence has already counted it.
    pub(crate) fn take_due(&mut self, now: std::time::Instant) -> bool {
        if !self.due.is_some_and(|due| now >= due) {
            return false;
        }
        self.due = None;
        self.spent = self.spent.map(|spent| spent.saturating_add(1));
        true
    }

    /// Records an attempt that failed, answering whether the schedule
    /// absorbed it.
    ///
    /// `false` for an attempt that was never part of a sequence: a local
    /// engine that cannot be replaced leaves the session with nothing to run
    /// and nothing to retry, which its caller reports rather than retries.
    /// `true` either schedules the next attempt or ends the sequence, and
    /// both are states the session keeps running in.
    pub(crate) fn note_failure(&mut self, now: std::time::Instant) -> bool {
        let Some(spent) = self.spent else {
            return false;
        };
        self.due = (spent < self.max_attempts).then(|| now + self.wait_before(spent + 1));
        true
    }

    /// Forgets the sequence, for a replacement that came up.
    pub(crate) fn clear(&mut self) {
        self.due = None;
        self.spent = None;
    }

    /// Whether an attempt is scheduled at all.
    ///
    /// Asked before either clock-reading question below it, so the pass that
    /// has nothing scheduled -- which is every pass of a healthy session --
    /// answers this one bool and reads no clock at all.
    pub(crate) fn armed(&self) -> bool {
        self.due.is_some()
    }

    /// How long the loop may wait before an attempt would be overdue, or
    /// `None` when none is scheduled.
    pub(crate) fn poll_deadline(&self, now: std::time::Instant) -> Option<std::time::Duration> {
        self.due.map(|due| due.saturating_duration_since(now))
    }

    /// Where the sequence has got to, for the banner that announces it.
    /// `None` outside a sequence, which is every immediate replacement.
    pub(crate) fn progress(&self) -> Option<ReconnectProgress> {
        self.spent.map(|spent| {
            ReconnectProgress::new(
                spent
                    .saturating_add(1)
                    .min(self.max_attempts.saturating_add(1)),
                self.max_attempts,
            )
        })
    }

    /// The wait owed before `attempt`, from this schedule's own base.
    fn wait_before(&self, attempt: u32) -> std::time::Duration {
        view_engine::remote_reconnect_backoff(self.base, attempt)
    }
}

/// The worker channels the loop's executor is wired to.
///
/// Kept beside the executor rather than moved into it, because every worker
/// behind them outlives any single engine: a restart rebuilds the executor
/// around a new [`EngineHandle`] and re-wires these same channels to it,
/// which is what keeps a recovered session's clipboard registers, queued
/// toasts and matcher worker the ones it already had (see
/// [`crate::clipboard::ReplyRoute`] for the one that must be re-pointed as
/// well as cloned).
pub(crate) struct LoopChannels {
    pub(crate) clipboard: mpsc::Sender<crate::clipboard::ClipboardJob>,
    pub(crate) osc52: mpsc::Sender<Osc52Job>,
    pub(crate) picker: mpsc::Sender<view_native::picker::matcher::WorkerRequest>,
    pub(crate) msg: crate::wake::LoopSender,
    /// The project's agent session worker -- see `Executor`'s own `ai`
    /// field doc for why this outlives any single engine the same way the
    /// other three do: a session already running, or already being spawned,
    /// must not be torn down and re-spawned just because the engine
    /// underneath it restarted.
    pub(crate) ai: crate::ai_worker::AiWorker,
    /// The context worker's job channel -- outlives any single engine the
    /// same way `clipboard`/`osc52`/`picker` do; the worker behind it is
    /// re-pointed at a restart's fresh engine through its own
    /// [`crate::ai_context_worker::OpsRoute`], not rebuilt.
    pub(crate) ai_context: mpsc::Sender<crate::ai_context_worker::AiContextJob>,
}

impl LoopChannels {
    /// The executor for `handle`, wired to every worker this session owns.
    ///
    /// `reply_epoch` is the clipboard route's count of engines *after* it
    /// has been re-pointed at `handle`, so the jobs this executor queues are
    /// distinguishable from the ones its predecessor left outstanding (see
    /// [`crate::clipboard::ReplyRoute::epoch`]).
    pub(crate) fn executor(
        &self,
        handle: EngineHandle,
        reply_epoch: u64,
    ) -> Executor<EngineHandle> {
        Executor::new(handle)
            .with_clipboard(self.clipboard.clone())
            .with_reply_epoch(reply_epoch)
            .with_osc52(self.osc52.clone())
            .with_toast_timer(self.msg.clone())
            .with_picker(self.picker.clone())
            .with_ai(self.ai.clone())
            .with_ai_context(self.ai_context.clone())
    }
}

/// The engine half of one session: the connection [`run`] drives, the
/// damage pump already attached to it, and the factory a restart builds its
/// replacement from.
///
/// `respawn` is a factory rather than a stored [`EngineConfig`] because the
/// config is consumed by the spawn it describes -- it can carry an owned
/// duplicate of this process's stdin (`main`'s `maybe_relay_stdin`), which a
/// second spawn needs a second duplicate of, not a clone of the first. So a
/// restart asks for a fresh config instead of holding one it could not
/// copy.
pub struct EngineSession<'a> {
    pub engine: Engine,
    pub pump: view_engine::DamagePump,
    pub respawn: &'a dyn Fn() -> view_engine::EngineConfig,
}

/// The replacement session [`restart_engine`] produced, and everything
/// staged against it before the loop could read it.
pub(crate) struct Restarted {
    pub(crate) engine: Engine,
    pub(crate) pump: view_engine::DamagePump,
    pub(crate) executor: Executor<EngineHandle>,
    pub(crate) staged: crate::startup::CutoverInput,
}

/// Brings up a replacement engine and re-points everything bound to the one
/// it replaced: the executor's connection, the clipboard worker's reply
/// route, and the damage pump.
///
/// The teardown happens first, inside [`crate::startup::restart_and_attach`],
/// so the two engines never exist at once and the fresh one opens the swap
/// files the dead one left -- which is the only state a restart recovers,
/// since view holds no buffer text of its own. `engine` is borrowed rather
/// than consumed so a failure leaves the caller holding the connection it
/// asked to replace: a dropped remote connection is retried, and a loop with
/// no engine at all has nothing to paint the failure with.
///
/// The grid is attached at [`Model::grid_target`], not at the terminal's own
/// size: the chrome this session already reserved was reserved through a
/// resize the dead engine was told about and the fresh one has never heard
/// of (see [`crate::startup::restart_and_attach`]).
///
/// # Latency
///
/// This is the one blocking call the loop makes, and the frame it is on is
/// the one it stalls: the teardown is bounded by the engine's
/// `shutdown_timeout` and the spawn/attach by its `handshake_timeout`, which
/// view leaves at its default. Nothing is painted and no keystroke is folded
/// for as long as it runs, and the banner on screen is frozen at whatever it
/// last said.
///
/// A local engine pays that once per death, on a transition a user asked
/// for. A dropped remote connection pays it per *attempt*: up to
/// [`REMOTE_RECONNECT_MAX_ATTEMPTS`] unattended ones, plus one more for each
/// restart the user picks off the modal afterwards. The worst case is a far
/// side that accepts the connection and then never completes the handshake,
/// which spends the whole handshake bound on every one of those attempts
/// while a connection nobody can type at is already gone. The waits
/// *between* attempts cost nothing here -- they are a deadline the loop's
/// own bounded wait reaches ([`ReconnectSchedule`]), and the loop keeps
/// painting and counting through them.
///
/// Neither the steady-state pass nor a healthy session reaches any of this.
pub(crate) fn restart_engine(
    engine: &mut Engine,
    respawn: &dyn Fn() -> view_engine::EngineConfig,
    model: &Model,
    channels: &LoopChannels,
    route: &crate::clipboard::ReplyRoute<EngineHandle>,
    ai_context_route: &crate::ai_context_worker::OpsRoute<EngineHandle>,
) -> Result<Restarted, crate::startup::AttachFailure> {
    let (width, height) = model.grid_target();
    let mut engine = crate::startup::restart_and_attach(engine, respawn(), width, height)?;
    let (pump, cutover) = engine.start_pump(channels.msg.clone());
    let pending_redraw = if cutover.redraw_pending {
        pump.take_damage()
    } else {
        Vec::new()
    };
    route.rebind(engine.handle.clone());
    ai_context_route.rebind(engine.handle.clone());
    // after the rebind, so the epoch read here is the replacement's
    let executor = channels.executor(engine.handle.clone(), route.epoch());
    Ok(Restarted {
        engine,
        pump,
        executor,
        staged: crate::startup::CutoverInput {
            presink: cutover.presink,
            pending_redraw,
            // nothing to replay: the attach above already used this
            // session's current grid size, and no key was buffered on the
            // way here -- the loop's own input path never stopped running
            resize: None,
            keys: Vec::new(),
        },
    })
}

/// What a stopped engine reports about how it stopped: the child's real
/// status, and whether it said it was on its way out before the connection
/// closed.
///
/// The two travel together because neither answers alone. The status says
/// what to leave with; the announcement says whether to leave at all, and it
/// is the only half a Windows build can read for a process death.
pub(crate) struct EngineStop {
    pub(crate) exit: ExitInfo,
    pub(crate) announced_exit: bool,
}

/// Dispatches one message and resolves whatever flow it produced, returning
/// `Some(exit_code)` when the loop must stop and return that code.
///
/// One resolver behind all three of [`run`]'s dispatch sites rather than a
/// copy each: a failed write resolved one way at the resize site and another
/// in the message queue is exactly the drift that would let a crashed engine
/// quit the editor down one path and offer recovery down another.
///
/// `stop` resolves the child's real status alongside whether the engine
/// announced it was leaving, and is called only on the failed write that has
/// not already been resolved -- a bounded (up to the engine's
/// `shutdown_timeout`) block on a transition that happens at most once per
/// engine, never on a steady-state pass.
pub(crate) fn step<E: EngineOps>(
    model: &mut Model,
    executor: &Executor<E>,
    follow_ups: &mut FollowUps<'_>,
    state: &mut LoopState,
    stop: impl FnOnce() -> EngineStop,
    msg: Msg,
) -> Option<i32> {
    match dispatch(model, executor, follow_ups, msg) {
        Flow::Continue => None,
        // run() owns the engine: returning here runs Drop (graceful qa!
        // then kill)
        Flow::Quit(code) => Some(code),
        Flow::RestartEngine => {
            state.restart_requested = true;
            None
        }
        // an engine write failed: the engine connection is gone, not the
        // UI. The rest of that batch targeted an engine that is already
        // gone, which is why `dispatch` stops on the first failure rather
        // than reporting the same loss once per remaining effect
        Flow::EngineLost if state.connection_lost => None,
        Flow::EngineLost => {
            let EngineStop {
                exit,
                announced_exit,
            } = stop();
            if model.supervision.note_engine_stop(exit, announced_exit) {
                state.connection_lost = true;
                None
            } else {
                // nvim stopped because it was told to (`:q`, `:cq`): the
                // session is over, and view leaves with nvim's own status
                match dispatch(model, executor, follow_ups, Msg::EngineDown(exit)) {
                    Flow::Quit(code) => Some(code),
                    _ => None,
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;
    use view_core::msg::Key;
    use view_core::native::supervision::{WedgeKind, RESTART_NOTATION};
    use view_core::update::update;

    /// The session-scoped reactors every dispatch carries, with neither of
    /// them wired to anything: these tests are about what [`step`] does with
    /// a flow, and a takeover or a theme write would only add traffic to the
    /// recorder they assert on.
    /// An `AiWorker` these tests never dispatch through: `[ai]` is off in
    /// every fixture here, so no `Effect::Ai` is ever produced for it to
    /// answer, and building a real one only needs a `LoopSender` clone --
    /// see `ai_worker::AiWorker::new`'s own doc for why it never touches
    /// the network until `dispatch` is actually called.
    fn inert_ai_worker(msg: &crate::wake::LoopSender) -> crate::ai_worker::AiWorker {
        crate::ai_worker::AiWorker::new(
            view_ai::AgentSpec::Id("claude-code".to_string()),
            std::path::PathBuf::from("."),
            msg.clone(),
        )
    }

    fn inert_follow_ups<'a>(
        native: &'a mut crate::native::NativeSession,
        theme: &'a mut crate::bridge::ThemeBridge,
    ) -> FollowUps<'a> {
        FollowUps {
            native,
            theme,
            speculate: crate::speculate::SpeculationClock::default(),
        }
    }

    /// The stop an engine that said it was leaving reports: `:q` / `:cq`.
    fn announced(code: i32) -> EngineStop {
        EngineStop {
            exit: ExitInfo {
                code: Some(code),
                by_signal: false,
            },
            announced_exit: true,
        }
    }

    /// The stop an engine that never got to say anything reports.
    fn silent(code: Option<i32>, by_signal: bool) -> EngineStop {
        EngineStop {
            exit: ExitInfo { code, by_signal },
            announced_exit: false,
        }
    }

    /// A model with the dead-engine modal already on screen, reached the way
    /// the loop reaches it rather than by assembling the state by hand.
    fn dead_modal() -> Model {
        let mut model = Model::with_term_size(80, 24);
        // the switch off, so the reading raises the modal this test presses
        // rather than recovering behind it
        model.supervision.auto_restart = false;
        let effects = update(
            &mut model,
            Msg::EngineLiveness {
                wedge: Some(WedgeKind::Dead),
                observed_for: std::time::Duration::ZERO,
            },
        );
        assert!(effects.is_empty(), "{effects:?}");
        assert!(model.engine_busy().is_some(), "the modal must be open");
        model
    }

    /// The restart is owed, not performed here: the loop replaces its engine
    /// at the top of the next pass, with nothing borrowed from the one it is
    /// replacing.
    #[test]
    fn a_restart_choice_is_deferred_to_the_loop_and_never_ends_the_session() {
        let ops = crate::engine_ops::FakeOps::default();
        let executor = Executor::new(&ops);
        let mut native = crate::native::NativeSession::inert();
        let mut theme = crate::bridge::ThemeBridge::new(None);
        let mut model = dead_modal();
        let mut state = LoopState::default();

        let code = step(
            &mut model,
            &executor,
            &mut inert_follow_ups(&mut native, &mut theme),
            &mut state,
            || unreachable!("a restart resolves no exit status"),
            Msg::Key(Key {
                notation: RESTART_NOTATION.to_string(),
            }),
        );

        assert_eq!(code, None, "picking Restart must not end the session");
        assert!(
            state.restart_requested,
            "the loop was never told a replacement is owed"
        );
        assert!(model.running, "the session is continuing, not ending");
    }

    /// An engine that stopped because it was told to is the session ending,
    /// and the failed write that discovers it must not be read as a crash to
    /// recover from.
    #[test]
    fn a_failed_write_to_an_engine_a_user_quit_ends_the_session_with_its_status() {
        let ops = crate::engine_ops::FakeOps::default();
        *ops.fail_next.borrow_mut() = true;
        let executor = Executor::new(&ops);
        let mut native = crate::native::NativeSession::inert();
        let mut theme = crate::bridge::ThemeBridge::new(None);
        let mut model = Model::with_term_size(80, 24);
        let mut state = LoopState::default();

        let code = step(
            &mut model,
            &executor,
            &mut inert_follow_ups(&mut native, &mut theme),
            &mut state,
            || announced(0),
            Msg::Key(Key {
                notation: "a".to_string(),
            }),
        );

        assert_eq!(code, Some(0), "view must leave with nvim's own status");
        assert!(
            !state.connection_lost,
            "an engine that was told to stop is not one supervision owns"
        );
    }

    /// The counterpart: nobody asked for this one, so the loop keeps the
    /// session and hands the connection to supervision.
    #[test]
    fn a_failed_write_to_an_engine_that_died_hands_it_to_supervision() {
        let ops = crate::engine_ops::FakeOps::default();
        *ops.fail_next.borrow_mut() = true;
        let executor = Executor::new(&ops);
        let mut native = crate::native::NativeSession::inert();
        let mut theme = crate::bridge::ThemeBridge::new(None);
        let mut model = Model::with_term_size(80, 24);
        let mut state = LoopState::default();

        let code = step(
            &mut model,
            &executor,
            &mut inert_follow_ups(&mut native, &mut theme),
            &mut state,
            || silent(Some(139), true),
            Msg::Key(Key {
                notation: "a".to_string(),
            }),
        );

        assert_eq!(code, None, "a crashed engine must not end the session");
        assert!(
            state.connection_lost,
            "the fold was never told it may reach a Dead verdict"
        );
        assert!(model.running);
        assert_eq!(
            model.supervision.exit_code(),
            139,
            "the status a user answering with Quit would leave with"
        );
    }

    /// Every effect in the batch after the first failure fails the same way,
    /// and each failure would otherwise repeat the bounded teardown the
    /// first one already ran.
    #[test]
    fn a_connection_already_resolved_is_not_resolved_a_second_time() {
        let ops = crate::engine_ops::FakeOps::default();
        *ops.fail_next.borrow_mut() = true;
        let executor = Executor::new(&ops);
        let mut native = crate::native::NativeSession::inert();
        let mut theme = crate::bridge::ThemeBridge::new(None);
        let mut model = Model::with_term_size(80, 24);
        let mut state = LoopState {
            connection_lost: true,
            restart_requested: false,
        };

        let code = step(
            &mut model,
            &executor,
            &mut inert_follow_ups(&mut native, &mut theme),
            &mut state,
            || unreachable!("the stop was already resolved once"),
            Msg::Key(Key {
                notation: "a".to_string(),
            }),
        );

        assert_eq!(code, None);
        assert!(state.connection_lost);
    }
    /// The whole replacement, against a real engine that really died: the
    /// child is killed out of band (the shape a crash leaves, which the
    /// ordinary teardown never produces), and everything the loop holds has
    /// to come back pointing at the engine that replaced it.
    ///
    /// Unix-only for the kill alone: nothing else here is platform-specific,
    /// and no portable way to end a child without going through `Engine`'s
    /// own teardown -- which is precisely what a crash does not do -- exists
    /// in std.
    #[cfg(unix)]
    #[test]
    fn a_restart_replaces_the_dead_engine_and_re_points_everything_bound_to_it() {
        let (msg_tx, msg_rx) = std::sync::mpsc::sync_channel(64);
        let (clipboard, _clipboard_jobs) = mpsc::channel();
        let (osc52, _osc52_jobs) = mpsc::channel();
        let (picker, _picker_requests) = mpsc::channel();
        let (ai_context, _ai_context_jobs) = mpsc::channel();
        let msg = crate::wake::LoopSender::new(msg_tx);
        let channels = LoopChannels {
            clipboard,
            osc52,
            picker,
            ai_context,
            // `cat`, not `inert_ai_worker`'s "claude-code" id: a real,
            // provisioning-free child this test can spawn, whose process
            // stays alive for as long as the test needs, which is what
            // gives the restart below a live child to prove survives it.
            // Its session, though, does not: echoing the driver's own
            // `initialize` request back is a malformed reply, so the
            // session reports `SessionCrashed` early and its watch is
            // correctly torn down. The slot still reads `Ready` here
            // because that demotion is lazy while watch teardown is eager
            // -- so this fixture can attest the worker and the child, and
            // never the watch.
            ai: crate::ai_worker::AiWorker::new(
                view_ai::AgentSpec::Command(vec!["cat".to_string()]),
                std::path::PathBuf::from("."),
                msg.clone(),
            ),
            msg,
        };
        let respawn = || view_engine::process::EngineConfig::isolated();
        let mut engine = Engine::spawn(respawn()).unwrap();
        engine.handle.ui_attach(80, 24).unwrap();
        let (_pump, _cutover) = engine.start_pump(channels.msg.clone());
        let route = crate::clipboard::ReplyRoute::new(engine.handle.clone());
        let ai_context_route = crate::ai_context_worker::OpsRoute::new(engine.handle.clone());
        let dead_pid = engine.pid();

        channels
            .ai
            .dispatch(view_core::native::ai_event::AiCommand::Prompt {
                text: "hello".to_string(),
                context: Vec::new(),
            });
        wait_until(
            "the cat-backed AI session becomes Ready with a live child",
            || channels.ai.ready_pid_for_test().is_some(),
        );
        let ai_pid_before = channels
            .ai
            .ready_pid_for_test()
            .expect("just waited for this to be Some");

        let killed = std::process::Command::new("kill")
            .args(["-KILL", &dead_pid.to_string()])
            .status()
            .expect("kill must run for an out-of-band crash to be simulable");
        assert!(killed.success(), "kill -KILL failed: {killed:?}");
        wait_until("the killed engine's connection closes", || {
            engine.handle.is_closed()
        });
        assert!(
            !route.addresses_a_live_connection(),
            "the reply route must start out pointing at the engine that died, \
             or its rebind below proves nothing"
        );

        let mut model = Model::with_term_size(80, 24);
        let fresh = restart_engine(
            &mut engine,
            &respawn,
            &model,
            &channels,
            &route,
            &ai_context_route,
        )
        .expect("a crashed engine must be replaceable");

        // the AI worker the restart's executor answers through must be the
        // very same one `channels.ai` already held, not a fresh clone --
        // replacing `.with_ai(self.ai.clone())` with `.with_ai(AiWorker::new(...))`
        // in `LoopChannels::executor` would compile and pass every other
        // assertion here, since a fresh worker's `Idle` slot never touches
        // the dying engine at all.
        let fresh_ai = fresh
            .executor
            .ai_worker()
            .expect("the restart's executor must still have an AI worker wired");
        assert!(
            fresh_ai.is_same_worker_as(&channels.ai),
            "the restart must reuse the shared AiWorker, not construct a fresh one"
        );
        assert_eq!(
            fresh_ai.ready_pid_for_test(),
            Some(ai_pid_before),
            "the restart must not have killed or replaced the live agent child -- \
             exactly the one `cat` spawned above must still be running"
        );

        // the same cutover the loop runs on the way back: a fresh engine
        // fires `VimEnter` as a blocked request, and one nobody answers
        // leaves nvim waiting inside its own startup rather than editing
        let mut native = crate::native::NativeSession::inert();
        let mut theme = crate::bridge::ThemeBridge::new(None);
        let outcome = crate::startup::run_cutover(
            &mut model,
            &fresh.executor,
            &mut inert_follow_ups(&mut native, &mut theme),
            fresh.staged,
            || ExitInfo {
                code: None,
                by_signal: false,
            },
        );
        assert!(
            matches!(outcome, crate::startup::CutoverOutcome::Continue),
            "the replacement engine was gone before the loop could resume"
        );

        assert_ne!(
            fresh.engine.pid(),
            dead_pid,
            "the restart returned the process that died"
        );
        // the loop's own pass, in the one shape this test needs it: a
        // fresh engine's `VimEnter` is a blocked request, and nvim sits
        // inside its own startup until something dispatches the reply
        wait_until("the replacement engine answers", || {
            while let Ok(msg) = msg_rx.try_recv() {
                let _ = crate::runtime::dispatch(
                    &mut model,
                    &fresh.executor,
                    &mut inert_follow_ups(&mut native, &mut theme),
                    msg,
                );
            }
            fresh
                .engine
                .handle
                .request_timeout(
                    "nvim_get_mode",
                    vec![],
                    std::time::Duration::from_millis(100),
                )
                .is_ok()
        });
        assert!(
            route.addresses_a_live_connection(),
            "the clipboard worker was left answering the engine that died"
        );
        // the executor addresses the replacement, proven by what the
        // replacement itself then holds rather than by the write returning
        let flow = fresh.executor.run(view_core::msg::Effect::Rpc(
            view_core::msg::RpcCall::Input {
                notation: "ihello<Esc>".to_string(),
            },
        ));
        assert_eq!(flow, Flow::Continue, "the executor's write failed");
        wait_until(
            "the replacement engine holds what the executor sent it",
            || fresh.engine.handle.eval_str("getline(1)").ok().as_deref() == Some("hello"),
        );
        // and the grid it attached at is this session's, not the terminal's
        let (width, height) = model.grid_target();
        assert_eq!(
            fresh
                .engine
                .handle
                .eval_str("&columns . \"x\" . &lines")
                .ok(),
            Some(format!("{width}x{height}")),
            "the replacement attached at a size this session never had"
        );
    }

    /// Fails a restart the way a broken `--nvim-bin` would, which is the one
    /// failure mode with no second engine to report through.
    #[test]
    fn a_restart_that_cannot_spawn_reports_the_failure_rather_than_pretending() {
        let (msg_tx, _msg_rx) = std::sync::mpsc::sync_channel(64);
        let (clipboard, _clipboard_jobs) = mpsc::channel();
        let (osc52, _osc52_jobs) = mpsc::channel();
        let (picker, _picker_requests) = mpsc::channel();
        let (ai_context, _ai_context_jobs) = mpsc::channel();
        let msg = crate::wake::LoopSender::new(msg_tx);
        let channels = LoopChannels {
            clipboard,
            osc52,
            picker,
            ai: inert_ai_worker(&msg),
            ai_context,
            msg,
        };
        let mut engine = Engine::spawn(view_engine::process::EngineConfig::isolated()).unwrap();
        let route = crate::clipboard::ReplyRoute::new(engine.handle.clone());
        let ai_context_route = crate::ai_context_worker::OpsRoute::new(engine.handle.clone());
        let respawn =
            || view_engine::process::EngineConfig::isolated().with_nvim_bin("/nonexistent/nvim");
        let model = Model::with_term_size(80, 24);

        let failed = restart_engine(
            &mut engine,
            &respawn,
            &model,
            &channels,
            &route,
            &ai_context_route,
        );
        assert!(
            matches!(failed, Err(crate::startup::AttachFailure::Spawn(_))),
            "a restart that could not spawn must report it"
        );
    }

    /// A base a test can afford to wait out, with the shipped doubling and
    /// the shipped cap left alone: what the sequences below prove is the
    /// sequence, and the shipped base would put half a minute of waiting in
    /// the unit suite. The shipped base is proven against an injected clock
    /// in `the_shipped_schedule_spends_its_documented_waits`.
    #[cfg(unix)]
    const TEST_BACKOFF_BASE: std::time::Duration = std::time::Duration::from_millis(80);

    /// A committed stand-in ssh client, by name.
    #[cfg(unix)]
    fn ssh_fixture(name: &str) -> std::path::PathBuf {
        std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../scripts/test-fixtures")
            .join(name)
            .canonicalize()
            .expect("the test fixtures are committed alongside the crate")
    }

    /// A remote config routed through the stand-in client `ssh` names.
    #[cfg(unix)]
    fn remote_through(ssh: &str) -> view_engine::EngineConfig {
        view_engine::process::EngineConfig::isolated()
            .with_remote(
                view_engine::RemoteSpec::new("view-test-host").with_ssh_bin(ssh_fixture(ssh)),
            )
            .with_handshake_timeout(std::time::Duration::from_secs(10))
    }

    /// The backoff belongs to a dropped *remote* connection and to nothing
    /// else, which is a claim about what does NOT wait: a local engine that
    /// died has no far side to become reachable, and a restart the user
    /// pressed a key for on a connection that is still open is a second of
    /// delay they would see.
    ///
    /// Both engines are really spawned, because the question is what
    /// `Engine::is_remote` answers about a session that exists, not what a
    /// config says it was asked for.
    #[cfg(unix)]
    #[test]
    fn only_a_dropped_remote_connection_waits_before_its_replacement() {
        let local = Engine::spawn(view_engine::process::EngineConfig::isolated())
            .expect("a local engine must spawn");
        assert!(!local.is_remote(), "a local spawn is not a remote session");
        assert!(
            !reconnects(&local, true),
            "a dead local engine is replaced at once: waiting cannot make a process that is \
             gone come back, and the delay is one the user watches"
        );

        let remote = Engine::spawn(remote_through("delay-relay")).expect("the stand-in must spawn");
        assert!(remote.is_remote(), "a spawn through a client is remote");
        assert!(
            reconnects(&remote, true),
            "a dropped remote connection is the one replacement that waits"
        );
        assert!(
            !reconnects(&remote, false),
            "a restart asked for over a connection that is still open waits for nothing"
        );

        let mut schedule = ReconnectSchedule::new(
            TEST_BACKOFF_BASE,
            view_engine::REMOTE_RECONNECT_MAX_ATTEMPTS,
        );
        let now = std::time::Instant::now();
        schedule.request(false, now);
        assert!(schedule.armed());
        assert_eq!(
            schedule.poll_deadline(now),
            Some(std::time::Duration::ZERO),
            "an immediate replacement is owed the moment it is asked for"
        );
        assert!(schedule.take_due(now));
        assert_eq!(
            schedule.progress(),
            None,
            "and no banner counts attempts for a replacement that is not a sequence"
        );
        assert!(
            !schedule.note_failure(now),
            "its failure is the caller's to report, not the schedule's to absorb"
        );
    }

    /// The waits a dropped remote connection's attempts are spaced by,
    /// measured off the attempts themselves rather than read back out of the
    /// schedule that computed them.
    ///
    /// The wait between two attempts is the loop's own bounded wait, so this
    /// performs that wait the way the loop does -- `recv_timeout` against a
    /// message channel -- rather than by sleeping: a schedule that armed a
    /// deadline the loop had no way to wait on would pass a test that slept
    /// and stall in the editor.
    ///
    /// The bounds are asymmetric on purpose. The lower one is exact: an
    /// attempt that ran early is the client spin this whole mechanism exists
    /// to prevent, and no tolerance is owed it. The upper one is loose,
    /// because a scheduler on a loaded host may be late by whatever it likes
    /// without anything here being wrong.
    #[cfg(unix)]
    #[test]
    fn a_dropped_remote_connection_retries_on_a_doubling_backoff() {
        let mut schedule = ReconnectSchedule::new(
            TEST_BACKOFF_BASE,
            view_engine::REMOTE_RECONNECT_MAX_ATTEMPTS,
        );
        // nothing ever sends on it: this is the loop's wait, not its traffic
        let (_tx, rx) = mpsc::channel::<Msg>();
        let armed = std::time::Instant::now();
        schedule.request(true, armed);

        let mut ran: Vec<std::time::Instant> = Vec::new();
        while let Some(wait) = schedule.poll_deadline(std::time::Instant::now()) {
            assert!(
                matches!(rx.recv_timeout(wait), Err(mpsc::RecvTimeoutError::Timeout)),
                "the wait a reconnect arms must be one the loop can actually wait on"
            );
            if !schedule.take_due(std::time::Instant::now()) {
                continue;
            }
            ran.push(std::time::Instant::now());
            assert!(
                schedule.note_failure(std::time::Instant::now()),
                "a reconnect sequence must absorb its own failures"
            );
        }

        assert_eq!(
            u32::try_from(ran.len()).unwrap_or(u32::MAX),
            view_engine::REMOTE_RECONNECT_MAX_ATTEMPTS,
            "the sequence ran {} attempts against a cap of {}",
            ran.len(),
            view_engine::REMOTE_RECONNECT_MAX_ATTEMPTS
        );
        let slack = std::time::Duration::from_secs(2);
        let mut previous = armed;
        for (index, at) in ran.iter().enumerate() {
            let attempt = u32::try_from(index).unwrap_or(u32::MAX).saturating_add(1);
            let owed = view_engine::remote_reconnect_backoff(TEST_BACKOFF_BASE, attempt);
            let waited = at.saturating_duration_since(previous);
            assert!(
                waited >= owed,
                "attempt {attempt} ran {waited:?} after the one before it, inside the \
                 {owed:?} it owes: an attempt that does not wait is the client spin \
                 the backoff exists to prevent"
            );
            assert!(
                waited < owed + slack,
                "attempt {attempt} waited {waited:?}, far past the {owed:?} it owes"
            );
            previous = *at;
        }
    }

    /// The shipped numbers, against an injected clock so the assertion costs
    /// nothing to make: every attempt waits the wait it is owed, and the
    /// sequence spends the documented total before giving up.
    #[test]
    fn the_shipped_schedule_spends_its_documented_waits() {
        let base = view_engine::REMOTE_RECONNECT_BACKOFF_BASE;
        let mut schedule = ReconnectSchedule::default();
        let mut now = std::time::Instant::now();
        schedule.request(true, now);
        let mut total = std::time::Duration::ZERO;
        for attempt in 1..=view_engine::REMOTE_RECONNECT_MAX_ATTEMPTS {
            let owed = view_engine::remote_reconnect_backoff(base, attempt);
            assert!(
                !schedule.take_due(now + owed - std::time::Duration::from_millis(1)),
                "attempt {attempt} came due inside the {owed:?} it owes"
            );
            now += owed;
            total += owed;
            assert!(
                schedule.take_due(now),
                "attempt {attempt} never came due after the {owed:?} it owes"
            );
            assert!(schedule.note_failure(now));
        }
        assert_eq!(
            total,
            std::time::Duration::from_secs(31),
            "the total the shipped base and cap spend before giving up"
        );
        assert_eq!(
            schedule.poll_deadline(now),
            None,
            "the sequence must stop retrying once its attempts are spent"
        );
        assert!(
            schedule
                .progress()
                .is_some_and(view_core::native::supervision::ReconnectProgress::exhausted),
            "a spent sequence must report itself spent, or the banner keeps counting"
        );
    }

    /// The give-up against spawns that really fail: a client refusing every
    /// connection exhausts the cap exactly, stops retrying on its own, and
    /// leaves the session still holding the connection it could not replace
    /// -- which is what the dead-engine modal is then offered against.
    #[cfg(unix)]
    #[test]
    fn a_remote_client_that_keeps_refusing_gives_up_at_the_cap_and_asks_the_user() {
        let (msg_tx, _msg_rx) = std::sync::mpsc::sync_channel(64);
        let (clipboard, _clipboard_jobs) = mpsc::channel();
        let (osc52, _osc52_jobs) = mpsc::channel();
        let (picker, _picker_requests) = mpsc::channel();
        let (ai_context, _ai_context_jobs) = mpsc::channel();
        let msg = crate::wake::LoopSender::new(msg_tx);
        let channels = LoopChannels {
            clipboard,
            osc52,
            picker,
            ai: inert_ai_worker(&msg),
            ai_context,
            msg,
        };
        // the relay double, so killing the client closes the pipes the way
        // the loss of a real connection does: the plain stand-in hands its
        // own stdio to the editor it starts, which would leave the channel
        // open with the client gone
        let mut engine = Engine::spawn(remote_through("delay-relay"))
            .expect("a remote spawn must handshake through the stand-in client");
        assert!(
            engine.is_remote(),
            "the engine under test must be a remote one"
        );
        let route = crate::clipboard::ReplyRoute::new(engine.handle.clone());
        let ai_context_route = crate::ai_context_worker::OpsRoute::new(engine.handle.clone());
        let killed = std::process::Command::new("kill")
            .args(["-KILL", &engine.pid().to_string()])
            .status()
            .expect("kill must run for a dropped connection to be simulable");
        assert!(killed.success(), "kill -KILL failed: {killed:?}");
        wait_until("the killed client's connection closes", || {
            engine.handle.is_closed()
        });

        // every attempt from here on meets a client that refuses
        let respawn = || remote_through("fake-ssh-reject");
        let model = Model::with_term_size(80, 24);
        let mut schedule = ReconnectSchedule::new(
            TEST_BACKOFF_BASE,
            view_engine::REMOTE_RECONNECT_MAX_ATTEMPTS,
        );
        schedule.request(true, std::time::Instant::now());
        let mut attempts = 0;
        while let Some(wait) = schedule.poll_deadline(std::time::Instant::now()) {
            std::thread::sleep(wait);
            if !schedule.take_due(std::time::Instant::now()) {
                continue;
            }
            attempts += 1;
            let failed = restart_engine(
                &mut engine,
                &respawn,
                &model,
                &channels,
                &route,
                &ai_context_route,
            );
            assert!(
                matches!(failed, Err(crate::startup::AttachFailure::Spawn(_))),
                "a refused client must fail the attempt rather than produce an engine"
            );
            assert!(
                schedule.note_failure(std::time::Instant::now()),
                "a reconnect sequence must absorb its own failures"
            );
        }

        assert_eq!(
            attempts,
            view_engine::REMOTE_RECONNECT_MAX_ATTEMPTS,
            "the sequence must stop at its own cap, neither early nor never"
        );
        // and what the user is left with is supervision's own dead-engine
        // annunciator, not a state belonging to this sequence
        let mut model = Model::with_term_size(80, 24);
        assert!(model.supervision.note_reconnect(schedule.progress()));
        let effects = update(
            &mut model,
            Msg::EngineLiveness {
                wedge: Some(WedgeKind::Dead),
                observed_for: std::time::Duration::ZERO,
            },
        );
        assert!(
            effects.is_empty(),
            "a spent sequence must ask for no further attempt: {effects:?}"
        );
        let busy = model
            .engine_busy()
            .expect("the dead-engine modal must be offered once the attempts run out");
        assert_eq!(busy.kind, WedgeKind::Dead);
        assert!(
            busy.offers(view_core::native::supervision::SupervisionChoice::Restart),
            "the modal must still offer the one further attempt a user can ask for"
        );

        // the attempt the user asks for, and the answer they are owed when it
        // fails too: the connection is exactly as closed as it was, no
        // keystroke reaches anything, and the modal is the only path to Quit
        let effects = update(
            &mut model,
            Msg::Key(Key {
                notation: view_core::native::supervision::RESTART_NOTATION.to_string(),
            }),
        );
        assert!(
            effects
                .iter()
                .any(|e| matches!(e, view_core::msg::Effect::RestartEngine)),
            "the modal's Restart must ask for the attempt it offers: {effects:?}"
        );
        assert!(
            model.engine_busy().is_none(),
            "and the modal goes with the attempt it asked for"
        );

        schedule.request(true, std::time::Instant::now());
        assert!(
            schedule.take_due(std::time::Instant::now()),
            "a user who has already waited out the whole sequence waits no longer"
        );
        let failed = restart_engine(
            &mut engine,
            &respawn,
            &model,
            &channels,
            &route,
            &ai_context_route,
        );
        assert!(
            matches!(failed, Err(crate::startup::AttachFailure::Spawn(_))),
            "the client refuses this attempt too"
        );
        assert!(schedule.note_failure(std::time::Instant::now()));
        assert_eq!(schedule.poll_deadline(std::time::Instant::now()), None);

        let _ = model.supervision.note_reconnect(schedule.progress());
        let effects = update(
            &mut model,
            Msg::EngineLiveness {
                wedge: Some(WedgeKind::Dead),
                observed_for: std::time::Duration::ZERO,
            },
        );
        assert!(effects.is_empty(), "{effects:?}");
        let busy = model.engine_busy().expect(
            "a restart that did not bring an engine back leaves the user with a dead engine, \
             and it must be asked again rather than swallowed",
        );
        assert_eq!(busy.kind, WedgeKind::Dead);
        assert_eq!(
            busy.choices(),
            vec![
                view_core::native::supervision::SupervisionChoice::Restart,
                view_core::native::supervision::SupervisionChoice::Quit
            ],
            "with both ways out, since Quit is reachable nowhere else"
        );
    }

    /// A bounded wait, never a sleep: the condition is the whole assertion.
    #[cfg(unix)]
    fn wait_until(what: &str, mut probe: impl FnMut() -> bool) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        while !probe() {
            assert!(std::time::Instant::now() < deadline, "timed out: {what}");
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
    }
}
