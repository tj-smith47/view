//! The startup sequence (see the design spec's startup-sequence section):
//! paint a themed placeholder shell
//! immediately from the cached theme, spawn the engine and attach on a
//! background thread so a slow-starting nvim can never delay that first
//! paint, buffer keys typed in the gap, and hand everything back to
//! `main.rs` once attach completes.
//!
//! [`paint_shell_frame`] happens entirely outside `runtime::run`'s
//! steady-state loop: nothing has set `Model::dirty` yet at this point,
//! since no `Flush` has ever arrived, so the loop's own `if model.dirty`
//! paint gate would never fire for this very first frame without an
//! explicit call here.

use std::collections::VecDeque;
use std::sync::mpsc::{Receiver, SyncSender};
use std::time::Instant;

use view_core::model::Model;
use view_core::msg::{Key, Msg};
use view_engine::handle::EngineError;
use view_engine::process::{Engine, EngineConfig};
use view_engine::DamagePump;
use view_tui::terminal::Term;

/// How many pre-attach keystrokes [`drain_pre_attach`] holds before it
/// starts dropping the oldest to make room for the newest -- the design
/// spec's "bounded ring of 64".
const KEY_RING_CAPACITY: usize = 64;

/// A fixed-capacity FIFO of pre-attach keystrokes. [`push`](Self::push)
/// evicts the oldest entry once full rather than rejecting the newest: a
/// keystroke the user just typed is more likely to still matter than one
/// typed a while ago, and reports whether an eviction happened so the
/// caller can surface exactly one updated toast per drop instead of
/// staying silent about lost input.
struct KeyRing {
    buf: VecDeque<Key>,
    cap: usize,
}

impl KeyRing {
    fn new(cap: usize) -> Self {
        Self {
            buf: VecDeque::with_capacity(cap),
            cap,
        }
    }

    /// Returns `true` if an older entry was evicted to make room for `key`.
    fn push(&mut self, key: Key) -> bool {
        let evicted = if self.buf.len() >= self.cap {
            self.buf.pop_front();
            true
        } else {
            false
        };
        self.buf.push_back(key);
        evicted
    }

    /// Drains every buffered key, oldest first, for replay through the
    /// normal `Msg::Key` path once attach completes.
    fn drain(&mut self) -> Vec<Key> {
        self.buf.drain(..).collect()
    }
}

/// Paints the very first frame directly, bypassing `runtime::run`'s own
/// `if model.dirty` gate: nothing has set `dirty` yet this early in startup
/// (no `Flush` has ever arrived), so without this explicit call the
/// terminal would show whatever `Term::init` last left on screen until the
/// engine attaches and streams real content -- exactly the blank-startup
/// experience this task exists to close. The caller must have already set
/// `model.content_painted = false` (`Model`'s default is `true`, the
/// ordinary steady state; startup is the one caller that opts into the
/// placeholder) for this frame to show the shell rather than an empty grid.
///
/// Debug builds log the elapsed time since `process_start` to stderr: the
/// design spec's informal 50ms shell-paint target, measured here but not
/// enforced (the formal budget gate lands with the bench harness).
///
/// # Errors
///
/// Returns the underlying `std::io::Error` if the terminal write fails.
pub fn paint_shell_frame(
    term: &mut Term,
    model: &Model,
    process_start: Instant,
) -> std::io::Result<()> {
    let surface = view_surface::render(model);
    term.draw_surface(model, &surface)?;
    log_shell_paint_latency(process_start);
    Ok(())
}

#[cfg(debug_assertions)]
fn log_shell_paint_latency(process_start: Instant) {
    eprintln!(
        "view: shell frame painted {:?} after process start\r",
        process_start.elapsed()
    );
}

#[cfg(not(debug_assertions))]
fn log_shell_paint_latency(_process_start: Instant) {}

/// Spawns `nvim --embed`, registers the `VimEnter` autocmd BEFORE
/// `ui_attach` (see
/// [`EngineHandle::register_vim_enter_autocmd`](view_engine::handle::EngineHandle::register_vim_enter_autocmd)'s
/// doc comment for why that ordering is load-bearing, not incidental),
/// attaches at `width`x`height`, forwards the capability-probe's leftover
/// `residue` bytes, and starts the damage pump feeding `msg_tx`.
fn spawn_and_attach(
    cfg: EngineConfig,
    width: u16,
    height: u16,
    residue: Vec<u8>,
    msg_tx: SyncSender<Msg>,
) -> Result<(Engine, DamagePump), EngineError> {
    let mut engine = Engine::spawn(cfg)?;
    engine
        .handle
        .register_vim_enter_autocmd(engine.api_info.channel_id)?;
    engine.handle.ui_attach(width, height)?;
    // best-effort, matching this project's original startup ordering: a
    // write failure here means the connection is already gone, which the
    // caller discovers through the engine's own EngineDown path moments
    // later rather than through this loop
    for notation in view_tui::keys::encode_residue_bytes(&residue) {
        let _ = engine.handle.input(&notation);
    }
    let pump = engine.start_pump(msg_tx);
    Ok((engine, pump))
}

