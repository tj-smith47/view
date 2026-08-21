//! Raw-mode / alternate-screen lifecycle for the interactive terminal, plus
//! the typed facade over `ratatui`/`crossterm` that keeps both crates out of
//! the `view` bin crate's dependency graph (`scripts/audit-deps.sh` denies
//! `view -> crossterm` and `view -> ratatui`: only `view-tui` may touch the
//! terminal).

use crate::paint::{overlay_rows, Damage, Shadow};
use crate::tiers;
use crossterm::event::{DisableMouseCapture, EnableMouseCapture};
use ratatui::backend::Backend;
use std::cell::RefCell;
use std::io::Write;
use std::rc::Rc;
#[cfg(not(unix))]
use std::sync::mpsc::SyncSender;
use view_core::grid::GridDamage;
use view_core::model::{Model, TermCaps, Tier};
#[cfg(not(unix))]
use view_core::msg::Msg;
use view_surface::{CursorShape, Surface};

/// Owns raw mode and the alternate screen for the lifetime of the value,
/// restoring both (plus mouse capture and bracketed paste) on drop.
/// Entry is two phases, not one: [`enter_raw_mode`](Self::enter_raw_mode)
/// constructs the guard, enabling raw mode and installing a panic hook that
/// restores the terminal before the default panic message prints;
/// [`finish_entering_alt_screen`](Self::finish_entering_alt_screen) is
/// called separately afterward to enter the alternate screen and enable
/// bracketed paste, once capability detection has had a chance to run in
/// between (see that method's own doc for why the split exists). Both
/// phases restore together, whether entry finished or not: [`Drop`] and
/// [`restore_now`](Self::restore_now) both call the same unconditional
/// [`restore`] that undoes every phase's effects regardless of how far
/// entry got.
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

/// Writes every teardown escape to `out`: the synchronized-update close
/// first, then mouse capture, bracketed paste, and the alternate screen.
/// Generic over `Write` (mirrors [`write_cursor_shape`]) so the byte
/// sequence and ordering are unit-testable against a `Vec<u8>` instead of
/// only provable via a live terminal.
///
/// The ESU close (`CSI ?2026l`) is written unconditionally, with no check
/// for whether `draw_surface`'s bracket is believed open: `restore` runs on
/// every exit path, including a panic or fatal engine exit mid-frame, where
/// `draw_surface` may have opened the bracket and never reached its own
/// closing write. Ordered before [`LeaveAlternateScreen`] deliberately: the
/// bracket must close while still on the screen that opened it, not after
/// switching back to the one the host shell was showing.
fn restore_bytes<W: Write>(out: &mut W) -> std::io::Result<()> {
    out.write_all(b"\x1b[?2026l")?;
    // mouse capture disabled unconditionally, even though it is only ever
    // turned on dynamically (see Term::draw_surface): leaving it enabled
    // across process exit would swallow the host shell's own mouse
    // gestures until the terminal emulator itself is reset
    crossterm::execute!(
        out,
        DisableMouseCapture,
        crossterm::event::DisableBracketedPaste,
        crossterm::terminal::LeaveAlternateScreen
    )
}

