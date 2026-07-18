//! Raw-mode / alternate-screen lifecycle for the interactive terminal, plus
//! the typed facade over `ratatui`/`crossterm` that keeps both crates out of
//! the `view` bin crate's dependency graph (`scripts/audit-deps.sh` denies
//! `view -> crossterm` and `view -> ratatui`: only `view-tui` may touch the
//! terminal).

use crate::keys::encode_key;
use crate::mouse::encode_mouse;
use crate::paint::composite;
use crate::tiers;
use crossterm::event::{DisableMouseCapture, EnableMouseCapture, Event};
use std::io::Write;
use std::sync::mpsc::SyncSender;
use view_core::model::{Model, TermCaps, Tier};
use view_core::msg::{Key, Msg};
use view_surface::{CursorShape, Surface};

/// Enters raw mode and the alternate screen for the lifetime of the value,
/// restoring both on drop and installing a panic hook that restores the
/// terminal before the default panic message prints.
///
/// This is the only place in the crate that enables raw mode, enters the
/// alternate screen, or installs a panic hook: [`Term::init`] holds one of
/// these as a field rather than repeating the setup (`ratatui::try_init`
/// does its own raw-mode/alt-screen/panic-hook dance, which would otherwise
/// chain a second, redundant hook and re-enter the alternate screen on top
/// of this one).
pub struct TerminalGuard;

impl TerminalGuard {
    /// The first half of entry: enables raw mode and installs a panic hook
    /// that restores the terminal before delegating to the previous hook.
    ///
    /// Split from alternate-screen entry (see
    /// [`finish_entering_alt_screen`](Self::finish_entering_alt_screen)) so
    /// [`Term::init`] can run capability detection in between: detection's
    /// CSI replies are only readable once canonical mode's line buffering,
    /// echo, and missing newline terminator are off, which raw mode alone
    /// provides, and the detection log line must still print to the
    /// visible screen rather than a not-yet-entered alternate buffer.
    ///
    /// # Errors
    ///
    /// Returns the underlying `std::io::Error` if raw mode cannot be
    /// enabled.
    pub fn enter_raw_mode() -> std::io::Result<Self> {
        crossterm::terminal::enable_raw_mode()?;
        // a panic must restore the terminal before the message prints, or the
        // user is left with a broken shell and an invisible error; installed
        // right after raw mode rather than after the alternate screen so a
        // panic during capability detection is covered too
        let prev = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            restore();
            prev(info);
        }));
        Ok(Self)
    }

    /// The second half of entry: switches to the alternate screen and
    /// enables bracketed-paste reporting. Must be called exactly once,
    /// after [`enter_raw_mode`](Self::enter_raw_mode).
    ///
    /// Bracketed paste is enabled unconditionally here (unlike mouse
    /// capture, which [`Term::draw_surface`] toggles only while nvim
    /// reports `mouse_on`): a paste is never ambiguous with ordinary typed
    /// input the way raw mouse tracking would be with the host terminal's
    /// own selection/scrollback gestures, so there is no reason to gate it
    /// behind engine state.
    ///
    /// # Errors
    ///
    /// Returns the underlying `std::io::Error` if the alternate screen
    /// cannot be entered.
    pub fn finish_entering_alt_screen(&self) -> std::io::Result<()> {
        crossterm::execute!(
            std::io::stdout(),
            crossterm::terminal::EnterAlternateScreen,
            crossterm::event::EnableBracketedPaste
        )
    }

    /// Restores the terminal immediately rather than waiting for [`Drop`].
    ///
    /// For exit paths that bypass destructors (`std::process::exit`).
    /// Safe to call even if [`Drop`] still runs afterward: leaving the
    /// alternate screen and disabling raw mode a second time on an already
    /// restored terminal is a no-op, not an error.
    pub fn restore_now(&self) {
        restore();
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        restore();
    }
}

