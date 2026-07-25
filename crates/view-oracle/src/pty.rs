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

use portable_pty::{native_pty_system, CommandBuilder, MasterPty, PtySize};

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

/// Detaches `cmd` from the host's own editor configuration: drops every
/// variable that redirects where an editor finds config, runtime files,
/// plugin manifests or startup commands, and points the two search-path
/// variables at an empty directory, whose system-wide defaults would
/// otherwise apply (see [`view_engine::env`] for the enumeration).
///
/// Applied to every pty spawn rather than left to each caller. Pointing the
/// four `XDG_*_HOME` variables at private directories, which the callers do
/// individually and for their own reasons, leaves these untouched, and a
/// session that reads one of them is indistinguishable from a correct one
/// until its results are compared against a machine where the variable is
/// not set. Nothing spawned through a pty here is a user's editor: these
/// are measured and asserted-on sessions, one and all, so there is no
/// caller for which inheriting the host's setup is the wanted behavior.
fn hermetic_env(cmd: &mut CommandBuilder) {
    for name in view_engine::env::HOST_REDIRECT_VARS {
        cmd.env_remove(name);
    }
    let empty = view_engine::env::empty_search_path();
    for name in view_engine::env::HOST_SEARCH_PATH_VARS {
        cmd.env(name, &empty);
    }
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
    master: Box<dyn MasterPty + Send>,
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
    /// Every host environment variable that redirects an editor's
    /// configuration is neutralized here, after whatever the caller
    /// configured, so no caller can spawn a session that answers to the
    /// machine it runs on (see [`view_engine::env`], and
    /// [`hermetic_env`] for why this is the funnel that applies it).
    ///
    /// # Errors
    ///
    /// Returns [`OracleError::Pty`] if the pty cannot be opened or the
    /// command fails to spawn.
    pub fn spawn_configured(
        mut cmd: CommandBuilder,
        cols: u16,
        rows: u16,
    ) -> Result<Self, OracleError> {
        hermetic_env(&mut cmd);
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
            master: pair.master,
        })
    }

    /// Resizes the pty to `cols`x`rows`: informs the kernel (which delivers
    /// `SIGWINCH` to the child) and resizes the local `vt100` screen so
    /// subsequent cursor-positioning escapes are interpreted against the
    /// new dimensions rather than the ones the session was opened with.
    ///
    /// # Errors
    ///
    /// Returns [`OracleError::Pty`] if the kernel resize call fails.
    pub fn resize(&mut self, cols: u16, rows: u16) -> Result<(), OracleError> {
        self.master
            .resize(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(|e| OracleError::Pty(e.to_string()))?;
        self.parser.screen_mut().set_size(rows, cols);
        Ok(())
    }

    /// Resizes to `cols`x`rows` like [`resize`](Self::resize), then retries
    /// (bounded by `timeout`) until `confirm` reports the child has actually
    /// reacted, rather than trusting a single `TIOCSWINSZ` call to have
    /// notified it.
    ///
    /// A single resize is not a reliable notification in practice: the
    /// kernel only raises `SIGWINCH` on a genuine size *change*, and that
    /// signal's delivery to the child's signal-handling thread has been
    /// observed to go missing outright under this harness (confirmed by
    /// instrumenting the child's own dispatch loop across repeated runs: in
    /// the failing runs, no resize event ever reached it at all, with no
    /// other sign of the child being stuck -- not a slow delivery, a lost
    /// one). Re-issuing the identical target size is a kernel no-op with no
    /// new signal, so each retry first nudges to `rows.saturating_sub(1)`
    /// and back: two genuine size deltas, each independently eligible for
    /// its own `SIGWINCH`, before `confirm` is asked again.
    ///
    /// # Errors
    ///
    /// Returns [`OracleError::Pty`] if a kernel resize call fails.
    pub fn resize_until(
        &mut self,
        cols: u16,
        rows: u16,
        timeout: Duration,
        mut confirm: impl FnMut(&mut Self) -> bool,
    ) -> Result<bool, OracleError> {
        let deadline = Instant::now() + timeout;
        self.resize(cols, rows)?;
        loop {
            if confirm(self) {
                return Ok(true);
            }
            if Instant::now() >= deadline {
                return Ok(false);
            }
            std::thread::sleep(Duration::from_millis(150));
            self.resize(cols, rows.saturating_sub(1))?;
            self.resize(cols, rows)?;
        }
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

    /// Drains pending pty output into the parser, then hands `f` a borrowed
    /// [`vt100::Screen`] to inspect. For a caller that only needs to peek at
    /// cell contents (a tight polling loop counting occurrences of one
    /// character, say), this is the non-allocating counterpart to
    /// [`screen`](Self::screen): that method builds a fresh `String` of the
    /// whole screen on every call, which is wasted work for a caller that
    /// immediately discards it after inspecting a handful of bytes.
    pub fn with_screen<R>(&mut self, f: impl FnOnce(&vt100::Screen) -> R) -> R {
        self.drain_available();
        f(self.parser.screen())
    }

    fn drain_available(&mut self) {
        while let Ok(chunk) = self.rx.try_recv() {
            self.parser.process(&chunk);
        }
    }

    /// Blocks (up to `timeout`) until `predicate` holds against the current
    /// screen state, returning whether it did. Shared by [`wait_for`]
    /// (whole-screen substring search) and [`wait_for_cell`] (single-cell
    /// match): both differ only in what they check, not in how they poll.
    ///
    /// Checks the already-processed screen state before blocking: a prior
    /// call (or another already-arrived chunk) may already have processed
    /// the data that satisfies `predicate`, and blocking on the channel
    /// unconditionally would otherwise wait for a *new* chunk that never
    /// comes once the screen has settled, timing out despite the condition
    /// already being true.
    ///
    /// [`wait_for`]: Self::wait_for
    /// [`wait_for_cell`]: Self::wait_for_cell
    fn wait_until(
        &mut self,
        timeout: Duration,
        mut predicate: impl FnMut(&vt100::Screen) -> bool,
    ) -> bool {
        if predicate(self.parser.screen()) {
            return true;
        }
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            match self.rx.recv_timeout(Duration::from_millis(200)) {
                Ok(chunk) => {
                    self.parser.process(&chunk);
                    if predicate(self.parser.screen()) {
                        return true;
                    }
                }
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => break,
            }
        }
        false
    }

    /// Blocks (up to `timeout`) until the screen contains `needle`,
    /// returning whether it appeared.
    #[must_use]
    pub fn wait_for(&mut self, needle: &str, timeout: Duration) -> bool {
        self.wait_until(timeout, |screen| screen.contents().contains(needle))
    }

    /// Blocks (up to `timeout`) until `predicate` holds against the screen,
    /// returning whether it did -- [`wait_for`](Self::wait_for)'s
    /// whole-screen substring check generalized to an arbitrary condition.
    ///
    /// For a caller asserting an *ordering* rather than a single needle:
    /// a predicate reading several things at once ("the placeholder is
    /// showing and the engine's content is not yet there") is checked
    /// against one screen state, whereas two successive `wait_for` calls
    /// each observe a possibly-different state and so cannot express a
    /// relationship between them.
    ///
    /// The predicate is the last parameter, unlike the needle in
    /// `wait_for`: it is the closure argument, and Rust reads a call with a
    /// trailing closure better than one that buries it before a duration.
    #[must_use]
    pub fn wait_for_screen(
        &mut self,
        timeout: Duration,
        predicate: impl FnMut(&vt100::Screen) -> bool,
    ) -> bool {
        self.wait_until(timeout, predicate)
    }

    /// Blocks (up to `timeout`) until the cell at `(row, col)` holds exactly
    /// `expected`, returning whether it did. Unlike [`wait_for`](Self::wait_for)
    /// (whole-screen substring search), this pins content to a specific
    /// cell, for assertions where position is the point.
    #[must_use]
    pub fn wait_for_cell(&mut self, row: u16, col: u16, expected: &str, timeout: Duration) -> bool {
        self.wait_until(timeout, |screen| {
            screen
                .cell(row, col)
                .is_some_and(|c| c.contents() == expected)
        })
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
    ///
    /// The post-kill path also reaps: a killed child that is never waited on
    /// stays a zombie entry in the process table until this session (or the
    /// whole test binary) exits, and `kill` alone only requests termination,
    /// it does not collect the exit status that removes the entry.
    pub fn wait_for_exit(&mut self, timeout: Duration) -> Option<portable_pty::ExitStatus> {
        let deadline = Instant::now() + timeout;
        loop {
            if let Ok(Some(status)) = self.child.try_wait() {
                return Some(status);
            }
            if Instant::now() >= deadline {
                let _ = self.child.kill();
                let _ = self.child.wait();
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

    /// Reports back what a spawned child sees in the two variables that
    /// stand for the whole enumeration: one removed, one overridden.
    ///
    /// `sh` rather than the `Command` builder's own accessors: the question
    /// is what a real child's environment holds after the spawn, and a
    /// builder that recorded the right plan but handed the child something
    /// else would satisfy every accessor while still leaking.
    fn env_report(preset: &str) -> String {
        let mut cmd = CommandBuilder::new("/bin/sh");
        cmd.arg("-c");
        cmd.arg(r#"echo "VIMINIT=[${VIMINIT-unset}]"; echo "DIRS=[${XDG_CONFIG_DIRS-unset}]""#);
        // planted the way the host would hand them down; `CommandBuilder`
        // holds inherited and explicitly set variables in one map, so
        // removing either is the same operation on the same entry
        cmd.env("VIMINIT", preset);
        cmd.env("XDG_CONFIG_DIRS", preset);
        let mut session = PtySession::spawn_configured(cmd, 200, 24).unwrap();
        // the second line's arrival proves the first line is final, so an
        // absent needle below means the child never printed it rather than
        // that this read was early
        assert!(
            session.wait_for("DIRS=[", Duration::from_secs(5)),
            "the child never reported its environment; screen:\n{}",
            session.screen()
        );
        session.screen()
    }

    #[test]
    fn a_pty_spawn_hands_the_child_none_of_the_hosts_editor_configuration() {
        let planted = "/host/nvim/config";
        let screen = env_report(planted);
        assert!(
            screen.contains("VIMINIT=[unset]"),
            "the host's startup commands reached the child; screen:\n{screen}"
        );
        assert!(
            !screen.contains(planted),
            "the host's config search path reached the child; screen:\n{screen}"
        );
        assert!(
            screen.contains(&format!(
                "DIRS=[{}]",
                view_engine::env::empty_search_path().display()
            )),
            "the child searches somewhere other than the empty directory; screen:\n{screen}"
        );
    }
}