fn restore() {
    let mut out = std::io::stdout();
    let _ = restore_bytes(&mut out);
    let _ = crossterm::terminal::disable_raw_mode();
    let _ = out.flush();
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
/// `writer`. Generic over `Write` so the byte sequence itself is unit
/// testable against an injected `Vec<u8>` writer instead of only being
/// provable via a live terminal.
fn write_cursor_shape<W: Write>(writer: &mut W, shape: CursorShape) -> std::io::Result<()> {
    write!(writer, "\x1b[{} q", decscusr_param(shape))
}

const BASE64_ALPHABET: &[u8; 64] =
    b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// Minimal RFC 4648 base64 encoder (standard alphabet, `=` padding).
/// Hand-rolled rather than a dependency: OSC 52 is the only base64 consumer
/// this crate has, and the whole encoder is smaller than the `Cargo.lock`
/// diff and `scripts/audit-deps.sh` row a new crate would cost for it.
fn write_base64<W: Write>(writer: &mut W, bytes: &[u8]) -> std::io::Result<()> {
    for chunk in bytes.chunks(3) {
        let b0 = chunk[0];
        let b1 = chunk.get(1).copied();
        let b2 = chunk.get(2).copied();
        let n =
            (u32::from(b0) << 16) | (u32::from(b1.unwrap_or(0)) << 8) | u32::from(b2.unwrap_or(0));
        let c0 = BASE64_ALPHABET[usize::try_from((n >> 18) & 0x3f).unwrap_or(0)];
        let c1 = BASE64_ALPHABET[usize::try_from((n >> 12) & 0x3f).unwrap_or(0)];
        let c2 = if b1.is_some() {
            BASE64_ALPHABET[usize::try_from((n >> 6) & 0x3f).unwrap_or(0)]
        } else {
            b'='
        };
        let c3 = if b2.is_some() {
            BASE64_ALPHABET[usize::try_from(n & 0x3f).unwrap_or(0)]
        } else {
            b'='
        };
        writer.write_all(&[c0, c1, c2, c3])?;
    }
    Ok(())
}

/// Writes an OSC 52 clipboard-set escape (`ESC ] 5 2 ; {selection} ;
/// {base64} ESC \`, `:help clipboard-osc52`) for `text` to `writer`.
/// `register` selects the selection code the same way nvim's own bundled
/// `lua/vim/ui/clipboard/osc52.lua` provider does (read directly off the
/// pinned install, see `docs/clipboard-provider-wire-capture.md`): `'*'`
/// maps to the primary-selection code `p`, everything else (in practice
/// always `'+'`) to the clipboard code `c`. Generic over `Write` for the
/// same testability reason as `write_cursor_shape`.
fn write_osc52_bytes<W: Write>(writer: &mut W, register: char, text: &str) -> std::io::Result<()> {
    let selection = if register == '*' { 'p' } else { 'c' };
    write!(writer, "\x1b]52;{selection};")?;
    write_base64(writer, text.as_bytes())?;
    write!(writer, "\x1b\\")
}

/// The whole-terminal paint area for one frame, sized from the model's
/// last-known terminal dimensions rather than an OS query on every frame.
///
/// `Model::term_width`/`term_height` are fed by `Msg::Resized` and startup
/// wiring, so reading them here keeps [`Term::draw_surface`] off the
/// per-frame `TIOCGWINSZ` syscall while still following every resize on the
/// next frame: the shadow's own `resize` sees the new area and forces a full
/// repaint, exactly as it did when the size came from the syscall.
fn frame_area(model: &Model) -> ratatui::layout::Rect {
    ratatui::layout::Rect::new(0, 0, model.term_width, model.term_height)
}

/// A frame-scoped byte accumulator standing in for stdout as ratatui's
/// backend writer: `write` appends, `flush` is a no-op, so everything
/// ratatui and the cursor/bracket escapes emit for one frame coalesces
/// into a single buffer [`Term::draw_surface`] then writes to the real
/// terminal in ONE `write`+`flush`. One syscall per frame instead of
/// ~5 (bracket open, content flush, cursor position, cursor show,
/// bracket close): each separate pty write costs a kernel copy plus a
/// reader wakeup, which the output-path bench measured as the majority
/// of the paint segment's non-CPU time.
///
/// `Rc<RefCell<..>>` rather than a plain field because ratatui owns its
/// backend writer for the terminal's lifetime while `draw_surface` must
/// also drain the same buffer after each draw; `Term` lives on one
/// thread (nothing here is `Send`), so the shared handle is safe by
/// construction and the borrows never overlap (ratatui borrows only
/// inside `write` calls, the drain happens strictly after `draw`
/// returns).
struct FrameBuf(Rc<RefCell<Vec<u8>>>);

impl Write for FrameBuf {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.borrow_mut().extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// The ratatui-backed terminal: draws grid frames and reports its size,
/// without exposing `ratatui` types to callers outside this crate.
pub struct Term {
    guard: TerminalGuard,
    /// The backend directly, not a `ratatui::Terminal`: this type keeps its
    /// own [`Shadow`] and emits the diff itself, so a `Terminal` would only
    /// add two more full-size cell buffers that nothing ever reads.
    inner: ratatui::backend::CrosstermBackend<FrameBuf>,
    /// The frame accumulator shared with `inner`'s writer; see [`FrameBuf`].
    frame_buf: Rc<RefCell<Vec<u8>>>,
    /// The last DECSCUSR shape written, so `draw_surface` only re-emits the
    /// escape when the `Surface` cursor's shape actually changed instead of
    /// writing it unconditionally on every frame.
    last_cursor_shape: Option<CursorShape>,
    /// The last terminal mouse-reporting state written, so `draw_surface`
    /// only toggles crossterm's mouse reporting when `model.engine.mouse_on`
    /// actually changed since the last frame instead of writing the escape
    /// on every paint. `None` before the first frame, matching
    /// `last_cursor_shape`'s convention.
    ///
    /// Named for reporting, not capture: this is whether the terminal sends
    /// mouse events at all, a different concept from
    /// `view_core::model::Model::mouse_capture`, which names the surface
    /// that owns the gesture in flight once an event has arrived.
    last_mouse_reporting: Option<bool>,
    /// The persistent double-buffered shadow of the terminal's cells. Each
    /// frame composites only its damaged rows into it, leaving every other
    /// cell as earlier frames painted it. This is what clips per-frame
    /// composite CPU to the damaged region instead of re-resolving all
    /// ~4800 cells every keystroke. Starts zero-sized so the first paint
    /// (and any later size change) rebuilds it and repaints in full.
    shadow: Shadow,
    /// The terminal-space rows every overlay layer covered last frame, so a
    /// vanished, moved, or shrunk overlay's vacated cells are marked dirty
    /// this frame and the grid (or the new overlay position) repaints under
    /// them; see [`crate::paint::Damage::from_frame`].
    last_overlay_rows: Vec<u16>,
    /// The reserved chrome-row offset last frame. A change (a tabline
    /// appearing or vanishing) shifts every grid row, so the next paint
    /// must be full rather than damage-clipped.
    last_offset: Option<u16>,
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
        let frame_buf = Rc::new(RefCell::new(Vec::new()));
        let inner = ratatui::backend::CrosstermBackend::new(FrameBuf(Rc::clone(&frame_buf)));
        Ok(Self {
            guard,
            inner,
            frame_buf,
            last_cursor_shape: None,
            last_mouse_reporting: None,
            shadow: Shadow::new(),
            last_overlay_rows: Vec::new(),
            last_offset: None,
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
    pub fn draw_surface(
        &mut self,
        model: &Model,
        surface: &Surface,
        grid_damage: &GridDamage,
    ) -> std::io::Result<()> {
        #[cfg(all(unix, feature = "bench-taps"))]
        crate::tap::tap(crate::tap::TAG_DRAW_START);
        #[cfg(all(unix, feature = "bench-taps"))]
        if surface.carries_speculation() {
            crate::tap::tap(crate::tap::TAG_SPECULATED_PAINT);
        }
        let mut sink = FrameBuf(Rc::clone(&self.frame_buf));
        if self.last_mouse_reporting != Some(model.engine.mouse_on) {
            if model.engine.mouse_on {
                crossterm::queue!(sink, EnableMouseCapture)?;
            } else {
                crossterm::queue!(sink, DisableMouseCapture)?;
            }
            self.last_mouse_reporting = Some(model.engine.mouse_on);
        }
        if model.caps.sync {
            sink.write_all(b"\x1b[?2026h")?;
        }
        // Translate this frame's grid damage into terminal-space rows, unioned
        // with the overlay rows of this frame and the last so a vanished or
        // moved overlay repaints the grid it uncovered. A chrome-offset change
        // (a tabline appearing), a first paint, or a resize forces a
        // whole-frame repaint instead.
        let offset = model.chrome_rows();
        let cur_overlay = overlay_rows(surface);
        #[cfg(all(unix, feature = "bench-taps"))]
        crate::tap::tap(crate::tap::TAG_FRAME_PREPARED);
        // the terminal size comes from the model (fed by Msg::Resized and
        // startup), not a per-frame TIOCGWINSZ query: the shadow's resize
        // still forces a full repaint on any change, so a resize is followed
        // on the next frame without the syscall.
        let area = frame_area(model);
        #[cfg(all(unix, feature = "bench-taps"))]
        crate::tap::tap(crate::tap::TAG_AREA_RESOLVED);
        let resized = self.shadow.resize(area);
        let force_full = resized || self.last_offset != Some(offset);
        if resized {
            // the terminal changed size: its on-screen contents are no longer
            // trustworthy, so clear it and repaint every cell from a blank
            // shadow -- the one place a full clear is warranted
            crossterm::queue!(
                sink,
                crossterm::terminal::Clear(crossterm::terminal::ClearType::All)
            )?;
        }
        let damage = Damage::from_frame(
            grid_damage,
            offset,
            &self.last_overlay_rows,
            &cur_overlay,
            force_full,
        );
        // the streamed turn repainting its own panel and nothing else: the
        // one repaint between a keystroke and its redraw that a bench row
        // holding an agent session live can attribute to the session rather
        // than to the editor answering the key. A frame that also carries
        // grid damage, or damage past the panel's rows, is left unexplained
        #[cfg(all(unix, feature = "bench-taps"))]
        let agent_repaint = !grid_damage.full
            && grid_damage.rows.is_empty()
            && damage.is_exactly(&crate::paint::agent_panel_rows(surface));
        #[cfg(all(unix, feature = "bench-taps"))]
        if agent_repaint {
            crate::tap::tap(crate::tap::TAG_AGENT_PAINT);
        }
        // paint only the damaged rows into the persistent shadow, then emit
        // the cells that actually changed against what the terminal already
        // shows; no full-buffer copy runs, because the shadow's buffers swap
        self.shadow.compose(model, surface, &damage);
        #[cfg(all(unix, feature = "bench-taps"))]
        crate::tap::tap(crate::tap::TAG_COMPOSED);
        // disjoint field borrows: `shadow` supplies the cells while `inner`'s
        // backend encodes them into the shared frame buffer
        self.shadow.emit_updates(&mut self.inner)?;
        self.shadow.commit();
        self.last_overlay_rows = cur_overlay;
        self.last_offset = Some(offset);
        match surface.cursor {
            Some(spec) => {
                self.inner.set_cursor_position((spec.col, spec.row))?;
                self.inner.show_cursor()?;
                if self.last_cursor_shape != Some(spec.shape) {
                    write_cursor_shape(&mut sink, spec.shape)?;
                    self.last_cursor_shape = Some(spec.shape);
                }
            }
            None => self.inner.hide_cursor()?,
        }
        if model.caps.sync {
            sink.write_all(b"\x1b[?2026l")?;
        }
        // the frame's single real write: everything queued above -- mouse
        // toggles, the sync bracket, the content diff, cursor escapes --
        // reaches the terminal in one syscall, atomically from the pty
        // reader's point of view. The tap here brackets exactly that write
        // and flush against TAG_TERM_WRITTEN, isolating the pty write cost.
        #[cfg(all(unix, feature = "bench-taps"))]
        crate::tap::tap(crate::tap::TAG_FLUSH_START);
        let mut out = std::io::stdout().lock();
        {
            let mut frame = self.frame_buf.borrow_mut();
            out.write_all(&frame)?;
            frame.clear();
        }
        out.flush()?;
        #[cfg(all(unix, feature = "bench-taps"))]
        crate::tap::tap(crate::tap::TAG_TERM_WRITTEN);
        Ok(())
    }

    /// Writes an OSC 52 clipboard-set escape directly to the real
    /// terminal, bypassing `frame_buf`. The clipboard worker thread must
    /// never write to stdout itself -- only `view-tui` touches the
    /// terminal, and a background thread racing `draw_surface`'s own
    /// buffered flush could interleave escape sequences into a corrupted
    /// stream -- so this is the method that lets the thread already
    /// driving `Term` speak on the worker's behalf (see
    /// `view_core::msg::Effect::Osc52Copy`'s doc).
    ///
    /// Mirrors the free [`restore`] function's direct-write pattern rather
    /// than `draw_surface`'s buffered one: an OSC 52 write is not part of
    /// any frame's damage and has no shadow state to reconcile against, so
    /// routing it through `frame_buf` would only delay it to the next
    /// paint for no benefit.
    ///
    /// # Errors
    ///
    /// Returns the underlying `std::io::Error` if the write or flush
    /// fails.
    pub fn write_osc52(&mut self, register: char, text: &str) -> std::io::Result<()> {
        let mut out = std::io::stdout().lock();
        write_osc52_bytes(&mut out, register, text)?;
        out.flush()
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

/// The terminal size as the input reader last observed it, readable by the
/// paint loop.
///
/// Not a second source of truth for the size -- [`Model`] stays that -- but
/// a second *transport* for it, the way `Msg::Resized` is. The message
/// still flows and still drives the engine's own `TryResize` in message
/// order; this only stops the frames painted between the resize happening
/// and its message being dequeued from addressing a shape the terminal has
/// already left. On a shrink those frames size the shadow larger than the
/// real terminal and emit cells for rows and columns it no longer has.
///
/// A packed `u16` pair in one atomic, with `0` meaning "nothing published":
/// a terminal is never 0x0, and one word keeps width and height inherently
/// consistent with each other, which two atomics would not be.
#[derive(Clone, Default, Debug)]
pub struct TermSizeCell(std::sync::Arc<std::sync::atomic::AtomicU32>);

impl TermSizeCell {
    /// Publishes the size the terminal has just become.
    pub fn publish(&self, width: u16, height: u16) {
        let packed = (u32::from(width) << 16) | u32::from(height);
        self.0.store(packed, std::sync::atomic::Ordering::Release);
    }

    /// Takes a published size if one is waiting, leaving the cell empty.
    ///
    /// The empty case is a single relaxed load, which is what a frame pays
    /// on every pass: the whole point of sourcing the paint area from the
    /// model was to stop paying a `TIOCGWINSZ` per frame, and this must not
    /// quietly put a cost back.
    #[must_use]
    pub fn take(&self) -> Option<(u16, u16)> {
        use std::sync::atomic::Ordering;
        if self.0.load(Ordering::Relaxed) == 0 {
            return None;
        }
        match self.0.swap(0, Ordering::Acquire) {
            0 => None,
            #[allow(clippy::cast_possible_truncation)]
            packed => Some(((packed >> 16) as u16, (packed & 0xffff) as u16)),
        }
    }
}

/// Spawns a dedicated thread that blocks on `crossterm::event::read()` and
/// forwards every key, resize, paste, or mouse event to `tx` as a core
/// [`Msg`] (see [`crate::input::event_to_msg`] for the translation both
/// platforms share). Exits once `crossterm::event::read()` errors or `tx`'s
/// receiver is gone.
///
/// The non-unix input path only: on unix the runtime loop polls the
/// terminal fd itself and decodes inline through [`crate::input`], which
/// deletes this thread's cross-thread wake from the keystroke path. Off
/// unix there is no portable readiness poll over both the console and a
/// wake pipe, so the blocking-thread shape stays: it is what lets the
/// loop's `recv()` wake immediately on a keystroke without a timeout-based
/// drain. A blocking `send` (not `try_send`) is deliberate: a dropped
/// keystroke is never an acceptable loss the way a coalescible redraw
/// token is, so this thread blocks rather than discards when the channel
/// is momentarily full.
#[cfg(not(unix))]
pub fn spawn_input_thread(tx: SyncSender<Msg>, size: TermSizeCell) {
    std::thread::spawn(move || {
        while let Ok(event) = crossterm::event::read() {
            if let Some(msg) = crate::input::event_to_msg(event, &size) {
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
    fn an_empty_size_cell_yields_nothing() {
        let cell = TermSizeCell::default();
        assert_eq!(cell.take(), None);
    }

    #[test]
    fn a_published_size_is_taken_exactly_once() {
        let cell = TermSizeCell::default();
        cell.publish(120, 40);
        assert_eq!(cell.take(), Some((120, 40)));
        assert_eq!(
            cell.take(),
            None,
            "a second take must not re-apply a resize that was already folded in"
        );
    }

    #[test]
    fn a_second_publish_supersedes_the_first() {
        // only the terminal's current shape can be right; an intermediate
        // size the terminal has already left must never reach a frame
        let cell = TermSizeCell::default();
        cell.publish(120, 40);
        cell.publish(80, 24);
        assert_eq!(cell.take(), Some((80, 24)));
    }

    #[test]
    fn the_size_cell_round_trips_the_extremes_of_a_u16_pair() {
        // the packing puts width in the high half and height in the low
        // half; a swap or a sign error shows up here and nowhere else
        let cell = TermSizeCell::default();
        for size in [(1, 1), (u16::MAX, 1), (1, u16::MAX), (u16::MAX, u16::MAX)] {
            cell.publish(size.0, size.1);
            assert_eq!(cell.take(), Some(size));
        }
    }

    #[test]
    fn frame_area_is_sourced_from_the_model_terminal_size_and_follows_a_resize() {
        // the paint area must be the model's last-known terminal size, so a
        // per-frame OS size query is never needed and a stale size can never
        // paint.
        let mut model = Model::with_term_size(80, 24);
        assert_eq!(
            frame_area(&model),
            ratatui::layout::Rect::new(0, 0, 80, 24),
            "the initial paint area must match the startup terminal size"
        );

        // a resize lands as Msg::Resized, which sets term_width/height (see
        // view_core::update). the very next frame must paint at the new size,
        // never the size the previous frame used.
        model.term_width = 120;
        model.term_height = 40;
        assert_eq!(
            frame_area(&model),
            ratatui::layout::Rect::new(0, 0, 120, 40),
            "after a resize the paint area must follow the model, not a cached or queried size"
        );
    }

    // restore_bytes drives crossterm's LeaveAlternateScreen/DisableMouseCapture
    // through execute!; on Windows those touch the WinAPI console layer and
    // fail ("Initial console modes not set") when no console was entered, so
    // this ANSI-byte-ordering assertion can only be exercised where crossterm
    // emits the raw sequences unconditionally. The production restore path is
    // exercised after a real EnterAlternateScreen; this isolated unit check is
    // a unix/VT-terminal concern.
    #[cfg(unix)]
    #[test]
    fn restore_bytes_closes_the_sync_bracket_before_leaving_the_alternate_screen() {
        let mut buf = Vec::new();
        restore_bytes(&mut buf).unwrap();

        let esu_close = find_subslice(&buf, b"\x1b[?2026l")
            .expect("restore must write the ESU close unconditionally");
        let leave_alt = find_subslice(&buf, b"\x1b[?1049l")
            .expect("restore must still leave the alternate screen");
        assert!(
            esu_close < leave_alt,
            "ESU close must be written before leaving the alternate screen, so the bracket \
             closes on the screen that opened it"
        );
    }

    // Serves only the unix-gated restore_bytes test above.
    #[cfg(unix)]
    fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
        haystack
            .windows(needle.len())
            .position(|window| window == needle)
    }

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

    #[test]
    fn write_base64_pads_to_the_next_multiple_of_four() {
        let mut buf = Vec::new();
        write_base64(&mut buf, b"hello").unwrap();
        assert_eq!(buf, b"aGVsbG8=", "5 bytes -> one pad char");

        buf.clear();
        write_base64(&mut buf, b"hi!").unwrap();
        assert_eq!(buf, b"aGkh", "3 bytes -> no padding needed");

        buf.clear();
        write_base64(&mut buf, b"").unwrap();
        assert_eq!(buf, b"", "empty input encodes to nothing");
    }

    #[test]
    fn write_osc52_bytes_selects_c_for_plus_and_p_for_star() {
        let mut buf = Vec::new();
        write_osc52_bytes(&mut buf, '+', "hi!").unwrap();
        assert_eq!(buf, b"\x1b]52;c;aGkh\x1b\\", "'+' maps to clipboard code c");

        buf.clear();
        write_osc52_bytes(&mut buf, '*', "hi!").unwrap();
        assert_eq!(
            buf, b"\x1b]52;p;aGkh\x1b\\",
            "'*' maps to primary-selection code p"
        );
    }
}
