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

use std::ffi::{OsStr, OsString};
use std::io::{Read, Write};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use portable_pty::{native_pty_system, CommandBuilder, MasterPty, PtySize};

use crate::OracleError;

/// A pty writer shared between the caller (typed input) and the reply
/// thread (autonomous answers to a child's capability queries).
type SharedWriter = Arc<Mutex<Box<dyn Write + Send>>>;

/// Whether a session answers the terminal capability queries its child
/// writes, which decides which startup path that child then takes.
///
/// A real terminal answers, so answering is what a measurement must see:
/// left unanswered, a probing child waits out its whole fallback deadline
/// and every timing taken through the session carries it. Which *set* of
/// queries gets answered is the second axis, because it decides the
/// capability tier the child then derives and therefore how much work each
/// of its frames does. [`Silent`](Self::Silent) is the deliberate opposite,
/// for a test whose subject IS the deadline path -- without it, no session
/// in the tree can reach that path at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum QueryPolicy {
    /// Answer only the DA1 fence, as a VT100-class terminal does. The child
    /// resolves no optional capability and derives its most conservative
    /// tier, which keeps an assertion about *content* free of the escapes a
    /// richer tier would interleave.
    AnswerDa1,
    /// Answer the whole probe batch, as a modern terminal does, so the child
    /// derives its full tier. A benchmark wants this: the budget table names
    /// the tier its rows were promised at, and a session that silently
    /// downgraded the child would time less work than the row claims.
    AnswerFullTier,
    /// Answer nothing, modelling a terminal that ignores every query.
    Silent,
}

/// A capability query a probing child writes, paired with the reply a
/// terminal possessing that capability answers with.
type Answer = (&'static [u8], &'static [u8]);

/// The DA1 (Send Device Attributes) request, and a reply naming a VT100-class
/// terminal with the advanced video option.
///
/// A child such as `view` writes this fence last in its probe batch and reads
/// its reply as the signal that every earlier query has been answered too, so
/// a terminal that never sends it forces the child to wait out its whole
/// fallback deadline.
const DA1: Answer = (b"\x1b[c", b"\x1b[?1;2c");

/// The DECRQM request for synchronized output (mode 2026), and a reply
/// setting `Pm = 1` (permanently set). A child that sees this brackets each
/// frame in begin/end-sync escapes, which is real per-frame work a session
/// measuring frame cost must not silently omit.
const SYNC: Answer = (b"\x1b[?2026$p", b"\x1b[?2026;1$y");

/// The kitty keyboard protocol progressive-enhancement query, and a reply
/// reporting flags set. A child that sees this enables the protocol's
/// disambiguating key encoding.
const KITTY: Answer = (b"\x1b[?u", b"\x1b[?1u");

const DA1_ONLY: &[Answer] = &[DA1];
const FULL_TIER: &[Answer] = &[SYNC, KITTY, DA1];

impl QueryPolicy {
    /// The query/reply pairs a session under this policy answers.
    fn answers(self) -> &'static [Answer] {
        match self {
            Self::AnswerDa1 => DA1_ONLY,
            Self::AnswerFullTier => FULL_TIER,
            Self::Silent => &[],
        }
    }
}

/// Scans a child's output byte stream for terminal capability queries and
/// yields the bytes a real terminal would write back. Stateful across chunks:
/// a query straddling a read boundary is still recognized, because the tail
/// of one chunk is carried into the scan of the next.
struct QueryResponder {
    /// The trailing bytes of the last chunk that could still begin a query
    /// completed by the next one (bounded by the longest query minus one).
    tail: Vec<u8>,
    answers: &'static [Answer],
}

impl QueryResponder {
    fn new(answers: &'static [Answer]) -> Self {
        Self {
            tail: Vec::new(),
            answers,
        }
    }

    /// How far back a chunk boundary can cut a query this responder answers.
    fn longest_query(&self) -> usize {
        self.answers.iter().map(|(q, _)| q.len()).max().unwrap_or(1)
    }

    /// The replies (concatenated) for every query found in `chunk`, or empty
    /// if none. Carries an unmatched tail forward so a split query is caught.
    fn replies_for(&mut self, chunk: &[u8]) -> Vec<u8> {
        let mut scan = std::mem::take(&mut self.tail);
        scan.extend_from_slice(chunk);
        let mut out = Vec::new();
        let mut i = 0;
        let mut consumed = 0;
        while i < scan.len() {
            // longest match wins, so a table where one query is a prefix of
            // another cannot answer the short one and swallow the rest
            let matched = self
                .answers
                .iter()
                .filter(|(query, _)| scan[i..].starts_with(query))
                .max_by_key(|(query, _)| query.len());
            if let Some((query, reply)) = matched {
                out.extend_from_slice(reply);
                i += query.len();
                consumed = i;
            } else {
                i += 1;
            }
        }
        // keep only bytes past the last match that could still start a query,
        // so a query cut by this chunk's end completes against the next
        let keep_from = scan
            .len()
            .saturating_sub(self.longest_query() - 1)
            .max(consumed);
        self.tail = scan[keep_from..].to_vec();
        out
    }
}

/// Spawns a background thread that forwards every chunk read from `reader`
/// onto the returned channel, so the caller can poll with a bounded timeout
/// instead of blocking on a single `read` that may return only part of the
/// child's output. The thread also answers the child's terminal capability
/// queries through `writer`, standing in for the real terminal a child under
/// a pty would otherwise probe in vain.
fn spawn_reader(
    mut reader: Box<dyn Read + Send>,
    writer: &SharedWriter,
    policy: QueryPolicy,
) -> mpsc::Receiver<Vec<u8>> {
    let (tx, rx) = mpsc::channel();
    // replies leave on their own thread rather than being written from the
    // reader. Writing them here would hold the writer lock across a blocking
    // pty write, and this is the only thread draining the child: a write that
    // blocked (tty input queue full) would stop the drain, the child would
    // block writing, and neither side could ever make progress again.
    let answers = policy.answers();
    let reply_tx = if answers.is_empty() {
        None
    } else {
        let (reply_tx, reply_rx) = mpsc::channel::<Vec<u8>>();
        let writer = Arc::clone(writer);
        std::thread::spawn(move || {
            while let Ok(bytes) = reply_rx.recv() {
                // poison is recovered rather than propagated: what this
                // mutex guards is a `Box<dyn Write>` over a file
                // descriptor, which holds no invariant a panicking
                // writer could leave half-updated, and a silent responder
                // would present as the same unexplained hang a real
                // deadlock does
                let mut guard = writer.lock().unwrap_or_else(|e| e.into_inner());
                if guard.write_all(&bytes).is_err() || guard.flush().is_err() {
                    break;
                }
            }
        });
        Some(reply_tx)
    };
    std::thread::spawn(move || {
        let mut responder = QueryResponder::new(answers);
        let mut buf = [0_u8; 65536];
        loop {
            match reader.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if let Some(reply_tx) = &reply_tx {
                        let replies = responder.replies_for(&buf[..n]);
                        if !replies.is_empty() && reply_tx.send(replies).is_err() {
                            break;
                        }
                    }
                    if tx.send(buf[..n].to_vec()).is_err() {
                        break;
                    }
                }
            }
        }
    });
    rx
}

