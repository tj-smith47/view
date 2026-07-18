//! The startup sequence (see the design spec's startup-sequence section):
//! paint a themed placeholder shell
//! immediately from the cached theme, spawn the engine and attach on a
//! background thread so a slow-starting nvim can never delay that first
//! paint, buffer keys (and the latest resize) seen in the gap, and hand
//! everything back to `main.rs` once attach completes.
//!
//! [`paint_shell_frame`] happens entirely outside `runtime::run`'s
//! steady-state loop: nothing has set `Model::dirty` yet at this point,
//! since no `Flush` has ever arrived, so the loop's own `if model.dirty`
//! paint gate would never fire for this very first frame without an
//! explicit call here.
//!
//! [`attach_in_background`] deliberately never calls
//! [`Engine::start_pump`]: only `main.rs` does, strictly after
//! [`drain_pre_attach`] has already observed the [`Msg::EngineReady`]
//! marker this module sends and the buffered window has been replayed.
//! See `attach_in_background`'s doc comment for the full ordering
//! argument this depends on.

use std::collections::VecDeque;
use std::sync::mpsc::{Receiver, SyncSender};
use std::time::Instant;

use view_core::model::Model;
use view_core::msg::{Key, Msg};
use view_engine::handle::EngineError;
use view_engine::process::{Engine, EngineConfig};
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
/// engine attaches and streams real content -- a blank screen for however
/// long attach takes, rather than an immediate themed placeholder. The caller must have already set
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

/// Distinguishes a failure launching the `nvim` process at all (a missing
/// or broken `--nvim-bin`/`PATH` binary, most commonly) from a failure
/// during the post-spawn handshake/attach sequence against a process that
/// did launch, so `main.rs` can report each with its own actionable
/// context instead of one blanket "attach failed" message that could mean
/// either.
#[derive(Debug)]
pub enum AttachFailure {
    /// `Engine::spawn` itself failed: the process never started.
    Spawn(EngineError),
    /// The process started, but registering the `VimEnter` autocmd or
    /// `ui_attach` failed or timed out.
    Attach(EngineError),
}

/// Spawns `nvim --embed`, registers the `VimEnter` autocmd BEFORE
/// `ui_attach` (see
/// [`EngineHandle::register_vim_enter_autocmd`](view_engine::handle::EngineHandle::register_vim_enter_autocmd)'s
/// doc comment for why that ordering is load-bearing, not incidental),
/// attaches at `width`x`height`, and forwards the capability-probe's
/// leftover `residue` bytes. Deliberately does not call
/// [`Engine::start_pump`]: only `main.rs` does, once the buffered
/// pre-attach window has been fully replayed (see
/// [`attach_in_background`]'s doc comment for why).
fn spawn_and_attach(
    cfg: EngineConfig,
    width: u16,
    height: u16,
    residue: Vec<u8>,
) -> Result<Engine, AttachFailure> {
    let engine = Engine::spawn(cfg).map_err(AttachFailure::Spawn)?;
    engine
        .handle
        .register_vim_enter_autocmd(engine.api_info.channel_id)
        .map_err(AttachFailure::Attach)?;
    engine
        .handle
        .ui_attach(width, height)
        .map_err(AttachFailure::Attach)?;
    // best-effort, matching this project's original startup ordering: a
    // write failure here means the connection is already gone, which the
    // caller discovers through the engine's own EngineDown path moments
    // later rather than through this loop
    for notation in view_tui::keys::encode_residue_bytes(&residue) {
        let _ = engine.handle.input(&notation);
    }
    Ok(engine)
}

