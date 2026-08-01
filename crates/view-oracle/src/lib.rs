//! The headless driver stack a compatibility oracle scripts against: three
//! levels of increasing fidelity, per the design spec's differential-oracle
//! requirement (the testing-and-oracle section).
//!
//! - [`Session`]: pure Msg-level driver. No engine, no terminal, no
//!   process; feed it hand-built [`view_core::msg::Msg`]s and read back a
//!   deterministic [`view_surface::Surface`]/screen text. The fast oracle
//!   path for cases that do not need a real nvim to prove.
//! - [`EngineSession`]: a real embedded engine, no terminal. The truth
//!   path: real keys into nvim's own typeahead, actual redraw traffic,
//!   actual `nvim_eval` state probes, but never touches a pty or the real
//!   terminal. Corpus and fuzz runs script it through
//!   [`EngineSession::arm_and_input`], whose `feedkeys()` delivery is what
//!   makes a settle point provable (see [`settle`]); the raw
//!   [`EngineSession::input`] leg stays for single interactive keystrokes
//!   and for the driver tests that exercise `nvim_input` itself.
//! - [`PtySession`] (in [`pty`]): the full stack through a real pty. The
//!   integration path, the only leg that proves terminal input decode and
//!   real-process behavior end to end.
//! - [`ReferenceSession`] (in [`reference`]): a second embedded engine, applying
//!   the identical decoded redraw stream `EngineSession` consumes with an
//!   independent, deliberately naive grid applier instead of view's own
//!   `Model`/`Grid`. Not another fidelity tier: a differential second
//!   opinion at the same tier as `EngineSession`, for comparing the two
//!   appliers against each other rather than against nvim's own state.
//! - [`parity`]: the comparison layer a corpus runner drives -- state
//!   probes ([`StateSnapshot`]/[`snapshot`]) plus a masked row-by-row grid
//!   diff ([`compare`]/[`masked_rows`]) between any two [`Probe`] sources,
//!   most usefully `EngineSession` against `ReferenceSession`.
//!
//! Dependency direction: this crate takes no dependency on `view-tui` ([`raster`]
//! is pure `Surface` + `Grid` -> text, no ratatui/crossterm) and stays
//! `rmpv`-free at its own API surface -- only `view-engine` speaks `rmpv`;
//! every probe here returns a typed value ([`Surface`](view_surface::Surface),
//! `String`), never a raw wire `Value`. `scripts/audit-deps.sh` enforces
//! both.

mod attr;
pub mod compat;
mod minimize;
mod parity;
pub mod pty;
mod raster;
mod reference;
mod settle;
#[cfg(test)]
mod testenv;

use std::sync::mpsc::sync_channel;
use std::time::{Duration, Instant};

use view_core::events::UiEvent;
use view_core::model::Model;
use view_core::msg::{Effect, Msg, RpcCall};
use view_core::update::update;
use view_engine::handle::EngineError;
use view_engine::process::{Engine, EngineConfig};
use view_engine::DamagePump;
use view_surface::Surface;

pub use compat::CompatSession;
pub use minimize::{ddmin, join_tokens, tokenize};
pub use parity::{
    compare, masked_rows, snapshot, Divergence, DivergenceKind, Probe, ReferenceSide, Screen,
    StateSnapshot, ViewSide,
};
pub use pty::{make_hermetic, PtySession, QueryPolicy, SpawnEnv};
pub use reference::ReferenceSession;