/// Detaches `cmd` from the host: drops every variable the host exports that
/// [`view_engine::env`]'s allowlist does not name, then drops every variable
/// that redirects where an editor finds config, runtime files, plugin
/// manifests or startup commands, and points the two search-path variables
/// at an empty directory, whose system-wide defaults would otherwise apply.
///
/// Applied to every pty spawn rather than left to each caller. Pointing the
/// four `XDG_*_HOME` variables at private directories, which the callers do
/// individually and for their own reasons, leaves these untouched, and a
/// session that reads one of them is indistinguishable from a correct one
/// until its results are compared against a machine where the variable is
/// not set. Nothing spawned through a pty here is a user's editor: these
/// are measured and asserted-on sessions, one and all, so there is no
/// caller for which inheriting the host's setup is the wanted behavior.
///
/// The allowlist leads because an enumeration of what to drop can only be
/// complete about the day it was written, and its incompleteness is silent:
/// an unenumerated variable reaches a child, changes what it loads or hands
/// it a credential the host happens to carry, and the child still starts and
/// still measures. The enumerations stay behind it because they also bind a
/// caller who sets one of those names deliberately, which the allowlist by
/// design does not.
///
/// # Errors
///
/// Returns [`OracleError::Io`] if the empty search path cannot be
/// established empty, which fails the spawn rather than pointing a session
/// at a directory somebody may have planted a `plugin/` script in.
fn hermetic_env(cmd: &mut CommandBuilder) -> Result<(), OracleError> {
    make_hermetic(cmd)
}

/// The environment writes a spawn needs for [`make_hermetic`] to reach it,
/// so one implementation of the rule covers a pty spawn and a plain one
/// alike.
///
/// Two spawn builders in this tree start editor processes and neither is a
/// superset of the other: [`CommandBuilder`] for anything hosted in a pty,
/// [`std::process::Command`] for a headless process with no terminal to
/// give it. A second copy of the rule for the second builder is a copy that
/// drifts, and the drift is silent -- a child inheriting one host variable
/// still runs, still measures, and only disagrees with the other class once
/// somebody compares two machines.
pub trait SpawnEnv {
    /// Unsets `name` for the spawned child.
    fn unset(&mut self, name: &OsStr);
    /// Sets `name` to `value` for the spawned child.
    fn set(&mut self, name: &OsStr, value: &OsStr);
    /// The value the child would see for `name` if the spawn happened now:
    /// what a caller set, or what the host exports and the builder is
    /// letting through, with no distinction drawn between the two.
    ///
    /// Deliberately not a "did the caller set this" query, which cannot mean
    /// one thing on both builders: [`CommandBuilder`] pre-populates its map
    /// with the entire host environment and answers for every host variable,
    /// while [`std::process::Command`] records overrides only and knows
    /// nothing about the rest. A trait method whose semantics turn on the
    /// implementation is the drift this trait exists to prevent.
    ///
    /// Owned rather than borrowed so a sweep can read a name and then act on
    /// the same builder.
    fn value_of(&self, name: &OsStr) -> Option<OsString>;
}

impl SpawnEnv for CommandBuilder {
    fn unset(&mut self, name: &OsStr) {
        self.env_remove(name);
    }
    fn set(&mut self, name: &OsStr, value: &OsStr) {
        self.env(name, value);
    }
    fn value_of(&self, name: &OsStr) -> Option<OsString> {
        self.get_env(name).map(OsStr::to_os_string)
    }
}

impl SpawnEnv for std::process::Command {
    fn unset(&mut self, name: &OsStr) {
        self.env_remove(name);
    }
    fn set(&mut self, name: &OsStr, value: &OsStr) {
        self.env(name, value);
    }
    fn value_of(&self, name: &OsStr) -> Option<OsString> {
        // this builder reports only what a caller pushed, so a name it says
        // nothing about is one the child inherits from the host as-is
        match self
            .get_envs()
            .find(|(known, _)| view_engine::env::env_names_eq(known, name))
        {
            Some((_, value)) => value.map(OsStr::to_os_string),
            None => std::env::var_os(name),
        }
    }
}

/// Applies the hermetic-environment rule to any spawn builder: see
/// [`hermetic_env`] for what it drops and why every editor process in this
/// tree passes through it.
///
/// A swept name is dropped only while the builder still holds the host's own
/// value for it, since a value that differs is what a caller's deliberate
/// override looks like on either builder. The hole in that test -- a caller
/// setting a name to exactly what the host already holds -- fails loudly:
/// the child simply does not receive the variable.
///
/// `EngineConfig::env_plan` applies the same layers in the same order but
/// decides the sweep by *name*, keeping anything a caller planned, because
/// it holds the caller's entries as a list and can tell. Neither builder
/// here can: `CommandBuilder` answers for every host variable and
/// `std::process::Command` for none. So the two funnels part company on
/// exactly one input -- a caller setting a swept name to the host's own
/// value, kept there and dropped here -- and this side takes the loud half
/// of that rather than guessing.
///
/// # Errors
///
/// Returns [`OracleError::Io`] if the empty search path cannot be
/// established empty.
pub fn make_hermetic<E: SpawnEnv>(cmd: &mut E) -> Result<(), OracleError> {
    for (name, host_value) in view_engine::env::hermetic_sweep() {
        if cmd.value_of(&name).as_deref() == Some(host_value.as_os_str()) {
            cmd.unset(&name);
        }
    }
    for name in view_engine::env::HOST_REDIRECT_VARS {
        cmd.unset(OsStr::new(name));
    }
    let empty = view_engine::env::prepare_empty_search_path()?;
    for name in view_engine::env::HOST_SEARCH_PATH_VARS {
        cmd.set(OsStr::new(name), empty.as_os_str());
    }
    let absent = view_engine::env::absent_config_file().into_os_string();
    for name in view_engine::env::HOST_SUBPROCESS_CONFIG_VARS {
        cmd.set(OsStr::new(name), absent.as_os_str());
    }
    let home = view_engine::env::prepare_hermetic_home()?;
    cmd.set(
        OsStr::new(view_engine::env::HERMETIC_HOME_VAR),
        home.as_os_str(),
    );
    Ok(())
}