fn restore() {
    // disabled unconditionally, even though mouse capture is only ever
    // turned on dynamically (see Term::draw_surface): leaving it enabled
    // across process exit would swallow the host shell's own mouse
    // gestures until the terminal emulator itself is reset
    let _ = crossterm::execute!(
        std::io::stdout(),
        DisableMouseCapture,
        crossterm::event::DisableBracketedPaste,
        crossterm::terminal::LeaveAlternateScreen
    );
    let _ = crossterm::terminal::disable_raw_mode();
    let _ = std::io::stdout().flush();
}

/// Writes a synchronized-update bracket escape (`CSI ? 2026 h` to begin,
/// `CSI ? 2026 l` to end) directly to stdout, bypassing `ratatui`'s own
/// buffered writer: the bracket must wrap the entire frame write including
/// the cursor move that follows it, not just the buffer diff `ratatui`
/// flushes internally.
fn write_sync_bracket(begin: bool) -> std::io::Result<()> {
    let seq: &[u8] = if begin {
        b"\x1b[?2026h"
    } else {
        b"\x1b[?2026l"
    };
    let mut out = std::io::stdout();
    out.write_all(seq)?;
    out.flush()
}

/// Maps a [`CursorShape`] to its DECSCUSR steady parameter: `2` (block),
/// `4` (underline/horizontal), `6` (bar/vertical). Steady rather than
/// blinking (`1`/`3`/`5`): a deterministic cursor is safer to test against
/// and there is no terminal-side blink capability probe to key a choice of
/// blinking variant on; `CursorShape` itself carries no blink state to
/// select a blinking variant from.
fn decscusr_param(shape: CursorShape) -> u8 {
    match shape {
        CursorShape::Block => 2,
        CursorShape::Horizontal(_) => 4,
        CursorShape::Vertical(_) => 6,
        // CursorShape is #[non_exhaustive]: a future shape falls back to the
        // steady block rather than failing to compile
        _ => 2,
    }
}

/// Writes the DECSCUSR cursor-shape escape (`CSI n SP q`) for `shape` to
/// `writer`. Generic over `Write` (rather than writing straight to stdout
/// like [`write_sync_bracket`]) so the byte sequence itself is unit
/// testable against an injected `Vec<u8>` writer instead of only being
/// provable via a live terminal.
fn write_cursor_shape<W: Write>(writer: &mut W, shape: CursorShape) -> std::io::Result<()> {
    write!(writer, "\x1b[{} q", decscusr_param(shape))
}

/// The ratatui-backed terminal: draws grid frames and reports its size,
/// without exposing `ratatui` types to callers outside this crate.
pub struct Term {
    guard: TerminalGuard,
    inner: ratatui::DefaultTerminal,
    /// The last DECSCUSR shape written, so `draw_surface` only re-emits the
    /// escape when the `Surface` cursor's shape actually changed instead of
    /// writing it unconditionally on every frame.
    last_cursor_shape: Option<CursorShape>,
    /// The last mouse-capture state written, so `draw_surface` only toggles
    /// crossterm's mouse capture when `model.engine.mouse_on` actually
    /// changed since the last frame instead of writing the escape on every
    /// paint. `None` before the first frame, matching `last_cursor_shape`'s
    /// convention.
    last_mouse_capture: Option<bool>,
    /// The capabilities resolved during [`Term::init`], either probed or
    /// from a `--tier` override. Stored so [`Term::caps`] can hand a copy
    /// to the caller without re-running the (stdin-consuming, one-shot)
    /// detection probe.
    caps: TermCaps,
    /// Bytes the capability probe read that were not part of a recognized
    /// reply -- almost always keystrokes the user typed before or during
    /// the probe window. [`Term::take_residue`] hands this to the caller
    /// exactly once so nothing typed at startup is silently lost.
    residue: Vec<u8>,
}