/// Errors surfaced by the headless drivers.
#[non_exhaustive]
#[derive(Debug, thiserror::Error)]
pub enum OracleError {
    /// An underlying engine RPC/process error.
    #[error(transparent)]
    Engine(#[from] EngineError),
    /// A pty could not be opened, or the command inside it failed to spawn.
    #[error("pty error: {0}")]
    Pty(String),
    /// An I/O error writing to or reading from a pty.
    #[error(transparent)]
    Io(#[from] std::io::Error),
    /// A session stayed blocked in a key-wait (a hit-enter prompt, a
    /// pending `t`/`f`/`r` character argument) even after the snapshot
    /// layer's `<Esc>` dismissal (see [`snapshot`]'s doc comment), so its
    /// eval probes can never answer. Named rather than left to surface as
    /// a generic eval timeout so a report line says which nvim state
    /// actually wedged the probe.
    #[error("session still blocked waiting for a key (mode {mode:?}) after <Esc> dismissal")]
    Blocked {
        /// The mode name the fast `nvim_get_mode` probe reported while the
        /// session stayed blocked.
        mode: String,
    },
    /// A driver's quiesce marker failed its integrity check: nvim's
    /// `SafeState` hook published the mode it reached idle in, and the fast
    /// probe then reported a different state. Raised by either side of a
    /// differential run ([`EngineSession::quiesce`],
    /// [`ReferenceSession::quiesce`]).
    ///
    /// The published mode is nvim's own proof that its typeahead was empty
    /// at that instant, so a session that no longer holds that state moved
    /// on input this protocol cannot account for -- and a comparison against
    /// where it moved to would be measuring that input rather than the
    /// script's. Surfaced as an error rather than a settled-or-timeout bool
    /// so a report line names the true cause instead of fabricating a
    /// divergence out of harness-perturbed state.
    #[error(
        "quiesce marker fired with the session idle in state {armed:?}, but it then moved to \
         state {observed:?}; input this protocol cannot account for reached the session, so it \
         may no longer hold the script's final state"
    )]
    QuiescePerturbed {
        /// The `mode(1)` the `SafeState` hook itself captured, at fire time,
        /// in the same instant nvim proved its typeahead empty.
        armed: String,
        /// The state the fast probe reported afterwards, rendered with its
        /// blocked flag folded in.
        observed: String,
    },
    /// A state-probe reply did not match the shape its parser requires
    /// (the cursor or marks parsers behind [`snapshot`]). Surfaced as an
    /// error rather than degraded to a placeholder value: registers,
    /// marks, and the cursor all ride one shared probe expression and one
    /// shared parser across both sides of a differential comparison, so a
    /// malformation there is common-mode -- both sides would degrade
    /// identically and compare equal, silently erasing coverage instead of
    /// reporting a broken probe.
    #[error("state probe parse error: {0}")]
    Parse(String),
}

/// Msg-level headless driver: pure, no engine, no terminal. The fast oracle
/// path (leg (a): Msg-level injection; leg (b): deterministic `Surface`
/// capture).
pub struct Session {
    model: Model,
}

impl Session {
    /// Creates a session with a `cols`x`rows` terminal size and no grid
    /// content yet (matching [`Model::with_term_size`]'s startup-time
    /// state, before any redraw has arrived).
    #[must_use]
    pub fn new(cols: u16, rows: u16) -> Self {
        Self {
            model: Model::with_term_size(cols, rows),
        }
    }

    /// Feeds one [`Msg`] through [`update`], discarding the returned
    /// [`view_core::msg::Effect`]s: a pure Msg-level driver has no engine
    /// or terminal to carry an RPC/paint effect out to, so there is nothing
    /// for a caller to do with them, unlike [`EngineSession`] (a real
    /// engine to route `Effect::Rpc` to) or the production runtime loop
    /// (both an engine and a terminal).
    pub fn feed(&mut self, msg: Msg) {
        let _ = update(&mut self.model, msg);
    }

    /// Captures the current [`Surface`] (leg (b): deterministic capture).
    #[must_use]
    pub fn surface(&self) -> Surface {
        view_surface::render(&self.model)
    }

    /// Renders the current [`Surface`] to plain text via [`raster::screen_text`].
    #[must_use]
    pub fn screen_text(&self) -> String {
        raster::screen_text(&self.surface(), self.model.engine.grid())
    }
}

/// Engine-attached headless driver: a real embedded engine, no terminal.
/// The truth path (leg (c): harness-owned polling; leg (d): engine
/// state-parity probes via [`eval_str`](Self::eval_str)).
pub struct EngineSession {
    model: Model,
    engine: Engine,
    pump: DamagePump,
    /// This session's half of the shared quiesce protocol's state (see
    /// [`crate::settle`]).
    markers: settle::QuiesceMarkers,
}

