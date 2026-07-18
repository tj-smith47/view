//! The pty-level driver: spawns an arbitrary command inside a real pty and
//! turns its output into a queryable [`vt100`] screen -- the integration
//! leg of the oracle's three-level driver stack, exercising the full stack
//! (terminal input decode, paint, real process) the way a human at a
//! terminal would, unlike [`crate::Session`] (pure, no process at all) or
//! [`crate::EngineSession`] (a real engine, but no terminal).
//!
//! Promoted from `view-oracle`'s own `tests/smoke.rs`, which duplicated
//! this exact spawn/wait/send machinery ad hoc across a session of
//! hardening (an always-rebuild binary check so a stale target/debug/view
//! can never produce a false pass, echo-immune oracles that read a file
//! back rather than trust the pty's own canonical-mode echo, and
//! current-state-check-first waits so a condition already true when a wait
//! call starts is never missed waiting for a *new* chunk that never
//! arrives). This module is the reusable core of that machinery; `view`
//! binary-specific concerns (isolating the host's real nvim config,
//! locating the always-rebuilt `target/debug/view` path, reading a saved
//! scratch file back) stay in `tests/smoke.rs`, which now builds on
//! [`PtySession::spawn_configured`] instead of duplicating the pty-opening
//! logic itself.

use std::io::{Read, Write};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::time::{Duration, Instant};

use portable_pty::{native_pty_system, CommandBuilder, PtySize};

use crate::OracleError;

/// Spawns a background thread that forwards every chunk read from `reader`
/// onto the returned channel, so the caller can poll with a bounded timeout
/// instead of blocking on a single `read` that may return only part of the
/// child's output.
fn spawn_reader(mut reader: Box<dyn Read + Send>) -> mpsc::Receiver<Vec<u8>> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut buf = [0_u8; 65536];
        loop {
            match reader.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if tx.send(buf[..n].to_vec()).is_err() {
                        break;
                    }
                }
            }
        }
    });
    rx
}

/// A process running inside a real pty, with everything a test needs to
/// drive it and observe its screen: the child handle, a byte channel fed by
/// a background reader thread, and a `vt100` parser that turns those bytes
/// into a queryable screen.
pub struct PtySession {
    child: Box<dyn portable_pty::Child + Send + Sync>,
    rx: mpsc::Receiver<Vec<u8>>,
    writer: Box<dyn Write + Send>,
    parser: vt100::Parser,
}

impl PtySession {
    /// Opens a `cols`x`rows` pty and spawns `cmd` with `args` inside it.
    ///
    /// A thin wrapper over [`spawn_configured`](Self::spawn_configured) for
    /// the common case that needs no environment or working-directory
    /// control; a caller that does (isolating a test's `view` invocation
    /// from the host's real nvim config, for instance) builds its own
    /// [`CommandBuilder`] and calls `spawn_configured` directly.
    ///
    /// # Errors
    ///
    /// Returns [`OracleError::Pty`] if the pty cannot be opened or the
    /// command fails to spawn.
    pub fn spawn(cmd: &str, args: &[&str], cols: u16, rows: u16) -> Result<Self, OracleError> {
        let mut builder = CommandBuilder::new(cmd);
        for arg in args {
            builder.arg(arg);
        }
        Self::spawn_configured(builder, cols, rows)
    }

    /// Like [`spawn`](Self::spawn), but takes an already-configured
    /// [`CommandBuilder`] (environment variables, working directory) rather
    /// than building a bare one from `cmd`/`args`.
    ///
    /// # Errors
    ///
    /// Returns [`OracleError::Pty`] if the pty cannot be opened or the
    /// command fails to spawn.
    pub fn spawn_configured(
        cmd: CommandBuilder,
        cols: u16,
        rows: u16,
    ) -> Result<Self, OracleError> {
        let pty = native_pty_system();
        let pair = pty
            .openpty(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(|e| OracleError::Pty(e.to_string()))?;

        let child = pair
            .slave
            .spawn_command(cmd)
            .map_err(|e| OracleError::Pty(e.to_string()))?;
        let reader = pair
            .master
            .try_clone_reader()
            .map_err(|e| OracleError::Pty(e.to_string()))?;
        let writer = pair
            .master
            .take_writer()
            .map_err(|e| OracleError::Pty(e.to_string()))?;
        // the slave fd must not outlive the child's own copy, or the master
        // never sees EOF once the child exits
        drop(pair.slave);

        let rx = spawn_reader(reader);
        let parser = vt100::Parser::new(rows, cols, 0);

        Ok(Self {
            child,
            rx,
            writer,
            parser,
        })
    }

    /// Writes `bytes` to the pty as if a user typed them.
    ///
    /// # Errors
    ///
    /// Returns [`OracleError::Io`] if the write or flush fails.
    pub fn send(&mut self, bytes: &[u8]) -> Result<(), OracleError> {
        self.writer.write_all(bytes)?;
        self.writer.flush()?;
        Ok(())
    }

    /// Pulls every chunk already buffered on the reader channel into the
    /// parser without blocking, then returns the screen's current text
    /// content.
    #[must_use]
    pub fn screen(&mut self) -> String {
        self.drain_available();
        self.parser.screen().contents()
    }

    /// Same as [`screen`](Self::screen), but returns the parsed [`vt100::Screen`]
    /// itself rather than its plain-text contents, for callers that need
    /// per-cell detail (wide-character continuation cells, a specific
    /// row/column) beyond a whole-screen string.
    #[must_use]
    pub fn screen_raw(&mut self) -> &vt100::Screen {
        self.drain_available();
        self.parser.screen()
    }

    fn drain_available(&mut self) {
        while let Ok(chunk) = self.rx.try_recv() {
            self.parser.process(&chunk);
        }
    }

    /// Blocks (up to `timeout`) until the screen contains `needle`,
    /// returning whether it appeared.
    ///
    /// Checks the already-processed screen state before blocking: a prior
    /// call (or another already-arrived chunk) may already have processed
    /// the data that satisfies this condition, and blocking on the channel
    /// unconditionally would otherwise wait for a *new* chunk that never
    /// comes once the screen has settled, timing out despite the condition
    /// already being true.
    #[must_use]
    pub fn wait_for(&mut self, needle: &str, timeout: Duration) -> bool {
        if self.parser.screen().contents().contains(needle) {
            return true;
        }
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            match self.rx.recv_timeout(Duration::from_millis(200)) {
                Ok(chunk) => {
                    self.parser.process(&chunk);
                    if self.parser.screen().contents().contains(needle) {
                        return true;
                    }
                }
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => break,
            }
        }
        false
    }

