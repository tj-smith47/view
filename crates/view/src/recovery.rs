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
use view_engine::handle::EngineHandle;
use view_engine::process::Engine;

use crate::runtime::{dispatch, EngineOps, Executor, Flow, FollowUps, Osc52Job};

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
/// The teardown happens inside [`Engine::restart`], so the two engines never
/// exist at once and the fresh one opens the swap files the dead one left --
/// which is the only state a restart recovers, since view holds no buffer
/// text of its own.
///
/// The grid is attached at [`Model::grid_target`], not at the terminal's own
/// size: the chrome this session already reserved was reserved through a
/// resize the dead engine was told about and the fresh one has never heard
/// of (see [`crate::startup::restart_and_attach`]).
///
/// # Latency
///
/// This is the one blocking call the loop makes: the teardown is bounded by
/// the engine's `shutdown_timeout` and the spawn/attach by the handshake
/// itself. It runs on a transition a user asked for, at most once per engine
/// death, and never on a steady-state pass -- the same terms the bounded
/// `wait_exit` teardown already runs on.
pub(crate) fn restart_engine(
    engine: Engine,
    respawn: &dyn Fn() -> view_engine::EngineConfig,
    model: &Model,
    channels: &LoopChannels,
    route: &crate::clipboard::ReplyRoute<EngineHandle>,
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
    fn inert_follow_ups<'a>(
        native: &'a mut crate::native::NativeSession,
        theme: &'a mut crate::bridge::ThemeBridge,
    ) -> FollowUps<'a> {
        FollowUps { native, theme }
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
        let ops = crate::runtime::FakeOps::default();
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
        let ops = crate::runtime::FakeOps::default();
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
        let ops = crate::runtime::FakeOps::default();
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
        let ops = crate::runtime::FakeOps::default();
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
        let channels = LoopChannels {
            clipboard,
            osc52,
            picker,
            msg: crate::wake::LoopSender::new(msg_tx),
        };
        let respawn = || view_engine::process::EngineConfig::isolated();
        let mut engine = Engine::spawn(respawn()).unwrap();
        engine.handle.ui_attach(80, 24).unwrap();
        let (_pump, _cutover) = engine.start_pump(channels.msg.clone());
        let route = crate::clipboard::ReplyRoute::new(engine.handle.clone());
        let dead_pid = engine.pid();

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
        let fresh = restart_engine(engine, &respawn, &model, &channels, &route)
            .expect("a crashed engine must be replaceable");
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
        let channels = LoopChannels {
            clipboard,
            osc52,
            picker,
            msg: crate::wake::LoopSender::new(msg_tx),
        };
        let engine = Engine::spawn(view_engine::process::EngineConfig::isolated()).unwrap();
        let route = crate::clipboard::ReplyRoute::new(engine.handle.clone());
        let respawn =
            || view_engine::process::EngineConfig::isolated().with_nvim_bin("/nonexistent/nvim");
        let model = Model::with_term_size(80, 24);

        let failed = restart_engine(engine, &respawn, &model, &channels, &route);
        assert!(
            matches!(failed, Err(crate::startup::AttachFailure::Spawn(_))),
            "a restart that could not spawn must report it"
        );
    }

    /// A bounded wait, never a sleep: the condition is the whole assertion.
    fn wait_until(what: &str, mut probe: impl FnMut() -> bool) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        while !probe() {
            assert!(std::time::Instant::now() < deadline, "timed out: {what}");
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
    }
}