impl EngineSession {
    /// Spawns a real `nvim --embed`, attaches at `cols`x`rows` with the
    /// full `ext_*` set, and returns a session ready to drive.
    ///
    /// Deliberately skips the `VimEnter` autocmd registration
    /// `view`'s own production startup performs
    /// (`view_engine::handle::EngineHandle::register_vim_enter_autocmd`):
    /// that registration exists to prove the paint loop's
    /// `Msg::EngineRequest` -> `Effect::Reply` plumbing end to end, which
    /// this driver has no paint loop to exercise. Registering it here with
    /// nothing consuming the resulting `Msg::EngineRequest` would leave
    /// nvim's `VimEnter` autocmd's blocking `rpcrequest` waiting forever
    /// for a reply this driver never sends.
    ///
    /// Always spawns with `--clean`: an oracle a compat script drives must
    /// be deterministic across hosts and CI, which the developer's own
    /// `init.lua` (plugins, autocmds, a dashboard or notification popup
    /// that can swallow a bare `i` behind a floating window) cannot
    /// guarantee. Live-verified as load-bearing, not defensive-only: this
    /// method's own self-test
    /// (`engine_session_input_and_pump_until_flush_agree_with_eval_str_probe`
    /// in `tests/driver_legs.rs`) failed against this host's real config
    /// before `--clean` was added here, with a floating popup swallowing
    /// the typed `i` instead of entering insert mode.
    ///
    /// Also always spawns with `-n` (no swap file): this crate's own test
    /// binary spawns multiple `EngineSession`s (and, in `reference.rs`,
    /// `ReferenceSession`s) across parallel test threads, each typing into
    /// an unnamed buffer in the same working directory. Two unnamed-buffer
    /// swap files colliding there produces a live `E303` recovery error on
    /// whichever side loses the race, not a hang or a decode error. A
    /// short-lived oracle session has no crash to recover from, so there is
    /// nothing this trades away.
    ///
    /// # Errors
    ///
    /// Returns [`OracleError::Engine`] if the process fails to spawn, the
    /// `ui_attach` handshake fails or times out, or the quiesce-protocol
    /// setup commands cannot be written to the connection.
    pub fn spawn(cols: u16, rows: u16) -> Result<Self, OracleError> {
        let mut engine = Engine::spawn(EngineConfig::isolated())?;
        engine.handle.ui_attach(cols, rows)?;
        // no consumer ever drains this channel: EngineSession polls
        // DamagePump::take_damage directly instead (leg (c) is
        // harness-owned polling, not a blocking recv on a sink), and
        // RedrawReady tokens are safely lossy (see view_engine::damage's
        // module docs) when nothing ever removes them
        let (sink, _unused_rx) = sync_channel(64);
        let (pump, _cutover) = engine.start_pump(sink);
        settle::install_hooks(&engine.handle)?;
        Ok(Self {
            model: Model::with_term_size(cols, rows),
            engine,
            pump,
            markers: settle::QuiesceMarkers::default(),
        })
    }

    /// Queues the next quiesce marker's arm command and `notation` into
    /// nvim's typeahead as one `feedkeys()` payload, in that order, and
    /// records the marker for the next [`quiesce`](Self::quiesce) call to
    /// wait on. The way a script under test must be driven; see
    /// [`crate::settle::arm_and_input`] for why the fusion into a single
    /// payload is the whole settle argument, and for the already-settled
    /// contract a caller owes it.
    ///
    /// Blocking, unlike [`input`](Self::input): the payload rides a
    /// request whose reply says nvim accepted it (the keys themselves are
    /// left for the main loop to consume).
    ///
    /// # Errors
    ///
    /// Returns [`OracleError::Engine`] if the connection has closed, nvim
    /// rejects the payload, or no reply arrives within the engine's eval
    /// timeout.
    pub fn arm_and_input(&mut self, notation: &str) -> Result<(), OracleError> {
        settle::arm_and_input(self, notation)
    }

    /// Waits until everything typed into this session has been fully
    /// processed, per the shared marker protocol [`crate::settle::settle`]
    /// documents in full. Returns whether the session settled before
    /// `deadline`.
    ///
    /// The settle criterion is nvim's own `SafeState` signal, identical to
    /// [`ReferenceSession::quiesce`]'s, rather than this driver's
    /// [`pump_until_flush`](Self::pump_until_flush) redraw-boundary drain:
    /// nvim suppresses redraws for as long as it has typeahead to chew
    /// through, so a script that stalls the main loop mid-run leaves a
    /// flush-boundary drain no traffic to wait on and it declares the
    /// script finished while its tail is still queued. Both sides of a
    /// differential run must decide they are done by the same rule, or one
    /// reads its state at a different point in the same script and the
    /// mismatch is reported as a divergence in view's own pipeline.
    ///
    /// # Errors
    ///
    /// - [`OracleError::Engine`] if the fast state probe, the parked-state
    ///   probe, or the marker's arm call fails at the RPC layer.
    /// - [`OracleError::QuiescePerturbed`] if the marker round-trip failed
    ///   the protocol's integrity check.
    pub fn quiesce(&mut self, silence: Duration, deadline: Duration) -> Result<bool, OracleError> {
        settle::settle(self, silence, deadline)
    }

