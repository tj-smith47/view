//! Raw-mode / alternate-screen lifecycle for the interactive terminal, plus
//! the typed facade over `ratatui`/`crossterm` that keeps both crates out of
//! the `view` bin crate's dependency graph (`scripts/audit-deps.sh` denies
//! `view -> crossterm` and `view -> ratatui`: only `view-tui` may touch the
//! terminal).

use crate::paint::{Damage, Shadow};
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
/// [`enter`](Self::enter) performs the whole entry: raw mode, a panic hook
/// that restores the terminal before the default panic message prints, then
/// the alternate screen and bracketed paste. The keyboard protocol is the
/// one entry byte that cannot be written there
/// ([`push_keyboard_protocol`](Self::push_keyboard_protocol)), because
/// whether the terminal speaks it is what capability detection answers and
/// that detection now runs on the alternate screen. Everything restores
/// together, whether entry finished or not: [`Drop`] and
/// [`restore_now`](Self::restore_now) both call the same unconditional
/// [`restore`] that undoes every effect regardless of how far entry got.
///
/// This is the only place in the crate that enables raw mode or enters the
/// alternate screen, and the only one whose panic hook restores them
/// ([`StderrGuard`] chains one that restores fd 2 and nothing else):
/// [`Term::init`] holds one of
/// these as a field rather than repeating the setup (`ratatui::try_init`
/// does its own raw-mode/alt-screen/panic-hook dance, which would otherwise
/// chain a second, redundant hook and re-enter the alternate screen on top
/// of this one).
#[must_use = "dropping the guard restores the terminal immediately"]
pub struct TerminalGuard(bool);

impl TerminalGuard {
    /// Enables raw mode, installs a panic hook that restores the terminal
    /// before delegating to the previous hook, and switches to the
    /// alternate screen with bracketed-paste reporting on.
    ///
    /// The alternate screen goes up before any escape sequence this process
    /// writes to the terminal, capability detection's probe batch included.
    /// A plain-text notice can still precede it -- `main.rs` prints one to
    /// stderr for a session with no terminal stdin, where the main screen
    /// is the only sink left and is where the notice belongs.
    /// What a program leaves on the main screen ahead of `CSI ? 1049 h` is
    /// at the mercy of how the emulator saves and restores that screen
    /// around the switch, and one (Termius) put view's last alternate
    /// screen back over the shell's scrollback because of it -- the probe's
    /// queries being the only bytes nvim, which leaves no such residue,
    /// does not also write. Every question in that batch is answered the
    /// same on either screen, and the `╭` the cell-width query prints is
    /// erased by that query's own `CSI K` on a line nothing else has
    /// written.
    ///
    /// Raw mode still comes first within that: canonical mode's line
    /// buffering, echo, and missing newline terminator would corrupt or
    /// swallow the probe's CSI replies. The hook is installed between the
    /// two, so a panic during entry itself is covered.
    ///
    /// Bracketed paste is enabled unconditionally (unlike mouse capture,
    /// which [`Term::draw_surface`] toggles only while nvim reports
    /// `mouse_on`): a paste is never ambiguous with ordinary typed input
    /// the way raw mouse tracking would be with the host terminal's own
    /// selection/scrollback gestures, so there is no reason to gate it
    /// behind engine state.
    ///
    /// # Errors
    ///
    /// Returns the underlying `std::io::Error` if raw mode or the alternate
    /// screen cannot be entered.
    pub fn enter() -> std::io::Result<Self> {
        Self::enter_raw(true)
    }

    /// Raw mode and the panic hook alone, with the alternate screen left
    /// down: `--print-caps` needs raw mode for the capability probe's CSI
    /// replies but touches nothing an editing session paints, so entering
    /// the alternate screen only to leave it again blinks the host
    /// terminal for a flag that never draws a frame, and teardown then
    /// writes `EnterAlternateScreen`/`LeaveAlternateScreen` and the rest of
    /// [`restore_bytes`]'s frame-shaped bytes into whatever `--print-caps`'s
    /// stdout is redirected to, ahead of the capability table.
    ///
    /// # Errors
    ///
    /// Returns the underlying `std::io::Error` if raw mode cannot be
    /// entered.
    pub fn enter_bare() -> std::io::Result<Self> {
        Self::enter_raw(false)
    }

    fn enter_raw(alt_screen: bool) -> std::io::Result<Self> {
        crossterm::terminal::enable_raw_mode()?;
        // a panic must restore the terminal before the message prints, or the
        // user is left with a broken shell and an invisible error; installed
        // right after raw mode rather than after the alternate screen so a
        // panic during capability detection is covered too
        let prev = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            restore(alt_screen);
            prev(info);
        }));
        // the value first: from here the alternate screen (when entered) is
        // this guard's to undo, and a failed write leaves a dropped guard to
        // undo it
        let guard = Self(alt_screen);
        enter_bytes_if(&mut std::io::stdout(), alt_screen)?;
        Ok(guard)
    }

    /// Pushes the kitty keyboard protocol, once capability detection has
    /// said the terminal speaks it, inside the alternate screen
    /// [`enter`](Self::enter) already opened -- so the protocol's lifetime
    /// nests strictly inside the alternate screen's, which is the order
    /// nvim's own teardown uses: `terminfo_disable` pops the key encoding
    /// at `src/nvim/tui/tui.c:556`, and only the later `terminfo_stop`
    /// emits `exit_ca_mode` at `:599`.
    ///
    /// `kitty_kbd` is `Model.caps.kitty_kbd` -- the startup probe's answer,
    /// or what a `--tier` override asserted. When it holds, the push lands
    /// here and [`restore_bytes`] pops it, so `<S-CR>`, `<C-i>` and `<Esc>`
    /// reach [`crate::keys::encode_key`] as keys distinct from `<CR>`,
    /// `<Tab>` and an Alt prefix.
    ///
    /// # Errors
    ///
    /// Returns the underlying `std::io::Error` if the keyboard-protocol
    /// push cannot be written.
    pub fn push_keyboard_protocol(&self, kitty_kbd: bool) -> std::io::Result<()> {
        // cfg!(unix) rather than a cfg attribute so the parameter stays used
        // on every platform: the capability probe only runs on unix, and a
        // `--tier full` override elsewhere asserts a protocol nothing
        // negotiated
        let pushed = cfg!(unix) && kitty_kbd;
        push_kitty_keyboard(&mut std::io::stdout(), false, pushed)?;
        set_kitty_keyboard_pushed(pushed);
        Ok(())
    }

    /// Restores the terminal immediately rather than waiting for [`Drop`].
    ///
    /// For exit paths that bypass destructors (`std::process::exit`).
    /// Safe to call even if [`Drop`] still runs afterward: leaving the
    /// alternate screen and disabling raw mode a second time on an already
    /// restored terminal is a no-op, not an error.
    pub fn restore_now(&self) {
        restore(self.0);
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        restore(self.0);
    }
}

/// The kitty keyboard-protocol push view sends on entry when the terminal
/// speaks it: `CSI > 1 u`, `DISAMBIGUATE_ESCAPE_CODES` alone.
///
/// One bit narrower than nvim's own, and deliberately so. Neovim v0.12.4's
/// `tui_set_key_encoding` (`src/nvim/tui/tui.c:315`) sends `CSI > 3 u`,
/// adding `REPORT_EVENT_TYPES`; [`crate::keys::encode_key`] discards every
/// release event, so that bit would only make the terminal send, and the
/// input drain parse, one more escape per keystroke that is then thrown
/// away -- a per-key cost on the latency-gated dispatch path buying no
/// behaviour at all. Widen it the day a release event has a use here.
const KITTY_KBD_PUSH: &[u8] = b"\x1b[>1u";

/// The pop that undoes [`KITTY_KBD_PUSH`]: `CSI < u`, byte-identical to
/// nvim's `tui_reset_key_encoding` (`src/nvim/tui/tui.c:330`). Written by
/// hand rather than through `crossterm::event::PopKeyboardEnhancementFlags`
/// because that command emits `CSI < 1 u` and reports itself unsupported on
/// Windows, where its `execute!` arm returns an error that would abandon the
/// rest of [`restore_bytes`] -- including leaving the alternate screen.
const KITTY_KBD_POP: &[u8] = b"\x1b[<u";