/// A process running inside a real pty, with everything a test needs to
/// drive it and observe its screen: the child handle, a byte channel fed by
/// a background reader thread, and a `vt100` parser that turns those bytes
/// into a queryable screen.
pub struct PtySession {
    child: Box<dyn portable_pty::Child + Send + Sync>,
    rx: mpsc::Receiver<Vec<u8>>,
    writer: SharedWriter,
    parser: vt100::Parser,
    master: Box<dyn MasterPty + Send>,
    raw: Option<Vec<u8>>,
    // once the child has been reaped its pid can be recycled by the OS, so a
    // group kill aimed at that pid could hit an unrelated process; before
    // reaping the pid is held (live or zombie) and the group signal is safe
    reaped: bool,
}

/// Sends `SIGKILL` to the process group led by `pid`.
///
/// A pty child is spawned as a session leader (`setsid` runs before exec),
/// so its pid names the process group holding everything it spawned. A
/// kill aimed at the leader alone gives it no chance to reap its own
/// children (`view` SIGKILLed mid-run leaves its embedded `nvim --embed`
/// engine orphaned on init), while a group kill takes the whole tree down
/// in one signal.
///
/// The caller must not have reaped `pid` yet: a reaped pid can be recycled
/// and the signal would then land on an unrelated group.
///
/// On non-Unix platforms this is a no-op; process groups are a Unix
/// mechanism, and the per-child kill is the only lever there.
pub fn kill_process_group(pid: u32) {
    #[cfg(unix)]
    if let Ok(pid) = i32::try_from(pid) {
        let _ = nix::sys::signal::killpg(
            nix::unistd::Pid::from_raw(pid),
            nix::sys::signal::Signal::SIGKILL,
        );
    }
    #[cfg(not(unix))]
    let _ = pid;
}

/// How much of a child's raw output a recording session keeps.
///
/// Bounded rather than open-ended because the escape traffic worth asserting
/// on is what a child writes near its start -- its capability probe, the
/// modes it sets, its first frames -- while an editor driven by the flood row
/// writes megabytes no assertion has a use for.
const RAW_RECORD_LIMIT: usize = 256 * 1024;

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
    /// configured, so no caller can spawn a *pty session* that answers to
    /// the machine it runs on (see [`view_engine::env`], and
    /// [`hermetic_env`] for why this is the funnel that applies it).
    ///
    /// One editor process in this tree is started outside this funnel and
    /// outside `EngineConfig::isolated`'s: the `nvim --server ...
    /// --remote-expr` probe client in [`crate::compat`], which is a
    /// remote-control client rather than an editor session and performs no
    /// startup initialization to redirect. That exception is pinned by test
    /// (`the_probe_client_runs_none_of_the_hosts_startup_commands`), not
    /// left to this sentence.
    ///
    /// An editor with no pty to host it (the bench's headless control
    /// server) cannot come through here, and does not need an exception:
    /// it calls [`make_hermetic`] on its own builder, which is the same
    /// rule this funnel applies.
    ///
    /// # Errors
    ///
    /// Returns [`OracleError::Pty`] if the pty cannot be opened or the
    /// command fails to spawn, or [`OracleError::Io`] if the hermetic empty
    /// search path cannot be established (see [`hermetic_env`]).
    pub fn spawn_configured(
        cmd: CommandBuilder,
        cols: u16,
        rows: u16,
    ) -> Result<Self, OracleError> {
        Self::spawn_configured_with(cmd, cols, rows, QueryPolicy::AnswerDa1)
    }

    /// Like [`spawn_configured`](Self::spawn_configured), but chooses whether
    /// this pty answers the child's capability queries.
    ///
    /// Only a caller whose subject is the unanswered-probe path wants
    /// [`QueryPolicy::Silent`]; everything measuring or asserting ordinary
    /// behavior wants the default, since a real terminal answers.
    ///
    /// # Errors
    ///
    /// As [`spawn_configured`](Self::spawn_configured).
    pub fn spawn_configured_with(
        mut cmd: CommandBuilder,
        cols: u16,
        rows: u16,
        policy: QueryPolicy,
    ) -> Result<Self, OracleError> {
        hermetic_env(&mut cmd)?;
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
        let writer: SharedWriter = Arc::new(Mutex::new(
            pair.master
                .take_writer()
                .map_err(|e| OracleError::Pty(e.to_string()))?,
        ));
        // the slave fd must not outlive the child's own copy, or the master
        // never sees EOF once the child exits
        drop(pair.slave);

        let rx = spawn_reader(reader, &writer, policy);
        let parser = vt100::Parser::new(rows, cols, 0);

        Ok(Self {
            child,
            rx,
            writer,
            parser,
            master: pair.master,
            raw: None,
            reaped: false,
        })
    }

    /// Starts keeping the child's raw output bytes, up to
    /// [`RAW_RECORD_LIMIT`], alongside the parsed screen.
    ///
    /// The parsed screen cannot answer every question about a terminal
    /// child: a private mode it sets (synchronized output, the kitty
    /// keyboard protocol) leaves no cell to inspect, so an assertion about
    /// whether the child took that path has to read the bytes themselves.
    ///
    /// Recording happens where the caller drains, so it costs the reader
    /// thread nothing and costs an unrecording session nothing at all. The
    /// consequence is that it captures from the next drain onward: call this
    /// before [`screen`](Self::screen), [`with_screen`](Self::with_screen),
    /// [`wait_for`](Self::wait_for) or any other method that drains, or the
    /// bytes those already parsed are gone.
    pub fn record_raw_output(&mut self) {
        self.raw.get_or_insert_with(Vec::new);
    }

    /// Every recorded byte the child has written so far, or empty if
    /// [`record_raw_output`](Self::record_raw_output) was never called.
    #[must_use]
    pub fn raw_output(&mut self) -> &[u8] {
        self.drain_available();
        self.raw.as_deref().unwrap_or_default()
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
        // poison is recovered, not propagated: the guarded value is a
        // `Box<dyn Write>` over a file descriptor with no invariant a
        // panicking writer could break, and `OracleError` carries no variant
        // that would say anything more useful than the io error a genuinely
        // broken fd already returns below
        let mut guard = self.writer.lock().unwrap_or_else(|e| e.into_inner());
        guard.write_all(bytes)?;
        guard.flush()?;
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
            if let Some(raw) = &mut self.raw {
                let room = RAW_RECORD_LIMIT.saturating_sub(raw.len());
                raw.extend_from_slice(&chunk[..room.min(chunk.len())]);
            }
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
        let status = self.child.wait();
        if status.is_ok() {
            self.reaped = true;
        }
        status.map_err(Into::into)
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
                self.reaped = true;
                return Some(status);
            }
            if Instant::now() >= deadline {
                self.kill();
                if self.child.wait().is_ok() {
                    self.reaped = true;
                }
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
    ///
    /// On Unix the kill targets the child's whole process group, not just
    /// the child: see [`kill_process_group`] for why the leader alone is
    /// not enough and why the group signal is skipped once the child has
    /// been reaped.
    pub fn kill(&mut self) {
        if !self.reaped {
            if let Some(pid) = self.child.process_id() {
                kill_process_group(pid);
            }
        }
        let _ = self.child.kill();
    }
}