    /// Forwards one encoded key `notation` via `nvim_input` (leg (a):
    /// Msg-level injection at the engine-attached tier).
    ///
    /// # Errors
    ///
    /// Returns [`OracleError::Engine`] if the connection's writer thread
    /// has already exited.
    pub fn input(&mut self, notation: &str) -> Result<(), OracleError> {
        self.engine.handle.input(notation).map_err(Into::into)
    }

    /// Polls [`DamagePump::take_damage`] and applies every drained batch
    /// through [`update`] until a batch containing [`UiEvent::Flush`]
    /// arrives, or `deadline` elapses. Returns whether a flush was
    /// observed.
    ///
    /// The harness owns all timing here, by design: this is a bounded
    /// polling loop the caller's own `deadline` parameter controls, not a
    /// blocking wait inside `view-engine` or `view-core` -- neither of
    /// which has a clock of its own (see `crates/view/src/runtime.rs`'s
    /// module docs: the production runtime loop blocks on one `recv`,
    /// deadline-bounded only while engine-bound output is pending).
    ///
    /// Every `Effect::Rpc` `update` returns is forwarded back to the real
    /// engine via [`apply_effects`](Self::apply_effects), the same way the
    /// production runtime's `Executor` closes the loop -- a headless
    /// driver that discarded these would let `Model` silently believe an
    /// RPC fired (e.g. the `nvim_ui_try_resize` a `TablineUpdate` crossing
    /// the chrome-reservation boundary produces) when nothing was ever
    /// sent, leaving the model's own idea of the engine's grid size
    /// disagree with the real nvim process underneath it.
    #[must_use]
    pub fn pump_until_flush(&mut self, deadline: Duration) -> bool {
        let start = Instant::now();
        loop {
            let events = self.pump.take_damage();
            let saw_flush = events.iter().any(|e| matches!(e, UiEvent::Flush));
            if !events.is_empty() {
                let effects = update(&mut self.model, Msg::Redraw(events));
                self.apply_effects(effects);
            }
            if saw_flush {
                return true;
            }
            if start.elapsed() >= deadline {
                return false;
            }
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    /// Forwards every [`Effect::Rpc`] to the real engine, mirroring the
    /// production runtime's `Executor::run` dispatch
    /// (`crates/view/src/runtime.rs`) for the subset of [`RpcCall`]
    /// variants a redraw-only driver (no key input, no mouse, no paint
    /// loop to reply to a blocked request) can ever actually produce from
    /// [`update`]. A write failure is dropped rather than surfaced: this
    /// driver has no `Flow::EngineLost`/`Msg::EngineDown` recovery path to
    /// hand it to, and the caller's own `deadline` bound already covers a
    /// wedged connection.
    fn apply_effects(&mut self, effects: Vec<Effect>) {
        for effect in effects {
            let Effect::Rpc(call) = effect else {
                continue;
            };
            let _ = match call {
                RpcCall::TryResize { width, height } => {
                    self.engine.handle.try_resize(width, height)
                }
                RpcCall::Input { notation } => self.engine.handle.input(&notation),
                RpcCall::Paste { text } => self.engine.handle.paste(&text),
                RpcCall::InputMouse {
                    button,
                    action,
                    modifier,
                    row,
                    col,
                } => self
                    .engine
                    .handle
                    .input_mouse(&button, &action, &modifier, row, col),
                RpcCall::GetDefaultHl { generation } => {
                    self.engine.handle.probe_default_hl(generation)
                }
                // RpcCall is #[non_exhaustive]: a future call kind degrades
                // to a no-op here rather than fail to compile, matching
                // Executor::run's own fallback arm.
                _ => Ok(()),
            };
        }
    }

    /// Captures the current [`Surface`] (leg (b): deterministic capture, at
    /// the engine-attached tier).
    #[must_use]
    pub fn surface(&self) -> Surface {
        view_surface::render(&self.model)
    }

    /// Renders the current [`Surface`] to plain text via [`raster::screen_text`].
    #[must_use]
    pub fn screen_text(&self) -> String {
        raster::screen_text(&self.surface(), self.model.engine.grid())
    }

    /// Renders the current [`Surface`] to one row of text per canvas line,
    /// via [`raster::screen_rows`]: the row-indexed form [`crate::compare`]
    /// and [`crate::masked_rows`] need, since a masked row index must line
    /// up with an element index rather than a position inside a joined
    /// string.
    #[must_use]
    pub fn screen_rows(&self) -> Vec<String> {
        raster::screen_rows(&self.surface(), self.model.engine.grid())
    }

    /// Captures the current [`Screen`] -- glyph rows plus per-cell highlight
    /// rows -- for [`crate::compare`], rendering the [`Surface`] once and
    /// feeding it to both [`raster::screen_rows`] and [`raster::attr_rows`]
    /// so the two dumps can never come from different frames. The
    /// highlight rows resolve each grid cell's `hl_id` through this session's
    /// own `HlTable`, the id-independent form the differential compares (see
    /// [`crate::attr`]'s docs).
    #[must_use]
    pub fn screen(&self) -> Screen {
        let surface = self.surface();
        Screen {
            rows: raster::screen_rows(&surface, self.model.engine.grid()),
            attr_rows: raster::attr_rows(
                &surface,
                self.model.engine.grid(),
                self.model.engine.hl(),
            ),
        }
    }

    /// Evaluates `expr` against the real engine and returns its result as
    /// text (leg (d): engine state-parity probes -- buffer text, cursor,
    /// mode, registers -- compared against this session's decoded
    /// [`screen_text`](Self::screen_text) to prove the two agree).
    ///
    /// # Errors
    ///
    /// Returns [`OracleError::Engine`] if the request fails, nvim rejects
    /// the expression, or the reply times out.
    pub fn eval_str(&mut self, expr: &str) -> Result<String, OracleError> {
        self.engine.handle.eval_str(expr).map_err(Into::into)
    }

    /// Reads nvim's current mode name and blocked flag via the fast
    /// `nvim_get_mode` probe (see `EngineHandle::get_mode`): answered even
    /// in the blocked key-wait states where [`eval_str`](Self::eval_str)
    /// would be deferred until the wait ends, which is what lets
    /// [`snapshot`] probe such a session at all instead of timing out.
    ///
    /// # Errors
    ///
    /// Returns [`OracleError::Engine`] if the request fails, the reply
    /// times out, or the reply shape is malformed.
    pub fn get_mode(&mut self) -> Result<(String, bool), OracleError> {
        self.engine.handle.get_mode().map_err(Into::into)
    }
}

impl settle::Settling for EngineSession {
    fn handle(&self) -> &view_engine::handle::EngineHandle {
        &self.engine.handle
    }

    fn take_damage(&self) -> Vec<UiEvent> {
        self.pump.take_damage()
    }

    fn apply_batch(&mut self, events: Vec<UiEvent>) {
        let effects = update(&mut self.model, Msg::Redraw(events));
        self.apply_effects(effects);
    }

    fn markers(&mut self) -> &mut settle::QuiesceMarkers {
        &mut self.markers
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;
    use view_core::events::GridCell;
    use view_core::msg::Key;

    #[test]
    fn session_fed_a_scripted_redraw_and_flush_yields_the_known_screen_text() {
        let mut session = Session::new(5, 2);

        session.feed(Msg::Redraw(vec![
            UiEvent::GridResize {
                grid: 1,
                width: 5,
                height: 2,
            },
            UiEvent::GridLine {
                grid: 1,
                row: 0,
                col_start: 0,
                cells: vec![GridCell {
                    text: "h".to_string(),
                    hl_id: 0,
                    repeat: 1,
                }],
            },
            UiEvent::Flush,
        ]));

        assert_eq!(session.screen_text(), "h    \n     ");
    }

    #[test]
    fn session_feed_ignores_a_key_msg_with_no_engine_to_route_it_to() {
        // Session has no engine/terminal to carry an RpcCall::Input effect
        // out to; feed() must not panic on a Msg whose only effect it has
        // nowhere to send.
        let mut session = Session::new(5, 2);
        session.feed(Msg::Key(Key {
            notation: "x".to_string(),
        }));
        assert_eq!(session.screen_text(), "");
    }
}