/// Runs [`spawn_and_attach`] on a background thread so a slow-starting
/// nvim can never delay [`paint_shell_frame`], and returns a receiver that
/// yields its result exactly once, success or failure. The same background
/// thread also sends `Msg::EngineReady` down `msg_tx` right after, so
/// [`drain_pre_attach`]'s blocking loop wakes deterministically instead of
/// polling, with no timer and no poll.
///
/// # Ordering: no pump message can ever precede `EngineReady` in `msg_tx`
///
/// This function never calls [`Engine::start_pump`] -- `main.rs` is the
/// only caller of `start_pump` in the whole process, and it only calls it
/// after `drain_pre_attach` has already returned (having observed this
/// exact `EngineReady` send, or a channel disconnect) and the buffered
/// window has been fully replayed. `Engine::start_pump` -> `attach_sink`
/// (`view-engine`'s `damage` module) is the *only* code path that connects
/// the engine's pump (and therefore any `Msg::RedrawReady`,
/// `Msg::EngineRequest`, or `Msg::EngineStopped`) to `msg_tx` at all --
/// before it runs, those messages stay staged in the pump's own presink,
/// untouched by anything reading `msg_tx`. Since that call cannot happen
/// until strictly after this `EngineReady` send has already been consumed
/// by `drain_pre_attach`, it is structurally impossible -- not merely
/// unobserved in testing -- for a pump-routed message to land in `msg_tx`
/// ahead of `EngineReady`. This is why `drain_pre_attach`'s catch-all match
/// arm for "any other message kind" is correct rather than merely lucky:
/// there is no other kind of message this loop could ever see before
/// `EngineReady`, besides `Msg::Key` and `Msg::Resized` from the input
/// thread.
pub fn attach_in_background(
    cfg: EngineConfig,
    width: u16,
    height: u16,
    residue: Vec<u8>,
    msg_tx: SyncSender<Msg>,
) -> Receiver<Result<Engine, AttachFailure>> {
    let (result_tx, result_rx) = std::sync::mpsc::sync_channel(1);
    std::thread::spawn(move || {
        let result = spawn_and_attach(cfg, width, height, residue);
        let _ = result_tx.send(result);
        // sent unconditionally, success or failure: drain_pre_attach must
        // wake up either way, so main.rs can move on to read engine_rx and
        // report the failure instead of blocking forever on an attach that
        // will never come
        let _ = msg_tx.send(Msg::EngineReady);
    });
    result_rx
}

/// The pre-attach window's buffered input, for the caller to replay
/// directly through `update()`/`Executor` (never by re-enqueuing onto the
/// bounded channel `msg_tx` -- see `main.rs`'s replay call site for why)
/// once attach completes.
pub struct DrainedInput {
    /// The most recent terminal size observed during the window, if it was
    /// resized at all. Only the latest matters at cutover: an intermediate
    /// size mid-resize is already stale by the time attach completes, and
    /// there is no engine yet during the window to notify of any of them.
    pub resize: Option<(u16, u16)>,
    /// Buffered keystrokes, oldest first.
    pub keys: Vec<Key>,
}

/// Drains `msg_rx` until [`attach_in_background`]'s `Msg::EngineReady`
/// marker arrives, buffering every `Msg::Key` seen in the meantime into a
/// bounded ring of [`KEY_RING_CAPACITY`] (oldest dropped first once full,
/// each drop repainting an updated toast through the normal
/// `view_core::model::Messages` overlay via `Messages::push_native` --
/// never silent), and tracking the latest `Msg::Resized` seen, if any.
///
/// No other message kind can reach `msg_rx` before `EngineReady` -- see
/// [`attach_in_background`]'s doc comment for the ordering argument. A
/// paste or mouse event is still out of this window's scope even though it
/// is unreachable here in practice: there is no engine yet to forward
/// either to, and only keystrokes and the terminal size are retained
/// across the window.
///
/// Delegates the drop-overflow repaint to [`drain_pre_attach_with`] so the
/// accumulation logic itself (the part [`Msg`] kind does what, and which
/// one wins on conflict) is testable without a live [`Term`], which
/// `cargo test` cannot construct outside a real tty.
pub fn drain_pre_attach(
    msg_rx: &Receiver<Msg>,
    model: &mut Model,
    term: &mut Term,
) -> DrainedInput {
    drain_pre_attach_with(msg_rx, model, |model| {
        let surface = view_surface::render(model);
        let _ = term.draw_surface(model, &surface);
    })
}