impl Drop for PtySession {
    /// Kills the child's process group and reaps the child, which nothing
    /// else guarantees: the writer is shared with the reply thread, so this
    /// session is no longer its sole owner and dropping it does not close
    /// the master or deliver the EOF that would end the child on its own.
    /// Without this, a caller that gives up early -- a failing assert, an
    /// early `?` -- leaks a live editor process and a reader thread parked
    /// on it for the rest of the test binary's run.
    fn drop(&mut self) {
        self.kill();
        let _ = self.child.wait();
    }
}

// The query responder is pure byte matching with no pty, no child and no
// platform surface, so it stays off the unix gate below: a Windows run gets
// the same coverage of the scanner that decides what this pty answers.
#[cfg(test)]
mod responder_tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    /// The probe batch `view-tui` writes, in the order it writes it.
    fn probe_batch() -> Vec<u8> {
        let mut batch = SYNC.0.to_vec();
        batch.extend_from_slice(KITTY.0);
        batch.extend_from_slice(DA1.0);
        batch
    }

    #[test]
    fn responder_answers_a_da1_query_in_one_chunk() {
        let mut r = QueryResponder::new(DA1_ONLY);
        assert_eq!(r.replies_for(DA1.0), DA1.1);
    }

    #[test]
    fn responder_answers_a_da1_query_split_across_two_chunks() {
        let mut r = QueryResponder::new(DA1_ONLY);
        let (head, tail) = DA1.0.split_at(2);
        assert!(r.replies_for(head).is_empty(), "no full query yet");
        assert_eq!(r.replies_for(tail), DA1.1);
    }

    #[test]
    fn responder_stays_silent_on_output_that_holds_no_query() {
        let mut r = QueryResponder::new(DA1_ONLY);
        assert!(r.replies_for(b"\x1b[1mbold\x1b[0m normal text").is_empty());
    }

    #[test]
    fn responder_answers_each_of_two_queries_in_one_chunk() {
        let mut r = QueryResponder::new(DA1_ONLY);
        let mut two = DA1.0.to_vec();
        two.extend_from_slice(DA1.0);
        let mut both = DA1.1.to_vec();
        both.extend_from_slice(DA1.1);
        assert_eq!(r.replies_for(&two), both);
    }

    #[test]
    fn a_da1_only_responder_leaves_the_optional_capabilities_unresolved() {
        let mut r = QueryResponder::new(DA1_ONLY);
        assert_eq!(r.replies_for(&probe_batch()), DA1.1);
    }

    #[test]
    fn a_full_tier_responder_answers_the_whole_batch_in_query_order() {
        let mut r = QueryResponder::new(FULL_TIER);
        let mut expected = SYNC.1.to_vec();
        expected.extend_from_slice(KITTY.1);
        expected.extend_from_slice(DA1.1);
        assert_eq!(r.replies_for(&probe_batch()), expected);
    }

    // The sync query is the longest in the table, so it is the one a chunk
    // boundary can cut furthest from its start; a tail sized for DA1 alone
    // would drop it and the child would derive the wrong tier.
    #[test]
    fn a_full_tier_responder_rejoins_a_sync_query_cut_anywhere() {
        for split in 1..SYNC.0.len() {
            let mut r = QueryResponder::new(FULL_TIER);
            let (head, tail) = SYNC.0.split_at(split);
            let first = r.replies_for(head);
            let mut got = first;
            got.extend_from_slice(&r.replies_for(tail));
            assert_eq!(got, SYNC.1, "split after {split} bytes");
        }
    }

    #[test]
    fn a_silent_policy_answers_nothing_it_would_otherwise_answer() {
        assert!(QueryPolicy::Silent.answers().is_empty());
        let mut r = QueryResponder::new(QueryPolicy::Silent.answers());
        assert!(r.replies_for(&probe_batch()).is_empty());
    }
}