/// Whether [`KITTY_KBD_PUSH`] is currently on the terminal's stack.
///
/// Held process-wide, like [`SAVED_STDERR`], because it describes the one
/// terminal this process owns rather than any value's state, and because
/// [`restore`] -- the pop -- is a free function the panic hook runs with no
/// guard in scope. Written only where the push and the pop bytes are, so
/// the flag cannot claim an encoding the terminal was never put into.
///
/// `Relaxed` on both sides: the input thread reads it while the main thread
/// writes it, and the flag is the entire message -- no other write is being
/// published behind it, so nothing is owed a happens-before.
static KITTY_KBD_PUSHED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Whether the teardown escapes have already been written this process.
///
/// Held process-wide for the same reason [`KITTY_KBD_PUSHED`] is, and read
/// by [`restore_bytes_once`] alone. The panic path is what needs it: the
/// hook runs [`restore`], the previous hook prints the message on the main
/// screen, and then unwinding drops the [`Term`] that owns the guard into
/// [`restore`] a second time -- which would clear the screen, park the
/// caret and switch buffers over the message a user is meant to read.
///
/// `Relaxed`, like its neighbour: the flag is the whole message, and
/// nothing is being published behind it.
static RESTORED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Whether the terminal is reporting keys in the kitty keyboard protocol,
/// which decides the name [`crate::keys::encode_terminal_key`] gives four
/// of the C0 bytes.
///
/// The non-unix reader's question alone. On unix the bytes are decoded
/// here rather than by crossterm ([`crate::keys::decode_residue`]), and
/// that decoder reads those four bytes as the chords nvim names them
/// without having to repair a table afterwards, so it never has to ask.
#[cfg(not(unix))]
pub(crate) fn kitty_keyboard_pushed() -> bool {
    KITTY_KBD_PUSHED.load(std::sync::atomic::Ordering::Relaxed)
}

fn set_kitty_keyboard_pushed(pushed: bool) {
    KITTY_KBD_PUSHED.store(pushed, std::sync::atomic::Ordering::Relaxed);
}

/// Writes every setup escape to `out`: the alternate screen and bracketed
/// paste, in that order and ahead of every other byte the process sends
/// the terminal. Generic over `Write` for the same reason [`restore_bytes`]
/// is -- the byte sequence and its ordering are provable against a
/// `Vec<u8>` rather than only against a live terminal.
///
/// The keyboard-protocol push is not here, because the answer that decides
/// it is not known yet: see
/// [`TerminalGuard::push_keyboard_protocol`].
fn enter_bytes<W: Write>(out: &mut W) -> std::io::Result<()> {
    crossterm::execute!(
        out,
        crossterm::terminal::EnterAlternateScreen,
        crossterm::event::EnableBracketedPaste
    )
}

/// [`enter_bytes`] when `alt_screen` is set, and nothing at all otherwise --
/// the alternate-screen-optional half of
/// [`TerminalGuard::enter_raw`](TerminalGuard::enter), factored out so a
/// bare entry's silence is provable against a `Vec<u8>` the same way the
/// entry bytes themselves are.
fn enter_bytes_if<W: Write>(out: &mut W, alt_screen: bool) -> std::io::Result<()> {
    if alt_screen {
        enter_bytes(out)?;
    }
    Ok(())
}

/// Writes the keyboard-protocol push when the terminal speaks it and has
/// not been pushed already, and nothing at all otherwise.
///
/// The one push site, for both moments a session can reach a decision at:
/// [`TerminalGuard::push_keyboard_protocol`] runs it on the probe's first
/// answer, with nothing pushed yet, and [`Term::adopt_caps`] runs it again
/// for a terminal that only admitted to speaking the protocol after that
/// window closed. Generic over `Write` for the same reason its two
/// siblings are: "exactly one push per session, however late the answer"
/// is a claim about bytes, and only a `Vec<u8>` can hold a whole session's
/// worth.
///
/// The pop in [`restore_bytes`] needs no matching condition: it is written
/// unconditionally on every exit path, and a pop against an empty stack is
/// a no-op, so a push that lands late is still popped exactly once and a
/// decision that never became a push pops nothing that exists.
fn push_kitty_keyboard<W: Write>(
    out: &mut W,
    already_pushed: bool,
    kitty_kbd: bool,
) -> std::io::Result<()> {
    if kitty_kbd && !already_pushed {
        out.write_all(KITTY_KBD_PUSH)?;
        out.flush()?;
    }
    Ok(())
}

/// Writes every teardown escape to `out`: the synchronized-update close
/// first, then the keyboard-protocol pop, the clear of the frame the
/// alternate screen is still showing, the caret parked at column 0 of that
/// screen's bottom row, the caret shown on that screen, and last mouse
/// capture, bracketed paste and the alternate screen -- then a second
/// `CSI ? 25 h` on the screen the host shell resumes on. Two caret shows,
/// one per screen, and nothing else after the switch back: every byte that
/// paints lands on the screen view drew on, which is what keeps its last
/// frame out of the host shell's scrollback. `rows` is the terminal's height in cells, which the
/// bottom-row park needs and which [`restore`] asks the terminal for.
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
///
/// The caret is shown again and its shape reset for the same reason and in
/// the same place: [`Term::draw_surface`] hides the cursor on any frame
/// whose surface carries none and writes a DECSCUSR shape for every frame
/// that does, so a session ending from such a frame would hand the host
/// shell an invisible or insert-mode caret at its own prompt -- which reads
/// to a user as a terminal that has stopped responding.
fn restore_bytes<W: Write>(out: &mut W, rows: u16) -> std::io::Result<()> {
    out.write_all(b"\x1b[?2026l")?;
    // popped unconditionally, for the same reason the ESU close above is
    // written unconditionally: `restore` is a free function reachable from
    // the panic hook, where no `TermCaps` is in scope to consult. A pop
    // against an empty stack is a no-op on terminals that speak the
    // protocol and an ignored unknown CSI on terminals that do not.
    out.write_all(KITTY_KBD_POP)?;
    // DECSCUSR 0 rather than any of the explicit shapes `write_cursor_shape`
    // emits. 0 is not a guaranteed restore -- xterm's own ctlseqs read it as
    // blinking block, the same as 1 -- but terminals that track a configured
    // default (VTE, kitty, foot, Windows Terminal) return to it on 0, and
    // nothing view can send restores a shape it never learned. Leaving the
    // session's last shape in place is the one certain wrong answer: a user
    // who quit from insert mode would otherwise keep a bar caret at their
    // shell prompt for the rest of that terminal's life.
    out.write_all(b"\x1b[0 q")?;
    // the last frame erased while it is still the visible screen: a
    // terminal is not obliged to restore what the alternate screen covered
    // when `CSI ? 1049 l` switches back, and the ones that do not leave
    // view's final frame -- a split's separator, a float -- printed over
    // the host shell's scrollback. nvim's own teardown writes the same
    // clear before `exit_ca_mode` for the same reason.
    out.write_all(b"\x1b[H\x1b[2J")?;
    // parked on the bottom row of the screen being left, which is what nvim
    // does (`\r CSI <rows-1> B` before `exit_ca_mode`): an emulator that
    // carries the alternate screen's cursor position back to the main screen
    // instead of restoring the saved one would otherwise resume the host
    // shell's prompt at row 1, printing it over the scrollback the user was
    // reading before view started.
    if let Some(down) = rows.checked_sub(1).filter(|down| *down > 0) {
        write!(out, "\r\x1b[{down}B")?;
    } else {
        out.write_all(b"\r")?;
    }
    // mouse capture disabled unconditionally, even though it is only ever
    // turned on dynamically (see Term::draw_surface): leaving it enabled
    // across process exit would swallow the host shell's own mouse
    // gestures until the terminal emulator itself is reset
    crossterm::execute!(
        out,
        crossterm::cursor::Show,
        DisableMouseCapture,
        crossterm::event::DisableBracketedPaste,
        crossterm::terminal::LeaveAlternateScreen
    )?;
    // shown a second time, on the other screen: a terminal that tracks
    // DECTCEM per buffer resumes the main screen with whatever visibility
    // it last had there, and the `Show` above applied to the alternate
    // screen only. nvim's recorded exit writes the same second show after
    // `exit_ca_mode` (`CSI ? 1049 l`, its title pop, `CSI ? 25 h`); it is
    // the only byte here that follows the switch back, and it paints
    // nothing.
    out.write_all(b"\x1b[?25h")?;
    out.flush()
}

/// [`restore_bytes`] the first time it is reached in this process, and
/// nothing at all afterwards -- see [`RESTORED`].
///
/// Only the escapes are guarded. Everything else [`restore`] does is
/// idempotent and costs a repeat nothing: raw mode is already off, the
/// push flag is already false, and fd 2 already points where it did.
fn restore_bytes_once<W: Write>(out: &mut W, rows: u16) -> std::io::Result<()> {
    if RESTORED.swap(true, std::sync::atomic::Ordering::Relaxed) {
        return Ok(());
    }
    restore_bytes(out, rows)
}

/// [`restore_bytes_once`] when `alt_screen` is set, and nothing at all
/// otherwise, so it never touches [`RESTORED`] for a guard that entered no
/// alternate screen to undo -- the exit-side mirror of [`enter_bytes_if`].
fn restore_bytes_once_if<W: Write>(
    out: &mut W,
    rows: u16,
    alt_screen: bool,
) -> std::io::Result<()> {
    if alt_screen {
        restore_bytes_once(out, rows)?;
    }
    Ok(())
}