    /// Blocks (up to `timeout`) until the cell at `(row, col)` holds exactly
    /// `expected`, returning whether it did. Unlike [`wait_for`](Self::wait_for)
    /// (whole-screen substring search), this pins content to a specific
    /// cell, for assertions where position is the point.
    ///
    /// Checks the already-processed screen state before blocking, for the
    /// same reason [`wait_for`](Self::wait_for) does.
    #[must_use]
    pub fn wait_for_cell(&mut self, row: u16, col: u16, expected: &str, timeout: Duration) -> bool {
        if self
            .parser
            .screen()
            .cell(row, col)
            .is_some_and(|c| c.contents() == expected)
        {
            return true;
        }
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            match self.rx.recv_timeout(Duration::from_millis(200)) {
                Ok(chunk) => {
                    self.parser.process(&chunk);
                    if self
                        .parser
                        .screen()
                        .cell(row, col)
                        .is_some_and(|c| c.contents() == expected)
                    {
                        return true;
                    }
                }
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => break,
            }
        }
        false
    }

    /// Blocks until the child exits, per [`portable_pty::Child::wait`].
    ///
    /// # Errors
    ///
    /// Returns [`OracleError::Io`] if the underlying wait fails.
    pub fn wait(&mut self) -> Result<portable_pty::ExitStatus, OracleError> {
        self.child.wait().map_err(Into::into)
    }

    /// Blocks (up to `timeout`), polling rather than
    /// [`wait`](Self::wait)'s unbounded blocking form, until the child has
    /// exited. Returns `None` -- after killing the child so it cannot
    /// outlive the caller -- if it is still running once `timeout` elapses,
    /// so a real deadlock in the child under test fails an assertion
    /// promptly instead of hanging the whole test binary (and, with it,
    /// CI) the way `wait` would.
    pub fn wait_for_exit(&mut self, timeout: Duration) -> Option<portable_pty::ExitStatus> {
        let deadline = Instant::now() + timeout;
        loop {
            if let Ok(Some(status)) = self.child.try_wait() {
                return Some(status);
            }
            if Instant::now() >= deadline {
                let _ = self.child.kill();
                return None;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    /// The OS pid of the spawned child, if the platform exposes one.
    #[must_use]
    pub fn pid(&self) -> Option<u32> {
        self.child.process_id()
    }

    /// Kills the child immediately, for a caller giving up mid-test rather
    /// than waiting out a full timeout.
    pub fn kill(&mut self) {
        let _ = self.child.kill();
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    #[test]
    fn spawn_and_send_shows_typed_output_on_screen() {
        let mut session = PtySession::spawn("/bin/cat", &[], 80, 24).unwrap();
        session.send(b"hello-pty\n").unwrap();
        assert!(
            session.wait_for("hello-pty", Duration::from_secs(5)),
            "screen never showed cat's echoed input; screen:\n{}",
            session.screen()
        );
        session.kill();
        let _ = session.wait_for_exit(Duration::from_secs(2));
    }

    #[test]
    fn wait_for_returns_true_immediately_when_the_needle_is_already_on_screen() {
        let mut session = PtySession::spawn("/bin/echo", &["already-there"], 80, 24).unwrap();
        // no send(): the text is already on screen from the process's own
        // startup output, the current-state-check-first path wait_for must
        // take rather than blocking for a chunk that may never arrive again
        assert!(session.wait_for("already-there", Duration::from_secs(5)));
    }

    #[test]
    fn wait_for_times_out_on_a_needle_that_never_appears() {
        let mut session = PtySession::spawn("/bin/echo", &["hi"], 80, 24).unwrap();
        assert!(!session.wait_for("this-never-appears", Duration::from_millis(200)));
    }
}