/// The accumulation logic behind [`drain_pre_attach`], generic over the
/// overflow repaint so tests can supply a no-op (or call-counting) closure
/// instead of a real [`Term`].
fn drain_pre_attach_with(
    msg_rx: &Receiver<Msg>,
    model: &mut Model,
    mut repaint: impl FnMut(&mut Model),
) -> DrainedInput {
    let mut ring = KeyRing::new(KEY_RING_CAPACITY);
    let mut resize = None;
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
                    repaint(model);
                }
            }
            Ok(Msg::Resized { width, height }) => resize = Some((width, height)),
            Ok(Msg::EngineReady) => break,
            // structurally unreachable before EngineReady (see
            // attach_in_background's doc comment); kept for the same
            // defensive-totality reason update()'s own no-op arms are
            Ok(_) => {}
            // the input thread and the attach thread are both gone: nothing
            // left to wait for
            Err(_) => break,
        }
    }
    DrainedInput {
        resize,
        keys: ring.drain(),
    }
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

    #[test]
    fn drain_pre_attach_keeps_only_the_latest_resize_and_every_key_in_order() {
        let (tx, rx) = std::sync::mpsc::sync_channel(16);
        tx.send(Msg::Resized {
            width: 80,
            height: 24,
        })
        .unwrap();
        tx.send(Msg::Key(key("a"))).unwrap();
        // a later resize supersedes the first: only the final size at
        // cutover matters
        tx.send(Msg::Resized {
            width: 120,
            height: 40,
        })
        .unwrap();
        tx.send(Msg::Key(key("b"))).unwrap();
        tx.send(Msg::EngineReady).unwrap();

        let mut model = Model::with_term_size(80, 24);
        let drained = drain_pre_attach_with(&rx, &mut model, |_| {});

        assert_eq!(drained.resize, Some((120, 40)));
        assert_eq!(
            drained
                .keys
                .iter()
                .map(|k| k.notation.as_str())
                .collect::<Vec<_>>(),
            vec!["a", "b"]
        );
    }

    #[test]
    fn drain_pre_attach_returns_on_channel_disconnect_with_no_engine_ready() {
        let (tx, rx) = std::sync::mpsc::sync_channel(4);
        tx.send(Msg::Key(key("a"))).unwrap();
        drop(tx);

        let mut model = Model::with_term_size(80, 24);
        let drained = drain_pre_attach_with(&rx, &mut model, |_| {});

        assert_eq!(
            drained
                .keys
                .iter()
                .map(|k| k.notation.as_str())
                .collect::<Vec<_>>(),
            vec!["a"]
        );
        assert_eq!(drained.resize, None);
    }

    #[test]
    fn drain_pre_attach_repaints_once_per_eviction_and_keeps_the_ring_at_capacity() {
        let (tx, rx) = std::sync::mpsc::sync_channel(KEY_RING_CAPACITY + 2);
        for i in 0..KEY_RING_CAPACITY + 1 {
            tx.send(Msg::Key(key(&i.to_string()))).unwrap();
        }
        tx.send(Msg::EngineReady).unwrap();

        let mut model = Model::with_term_size(80, 24);
        let repaints = std::cell::RefCell::new(0u32);
        let drained = drain_pre_attach_with(&rx, &mut model, |_| {
            *repaints.borrow_mut() += 1;
        });

        assert_eq!(drained.keys.len(), KEY_RING_CAPACITY);
        // the oldest ("0") was evicted to make room for the (KEY_RING_CAPACITY)-th key
        assert_eq!(drained.keys[0].notation, "1");
        assert_eq!(*repaints.borrow(), 1);
    }
}