fn restore(alt_screen: bool) {
    let mut out = std::io::stdout();
    // asked here rather than carried on the guard: `restore` is a free
    // function the panic hook runs with no value in scope, and a terminal
    // resized since entry parks at the height it has now. `window_size`
    // rather than `size`: the latter falls back to forking `tput cols` and
    // `tput lines` once the tty is gone, which is two blocking spawns on
    // the exit path and inside the panic hook. A terminal that reports no
    // size parks at column 0 of the row it is already on, which is what
    // this path did before the park existed.
    let rows = crossterm::terminal::window_size().map_or(1, |size| size.rows);
    // a guard that never entered the alternate screen (`--print-caps`, via
    // TerminalGuard::enter_bare) has nothing `restore_bytes`' frame-shaped
    // teardown undoes, and writing it anyway would land those bytes in
    // whatever the flag's stdout is redirected to, ahead of the table.
    let _ = restore_bytes_once_if(&mut out, rows, alt_screen);
    set_kitty_keyboard_pushed(false);
    let _ = crossterm::terminal::disable_raw_mode();
    let _ = out.flush();
    // last, and inside `restore` rather than beside its callers: the panic
    // hook runs this before the previous hook prints, so a panic message has
    // to find the terminal on fd 2 again and not the session's log
    #[cfg(unix)]
    restore_stderr();
}

/// Where fd 2 pointed before [`StderrGuard::redirect`] took it, held
/// process-wide because [`restore`] is a free function the panic hook runs
/// with no guard value in scope.
#[cfg(unix)]
static SAVED_STDERR: std::sync::OnceLock<std::os::fd::OwnedFd> = std::sync::OnceLock::new();

/// Keeps fd 2 off the terminal for as long as the value lives, restoring it
/// on [`Drop`].
///
/// Anything in the process may write to stderr -- a system library's own
/// diagnostic as much as view's code -- and while the TUI owns the terminal
/// those bytes land at the emulator's cursor, over cells the differential
/// painter believes it still owns and therefore never repaints. macOS is
/// where this is observed: an AppKit pasteboard write that the OS refuses
/// logs one such line, and the frame under it does not come back.
///
/// Restoring is idempotent and reachable three ways on purpose: [`restore`]
/// puts fd 2 back before the panic hook prints, so a panic message reaches
/// the terminal the same hook just restored; this value's [`Drop`] covers
/// every ordinary return out of the session; and [`redirect`](Self::redirect)
/// chains a hook of its own, so a panic between the redirect and
/// [`TerminalGuard::enter`] -- which is where the hook that calls
/// [`restore`] is installed -- still prints where a user can read it.
///
/// Unix only. The mechanism is `dup2` on fd 2, which rustix exposes under
/// `cfg(not(windows))`; the Windows equivalent is `SetStdHandle` on a
/// console handle, which no dependency of this crate offers and which the
/// observed member of the class -- an Apple framework -- cannot reach.
/// Same grounds as `view-engine`'s `KILLED_AT_SPAWN_WINDOW`: a unix-gated
/// mitigation for a hazard whose only known trigger is unix.
#[cfg(unix)]
#[must_use = "dropping the guard puts fd 2 back on the terminal immediately"]
pub struct StderrGuard(());

#[cfg(unix)]
impl StderrGuard {
    /// Duplicates the current fd 2 aside and puts `sink` in its place.
    ///
    /// Callers must run every `is_terminal` probe of stderr first: after
    /// this, fd 2 answers for the sink and no longer for whatever the shell
    /// opened. A second call keeps the first call's saved descriptor, so
    /// the terminal is what a restore hands back rather than an earlier
    /// redirect's sink.
    ///
    /// # Errors
    ///
    /// Returns the underlying `std::io::Error` if fd 2 cannot be duplicated
    /// aside, or if `sink` cannot be put in its place.
    pub fn redirect(sink: &std::fs::File) -> std::io::Result<Self> {
        // close-on-exec: a child spawned mid-session must not inherit a
        // second descriptor onto the user's terminal
        let saved = rustix::io::fcntl_dupfd_cloexec(std::io::stderr(), 0)?;
        if SAVED_STDERR.set(saved).is_ok() {
            // `restore` already covers a panic once `TerminalGuard` has
            // chained its own hook; this covers the window before that,
            // where the message would otherwise print into the sink
            let prev = std::panic::take_hook();
            std::panic::set_hook(Box::new(move |info| {
                restore_stderr();
                prev(info);
            }));
        }
        // on a dup2 failure the saved fd and hook above stay armed: both are
        // benign no-ops with fd 2 untouched, and installing them first keeps
        // every instant after fd 2 flips covered by a restoring hook
        rustix::stdio::dup2_stderr(sink)?;
        Ok(Self(()))
    }
}

#[cfg(unix)]
impl Drop for StderrGuard {
    fn drop(&mut self) {
        restore_stderr();
    }
}

