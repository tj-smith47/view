//! The headless driver stack a compatibility oracle scripts against: three
//! levels of increasing fidelity, per the design spec's differential-oracle
//! requirement (the testing-and-oracle section).
//!
//! - [`Session`]: pure Msg-level driver. No engine, no terminal, no
//!   process; feed it hand-built [`view_core::msg::Msg`]s and read back a
//!   deterministic [`view_surface::Surface`]/screen text. The fast oracle
//!   path for cases that do not need a real nvim to prove.
//! - [`EngineSession`]: a real embedded engine, no terminal. The truth
//!   path: drives actual `nvim_input`, actual redraw traffic, actual
//!   `nvim_eval` state probes, but never touches a pty or the real
//!   terminal.
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

mod parity;
pub mod pty;
mod raster;
mod reference;

use std::sync::mpsc::sync_channel;
use std::time::{Duration, Instant};

use view_core::events::UiEvent;
use view_core::model::Model;
use view_core::msg::Msg;
use view_core::update::update;
use view_engine::handle::EngineError;
use view_engine::process::{Engine, EngineConfig};
use view_engine::DamagePump;
use view_surface::Surface;

pub use parity::{compare, masked_rows, snapshot, Divergence, Probe, StateSnapshot};
pub use pty::PtySession;
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
        raster::screen_text(&self.surface(), &self.model.engine.grid)
    }
}

/// Engine-attached headless driver: a real embedded engine, no terminal.
/// The truth path (leg (c): harness-owned polling; leg (d): engine
/// state-parity probes via [`eval_str`](Self::eval_str)).
pub struct EngineSession {
    model: Model,
    engine: Engine,
    pump: DamagePump,
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
    /// Returns [`OracleError::Engine`] if the process fails to spawn or the
    /// `ui_attach` handshake fails or times out.
    pub fn spawn(cols: u16, rows: u16) -> Result<Self, OracleError> {
        let mut engine = Engine::spawn(EngineConfig {
            extra_args: vec!["--clean".into(), "-n".into()],
            ..EngineConfig::default()
        })?;
        engine.handle.ui_attach(cols, rows)?;
        // no consumer ever drains this channel: EngineSession polls
        // DamagePump::take_damage directly instead (leg (c) is
        // harness-owned polling, not a blocking recv on a sink), and
        // RedrawReady tokens are safely lossy (see view_engine::damage's
        // module docs) when nothing ever removes them
        let (sink, _unused_rx) = sync_channel(64);
        let (pump, _cutover) = engine.start_pump(sink);
        Ok(Self {
            model: Model::with_term_size(cols, rows),
            engine,
            pump,
        })
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
    /// module docs: the production runtime loop's only wait is one
    /// blocking `recv`, with no timer anywhere in its body).
    pub fn pump_until_flush(&mut self, deadline: Duration) -> bool {
        let start = Instant::now();
        loop {
            let events = self.pump.take_damage();
            let saw_flush = events.iter().any(|e| matches!(e, UiEvent::Flush));
            if !events.is_empty() {
                let _ = update(&mut self.model, Msg::Redraw(events));
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

    /// Captures the current [`Surface`] (leg (b): deterministic capture, at
    /// the engine-attached tier).
    #[must_use]
    pub fn surface(&self) -> Surface {
        view_surface::render(&self.model)
    }

    /// Renders the current [`Surface`] to plain text via [`raster::screen_text`].
    #[must_use]
    pub fn screen_text(&self) -> String {
        raster::screen_text(&self.surface(), &self.model.engine.grid)
    }

    /// Renders the current [`Surface`] to one row of text per canvas line,
    /// via [`raster::screen_rows`]: the row-indexed form [`crate::compare`]
    /// and [`crate::masked_rows`] need, since a masked row index must line
    /// up with an element index rather than a position inside a joined
    /// string.
    #[must_use]
    pub fn screen_rows(&self) -> Vec<String> {
        raster::screen_rows(&self.surface(), &self.model.engine.grid)
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