/// Runs [`spawn_and_attach`] on a background thread so a slow-starting
/// nvim can never delay [`paint_shell_frame`], and returns a receiver that
/// yields its result exactly once, success or failure. The same background
/// thread also sends `Msg::EngineReady` down `msg_tx` right after, so
/// [`drain_pre_attach`]'s blocking loop wakes deterministically instead of
/// polling: by the time that marker is observable on the paired `msg_rx`,
/// the result is already sitting in the returned receiver waiting to be
/// read (same-thread program order between the two sends guarantees this).
pub fn attach_in_background(
    cfg: EngineConfig,
    width: u16,
    height: u16,
    residue: Vec<u8>,
    msg_tx: SyncSender<Msg>,
) -> Receiver<Result<(Engine, DamagePump), EngineError>> {
    let (result_tx, result_rx) = std::sync::mpsc::sync_channel(1);
    let ready_tx = msg_tx.clone();
    std::thread::spawn(move || {
        let result = spawn_and_attach(cfg, width, height, residue, msg_tx);
        let _ = result_tx.send(result);
        let _ = ready_tx.send(Msg::EngineReady);
    });
    result_rx
}

/// Drains `msg_rx` until [`attach_in_background`]'s `Msg::EngineReady`
/// marker arrives, buffering every `Msg::Key` seen in the meantime into a
/// bounded ring of [`KEY_RING_CAPACITY`] (oldest dropped first once full,
/// each drop repainting an updated toast through the normal
/// `view_core::model::Messages` overlay via `Messages::push_native` --
/// never silent). Every other message kind possible in this narrow
/// pre-attach window (a terminal resize, a paste, a mouse event) is out of
/// this task's specified scope and is dropped without ceremony: there is
/// no engine yet to forward any of them to, and only keystrokes carry the
/// design spec's "never silently lose it" contract.
///
/// Returns the buffered keys in arrival order, for the caller to replay
/// through the ordinary `Msg::Key` path once attach has completed.
pub fn drain_pre_attach(msg_rx: &Receiver<Msg>, model: &mut Model, term: &mut Term) -> Vec<Key> {
    let mut ring = KeyRing::new(KEY_RING_CAPACITY);
    let mut dropped: u32 = 0;
    loop {
        match msg_rx.recv() {
            Ok(Msg::Key(key)) => {
                if ring.push(key) {
                    dropped = dropped.saturating_add(1);
                    let plural = if dropped == 1 { "" } else { "s" };
                    model.engine.messages.push_native(
                        format!(
                            "view: startup key buffer full, dropped {dropped} keystroke{plural}"
                        ),
                        dropped > 1,
                    );
                    let surface = view_surface::render(model);
                    let _ = term.draw_surface(model, &surface);
                }
            }
            Ok(Msg::EngineReady) => break,
            Ok(_) => {}
            // the input thread and the attach thread are both gone: nothing
            // left to wait for
            Err(_) => break,
        }
    }
    ring.drain()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    fn key(notation: &str) -> Key {
        Key {
            notation: notation.to_string(),
        }
    }

    #[test]
    fn ring_under_capacity_never_evicts_and_drains_in_order() {
        let mut ring = KeyRing::new(3);
        assert!(!ring.push(key("a")));
        assert!(!ring.push(key("b")));
        let drained = ring.drain();
        assert_eq!(
            drained
                .iter()
                .map(|k| k.notation.as_str())
                .collect::<Vec<_>>(),
            vec!["a", "b"]
        );
    }

    #[test]
    fn ring_at_capacity_evicts_oldest_and_keeps_arrival_order() {
        let mut ring = KeyRing::new(2);
        assert!(!ring.push(key("a")));
        assert!(!ring.push(key("b")));
        // over capacity: "a" (oldest) is evicted to make room for "c"
        assert!(ring.push(key("c")));
        let drained = ring.drain();
        assert_eq!(
            drained
                .iter()
                .map(|k| k.notation.as_str())
                .collect::<Vec<_>>(),
            vec!["b", "c"]
        );
    }

    #[test]
    fn drain_empties_the_ring_so_a_second_drain_yields_nothing() {
        let mut ring = KeyRing::new(4);
        ring.push(key("x"));
        assert_eq!(ring.drain().len(), 1);
        assert!(ring.drain().is_empty());
    }
}