impl Term {
    /// Initializes the backend terminal: raw mode first, then capability
    /// detection (or `tier_override` if given), then the alternate screen,
    /// matching [`TerminalGuard::enter_raw_mode`]'s ordering contract; the
    /// ratatui terminal is constructed last, directly over the now-prepared
    /// stdout.
    ///
    /// Deliberately does not use `ratatui::try_init`: that function repeats
    /// the same raw-mode/alternate-screen/panic-hook setup `TerminalGuard`
    /// already did, which would enter the alternate screen twice and chain
    /// a second panic hook on top of the first.
    ///
    /// # Errors
    ///
    /// Returns the underlying `std::io::Error` if raw mode or the alternate
    /// screen cannot be entered, if capability detection's I/O fails, or if
    /// the backend terminal cannot be built.
    pub fn init(tier_override: Option<Tier>) -> std::io::Result<Self> {
        let guard = TerminalGuard::enter_raw_mode()?;
        let (caps, residue) = tiers::resolve(tier_override)?;
        guard.finish_entering_alt_screen()?;
        let inner =
            ratatui::Terminal::new(ratatui::backend::CrosstermBackend::new(std::io::stdout()))?;
        Ok(Self {
            guard,
            inner,
            last_cursor_shape: None,
            last_mouse_capture: None,
            caps,
            residue,
        })
    }

    /// The capabilities resolved at [`Term::init`], for the caller to wire
    /// into `Model.caps`.
    #[must_use]
    pub fn caps(&self) -> TermCaps {
        self.caps
    }

    /// Takes ownership of the capability probe's residue bytes, leaving an
    /// empty buffer behind. Callable exactly once per [`Term::init`] with a
    /// meaningful result -- a second call returns an empty `Vec`, which is
    /// correct rather than surprising, since there is nothing left to take.
    ///
    /// The caller (`main.rs`, right after `ui_attach`) is expected to
    /// translate this into nvim input notation (see
    /// [`encode_residue_bytes`](crate::keys::encode_residue_bytes)) and
    /// forward it before the runtime loop starts, so a keystroke queued
    /// ahead of or during the startup probe is never silently dropped.
    #[must_use]
    pub fn take_residue(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.residue)
    }

    /// Current terminal size in `(width, height)` cells.
    ///
    /// # Errors
    ///
    /// Returns the underlying `std::io::Error` if the backend cannot report
    /// its size.
    pub fn size(&self) -> std::io::Result<(u16, u16)> {
        let size = self.inner.size()?;
        Ok((size.width, size.height))
    }

    /// Paints one frame from `surface`, then moves the real terminal cursor
    /// to `surface.cursor`'s position and shape (hiding it entirely when
    /// `None`). The runtime loop's only paint call: it renders a
    /// [`view_surface::Surface`] from the model first, then hands both the
    /// model and that surface here.
    ///
    /// `surface` describes *where* to paint and *what kind* of content goes
    /// there; the grid's own per-cell content still comes from `model`
    /// directly (see [`composite`]), so a `Surface` never needs to clone the
    /// grid to be paintable.
    ///
    /// When `model.caps.sync` is set, the whole write (paint plus cursor
    /// move) is wrapped in a terminal synchronized-update bracket
    /// (`CSI ? 2026 h` / `l`) so the terminal applies it atomically instead
    /// of showing a partially painted frame; this is two extra escape-code
    /// writes with no other added per-frame cost, and only when `sync` is
    /// set (conservative by construction: `TermCaps::default()` keeps it
    /// false until capability detection lands).
    ///
    /// The cursor's position is set every frame the cursor is visible
    /// (cheap, and correctness-critical: a stale position is wrong the
    /// instant the grid cursor moves), but its DECSCUSR shape escape is
    /// only written when [`CursorShape`] actually changed since the last
    /// frame, since re-emitting it unconditionally would be a needless
    /// terminal write on every single paint.
    ///
    /// Terminal mouse capture (`EnableMouseCapture`/`DisableMouseCapture`)
    /// tracks `model.engine.mouse_on` the same way: written once when it
    /// changes, never unconditionally. Capture is off by default and only
    /// turns on once nvim's own `redraw` stream reports `mouse_on`, so a
    /// buffer with `'mouse'` unset never steals the host terminal's
    /// selection/scrollback gestures.
    ///
    /// # Errors
    ///
    /// Returns the underlying `std::io::Error` if the backend write fails.
    pub fn draw_surface(&mut self, model: &Model, surface: &Surface) -> std::io::Result<()> {
        if self.last_mouse_capture != Some(model.engine.mouse_on) {
            let mut out = std::io::stdout();
            if model.engine.mouse_on {
                crossterm::execute!(out, EnableMouseCapture)?;
            } else {
                crossterm::execute!(out, DisableMouseCapture)?;
            }
            out.flush()?;
            self.last_mouse_capture = Some(model.engine.mouse_on);
        }
        if model.caps.sync {
            write_sync_bracket(true)?;
        }
        self.inner.draw(|f| composite(model, surface, f))?;
        match surface.cursor {
            Some(spec) => {
                self.inner.set_cursor_position((spec.col, spec.row))?;
                self.inner.show_cursor()?;
                if self.last_cursor_shape != Some(spec.shape) {
                    let mut out = std::io::stdout();
                    write_cursor_shape(&mut out, spec.shape)?;
                    out.flush()?;
                    self.last_cursor_shape = Some(spec.shape);
                }
            }
            None => self.inner.hide_cursor()?,
        }
        if model.caps.sync {
            write_sync_bracket(false)?;
        }
        Ok(())
    }

    /// Restores the terminal immediately rather than waiting for [`Drop`].
    ///
    /// `std::process::exit` bypasses destructors, so the exit path that
    /// propagates nvim's real exit code must restore explicitly before
    /// calling it; every other exit path is covered by `Drop` on the
    /// contained [`TerminalGuard`]. Delegates to the guard rather than
    /// calling `ratatui::restore()` directly, which would be a second,
    /// independent teardown path alongside the guard's own.
    pub fn restore_now(&mut self) {
        self.guard.restore_now();
    }
}