// Every test here spawns /bin/* or nvim inside a real pty; view's Windows
// terminal runtime is a tier-2 surface validated on winserver rather than in
// CI, so these unix-fixture tests are gated off the Windows build.
#[cfg(all(test, unix))]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;
    use std::path::{Path, PathBuf};

    use crate::testenv;

    #[test]
    fn spawn_and_send_shows_typed_output_on_screen() {
        let mut session = testenv::spawning(|| PtySession::spawn("/bin/cat", &[], 80, 24)).unwrap();
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
    fn dropping_a_session_kills_the_grandchildren_its_child_spawned() {
        // /bin/sh stands in for `view` and its background `sleep` for the
        // `nvim --embed` engine: a kill that reaches only the group leader
        // leaves the sleep orphaned on init, which is the exact leak shape
        // the group kill exists to prevent. The grandchild ignores SIGHUP
        // because the real engine survives the hangup the closing pty
        // master delivers; without that, the hangup alone would reap it
        // here and hide a missing group kill.
        let mut session = testenv::spawning(|| {
            let mut cmd = CommandBuilder::new("/bin/sh");
            cmd.arg("-c");
            cmd.arg("(trap '' HUP; exec sleep 300) & echo spawned-grandchild; wait");
            PtySession::spawn_configured(cmd, 80, 24)
        })
        .unwrap();
        assert!(
            session.wait_for("spawned-grandchild", Duration::from_secs(5)),
            "the shell never confirmed its background child; screen:\n{}",
            session.screen()
        );
        let leader = session.pid().unwrap();
        drop(session);

        // signal 0 probes for existence: the group is fully gone only once
        // every member (leader and grandchild alike) has been killed and
        // reaped, and init reaps a SIGKILLed orphan promptly. 30s, not 5s:
        // this waits out a two-step reap chain (the leader's own kill/reap
        // plus init's independent re-parented reap of the grandchild), and a
        // host under concurrent build load can starve the scheduler enough
        // to blow a much smaller wall-clock budget with no leak on view's
        // side at all -- 15s alone was still observed failing on this
        // exact host's own chronic baseline contention (load average above
        // its core count even with no concurrent build running at all),
        // and this poll costs nothing extra to wait out since it is a
        // correctness check, never a timed perf assertion
        let group = nix::unistd::Pid::from_raw(i32::try_from(leader).unwrap());
        let deadline = Instant::now() + Duration::from_secs(30);
        while nix::sys::signal::killpg(group, None::<nix::sys::signal::Signal>)
            != Err(nix::errno::Errno::ESRCH)
        {
            assert!(
                Instant::now() < deadline,
                "the child's process group survived the session drop"
            );
            std::thread::sleep(Duration::from_millis(25));
        }
    }

    #[test]
    fn wait_for_returns_true_immediately_when_the_needle_is_already_on_screen() {
        let mut session =
            testenv::spawning(|| PtySession::spawn("/bin/echo", &["already-there"], 80, 24))
                .unwrap();
        // no send(): the text is already on screen from the process's own
        // startup output, the current-state-check-first path wait_for must
        // take rather than blocking for a chunk that may never arrive again
        assert!(session.wait_for("already-there", Duration::from_secs(5)));
    }

    #[test]
    fn wait_for_times_out_on_a_needle_that_never_appears() {
        let mut session =
            testenv::spawning(|| PtySession::spawn("/bin/echo", &["hi"], 80, 24)).unwrap();
        assert!(!session.wait_for("this-never-appears", Duration::from_millis(200)));
    }

    #[test]
    fn answers_a_childs_da1_query_the_way_a_real_terminal_does() {
        // A child probing terminal capabilities writes the DA1 fence and
        // blocks reading its reply; a real terminal answers within
        // milliseconds. If this pty stays silent, a probe waits out its whole
        // fallback deadline instead -- the cost that inflated first_paint's
        // cold_ms by the probe's 50 ms safety net (see view-tui `tiers`).
        //
        // The child records the exact reply bytes to a scratch file under the
        // build tree (never the system temp dir, matching the other pty
        // tests). `head -c7` returns only once all seven reply bytes arrive,
        // so a written file means the whole reply landed; raw mode lets a
        // reply with no line terminator reach the child at all.
        let mut out = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        out.pop(); // crates/
        out.pop(); // workspace root
        let out = out.join("target").join("view-pty-da1-reply.hex");
        let _ = std::fs::remove_file(&out);
        let script = format!(
            "stty raw -echo; printf '\\033[c'; head -c7 | od -An -tx1 | tr -d ' \\n' > '{}'",
            out.display()
        );
        let mut session = testenv::spawning(|| {
            let mut cmd = CommandBuilder::new("/bin/sh");
            cmd.arg("-c");
            cmd.arg(&script);
            PtySession::spawn_configured(cmd, 80, 24)
        })
        .unwrap();

        let deadline = Instant::now() + Duration::from_secs(5);
        let hex = loop {
            if let Ok(s) = std::fs::read_to_string(&out) {
                if s.trim().len() >= 14 {
                    break s;
                }
            }
            assert!(
                Instant::now() < deadline,
                "child never received a DA1 reply; screen:\n{}",
                session.screen()
            );
            std::thread::sleep(Duration::from_millis(25));
        };
        session.kill();
        let _ = session.wait_for_exit(Duration::from_secs(2));
        let _ = std::fs::remove_file(&out);

        // exactly the bytes of `\x1b[?1;2c` -- a private CSI ending in `c`,
        // which view's probe reads as the DA1 fence and nothing more (sync
        // and kitty stay unanswered, so resolved caps are unchanged).
        assert_eq!(hex.trim(), "1b5b3f313b3263", "unexpected DA1 reply bytes");
    }

    #[test]
    fn a_silent_policy_leaves_a_childs_da1_query_unanswered() {
        // The disconfirming half of the test above: a test asking for the
        // unanswered-probe path has no way to tell "the pty stayed mute" from
        // "the pty answered and this scenario never happened" unless silence is
        // itself pinned. The final send is the positive control -- proof the
        // child was still parked on the read the whole time rather than dead,
        // which would produce the same empty file.
        let mut out = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        out.pop(); // crates/
        out.pop(); // workspace root
        let out = out.join("target").join("view-pty-da1-silent.hex");
        let _ = std::fs::remove_file(&out);
        let script = format!(
            "stty raw -echo; printf '\\033[c'; head -c7 | od -An -tx1 | tr -d ' \\n' > '{}'",
            out.display()
        );
        let mut session = testenv::spawning(|| {
            let mut cmd = CommandBuilder::new("/bin/sh");
            cmd.arg("-c");
            cmd.arg(&script);
            PtySession::spawn_configured_with(cmd, 80, 24, QueryPolicy::Silent)
        })
        .unwrap();

        // Comfortably past the millisecond-scale round trip the answering
        // policy takes, and past view's own 50ms probe deadline, so a reply
        // merely being slow cannot be mistaken for one never coming.
        std::thread::sleep(Duration::from_millis(500));
        let quiet = std::fs::read_to_string(&out).unwrap_or_default();
        assert!(
            quiet.trim().is_empty(),
            "silent policy still answered the DA1 query with {quiet:?}"
        );

        session.send(DA1.1).unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if std::fs::read_to_string(&out).is_ok_and(|s| s.trim().len() >= 14) {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "child never consumed a hand-written reply, so the empty file \
                 above proves nothing about the policy; screen:\n{}",
                session.screen()
            );
            std::thread::sleep(Duration::from_millis(25));
        }
        session.kill();
        let _ = session.wait_for_exit(Duration::from_secs(2));
        let _ = std::fs::remove_file(&out);
    }

    /// A name on none of the lists in [`view_engine::env`]: not a redirect,
    /// not a search path, not allowlisted. It stands for every variable a
    /// host exports that nobody enumerated -- a CI credential, a preload
    /// hook, some other tool's configuration -- which is what the allowlist
    /// catches and no enumeration of what to drop ever can.
    const UNENUMERATED: &str = "VIEW_PTY_UNENUMERATED_HOST_VAR";

    /// A name the allowlist's `LC_` prefix admits, standing for the other
    /// side of the same rule: a sweep that simply emptied the environment
    /// would satisfy every assertion about the name above while handing
    /// children an environment nothing can run in.
    const PASSTHROUGH: &str = "LC_VIEW_PTY_PASSTHROUGH";

    /// A name the host exports *and* a caller then overrides on the builder.
    /// It is the one shape that tells the sweep's rule -- drop a name the
    /// builder still holds the host's own value for -- apart from an
    /// unconditional drop, and the shape every measurement uses to hand its
    /// child a fixture directory, a tap descriptor or a probe socket.
    const OVERRIDDEN: &str = "VIEW_PTY_CALLER_OVERRIDE";

    /// The value the planted names carry, distinct from the caller's preset
    /// so a failing screen names which channel actually leaked.
    const HOST_VALUE: &str = "host-exported-value";

    /// What a caller sets [`OVERRIDDEN`] to, over the host's own value.
    const CALLER_VALUE: &str = "caller-chose-this";

    /// The names [`env_report`] and its plain-`Command` counterpart plant in
    /// this process's environment, one per rule the funnel applies.
    fn planted_vars(preset: &str) -> [(&str, &str); 5] {
        [
            ("VIMINIT", preset),
            ("XDG_CONFIG_DIRS", preset),
            (UNENUMERATED, HOST_VALUE),
            (PASSTHROUGH, HOST_VALUE),
            (OVERRIDDEN, HOST_VALUE),
        ]
    }

    /// The shell command both report helpers run: one labelled line per
    /// planted name, `XDG_CONFIG_DIRS` last so its arrival proves the
    /// earlier lines are final.
    fn report_command() -> String {
        format!(
            r#"echo "VIMINIT=[${{VIMINIT-unset}}]"; \
               echo "UNENUMERATED=[${{{UNENUMERATED}-unset}}]"; \
               echo "PASSTHROUGH=[${{{PASSTHROUGH}-unset}}]"; \
               echo "OVERRIDDEN=[${{{OVERRIDDEN}-unset}}]"; \
               echo "DIRS=[${{XDG_CONFIG_DIRS-unset}}]""#
        )
    }

    /// What every spawn through the funnel must hand its child, whichever
    /// builder it went through.
    fn assert_hermetic_report(report: &str, preset: &str) {
        assert!(
            report.contains("VIMINIT=[unset]"),
            "the host's startup commands reached the child; report:\n{report}"
        );
        assert!(
            !report.contains(preset),
            "the host's config search path reached the child; report:\n{report}"
        );
        assert!(
            report.contains("UNENUMERATED=[unset]"),
            "a variable on none of the hermetic lists reached the child, so \
             the funnel guards only against what somebody remembered to write \
             down and every measured session carries whatever the host \
             exports; report:\n{report}"
        );
        assert!(
            report.contains(&format!("PASSTHROUGH=[{HOST_VALUE}]")),
            "the sweep dropped a name the allowlist names, so it is an \
             emptied environment with extra steps and the assertion above \
             says nothing about selectivity; report:\n{report}"
        );
        assert!(
            report.contains(&format!("OVERRIDDEN=[{CALLER_VALUE}]")),
            "the sweep took a variable the caller set deliberately, so a \
             measurement cannot deliver its child a fixture at all; \
             report:\n{report}"
        );
    }

    /// Reports back what a spawned child sees in the five planted variables,
    /// one per rule the funnel applies.
    ///
    /// `sh` rather than the `Command` builder's own accessors: the question
    /// is what a real child's environment holds after the spawn, and a
    /// builder that recorded the right plan but handed the child something
    /// else would satisfy every accessor while still leaking.
    ///
    /// The variables are planted in *this process's* environment rather than
    /// through `CommandBuilder::env`. The property under test is inheritance,
    /// and a builder-set value only stands in for an inherited one while the
    /// pty layer happens to seed its map from `std::env::vars_os()`, which is
    /// its implementation's choice to change. It is also the distinction the
    /// sweep itself turns on, so a builder-set plant would be skipped as a
    /// caller override and prove the opposite of what it was written for. The
    /// plant is process-wide while it stands, so it is made through
    /// [`crate::testenv::plant`], which excludes every other spawn in this
    /// binary for its duration and puts the names back afterwards.
    fn env_report(preset: &str) -> String {
        let planted = testenv::plant(&planted_vars(preset));
        let mut session = planted
            .spawning(|| {
                let mut cmd = CommandBuilder::new("/bin/sh");
                cmd.arg("-c");
                cmd.arg(report_command());
                cmd.env(OVERRIDDEN, CALLER_VALUE);
                PtySession::spawn_configured(cmd, 200, 24)
            })
            .unwrap();
        // the child holds its own copy of the environment from here on, so
        // the plant is released before the wait rather than blocking every
        // other test's spawn for the length of it
        drop(planted);
        // the last line's arrival proves the earlier ones are final, so an
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
        assert_hermetic_report(&screen, planted);
        assert!(
            screen.contains(&format!(
                "DIRS=[{}]",
                view_engine::env::empty_search_path().display()
            )),
            "the child searches somewhere other than the empty directory; screen:\n{screen}"
        );
    }

    /// The plain-[`std::process::Command`] half of the same funnel, which no
    /// pty test reaches: the headless editors in this tree spawn that way,
    /// and a rule holding on one builder but not the other would leave the
    /// two arms of a comparison measuring differently-configured children.
    #[test]
    fn a_plain_command_spawn_obeys_the_same_rule_a_pty_spawn_does() {
        let planted_value = "/host/nvim/config";
        let planted = testenv::plant(&planted_vars(planted_value));
        let output = planted
            .spawning(|| {
                let mut cmd = std::process::Command::new("/bin/sh");
                cmd.arg("-c").arg(report_command());
                cmd.env(OVERRIDDEN, CALLER_VALUE);
                make_hermetic(&mut cmd).unwrap();
                cmd.output()
            })
            .unwrap();
        drop(planted);
        let report = String::from_utf8_lossy(&output.stdout).into_owned();
        assert_hermetic_report(&report, planted_value);
        assert!(
            report.contains(&format!(
                "DIRS=[{}]",
                view_engine::env::empty_search_path().display()
            )),
            "the child searches somewhere other than the empty directory; report:\n{report}"
        );
    }

    /// A `HOME` of this file's own holding a `.gitconfig` that rewrites
    /// every GitHub fetch to a host that cannot resolve.
    ///
    /// The rewrite is the payload because it is the consequence, not the
    /// mechanism: it reports whether an operator's configuration reached the
    /// programs a hermetic child runs, and says so without consulting a
    /// variable name or anything the funnel does. A fixture that installs
    /// plugins clones from inside such a child, so this is the live shape of
    /// the channel, not an invented one.
    struct HostileHome {
        dir: PathBuf,
    }

    impl HostileHome {
        /// The prefix the planted rewrite matches, which is where a plugin
        /// fixture's clones actually go.
        const REWRITTEN_FROM: &'static str = "https://github.com/";

        /// The rewritten-to host, which no name server resolves.
        const REWRITTEN_TO: &'static str = "https://view-test.invalid/";

        /// `label` keeps each test's home its own: planting begins with a
        /// `remove_dir_all`, which under a shared path would delete another
        /// test's planted home mid-assertion.
        fn plant(label: &str) -> Self {
            let mut root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
            root.pop(); // crates/
            root.pop(); // workspace root
                        // under the build tree, never the system temp dir: this is the
                        // host configuration a leak would read, so it must not be
                        // somewhere an unrelated process can reach into
            let dir = root
                .join("target")
                .join(format!("view-hostile-home-{label}"));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(
                dir.join(".gitconfig"),
                format!(
                    "[url \"{}\"]\n\tinsteadOf = {}\n",
                    Self::REWRITTEN_TO,
                    Self::REWRITTEN_FROM
                ),
            )
            .unwrap();
            Self { dir }
        }

        /// What `git` reports for the planted rewrite, run through `build`.
        fn rewrite_seen_by(&self, build: impl FnOnce(&mut std::process::Command)) -> String {
            let mut cmd = std::process::Command::new("git");
            cmd.arg("config")
                .arg("--get")
                .arg(format!("url.{}.insteadOf", Self::REWRITTEN_TO));
            // run from inside the planted home with repository discovery
            // ceilinged at its parent: this test binary's cwd is a checkout
            // of a real repository, whose own .git/config is a layer no
            // funnel variable diverts, and a stray rewrite there would fail
            // the guarded assertion for a reason unrelated to hermeticity
            cmd.current_dir(&self.dir);
            cmd.env("GIT_CEILING_DIRECTORIES", self.dir.parent().unwrap());
            build(&mut cmd);
            let out = cmd.output().expect("failed to run git");
            String::from_utf8_lossy(&out.stdout).trim().to_string()
        }
    }

    /// The programs a hermetic child spawns resolve their configuration
    /// through a home the editor's `XDG_*_HOME` overrides do nothing about.
    /// An operator's `insteadOf` rewrite, proxy, disabled certificate check
    /// or credential helper would otherwise decide what a measured plugin
    /// bootstrap fetches and from where, and report nothing: the clone
    /// succeeds and the run is green. The funnel closes the channel twice
    /// over -- the configuration files are diverted by name and `HOME`
    /// itself is re-pointed -- and this oracle checks the channel rather
    /// than either layer, staying green while at least one closure holds;
    /// the plan tests pin each layer individually.
    ///
    /// Gated off Windows with this module's other real-subprocess fixtures.
    #[test]
    fn a_hermetic_childs_own_subprocesses_read_none_of_the_hosts_configuration() {
        let home = HostileHome::plant("rewrite");
        let planted = testenv::plant(&[("HOME", &*home.dir.to_string_lossy())]);

        // the control: the same plant reaching a spawn no funnel touched.
        // Without it a silent subject below would equally describe a
        // .gitconfig git never read, a key it does not recognize, or a git
        // that failed to run at all
        let uncontrolled = planted.spawning(|| home.rewrite_seen_by(|_| {}));
        assert_eq!(
            uncontrolled,
            HostileHome::REWRITTEN_FROM,
            "the planted host configuration reached no subprocess even \
             unguarded, so this test proves nothing about a guarded one"
        );

        let guarded = planted.spawning(|| {
            home.rewrite_seen_by(|cmd| {
                make_hermetic(cmd).unwrap();
            })
        });
        drop(planted);
        assert!(
            guarded.is_empty(),
            "a hermetic child's own git still answers to the operator's \
             configuration ({guarded}), so which repository a measured \
             bootstrap clones, over what transport and with whose \
             credentials, are all properties of the machine it ran on"
        );
    }

    /// The credential files a child's subprocesses resolve through `HOME`
    /// sit below the configuration layer the two `GIT_CONFIG_*` overrides
    /// divert: no variable of git's or curl's turns off the `$HOME/.netrc`
    /// lookup git's http transport hands to libcurl, so an operator's
    /// credentials ride any allowlisted host `HOME` into every fetch a
    /// measured bootstrap makes -- and an authenticated fetch succeeding
    /// where an anonymous one would fail is a fixture green on one machine
    /// only, silently.
    ///
    /// The oracle reads the credential file exactly the way such a
    /// subprocess would, through the `HOME` the child actually received, so
    /// it goes red if the funnel stops re-pointing `HOME` and is
    /// indifferent to how the funnel does it.
    #[test]
    fn a_hermetic_childs_home_holds_none_of_the_operators_credentials() {
        let home = HostileHome::plant("netrc");
        let creds = "machine github.com login operator password host-secret";
        std::fs::write(home.dir.join(".netrc"), creds).unwrap();
        let planted = testenv::plant(&[("HOME", &*home.dir.to_string_lossy())]);

        let netrc_read = |build: fn(&mut std::process::Command)| {
            let mut cmd = std::process::Command::new("/bin/sh");
            cmd.arg("-c").arg("cat -- \"$HOME/.netrc\" 2>/dev/null");
            build(&mut cmd);
            let out = cmd.output().expect("failed to run sh");
            String::from_utf8_lossy(&out.stdout).into_owned()
        };

        // the control: the plant reaching a spawn no funnel touched. An
        // empty guarded read below otherwise equally describes a file that
        // was never readable at all
        let uncontrolled = planted.spawning(|| netrc_read(|_| {}));
        assert_eq!(
            uncontrolled.trim(),
            creds,
            "the planted credential file reached no subprocess even \
             unguarded, so this test proves nothing about a guarded one"
        );

        let guarded = planted.spawning(|| {
            netrc_read(|cmd| {
                make_hermetic(cmd).unwrap();
            })
        });
        drop(planted);
        assert!(
            guarded.is_empty(),
            "a hermetic child's HOME still reaches the operator's credential \
             file ({guarded:?}), so every fetch a measured bootstrap makes \
             carries the operator's credentials"
        );
    }

    /// The funnel must *run* the preparation of both directories it points
    /// a child at, not merely name them: the preparation is where the plant
    /// and emptiness refusals live, so a funnel that stopped calling either
    /// would keep every value-shaped assertion green while never refusing
    /// anything. A plant in each directory that the funnel then refuses by
    /// name is the observation nothing but the preparation can produce.
    ///
    /// Plants rather than removal: the two directories are shared with
    /// every live child in this binary, and a child that loses its home
    /// mid-run drops its fallback log wherever it can -- an entry the next
    /// preparation then refuses suite-wide. A single planted file disturbs
    /// no live child (children write only their tolerated state), and the
    /// environment lock's exclusive side keeps any concurrent funnel from
    /// meeting it. Both plants are removed and the funnel re-run clean
    /// before the lock is released, so no later spawn inherits them.
    #[test]
    fn the_funnel_refuses_a_plant_in_either_directory_it_points_a_child_at() {
        let planted = testenv::plant(&[]);
        let home = view_engine::env::hermetic_home();
        let empty = view_engine::env::empty_search_path();

        std::fs::create_dir_all(&home).unwrap();
        std::fs::write(home.join(".netrc"), "machine github.com").unwrap();
        let mut cmd = std::process::Command::new("true");
        let refused = make_hermetic(&mut cmd);
        let _ = std::fs::remove_file(home.join(".netrc"));
        assert!(
            refused.is_err_and(|err| err.to_string().contains(".netrc")),
            "the funnel accepted a hermetic home holding a planted \
             credential file, so its plant refusal never ran"
        );

        std::fs::create_dir_all(&empty).unwrap();
        // the preparation hardened this directory read-only on unix, and
        // planting through that is exactly what an unprivileged run cannot
        // do; restored below by the clean re-run's own preparation
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&empty, std::fs::Permissions::from_mode(0o700));
        }
        std::fs::write(empty.join("planted"), "-- planted").unwrap();
        let mut cmd = std::process::Command::new("true");
        let refused = make_hermetic(&mut cmd);
        let _ = std::fs::remove_file(empty.join("planted"));
        // "hermetic search path", not "planted": the home refusal's prose
        // also says "planted", so only the search-path phrase proves which
        // refusal fired
        assert!(
            refused.is_err_and(|err| err.to_string().contains("hermetic search path")),
            "the funnel accepted a search path holding a planted entry, so \
             its emptiness refusal never ran"
        );

        // both directories verified clean and re-hardened before any other
        // spawn can reach them
        let mut cmd = std::process::Command::new("true");
        make_hermetic(&mut cmd).unwrap();
        drop(planted);
    }

    /// A scratch world holding one plugin planted under each of the two
    /// layouts the search-path variables feed into 'runtimepath'
    /// (`$XDG_CONFIG_DIRS/nvim/plugin/` and
    /// `$XDG_DATA_DIRS/nvim/site/plugin/`), each writing a marker file when
    /// an editor sources it.
    ///
    /// The marker is what makes this an oracle rather than a restatement of
    /// the code under test: it reports whether a child *executed* a host
    /// plugin, which is the consequence the two variables exist to prevent,
    /// and it says so without consulting either the variable names or the
    /// replacement path the funnel uses.
    struct SearchPathWorld {
        config_dirs: PathBuf,
        data_dirs: PathBuf,
        markers: [PathBuf; 2],
    }

    impl SearchPathWorld {
        fn plant() -> Self {
            let mut root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
            root.pop(); // crates/
            root.pop(); // workspace root
                        // under the build tree, never the system temp dir: this world is
                        // the host config a leak would source, so it must not be
                        // somewhere an unrelated process can reach into
            let root = root.join("target").join("view-pty-search-path");
            let _ = std::fs::remove_dir_all(&root);
            let world = Self {
                config_dirs: root.join("config"),
                data_dirs: root.join("data"),
                markers: [root.join("config-marker"), root.join("data-marker")],
            };
            let plugins = [
                world.config_dirs.join("nvim/plugin"),
                world.data_dirs.join("nvim/site/plugin"),
            ];
            for (dir, marker) in plugins.iter().zip(&world.markers) {
                std::fs::create_dir_all(dir).unwrap();
                std::fs::write(
                    dir.join("leak.lua"),
                    format!("vim.fn.writefile({{'sourced'}}, '{}')\n", marker.display()),
                )
                .unwrap();
            }
            world
        }

        fn sourced(&self) -> Vec<&Path> {
            self.markers
                .iter()
                .filter(|marker| marker.exists())
                .map(PathBuf::as_path)
                .collect()
        }

        fn forget_what_was_sourced(&self) {
            for marker in &self.markers {
                let _ = std::fs::remove_file(marker);
            }
        }

        /// The editor arguments both spawns below use: `--clean` (which
        /// drops the *user* directories and not these), no swap file, and an
        /// immediate exit, since a plugin under either layout has already
        /// run by the time the first `-c` command does.
        const ARGS: [&'static str; 5] = ["--clean", "-n", "--headless", "-c", "qa!"];
    }

    #[test]
    fn a_pty_spawn_sources_no_plugin_from_the_hosts_search_path() {
        let world = SearchPathWorld::plant();

        // control first: the same planted world, reaching an editor that
        // never passes the funnel. Without it a green assertion below would
        // equally describe a plant that never worked, a layout Neovim
        // stopped reading, or an editor that failed to start.
        //
        // This is the one child in this binary that no funnel neutralizes,
        // so it is the one a concurrent environment plant would reach: it
        // takes the shared side of the same lock that plant holds
        // exclusively.
        //
        // Spawned and waited on separately, rather than through `status()`:
        // only the spawn itself reads the environment, and holding the lock
        // across the editor's whole run would stall an unrelated plant for
        // no gain.
        let mut control = testenv::spawning(|| {
            std::process::Command::new("nvim")
                .args(SearchPathWorld::ARGS)
                .env("XDG_CONFIG_DIRS", &world.config_dirs)
                .env("XDG_DATA_DIRS", &world.data_dirs)
                .spawn()
        })
        .unwrap();
        let control = control.wait().unwrap();
        assert!(control.success(), "the control editor failed to run");
        assert_eq!(
            world.sourced().len(),
            2,
            "the planted plugins never ran even unguarded, so this test can \
             prove nothing about a guarded spawn; sourced {:?}",
            world.sourced()
        );
        world.forget_what_was_sourced();

        let mut session = testenv::spawning(|| {
            let mut cmd = CommandBuilder::new("nvim");
            for arg in SearchPathWorld::ARGS {
                cmd.arg(arg);
            }
            cmd.env("XDG_CONFIG_DIRS", &world.config_dirs);
            cmd.env("XDG_DATA_DIRS", &world.data_dirs);
            PtySession::spawn_configured(cmd, 80, 24)
        })
        .unwrap();
        assert!(
            session.wait_for_exit(Duration::from_secs(30)).is_some(),
            "the guarded editor never exited; screen:\n{}",
            session.screen()
        );
        assert!(
            world.sourced().is_empty(),
            "{:?} ran inside a spawn that is supposed to search nothing of \
             the host's, so every measured session executes whatever the \
             machine has installed system-wide",
            world.sourced()
        );
    }
}
