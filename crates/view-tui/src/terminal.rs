//! Raw-mode / alternate-screen lifecycle for the interactive terminal, plus
//! the typed facade over `ratatui`/`crossterm` that keeps both crates out of
//! the `view` bin crate's dependency graph (`scripts/audit-deps.sh` denies
//! `view -> crossterm` and `view -> ratatui`: only `view-tui` may touch the
//! terminal).

use crate::keys::encode_key;
use crate::paint::{paint, HlTable};
use crossterm::event::{Event, KeyEventKind};
use std::io::Write;
use std::time::Duration;
use view_core::grid::Grid;

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
    /// Enables raw mode, switches to the alternate screen, and installs a
    /// panic hook that restores the terminal before delegating to the
    /// previous hook.
    ///
    /// # Errors
    ///
    /// Returns the underlying `std::io::Error` if raw mode cannot be
    /// enabled or the alternate screen cannot be entered.
    pub fn enter() -> std::io::Result<Self> {
        crossterm::terminal::enable_raw_mode()?;
        crossterm::execute!(std::io::stdout(), crossterm::terminal::EnterAlternateScreen)?;
        // a panic must restore the terminal before the message prints, or the
        // user is left with a broken shell and an invisible error
        let prev = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            restore();
            prev(info);
        }));
        Ok(Self)
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
    let _ = crossterm::execute!(std::io::stdout(), crossterm::terminal::LeaveAlternateScreen);
    let _ = crossterm::terminal::disable_raw_mode();
    let _ = std::io::stdout().flush();
}

/// The ratatui-backed terminal: draws grid frames and reports its size,
/// without exposing `ratatui` types to callers outside this crate.
pub struct Term {
    guard: TerminalGuard,
    inner: ratatui::DefaultTerminal,
}

impl Term {
    /// Initializes the backend terminal: [`TerminalGuard::enter`] performs
    /// the one raw-mode/alternate-screen/panic-hook setup, then the ratatui
    /// terminal is constructed directly over the now-prepared stdout.
    ///
    /// Deliberately does not use `ratatui::try_init`: that function repeats
    /// the same raw-mode/alternate-screen/panic-hook setup `TerminalGuard`
    /// already did, which would enter the alternate screen twice and chain
    /// a second panic hook on top of the first.
    ///
    /// # Errors
    ///
    /// Returns the underlying `std::io::Error` if raw mode or the alternate
    /// screen cannot be entered, or the backend terminal cannot be built.
    pub fn init() -> std::io::Result<Self> {
        let guard = TerminalGuard::enter()?;
        let inner =
            ratatui::Terminal::new(ratatui::backend::CrosstermBackend::new(std::io::stdout()))?;
        Ok(Self { guard, inner })
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

    /// Paints one frame from `grid` and `hl` onto the terminal.
    ///
    /// # Errors
    ///
    /// Returns the underlying `std::io::Error` if the backend write fails.
    pub fn draw(&mut self, grid: &Grid, hl: &HlTable) -> std::io::Result<()> {
        self.inner.draw(|f| paint(grid, hl, f))?;
        Ok(())
    }

    /// Moves the terminal cursor to `(row, col)` and shows it.
    ///
    /// # Errors
    ///
    /// Returns the underlying `std::io::Error` if the backend write fails.
    pub fn set_cursor(&mut self, row: u16, col: u16) -> std::io::Result<()> {
        self.inner.set_cursor_position((col, row))?;
        self.inner.show_cursor()
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

/// One decoded terminal input event, ready for the caller to forward to nvim
/// without touching `crossterm` types itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InputEvent {
    /// A key event already encoded to nvim `nvim_input` notation.
    Key(String),
    /// The terminal was resized to `width` x `height` cells.
    Resize(u16, u16),
}

/// Drains every terminal input event currently available without blocking,
/// decoding key events via [`encode_key`](crate::keys::encode_key) and
/// filtering out anything with no nvim equivalent (key releases, keys with
/// no notation, event kinds this frontend does not act on).
///
/// # Errors
///
/// Returns the underlying `std::io::Error` if polling or reading the next
/// event fails.
pub fn drain_input() -> std::io::Result<Vec<InputEvent>> {
    let mut events = Vec::new();
    while crossterm::event::poll(Duration::ZERO)? {
        match crossterm::event::read()? {
            Event::Key(k) if k.kind != KeyEventKind::Release => {
                if let Some(notation) = encode_key(&k) {
                    events.push(InputEvent::Key(notation));
                }
            }
            Event::Resize(width, height) => events.push(InputEvent::Resize(width, height)),
            _ => {}
        }
    }
    Ok(events)
}