/// Spawns a dedicated thread that blocks on `crossterm::event::read()` and
/// forwards every key, resize, paste, or mouse event to `tx` as a core
/// [`Msg`], translating key events via [`encode_key`] and mouse events via
/// [`encode_mouse`]. Events with no nvim equivalent (key releases, keys
/// with no notation) are dropped rather than forwarded. Exits once
/// `crossterm::event::read()` errors or `tx`'s receiver is gone.
///
/// Blocking on a dedicated thread rather than polling on the runtime loop's
/// own thread is what lets the loop's `recv()` wake immediately on a
/// keystroke: a poll-based drain needs a timeout to bound how long it can go
/// without checking input, which is exactly the structural latency this
/// design removes. A blocking `send` (not `try_send`) is deliberate: a
/// dropped keystroke is never an acceptable loss the way a coalescible
/// redraw token is, so this thread blocks rather than discards when the
/// channel is momentarily full.
pub fn spawn_input_thread(tx: SyncSender<Msg>) {
    std::thread::spawn(move || {
        while let Ok(event) = crossterm::event::read() {
            let msg = match event {
                Event::Key(k) => encode_key(&k).map(|notation| Msg::Key(Key { notation })),
                Event::Resize(width, height) => Some(Msg::Resized { width, height }),
                Event::Paste(text) => Some(Msg::Paste(text)),
                Event::Mouse(m) => Some(Msg::Mouse(encode_mouse(&m))),
                _ => None,
            };
            if let Some(msg) = msg {
                if tx.send(msg).is_err() {
                    break;
                }
            }
        }
    });
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    #[test]
    fn write_cursor_shape_emits_decscusr_for_each_steady_variant() {
        let mut buf = Vec::new();
        write_cursor_shape(&mut buf, CursorShape::Block).unwrap();
        assert_eq!(buf, b"\x1b[2 q", "block is DECSCUSR 2");

        buf.clear();
        write_cursor_shape(&mut buf, CursorShape::Horizontal(50)).unwrap();
        assert_eq!(buf, b"\x1b[4 q", "horizontal/underline is DECSCUSR 4");

        buf.clear();
        write_cursor_shape(&mut buf, CursorShape::Vertical(25)).unwrap();
        assert_eq!(buf, b"\x1b[6 q", "vertical/bar is DECSCUSR 6");
    }
}