/// Points fd 2 back at whatever [`StderrGuard::redirect`] found there, or
/// does nothing if no redirect ever happened.
#[cfg(unix)]
fn restore_stderr() {
    if let Some(saved) = SAVED_STDERR.get() {
        let _ = rustix::stdio::dup2_stderr(saved);
    }
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

/// Writes an OSC 52 clipboard-set escape (`ESC ] 5 2 ; {selection} ;
/// {base64} ESC \`, `:help clipboard-osc52`) for `text` to `writer`. The
/// escape itself is formed by
/// [`view_core::osc52::clipboard_escape`](view_core::osc52::clipboard_escape),
/// which the clipboard worker's paste answer shares, so a copy leaving
/// through the terminal and a paste answered back through nvim speak one
/// wire form. Generic over `Write` for the same testability reason as
/// `write_cursor_shape`.
fn write_osc52_bytes<W: Write>(writer: &mut W, register: char, text: &str) -> std::io::Result<()> {
    writer.write_all(view_core::osc52::clipboard_escape(register, text).as_bytes())
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
    /// on every paint. Starts at `Some(false)`: `enter_bytes` turns on
    /// bracketed paste alone, so reporting is known to be off when the
    /// terminal is entered, and a first frame that states it anyway writes a
    /// disable. On Windows crossterm serves both mouse commands through the
    /// console API instead of escape bytes, and the disable reads a console
    /// mode that only the enable stores, so a disable ahead of the first
    /// enable fails the frame.
    ///
    /// Named for reporting, not capture: this is whether the terminal sends
    /// mouse events at all, a different concept from
    /// `view_core::model::Model::mouse_capture`, which names the surface
    /// that owns the gesture in flight once an event has arrived.
    last_mouse_reporting: Option<bool>,
    /// Where the last frame left the real terminal's cursor, so a frame that
    /// repainted no cell and wants it in the same place writes no CUP.
    /// `None` before the first frame, matching `last_cursor_shape`'s
    /// convention.
    last_cursor: Option<(u16, u16)>,
    /// Whether the terminal's cursor is currently shown, so the show and the
    /// hide are each written once per change rather than once per frame.
    /// `None` before the first frame: the terminal's own state is unknown
    /// then, so the first frame states it either way.
    cursor_shown: Option<bool>,
    /// The persistent double-buffered shadow of the terminal's cells. Each
    /// frame composites only its damaged rows into it, leaving every other
    /// cell as earlier frames painted it. This is what clips per-frame
    /// composite CPU to the damaged region instead of re-resolving all
    /// ~4800 cells every keystroke. Starts zero-sized so the first paint
    /// (and any later size change) rebuilds it and repaints in full.
    shadow: Shadow,
    /// The reserved chrome-row offset last frame. A change (a tabline
    /// appearing or vanishing) shifts every grid row, so the next paint
    /// must be full rather than damage-clipped.
    last_offset: Option<u16>,
    /// The capabilities resolved during [`Term::init`], either probed or
    /// from a `--tier` override. Stored so [`Term::caps`] can hand a copy
    /// to the caller without re-running the (stdin-consuming, one-shot)
    /// detection probe.
    caps: TermCaps,
    /// How [`Term::caps`] was reached, for the caller's own diagnostics.
    /// Fixed at [`Term::init`]: a later upgrade revises the capabilities,
    /// never the fact that they were probed rather than assumed or
    /// overridden.
    caps_source: tiers::CapsSource,
    /// Whether the frame just queued repaints the agent panel's rows and
    /// nothing else. Carried from where the answer is known -- the frame's
    /// own damage -- to where the announcement belongs, beside the write
    /// that frame turns out to perform.
    #[cfg(all(unix, feature = "bench-taps"))]
    agent_repaint: bool,
    /// The capability probe still in flight, if the terminal was given a
    /// batch to answer at all. [`Term::settle_probe`] takes whatever it has
    /// heard by then; a `--tier` override leaves it `None`.
    probe: Option<tiers::Probe<'static>>,
}

impl Term {
    /// Initializes the backend terminal: raw mode and the alternate screen
    /// first ([`TerminalGuard::enter`]), then capability detection on that
    /// screen (or `tier_override` if given), then the keyboard-protocol
    /// push its answer decides; the ratatui terminal is constructed last,
    /// directly over the now-prepared stdout.
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
        Self::init_with_guard(TerminalGuard::enter()?, tier_override, true)
    }

    /// [`Term::init`] without the alternate screen: raw mode and capability
    /// detection alone, with no keyboard-protocol push, for a caller that
    /// never draws a frame or reads a key -- `--print-caps` is the one.
    /// See [`TerminalGuard::enter_bare`] for why the alternate screen is
    /// skipped rather than entered and immediately left.
    ///
    /// # Errors
    ///
    /// Returns the underlying `std::io::Error` if raw mode cannot be
    /// entered or capability detection's I/O fails.
    pub fn init_bare(tier_override: Option<Tier>) -> std::io::Result<Self> {
        Self::init_with_guard(TerminalGuard::enter_bare()?, tier_override, false)
    }

    fn init_with_guard(
        guard: TerminalGuard,
        tier_override: Option<Tier>,
        push_keyboard_protocol: bool,
    ) -> std::io::Result<Self> {
        let (caps, probe, caps_source) = tiers::resolve(tier_override)?;
        if push_keyboard_protocol {
            guard.push_keyboard_protocol(caps.kitty_kbd)?;
        }
        let frame_buf = Rc::new(RefCell::new(Vec::new()));
        let inner = ratatui::backend::CrosstermBackend::new(FrameBuf(Rc::clone(&frame_buf)));
        Ok(Self {
            guard,
            inner,
            frame_buf,
            last_cursor_shape: None,
            last_mouse_reporting: Some(false),
            last_cursor: None,
            cursor_shown: None,
            shadow: Shadow::new(),
            last_offset: None,
            caps,
            caps_source,
            #[cfg(all(unix, feature = "bench-taps"))]
            agent_repaint: false,
            probe,
        })
    }

    /// The capabilities resolved at [`Term::init`], for the caller to wire
    /// into `Model.caps`.
    #[must_use]
    pub fn caps(&self) -> TermCaps {
        self.caps
    }

    /// Where [`Term::caps`] came from, for the caller's own diagnostic line:
    /// this crate owns the terminal, never a channel to report about it on
    /// (see [`tiers::resolve`]).
    #[must_use]
    pub fn caps_source(&self) -> tiers::CapsSource {
        self.caps_source
    }

    /// Takes the capability probe [`Term::init`] left in flight off the
    /// terminal, returning what it has resolved so far, its residue, and
    /// whether the terminal ever answered the DA1 fence.
    ///
    /// **Never waits.** The tier decides how a frame is painted, not
    /// whether it can be: a terminal that has not answered yet is painted
    /// for at the conservative capabilities resolved so far, and the
    /// caller opens
    /// [`InputSource::open_after_probe`](crate::input::InputSource::open_after_probe)
    /// on a false `fence_seen` so a reply still in flight is recognized on
    /// the input path and delivered as
    /// [`Msg::CapsUpgraded`](view_core::msg::Msg::CapsUpgraded) instead.
    /// Waiting here is what put an ssh session's first frame behind
    /// [`PROBE_HARD_CAP`](crate::tiers::PROBE_HARD_CAP) -- 400ms of empty
    /// editor for a decision no frame needs to be blocked on.
    ///
    /// Callable exactly once per [`Term::init`] with a meaningful result --
    /// a second call reports the settled capabilities, no residue and no
    /// pending fence, which is correct rather than surprising, since there
    /// is nothing left to take.
    ///
    /// The caller (`main.rs`, right after `ui_attach`) is expected to
    /// translate the residue into nvim input notation (see
    /// [`encode_residue_bytes`](crate::keys::encode_residue_bytes)) and
    /// forward it before the runtime loop starts, so a keystroke queued
    /// ahead of or during the startup probe is never silently dropped.
    ///
    /// # Errors
    ///
    /// Returns the underlying `std::io::Error` if a terminal that only
    /// admitted to the kitty keyboard protocol inside the probe's first
    /// window cannot be sent the protocol's push.
    pub fn settle_probe(&mut self) -> std::io::Result<tiers::ProbeOutcome> {
        let Some(probe) = self.probe.take() else {
            return Ok(tiers::ProbeOutcome {
                caps: self.caps,
                residue: Vec::new(),
                fence_seen: true,
                cpr_seen: true,
                partial_reply: Vec::new(),
            });
        };
        // zero, not a budget: every byte the terminal has already sent is
        // in the probe's own buffer, and every byte it has not is the
        // guarded input path's to recognize
        let outcome = probe.finish(std::time::Duration::ZERO);
        self.adopt_caps(outcome.caps)?;
        Ok(outcome)
    }

    /// Takes `caps` as this terminal's own: the keyboard-protocol push the
    /// alternate screen may have gone up without, and the full repaint a
    /// capability change owes the frame already on screen.
    ///
    /// Both callers are upgrades of the same decision -- the probe's settle
    /// and, for an answer that arrived after it,
    /// [`draw_surface`](Self::draw_surface) following `Model::caps`.
    fn adopt_caps(&mut self, caps: TermCaps) -> std::io::Result<()> {
        // a decision that arrives after the alternate screen is already up
        // still owes the terminal the push `push_keyboard_protocol`
        // skipped, or `keys::encode_key` spends the session unable to tell
        // `<S-CR>` from `<CR>` on a terminal that can
        let pushed = cfg!(unix) && caps.kitty_kbd;
        push_kitty_keyboard(&mut std::io::stdout(), self.caps.kitty_kbd, pushed)?;
        // set true and never false, mirroring the push this guards: a
        // capability only ever upgrades here, so a false would claim a pop
        // nothing wrote
        if pushed {
            set_kitty_keyboard_pushed(true);
        }
        if caps != self.caps {
            // the frame on screen was painted under the old capabilities, in
            // the old border charset and palette; leaving the next frame free
            // to clip its damage to the rows that changed would leave the
            // rest of that frame drawn for a terminal this session no longer
            // believes it is talking to. Every field, not the tier alone:
            // `unicode_boxes` decides the charset without moving the tier.
            self.last_offset = None;
        }
        self.caps = caps;
        Ok(())
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
    /// Every escape here is written only when it says something the
    /// terminal does not already show. The cursor's position is re-stated
    /// when a cell was repainted (the emission left the terminal's own
    /// cursor somewhere else) or when the position itself moved; its show
    /// and its hide are each written on the change alone; its DECSCUSR
    /// shape likewise; and terminal mouse capture
    /// (`EnableMouseCapture`/`DisableMouseCapture`) tracks
    /// `model.engine.mouse_on` the same way. Capture is off by default and
    /// only turns on once nvim's own `redraw` stream reports `mouse_on`, so
    /// a buffer with `'mouse'` unset never steals the host terminal's
    /// selection/scrollback gestures.
    ///
    /// A frame left with nothing to say writes nothing at all -- no
    /// synchronization bracket, no syscall, no flush. nvim flushes a redraw
    /// batch for input it did not act on (a wheel report at a buffer's end),
    /// and each such batch was a packet the far terminal of an ssh session
    /// had to parse and paint for a screen that did not change.
    ///
    /// Returns whether this frame's bytes reached the terminal, which is
    /// the reading a caller dating a keystroke by the screen needs: a pass
    /// that rendered and wrote nothing put nothing in front of the user, so
    /// it answered no key.
    ///
    /// # Errors
    ///
    /// Returns the underlying `std::io::Error` if the backend write fails.
    pub fn draw_surface(
        &mut self,
        model: &Model,
        surface: &Surface,
        grid_damage: &GridDamage,
    ) -> std::io::Result<bool> {
        self.queue_frame(model, surface, grid_damage)?;
        // the frame's single real write: everything queued above -- mouse
        // toggles, the sync bracket, the content diff, cursor escapes --
        // reaches the terminal in one syscall, atomically from the pty
        // reader's point of view. Every tap sits inside the emptiness
        // check: a frame with nothing to write performs no write, and a
        // TAG_TERM_WRITTEN stamped for it would be read as this frame's
        // bytes having reached the terminal. The two announcements are
        // here rather than at the head of the frame for the same reason --
        // one made before the frame is known to be empty pairs with the
        // next frame's write -- and still ahead of TAG_FLUSH_START, so
        // neither lands inside the bracket that isolates the pty write.
        let mut frame = self.frame_buf.borrow_mut();
        let wrote = !frame.is_empty();
        if wrote {
            #[cfg(all(unix, feature = "bench-taps"))]
            if surface.carries_speculation() {
                crate::tap::tap(crate::tap::TAG_SPECULATED_PAINT);
            }
            #[cfg(all(unix, feature = "bench-taps"))]
            if self.agent_repaint {
                crate::tap::tap(crate::tap::TAG_AGENT_PAINT);
            }
            #[cfg(all(unix, feature = "bench-taps"))]
            crate::tap::tap(crate::tap::TAG_FLUSH_START);
            let mut out = std::io::stdout().lock();
            out.write_all(&frame)?;
            frame.clear();
            out.flush()?;
            #[cfg(all(unix, feature = "bench-taps"))]
            crate::tap::tap(crate::tap::TAG_TERM_WRITTEN);
        }
        Ok(wrote)
    }

    /// Queues one frame's whole byte stream into `frame_buf`, leaving the
    /// write to [`draw_surface`](Self::draw_surface).
    ///
    /// Split from it so the bytes a frame produces are provable against the
    /// buffer rather than only against a live terminal, the same reason
    /// [`restore_bytes`] and [`enter_bytes`] are generic over `Write`.
    fn queue_frame(
        &mut self,
        model: &Model,
        surface: &Surface,
        grid_damage: &GridDamage,
    ) -> std::io::Result<()> {
        #[cfg(all(unix, feature = "bench-taps"))]
        crate::tap::tap(crate::tap::TAG_DRAW_START);
        // capabilities the terminal only admitted to after the probe handed
        // it over reach this type the same way `mouse_on` does -- off the
        // model, on the frame that first carries them -- so the upgrade
        // needs no second paint path and no seam for a caller to forget.
        // One struct compare per frame in the steady state, where the two
        // are equal from the settle onward
        if self.caps != model.caps {
            self.adopt_caps(model.caps)?;
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
        // the bracket is written ahead of the content it wraps and taken
        // back below when the content turned out to be empty: the alternative
        // -- inserting the opener afterwards -- would memmove the whole frame
        // on every paint that does have something to say
        let bracket_at = self.frame_buf.borrow().len();
        if model.caps.sync {
            sink.write_all(b"\x1b[?2026h")?;
        }
        let content_at = self.frame_buf.borrow().len();
        // Translate this frame's grid damage into terminal-space rows,
        // unioned with the rows this frame's overlay stack draws
        // differently than the one on screen -- which covers the grid a
        // vanished or moved overlay uncovered. A chrome-offset change (a
        // tabline appearing), a first paint, or a resize forces a
        // whole-frame repaint instead.
        //
        // The ring is part of the offset: under tiles the outer grid is
        // placed one cell in, so a grid row lands one terminal row lower
        // than the chrome alone puts it, and a damage set built without it
        // repaints the row above the one that changed.
        let offset = view_surface::grid_origin(model).0;
        let overlay_damage = self.shadow.overlay_damage(surface);
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
        let mut overlay_damage = overlay_damage;
        overlay_damage.extend(self.shadow.native_pane_damage(model, surface, area));
        let force_full = resized || self.last_offset != Some(offset);
        if resized {
            // the terminal changed size: its on-screen contents are no longer
            // trustworthy, so clear it and repaint every cell from a blank
            // shadow -- the one place a full clear is warranted
            crossterm::queue!(
                sink,
                crossterm::terminal::Clear(crossterm::terminal::ClearType::All)
            )?;
            // the terminal moves its own cursor on a resize -- clamped on a
            // shrink, reflowed on a width change -- and the erase does not
            // put it back, so no recorded position survives one
            self.last_cursor = None;
        }
        let damage = Damage::from_frame(grid_damage, offset, &overlay_damage, force_full);
        // the streamed turn repainting its own panel and nothing else: the
        // one repaint between a keystroke and its redraw that a bench row
        // holding an agent session live can attribute to the session rather
        // than to the editor answering the key. A frame that also carries
        // grid damage, or damage past the panel's rows, is left unexplained
        #[cfg(all(unix, feature = "bench-taps"))]
        {
            self.agent_repaint = !grid_damage.full
                && grid_damage.rows.is_empty()
                && damage.covers_only(&crate::paint::agent_panel_rows(surface));
        }
        // paint only the damaged rows into the persistent shadow, then emit
        // the cells that actually changed against what the terminal already
        // shows; no full-buffer copy runs, because the shadow's buffers swap
        self.shadow.compose(model, surface, &damage);
        #[cfg(all(unix, feature = "bench-taps"))]
        crate::tap::tap(crate::tap::TAG_COMPOSED);
        // the frame's escapes join everything else already queued into the
        // shared frame buffer, so the whole frame still leaves in one write
        let painted_cells = self.shadow.emit_updates(&mut sink)?;
        self.shadow.commit();
        self.last_offset = Some(offset);
        match surface.cursor {
            Some(spec) => {
                let at = (spec.col, spec.row);
                // a repainted cell left the terminal's own cursor wherever
                // the last printed glyph put it, so the position is owed
                // again even when the model's cursor never moved
                if painted_cells || self.last_cursor != Some(at) {
                    self.inner.set_cursor_position(at)?;
                    self.last_cursor = Some(at);
                }
                if self.cursor_shown != Some(true) {
                    self.inner.show_cursor()?;
                    self.cursor_shown = Some(true);
                }
                if self.last_cursor_shape != Some(spec.shape) {
                    write_cursor_shape(&mut sink, spec.shape)?;
                    self.last_cursor_shape = Some(spec.shape);
                }
            }
            None => {
                // the emission left the terminal's cursor after the last
                // glyph it printed and this frame re-addresses nothing, so
                // the recorded position is now a claim about a cell the
                // caret has left
                if painted_cells {
                    self.last_cursor = None;
                }
                if self.cursor_shown != Some(false) {
                    self.inner.hide_cursor()?;
                    self.cursor_shown = Some(false);
                }
            }
        }
        let mut frame = self.frame_buf.borrow_mut();
        if frame.len() > content_at {
            if model.caps.sync {
                frame.extend_from_slice(b"\x1b[?2026l");
            }
        } else {
            frame.truncate(bracket_at);
        }
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

    /// Writes already-formed bytes directly to the real terminal, the same
    /// frame-buffer-bypassing route [`write_osc52`](Self::write_osc52)
    /// takes and for the same reason -- see that method's doc.
    ///
    /// Nothing here inspects or escapes what it is handed: the caller owns
    /// deciding that the bytes are a self-contained sequence safe to
    /// interleave between frames (`view_core::msg::Effect::TermWrite`
    /// states that policy).
    ///
    /// # Errors
    ///
    /// Returns the underlying `std::io::Error` if the write or flush
    /// fails.
    pub fn write_bytes(&mut self, bytes: &[u8]) -> std::io::Result<()> {
        let mut out = std::io::stdout().lock();
        out.write_all(bytes)?;
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

    /// A terminal whose frames stop at [`FrameBuf`], for the byte-level pins
    /// on [`queue_frame`](Self::queue_frame).
    ///
    /// `ManuallyDrop` because the contained [`TerminalGuard`] restores
    /// unconditionally, and this value entered no terminal to restore: a
    /// drop here would write a teardown sequence to whatever the test
    /// harness's stdout is and disable raw mode process-wide.
    #[cfg(test)]
    fn frame_probe(caps: TermCaps) -> std::mem::ManuallyDrop<Self> {
        let frame_buf = Rc::new(RefCell::new(Vec::new()));
        let inner = ratatui::backend::CrosstermBackend::new(FrameBuf(Rc::clone(&frame_buf)));
        std::mem::ManuallyDrop::new(Self {
            guard: TerminalGuard(true),
            inner,
            frame_buf,
            last_cursor_shape: None,
            last_mouse_reporting: Some(false),
            last_cursor: None,
            cursor_shown: None,
            shadow: Shadow::new(),
            last_offset: None,
            caps,
            caps_source: tiers::CapsSource::Assumed,
            #[cfg(all(unix, feature = "bench-taps"))]
            agent_repaint: false,
            probe: None,
        })
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

/// The terminal's current size in `(width, height)` cells, read before any
/// [`Term`] exists.
///
/// The same question [`Term::size`] answers, asked at the one point in
/// startup where there is no terminal object yet to ask: `nvim --embed`
/// sources nothing until a UI attaches, and the size is all the attach is
/// missing, so asking here is what lets the child run the user's config
/// underneath the capability probe instead of behind it.
///
/// # Errors
///
/// Returns the underlying `std::io::Error` if the terminal cannot report
/// its size.
pub fn size_now() -> std::io::Result<(u16, u16)> {
    crossterm::terminal::size()
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
        restore_bytes(&mut buf, 24).unwrap();

        let esu_close = find_subslice(&buf, b"\x1b[?2026l")
            .expect("restore must write the ESU close unconditionally");
        let leave_alt = find_subslice(&buf, b"\x1b[?1049l")
            .expect("restore must still leave the alternate screen");
        assert!(
            esu_close < leave_alt,
            "ESU close must be written before leaving the alternate screen, so the bracket \
             closes on the screen that opened it"
        );

        let show = find_subslice(&buf, b"\x1b[?25h").expect(
            "restore must show the cursor again: draw_surface hides it on any frame whose \
             surface carries none, and a hidden caret at the host shell's prompt reads as a \
             frozen terminal",
        );
        let shape_reset = find_subslice(&buf, b"\x1b[0 q").expect(
            "restore must reset DECSCUSR: a session ending in insert mode otherwise leaves a \
             bar caret at the host shell's prompt",
        );
        assert!(
            show < leave_alt && shape_reset < leave_alt,
            "the caret must be restored on the screen view drew on, before switching back"
        );

        let clear = find_subslice(&buf, b"\x1b[H\x1b[2J").expect(
            "restore must clear the alternate screen it is about to leave: a terminal that \
             does not restore the covered screen on CSI ? 1049 l otherwise keeps view's last \
             frame over the host shell",
        );
        assert!(
            clear < leave_alt,
            "the clear must land while the alternate screen is still the visible one"
        );
        let park = find_subslice(&buf, b"\r\x1b[23B").expect(
            "restore must park the caret on the bottom row of the screen it is leaving, the \
             way nvim does: an emulator that carries the alternate screen's cursor back to \
             the main screen otherwise resumes the shell prompt mid-screen",
        );
        assert!(
            clear < park && park < leave_alt,
            "the park is written on the alternate screen, after the clear and before the \
             switch back"
        );
        assert_eq!(
            &buf[leave_alt..],
            b"\x1b[?1049l\x1b[?25h",
            "the switch back is followed by the caret show and nothing else, matching nvim's \
             own exit: a terminal tracking DECTCEM per buffer needs it, and every byte that \
             paints has already landed on the screen view drew on"
        );
    }

    /// A [`tiers::ReplySource`] a terminal that answers nothing looks like,
    /// so the probe's query batch is written and its window closes at once.
    #[cfg(unix)]
    struct SilentTerminal;

    #[cfg(unix)]
    impl tiers::ReplySource for SilentTerminal {
        fn next_chunk(&mut self, _budget: std::time::Duration) -> Option<Vec<u8>> {
            None
        }
    }

    /// Every byte one session writes the terminal, in the order
    /// [`Term::init`] and [`restore`] write them: entry, the capability
    /// probe's query batch, the keyboard-protocol push its answer decides,
    /// and teardown. Composed from the same functions the real path calls
    /// -- a `Vec<u8>` stands in for the one stdout they share -- because
    /// the claim is about the whole stream and no single one of them can
    /// see it.
    #[cfg(unix)]
    fn session_bytes(kitty_kbd: bool) -> Vec<u8> {
        let mut wire = Vec::new();
        enter_bytes(&mut wire).unwrap();
        let probe = tiers::Probe::start(
            SilentTerminal,
            &mut wire,
            std::time::Duration::ZERO,
            &tiers::EnvHints::default(),
        )
        .unwrap();
        drop(probe);
        push_kitty_keyboard(&mut wire, false, kitty_kbd).unwrap();
        restore_bytes(&mut wire, 24).unwrap();
        wire
    }

    // Same unix gate and same reason as the restore_bytes test above:
    // enter_bytes drives EnterAlternateScreen through execute!, which on
    // Windows reaches the WinAPI console layer instead of emitting bytes.
    #[cfg(unix)]
    #[test]
    fn no_escape_a_session_writes_lands_on_the_main_screen() {
        for kitty_kbd in [false, true] {
            let wire = session_bytes(kitty_kbd);
            assert!(
                wire.starts_with(b"\x1b[?1049h"),
                "the switch to the alternate screen is the first thing written: a terminal \
                 that restores the main screen around CSI ? 1049 h/l is free to put anything \
                 written ahead of the switch back over the shell's scrollback, and the \
                 capability probe's queries were the only such bytes -- {:?}",
                String::from_utf8_lossy(&wire[..wire.len().min(32)])
            );
            let leave_alt = find_subslice(&wire, b"\x1b[?1049l")
                .expect("a session still leaves the alternate screen");
            assert_eq!(
                &wire[leave_alt..],
                b"\x1b[?1049l\x1b[?25h",
                "the only byte after the switch back is the caret show, which paints nothing: \
                 anything else would reach the screen the host shell resumes on -- {:?}",
                String::from_utf8_lossy(&wire[leave_alt..])
            );
        }
    }

    /// [`TerminalGuard::enter_bare`]'s side of the wire: no `EnterAlternateScreen`,
    /// no bracketed paste, and teardown writes nothing either -- the two
    /// gates [`Term::init_bare`] rides so `--print-caps`'s stdout carries
    /// only the capability table, never the frame-shaped bytes a session
    /// that draws would owe the screen it painted on.
    #[test]
    fn a_bare_entry_writes_no_escape_at_all() {
        let mut entry = Vec::new();
        enter_bytes_if(&mut entry, false).unwrap();
        assert!(
            entry.is_empty(),
            "a bare entry must never switch to the alternate screen: {:?}",
            String::from_utf8_lossy(&entry)
        );

        let mut teardown = Vec::new();
        restore_bytes_once_if(&mut teardown, 24, false).unwrap();
        assert!(
            teardown.is_empty(),
            "a bare entry has no alternate screen to leave, so its teardown must write nothing: {:?}",
            String::from_utf8_lossy(&teardown)
        );

        let mut real_entry = Vec::new();
        enter_bytes_if(&mut real_entry, true).unwrap();
        assert!(
            !real_entry.is_empty(),
            "the gate must still let an ordinary session enter the alternate screen"
        );
    }

    /// The probe's `╭` is printed and erased inside the alternate screen
    /// now, which is what makes the ordering above free: the glyph the
    /// cell-width query borrows a line for never reaches the screen the
    /// shell resumes on, whatever the emulator does with either buffer.
    #[cfg(unix)]
    #[test]
    fn the_cell_width_glyph_is_printed_and_erased_on_the_alternate_screen() {
        let wire = session_bytes(false);
        let glyph = find_subslice(&wire, "╭".as_bytes())
            .expect("the cell-width query still prints its glyph");
        let enter_alt = find_subslice(&wire, b"\x1b[?1049h")
            .expect("a session still enters the alternate screen");
        let erase = find_subslice(&wire, b"\r\x1b[K")
            .expect("the cell-width query still erases the line it borrowed");
        let leave_alt = find_subslice(&wire, b"\x1b[?1049l")
            .expect("a session still leaves the alternate screen");
        assert!(
            enter_alt < glyph && glyph < erase && erase < leave_alt,
            "the glyph is drawn and erased between the two switches"
        );
    }

    #[cfg(unix)]
    #[test]
    fn the_keyboard_protocol_push_lands_inside_the_alternate_screen() {
        let on = session_bytes(true);
        let enter_alt = find_subslice(&on, b"\x1b[?1049h")
            .expect("entry still switches to the alternate screen");
        let push = find_subslice(&on, b"\x1b[>1u").expect(
            "the full tier must push DISAMBIGUATE_ESCAPE_CODES, or a shifted and a plain \
             <CR> stay the same byte and <S-CR> can never fire",
        );
        assert!(
            enter_alt < push,
            "the protocol's lifetime nests inside the alternate screen's: push after entering"
        );

        let off = session_bytes(false);
        assert!(
            find_subslice(&off, b"\x1b[>1u").is_none(),
            "a terminal whose probe said no must be sent no keyboard-protocol push at all"
        );
        assert!(
            find_subslice(&off, b"\x1b[?1049h").is_some(),
            "declining the push must not cost the alternate screen"
        );
    }

    /// The panic path reaches [`restore`] twice: the hook runs it, the
    /// previous hook prints the message on the main screen, and unwinding
    /// then drops the [`Term`] that owns the guard. The second pass must
    /// write nothing, or the clear, the park and the buffer switch land on
    /// top of the message the first pass made readable.
    ///
    /// The only test that touches [`RESTORED`], and it leaves the flag set
    /// on purpose: the flag is the process's, and nothing else in this
    /// binary reaches [`restore_bytes_once`].
    #[cfg(unix)]
    #[test]
    fn the_teardown_escapes_are_written_once_per_process() {
        let mut first = Vec::new();
        restore_bytes_once(&mut first, 24).unwrap();
        assert!(
            !first.is_empty(),
            "the first pass writes the teardown the terminal is owed"
        );

        let mut second = Vec::new();
        restore_bytes_once(&mut second, 24).unwrap();
        assert!(
            second.is_empty(),
            "a second pass writes nothing: on the panic path it would otherwise clear and \
             switch buffers over the message the first pass made readable -- {:?}",
            String::from_utf8_lossy(&second)
        );
    }

    #[cfg(unix)]
    #[test]
    fn restore_bytes_pops_the_kitty_keyboard_protocol_before_leaving_the_alternate_screen() {
        let mut buf = Vec::new();
        restore_bytes(&mut buf, 24).unwrap();

        let pop = find_subslice(&buf, b"\x1b[<u").expect(
            "restore must pop the keyboard protocol unconditionally: the panic hook reaches \
             this path with no TermCaps in scope, and a shell left in disambiguating mode \
             reads every Esc as a CSI",
        );
        let leave_alt = find_subslice(&buf, b"\x1b[?1049l")
            .expect("restore must still leave the alternate screen");
        assert!(
            pop < leave_alt,
            "the pop must be written on the screen the push applied to, matching nvim's own \
             terminfo_disable-then-terminfo_stop order"
        );
    }

    /// The whole session's worth of keyboard-protocol bytes, for the one
    /// order the unit pins above cannot see between them: the entry window
    /// that heard no answer, the settle that heard one, and every frame
    /// after it asking the same question again. `draw_surface` compares
    /// `model.caps` to its own on every frame, so "push once" is a claim
    /// about a run of adoptions rather than about any single one.
    #[cfg(unix)]
    #[test]
    fn a_kitty_answer_arriving_after_entry_pushes_the_protocol_exactly_once() {
        let mut wire = Vec::new();
        enter_bytes(&mut wire).unwrap();
        assert_eq!(
            occurrences(&wire, KITTY_KBD_PUSH),
            0,
            "the entry window heard no answer, so it pushes nothing"
        );

        // the settle, and then the frames that follow it
        push_kitty_keyboard(&mut wire, false, true).unwrap();
        for _ in 0..3 {
            push_kitty_keyboard(&mut wire, true, true).unwrap();
        }
        assert_eq!(
            occurrences(&wire, KITTY_KBD_PUSH),
            1,
            "a terminal pushed twice needs two pops, and this session writes one"
        );

        restore_bytes(&mut wire, 24).unwrap();
        assert_eq!(
            occurrences(&wire, KITTY_KBD_POP),
            1,
            "one push, one pop: the shell gets its own Esc handling back"
        );
    }

    /// The other end of the same rule: an answer that never arrives leaves
    /// the wire clean, and the unconditional pop is still a no-op there.
    #[cfg(unix)]
    #[test]
    fn a_terminal_that_never_answers_is_pushed_nothing() {
        let mut wire = Vec::new();
        enter_bytes(&mut wire).unwrap();
        for _ in 0..3 {
            push_kitty_keyboard(&mut wire, false, false).unwrap();
        }
        assert_eq!(occurrences(&wire, KITTY_KBD_PUSH), 0);
    }

    fn occurrences(haystack: &[u8], needle: &[u8]) -> usize {
        haystack
            .windows(needle.len())
            .filter(|window| *window == needle)
            .count()
    }

    // Serves only the unix-gated enter/restore byte tests above.
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

    /// The escapes a frame's own bytes are searched for. Each is what
    /// crossterm emits for the command named beside it, restated here so a
    /// pin reads as the wire rather than as a call.
    const SHOW_CURSOR: &[u8] = b"\x1b[?25h";
    const HIDE_CURSOR: &[u8] = b"\x1b[?25l";
    const SGR_TRAILER: &[u8] = b"\x1b[39m\x1b[49m\x1b[59m\x1b[0m";
    const SYNC_BEGIN: &[u8] = b"\x1b[?2026h";
    const SYNC_END: &[u8] = b"\x1b[?2026l";

    /// The CUP a cursor at `(col, row)` is addressed with, one-based the way
    /// `CSI H` counts.
    fn cup(col: u16, row: u16) -> Vec<u8> {
        format!("\x1b[{};{}H", row + 1, col + 1).into_bytes()
    }

    /// A model whose grid is painted and sized to the terminal, so a frame
    /// composes real cells rather than the startup shell.
    fn probe_model(caps: TermCaps) -> Model {
        let mut model = Model::with_term_size(20, 4);
        model.caps = caps;
        model.engine.apply_grid(view_core::grid::GridOp::Resize {
            width: 20,
            height: 4,
        });
        model
    }

    /// The caret `render` places for `model`, moved to `(col, row)`.
    ///
    /// Taken from a rendered surface rather than built here: `CursorSpec` is
    /// `#[non_exhaustive]`, so this crate cannot name its fields into
    /// existence, and the shape a real frame carries is the one to move.
    fn caret_at(model: &Model, col: u16, row: u16) -> Option<view_surface::CursorSpec> {
        let mut spec = view_surface::render(model)
            .cursor
            .expect("a painted grid places a caret");
        spec.col = col;
        spec.row = row;
        Some(spec)
    }

    /// The bytes `queue_frame` produced, taken out of the buffer the way
    /// `draw_surface`'s own write does.
    fn frame_bytes(
        term: &mut Term,
        model: &Model,
        surface: &Surface,
        damage: &GridDamage,
    ) -> Vec<u8> {
        term.queue_frame(model, surface, damage).unwrap();
        std::mem::take(&mut *term.frame_buf.borrow_mut())
    }

    /// nvim flushes a redraw batch for input it did not act on -- a wheel
    /// report at a buffer's end -- and a frame composing the same cells at
    /// the same cursor must cost the terminal nothing at all.
    ///
    /// Disconfirm: writing the SGR trailer unconditionally, or re-stating the
    /// cursor position every frame, makes the second frame non-empty here.
    #[test]
    fn a_frame_that_repaints_nothing_and_moves_nothing_writes_no_bytes() {
        let model = probe_model(TermCaps::default());
        let mut surface = view_surface::render(&model);
        surface.cursor = caret_at(&model, 2, 1);
        let mut term = Term::frame_probe(model.caps);

        let first = frame_bytes(&mut term, &model, &surface, &GridDamage::full());
        assert!(
            !first.is_empty(),
            "the first frame paints the whole grid, so it must reach the terminal"
        );
        let second = frame_bytes(&mut term, &model, &surface, &GridDamage::default());
        assert!(
            second.is_empty(),
            "an unchanged frame wrote {second:?} instead of nothing"
        );
    }

    /// A cursor that moved with no cell repainted is one CUP and nothing
    /// else: no trailer (no style was set), no show (it was already shown).
    #[test]
    fn a_cursor_only_move_writes_exactly_one_cursor_position() {
        let model = probe_model(TermCaps::default());
        let mut surface = view_surface::render(&model);
        surface.cursor = caret_at(&model, 2, 1);
        let mut term = Term::frame_probe(model.caps);
        let _ = frame_bytes(&mut term, &model, &surface, &GridDamage::full());

        surface.cursor = caret_at(&model, 5, 2);
        let moved = frame_bytes(&mut term, &model, &surface, &GridDamage::default());
        assert_eq!(
            moved,
            cup(5, 2),
            "a frame whose only news is where the caret sits owes the terminal \
             one CUP and nothing else"
        );
    }

    /// A frame that did repaint a cell owes the trailer once and the cursor
    /// position again -- the emission left the terminal's own caret after the
    /// last glyph it printed -- and still owes no show.
    #[test]
    fn a_repainted_cell_carries_one_trailer_and_restates_the_cursor() {
        let mut model = probe_model(TermCaps::default());
        let mut surface = view_surface::render(&model);
        surface.cursor = caret_at(&model, 7, 3);
        let mut term = Term::frame_probe(model.caps);
        let _ = frame_bytes(&mut term, &model, &surface, &GridDamage::full());

        model.engine.apply_grid(view_core::grid::GridOp::PutLine {
            row: 1,
            col_start: 3,
            cells: vec![("X".to_string(), 0, 1)],
        });
        let painted = frame_bytes(&mut term, &model, &surface, &GridDamage::full());

        assert_eq!(
            occurrences(&painted, SGR_TRAILER),
            1,
            "the trailer resets what this frame's own styles set, so it is \
             written once after the cells; frame: {painted:?}"
        );
        let caret = cup(7, 3);
        assert_eq!(
            occurrences(&painted, &caret),
            1,
            "the caret is re-addressed once, after the glyph the emission left \
             the terminal's cursor behind; frame: {painted:?}"
        );
        assert!(
            painted.ends_with(&caret),
            "the caret's CUP is the frame's last word; frame: {painted:?}"
        );
        assert_eq!(
            occurrences(&painted, SHOW_CURSOR),
            0,
            "the caret was already shown, so this frame states nothing about it"
        );
    }

    /// Show and hide are each a change, not a per-frame restatement.
    #[test]
    fn the_caret_is_shown_once_when_it_comes_back() {
        let model = probe_model(TermCaps::default());
        let mut surface = view_surface::render(&model);
        surface.cursor = None;
        let mut term = Term::frame_probe(model.caps);

        let hidden = frame_bytes(&mut term, &model, &surface, &GridDamage::full());
        assert!(
            hidden.ends_with(HIDE_CURSOR),
            "a surface carrying no caret hides the terminal's, and the hide is \
             the frame's last word; frame: {hidden:?}"
        );

        surface.cursor = caret_at(&model, 1, 1);
        let mut shape = Vec::new();
        write_cursor_shape(&mut shape, surface.cursor.unwrap().shape).unwrap();
        let shown = frame_bytes(&mut term, &model, &surface, &GridDamage::default());
        assert_eq!(
            shown,
            [cup(1, 1), SHOW_CURSOR.to_vec(), shape].concat(),
            "the caret coming back is a position, a show and the shape no frame \
             has stated yet, in that order and nothing else"
        );

        let again = frame_bytes(&mut term, &model, &surface, &GridDamage::default());
        assert_eq!(
            occurrences(&again, SHOW_CURSOR),
            0,
            "a caret that never left is shown no second time; frame: {again:?}"
        );
    }

    /// A frame that repainted cells with the caret hidden left the
    /// terminal's own caret wherever the last glyph landed, so the position
    /// this type remembers is no longer a fact about the terminal.
    ///
    /// Disconfirm: removing the `last_cursor = None` from `queue_frame`'s
    /// hidden arm leaves the third frame writing only the show, and the
    /// caret comes back at the last glyph rather than where the model has
    /// it.
    #[test]
    fn a_painted_frame_with_the_caret_hidden_forgets_where_it_was() {
        let mut model = probe_model(TermCaps::default());
        let mut surface = view_surface::render(&model);
        surface.cursor = caret_at(&model, 2, 1);
        let mut term = Term::frame_probe(model.caps);
        let _ = frame_bytes(&mut term, &model, &surface, &GridDamage::full());

        model.engine.apply_grid(view_core::grid::GridOp::PutLine {
            row: 1,
            col_start: 3,
            cells: vec![("X".to_string(), 0, 1)],
        });
        surface.cursor = None;
        let painted = frame_bytes(&mut term, &model, &surface, &GridDamage::full());
        assert!(
            painted.ends_with(HIDE_CURSOR),
            "the frame that hides the caret must still say so; frame: {painted:?}"
        );

        surface.cursor = caret_at(&model, 2, 1);
        let back = frame_bytes(&mut term, &model, &surface, &GridDamage::default());
        assert_eq!(
            back,
            [cup(2, 1), SHOW_CURSOR.to_vec()].concat(),
            "the caret returning to the cell it left must still be addressed: \
             the frame in between moved the terminal's own caret and \
             re-addressed nothing"
        );
    }

    /// A resize moves the terminal's own caret -- clamped on a shrink,
    /// reflowed on a width change -- and the erase that follows does not put
    /// it back, so the position this type remembers cannot survive one.
    ///
    /// Disconfirm: removing the `last_cursor = None` from `queue_frame`'s
    /// resize arm leaves the resized frame carrying no CUP at all.
    #[test]
    fn a_resize_forgets_where_the_caret_was() {
        let mut model = probe_model(TermCaps::default());
        let mut surface = view_surface::render(&model);
        surface.cursor = caret_at(&model, 2, 1);
        let mut term = Term::frame_probe(model.caps);
        let _ = frame_bytes(&mut term, &model, &surface, &GridDamage::full());

        model.term_height += 1;
        let resized = frame_bytes(&mut term, &model, &surface, &GridDamage::default());
        assert!(
            occurrences(&resized, &cup(2, 1)) > 0,
            "a frame whose terminal changed size owes the caret its position \
             again, whatever its cells compose to; frame: {resized:?}"
        );
    }

    /// The bracket wraps a frame's contents, so a frame with no contents
    /// opens none: an empty synchronized update is two escapes the terminal
    /// parses for nothing.
    #[test]
    fn an_empty_frame_opens_no_synchronization_bracket() {
        let model = probe_model(TermCaps::from_probe(true, true, true));
        let mut surface = view_surface::render(&model);
        surface.cursor = caret_at(&model, 2, 1);
        let mut term = Term::frame_probe(model.caps);

        let first = frame_bytes(&mut term, &model, &surface, &GridDamage::full());
        assert_eq!(
            (
                occurrences(&first, SYNC_BEGIN),
                occurrences(&first, SYNC_END)
            ),
            (1, 1),
            "a frame with content is bracketed exactly once"
        );

        let second = frame_bytes(&mut term, &model, &surface, &GridDamage::default());
        assert!(
            second.is_empty(),
            "an unchanged frame at a synchronizing terminal wrote {second:?}"
        );
    }

    /// A cursor-only move at a synchronizing terminal is content, so it is
    /// bracketed: the pair is about whether anything was written between the
    /// two escapes, never about whether a cell was repainted.
    #[test]
    fn a_bracketed_cursor_move_is_wrapped_by_the_pair() {
        let model = probe_model(TermCaps::from_probe(true, true, true));
        let mut surface = view_surface::render(&model);
        surface.cursor = caret_at(&model, 2, 1);
        let mut term = Term::frame_probe(model.caps);
        let _ = frame_bytes(&mut term, &model, &surface, &GridDamage::full());

        surface.cursor = caret_at(&model, 5, 2);
        let moved = frame_bytes(&mut term, &model, &surface, &GridDamage::default());
        let at = cup(5, 2);
        assert_eq!(
            moved,
            [SYNC_BEGIN, at.as_slice(), SYNC_END].concat(),
            "a frame whose only news is the caret's position is still a frame \
             with content, so the pair wraps that CUP and nothing else"
        );
    }

    /// A shape change is the frame's whole content, and it is written after
    /// the cursor arm rather than before the opener, so the pair wraps it.
    #[test]
    fn a_bracketed_shape_change_is_wrapped_by_the_pair() {
        let model = probe_model(TermCaps::from_probe(true, true, true));
        let mut surface = view_surface::render(&model);
        surface.cursor = caret_at(&model, 2, 1);
        let mut term = Term::frame_probe(model.caps);
        let _ = frame_bytes(&mut term, &model, &surface, &GridDamage::full());

        let mut spec = surface.cursor.unwrap();
        spec.shape = if spec.shape == CursorShape::Block {
            CursorShape::Vertical(25)
        } else {
            CursorShape::Block
        };
        surface.cursor = Some(spec);
        let reshaped = frame_bytes(&mut term, &model, &surface, &GridDamage::default());
        let mut shape = Vec::new();
        write_cursor_shape(&mut shape, spec.shape).unwrap();
        assert_eq!(
            reshaped,
            [SYNC_BEGIN, shape.as_slice(), SYNC_END].concat(),
            "the caret has not moved and no cell changed, so the shape escape \
             is all the pair has to wrap"
        );
    }

    /// The mouse toggle is queued ahead of the opener -- it is a terminal
    /// mode change, not part of the update being synchronized -- so a frame
    /// carrying only a toggle leaves the pair unopened.
    // On Windows the toggle is a console-mode call that writes no bytes, so
    // the expectation this builds through `queue!` cannot be built there.
    #[cfg(not(windows))]
    #[test]
    fn a_mouse_toggle_alone_opens_no_bracket() {
        let mut model = probe_model(TermCaps::from_probe(true, true, true));
        let mut surface = view_surface::render(&model);
        surface.cursor = caret_at(&model, 2, 1);
        let mut term = Term::frame_probe(model.caps);
        let _ = frame_bytes(&mut term, &model, &surface, &GridDamage::full());

        model.engine.mouse_on = true;
        let toggled = frame_bytes(&mut term, &model, &surface, &GridDamage::default());
        let mut expected = Vec::new();
        crossterm::queue!(expected, EnableMouseCapture).unwrap();
        assert_eq!(
            toggled, expected,
            "a frame that only turns mouse reporting on writes that escape \
             alone: the update it would bracket is empty"
        );
    }

    /// On Windows both mouse commands are console-API calls, and the disable
    /// reads a console mode that only the enable stores, so a disable written
    /// before any enable is an error and the frame that carries it fails. The
    /// bytes are readable on unix, which is where the contract can be pinned.
    #[test]
    fn a_first_frame_with_reporting_off_writes_no_mouse_toggle() {
        let model = probe_model(TermCaps::from_probe(true, true, true));
        assert!(
            !model.engine.mouse_on,
            "the fixture starts with mouse reporting off"
        );
        let mut surface = view_surface::render(&model);
        surface.cursor = caret_at(&model, 2, 1);
        let mut term = Term::frame_probe(model.caps);

        let first = frame_bytes(&mut term, &model, &surface, &GridDamage::full());
        let first = String::from_utf8_lossy(&first);
        for mode in ["?1000", "?1002", "?1003", "?1006", "?1015"] {
            assert!(
                !first.contains(mode),
                "the first frame states no mouse-tracking mode, and it wrote {mode}"
            );
        }
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
