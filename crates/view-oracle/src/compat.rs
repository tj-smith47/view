//! The compat harness's pty-level driver: domain types for a scenario's
//! steps ([`Step`], [`PluginClass`], [`ScenarioState`]) plus [`CompatSession`],
//! which drives them against a real `view` process over a pty (via
//! [`PtySession`]) and answers `probe`/zero-error queries over a *second*,
//! independent channel.
//!
//! Serde-free by design, matching this crate's own `rmpv`-free rule (see the
//! crate root's module docs): `view-harness`'s `scenario` module owns the
//! TOML schema and hands this module already-validated [`Step`] values, the
//! same layering `corpus::CorpusEntry` uses for the differential oracle.
//!
//! # The probe channel
//!
//! A pty's rendered screen (what [`PtySession::screen`] exposes) is not a
//! channel a `probe` step or the implicit zero-error epilogue can query: it
//! is a human-shaped rendering, not a value. Scraping it with `:echo
//! luaeval(...)` and pattern-matching the redrawn text was considered and
//! rejected -- it collides with whatever plugin UI a scenario is actually
//! testing, and breaks on overlays, truncation, and timing, which is
//! exactly the fragility this harness exists to catch, not to inherit.
//!
//! Instead, each compat fixture's `init.lua` calls
//! `vim.fn.serverstart(vim.env.VIEW_COMPAT_SOCK)`, opening a second RPC
//! channel nvim itself exposes; [`CompatSession::probe`] and
//! [`CompatSession::zero_error_check`] act as the *client* of that channel
//! by shelling out to the pinned `nvim` binary itself: `nvim --server $SOCK
//! --remote-expr '<expr>'`. This is zero new `view` surface and zero new RPC
//! code -- nvim is both the server (already required) and the client
//! (already pinned) -- at the cost of one subprocess per probe. A
//! fixture-less scenario (no committed `init.lua` to carry the
//! `serverstart` call) has [`CompatSession::prime_probe_channel`] type the
//! equivalent command into the pty itself instead.
//!
//! Every probe subprocess is spawned with an explicit bounded wait
//! ([`wait_with_timeout`]), not `Child::wait`'s unbounded block: the
//! embed-channel `nvim_eval` wedge a hit-enter prompt can cause (tracked,
//! not fixed, in `corpus/quarantine/fuzz-42-6.toml`) is a different RPC path
//! than this one, but the same underlying nvim process can still leave an
//! `--remote-expr` client's request unanswered if it is showing a blocking
//! prompt -- this channel existing at all does not itself immunize a probe
//! call from that, so it must fail loud on its own deadline rather than
//! hang the whole compat run.

use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use portable_pty::CommandBuilder;

use crate::pty::PtySession;
use crate::OracleError;

/// Bound on how long a single probe subprocess (`nvim --server ... --remote-expr`)
/// is allowed to run before [`wait_with_timeout`] kills it. Generous
/// relative to a normal probe's near-instant reply (confirmed live: a clean
/// `nvim --server $SOCK --remote-expr` round-trip completes in well under a
/// second), short enough that a wedged target still fails a scenario
/// promptly instead of hanging the whole compat run.
const PROBE_TIMEOUT: Duration = Duration::from_secs(10);

/// Which of the design spec's three config-reconciliation classes a
/// scenario's plugin belongs to. Purely descriptive at this schema layer --
/// no class-specific driving logic exists yet -- but recorded per scenario
/// since the compat-evidence page's own row schema reports it, and a future
/// coverage model (top-N by class) groups by it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PluginClass {
    /// No UI ownership (treesitter, LSP servers, cmp sources, surround,
    /// gitsigns data): by construction, compat by definition.
    Semantic,
    /// Draws inside the grid but owns no surface `view` itself renders
    /// natively (telescope, which-key, floating plugins): must coexist
    /// untouched.
    UiAdjacent,
    /// Occupies a surface `view` renders natively (lualine/statusline,
    /// noice, nvim-notify, tree sidebars): native wins by default, per
    /// the design spec's supersession policy.
    UiOwning,
}

/// A scenario's `state` field. Only [`Self::Present`] is accepted today:
/// the design spec names three config-reconciliation states (superseded,
/// deferred, native-without-plugin), but the supersession machinery the
/// other two need does not exist in the engine yet -- a scenario naming
/// them today would silently assert against a mechanism that is not built,
/// so the schema loader rejects them outright rather than accept and no-op.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScenarioState {
    /// The plugin is present and loaded; no supersession assertion is made.
    Present,
}

/// One scripted action a scenario drives, in order. Each variant maps to
/// exactly one primitive on [`CompatSession`] or the underlying
/// [`PtySession`] -- there is no control flow (loop, conditional) a step can
/// express, by design: see this crate's compat-harness module docs (and the
/// design brief's own "rejected: imperative Lua" note) for why a fixed,
/// introspectable record beats a scripting language here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Step {
    /// Types `keys` into the pty verbatim, as a user would.
    Send(String),
    /// Blocks until the pty's rendered screen contains `needle`, or `timeout`
    /// elapses.
    WaitFor { needle: String, timeout: Duration },
    /// Blocks until the single cell at `(row, col)` holds exactly `expected`,
    /// or `timeout` elapses.
    WaitForCell {
        row: u16,
        col: u16,
        expected: String,
        timeout: Duration,
    },
    /// Fails the scenario if `needle` is present anywhere in the current
    /// screen content -- the inverse of `wait_for`, for asserting an error
    /// marker never appeared.
    AssertAbsent(String),
    /// Evaluates `expr` over the probe channel once and fails the scenario
    /// unless the (trimmed) result equals `expect` exactly. A single check,
    /// not a wait: use [`Step::WaitForProbe`] for an expression whose value
    /// only becomes true asynchronously (a plugin still loading, an install
    /// still running).
    Probe { expr: String, expect: String },
    /// Retries `expr` over the probe channel until its (trimmed) result
    /// equals `expect`, or `timeout` elapses. Discovered live, not
    /// speculative: a bare [`Step::Probe`] taken immediately after sending
    /// keys raced a still-installing plugin (lazy.nvim's own install
    /// window still had input focus) and failed a scenario that was
    /// otherwise correct -- the async-load case a one-shot probe cannot
    /// express on its own.
    WaitForProbe {
        expr: String,
        expect: String,
        timeout: Duration,
    },
}

/// Errors driving a compat scenario. Never carries a scenario name or step
/// index itself -- see this module's own docs and `bin/oracle.rs`'s
/// `run_tokens`/`EntryOutcome` precedent: composing "which scenario, which
/// step" is the orchestrating loop's job, the same layering the differential
/// oracle already uses.
#[non_exhaustive]
#[derive(Debug, thiserror::Error)]
pub enum CompatError {
    /// The underlying pty could not be opened or driven.
    #[error(transparent)]
    Pty(#[from] OracleError),
    /// An I/O error spawning or reading from a probe subprocess.
    #[error(transparent)]
    Io(#[from] std::io::Error),
    /// A `wait_for` step's needle never appeared before its timeout.
    #[error("wait_for {needle:?} timed out after {timeout:?}")]
    WaitTimedOut { needle: String, timeout: Duration },
    /// A `wait_for_cell` step's target cell never matched before its
    /// timeout.
    #[error("wait_for_cell ({row},{col}) != {expected:?} timed out after {timeout:?}")]
    WaitForCellTimedOut {
        row: u16,
        col: u16,
        expected: String,
        timeout: Duration,
    },
    /// An `assert_absent` step's forbidden needle was present on screen.
    #[error("assert_absent violated: {needle:?} is present on screen")]
    ForbiddenTextPresent { needle: String },
    /// A `probe` step's expression evaluated to a value other than what was
    /// expected.
    #[error("probe {expr:?} returned {actual:?}, expected {expected:?}")]
    ProbeMismatch {
        expr: String,
        expected: String,
        actual: String,
    },
    /// A `wait_for_probe` step's expression never matched `expect` before
    /// its own timeout.
    #[error("wait_for_probe {expr:?} never returned {expect:?} within {timeout:?}")]
    WaitForProbeTimedOut {
        expr: String,
        expect: String,
        timeout: Duration,
    },
    /// The probe subprocess (`nvim --server ... --remote-expr`) exited
    /// non-zero: the socket does not exist yet, the target rejected the
    /// expression, or the connection was refused.
    #[error("probe {expr:?} failed: {stderr}")]
    ProbeFailed { expr: String, stderr: String },
    /// The probe subprocess did not exit within [`PROBE_TIMEOUT`] and was
    /// killed -- the target is wedged (e.g. a blocking hit-enter prompt)
    /// rather than merely slow.
    #[error("probe {expr:?} did not respond within {timeout:?}; target may be wedged")]
    ProbeTimedOut { expr: String, timeout: Duration },
    /// [`CompatSession::prime_probe_channel`] typed the `serverstart` command
    /// into the pty, but no probe succeeded before its own deadline: the
    /// fixture-less priming path never actually opened the channel.
    #[error("probe channel never opened within {0:?} of priming")]
    ProbeChannelNeverOpened(Duration),
    /// The implicit zero-error epilogue found an E-numbered error or a Lua
    /// traceback in `:messages` or `v:errmsg`.
    #[error("zero-error epilogue violated ({origin}): {detail:?}")]
    ZeroErrorViolation {
        /// Which probe surfaced it: `"messages"` or `"v:errmsg"`.
        origin: &'static str,
        detail: String,
    },
}

/// Runs `child` to completion, polling rather than blocking, killing (and
/// reaping, so it cannot become a zombie) it if `timeout` elapses first.
/// Mirrors [`PtySession::wait_for_exit`]'s own bounded-wait shape: a probe
/// subprocess reaching a genuinely wedged target must fail this call's own
/// deadline rather than hang the caller, the same property that method's
/// doc comment argues for.
fn wait_with_timeout(
    mut child: Child,
    timeout: Duration,
) -> Result<std::process::Output, CompatError> {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(status) = child.try_wait()? {
            let mut stdout = Vec::new();
            let mut stderr = Vec::new();
            if let Some(mut out) = child.stdout.take() {
                use std::io::Read;
                let _ = out.read_to_end(&mut stdout);
            }
            if let Some(mut err) = child.stderr.take() {
                use std::io::Read;
                let _ = err.read_to_end(&mut stderr);
            }
            return Ok(std::process::Output {
                status,
                stdout,
                stderr,
            });
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Err(CompatError::ProbeTimedOut {
                expr: String::new(),
                timeout,
            });
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Drives one compat scenario's steps against a real `view` process over a
/// pty, plus a second, independent probe channel to the embedded nvim (see
/// this module's own docs). Deliberately thin: [`spawn_configured`] mirrors
/// [`PtySession::spawn_configured`]'s own signature so a caller configuring
/// XDG isolation and `VIEW_COMPAT_SOCK` builds one familiar
/// [`CommandBuilder`], the same way `view-oracle`'s own smoke tests already
/// configure a `view` pty.
///
/// [`spawn_configured`]: Self::spawn_configured
pub struct CompatSession {
    pty: PtySession,
    nvim_bin: PathBuf,
    sock_path: PathBuf,
}

impl CompatSession {
    /// Opens a `cols`x`rows` pty and spawns `cmd` (an already-configured
    /// `view` invocation -- XDG isolation, `VIEW_COMPAT_SOCK`, any
    /// `--nvim-bin` override -- already set as env/args by the caller)
    /// inside it. `nvim_bin` is the pinned `nvim` binary this session's
    /// probe channel shells out to as a client; `sock_path` is the same
    /// path the spawned `view` process's `VIEW_COMPAT_SOCK` env var names.
    ///
    /// # Errors
    ///
    /// Returns [`CompatError::Pty`] if the pty cannot be opened or `cmd`
    /// fails to spawn.
    pub fn spawn_configured(
        cmd: CommandBuilder,
        cols: u16,
        rows: u16,
        nvim_bin: PathBuf,
        sock_path: PathBuf,
    ) -> Result<Self, CompatError> {
        let pty = PtySession::spawn_configured(cmd, cols, rows)?;
        Ok(Self {
            pty,
            nvim_bin,
            sock_path,
        })
    }

    /// Borrows the underlying [`PtySession`] for callers that need its own
    /// primitives directly (`wait`, `wait_for_exit`, `screen`, `kill`)
    /// beyond what [`drive_step`](Self::drive_step) covers.
    pub fn pty(&mut self) -> &mut PtySession {
        &mut self.pty
    }

    /// Evaluates `expr` against the probe channel via `nvim --server $SOCK
    /// --remote-expr '<expr>'`, returning its (trimmed) result.
    ///
    /// Live-verified reply shapes (protocol step 1, `nvim` v0.12.4): a
    /// successful call exits 0 with the evaluated value on stdout and no
    /// trailing content worth preserving (trimmed here); a Lua/Vim error
    /// (e.g. an undefined variable) exits 2 with `"Lua: Vim:E121: ..."` on
    /// stderr; a not-yet-listening socket exits 2 with `"E247: Failed to
    /// connect ...: connection refused"` on stderr. Both error shapes surface
    /// as [`CompatError::ProbeFailed`] -- this method does not distinguish
    /// "channel not open yet" from "expression rejected" itself, since
    /// [`prime_probe_channel`](Self::prime_probe_channel) is the caller that
    /// cares about that distinction, via its own bounded retry.
    ///
    /// # Errors
    ///
    /// Returns [`CompatError::Io`] if the subprocess cannot be spawned,
    /// [`CompatError::ProbeTimedOut`] if it does not exit within
    /// [`PROBE_TIMEOUT`], or [`CompatError::ProbeFailed`] if it exits
    /// non-zero.
    pub fn probe(&self, expr: &str) -> Result<String, CompatError> {
        let child = Command::new(&self.nvim_bin)
            .arg("--server")
            .arg(&self.sock_path)
            .arg("--remote-expr")
            .arg(expr)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;

        let output = wait_with_timeout(child, PROBE_TIMEOUT).map_err(|err| match err {
            CompatError::ProbeTimedOut { timeout, .. } => CompatError::ProbeTimedOut {
                expr: expr.to_string(),
                timeout,
            },
            other => other,
        })?;

        if !output.status.success() {
            return Err(CompatError::ProbeFailed {
                expr: expr.to_string(),
                stderr: String::from_utf8_lossy(&output.stderr).trim().to_string(),
            });
        }
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
    }

    /// For a fixture-less scenario (no committed `init.lua` to carry its own
    /// `serverstart` call): types `:call serverstart($VIEW_COMPAT_SOCK)<CR>`
    /// into the pty, exactly as a user would at the command line, then
    /// confirms the channel actually opened via a bounded retry loop of real
    /// [`probe`](Self::probe) calls -- never a fixed sleep. Each retry is a
    /// genuine attempt at the real probe subprocess, so this loop cannot
    /// pass vacuously the way a bare "wait N ms then assume it worked" would:
    /// it only returns `Ok` once a probe has actually round-tripped.
    ///
    /// # Errors
    ///
    /// Returns [`CompatError::Pty`] if typing the command fails, or
    /// [`CompatError::ProbeChannelNeverOpened`] if no probe succeeds before
    /// `timeout` elapses.
    pub fn prime_probe_channel(&mut self, timeout: Duration) -> Result<(), CompatError> {
        self.pty.send(&resolve_send_keys(
            "<Esc>:call serverstart($VIEW_COMPAT_SOCK)<CR>",
        ))?;
        self.await_probe_channel(timeout)
    }

    /// For a fixture whose own `init.lua` already calls `serverstart`
    /// (every committed compat fixture): confirms the channel is actually
    /// live via the same bounded, real-probe-retry loop
    /// [`prime_probe_channel`](Self::prime_probe_channel) uses, minus the
    /// pty typing step. Startup ordering (config sourced, `serverstart`
    /// called, socket file created) is otherwise unobserved from the
    /// caller's side, so a scenario's first real step must not race it.
    ///
    /// # Errors
    ///
    /// Returns [`CompatError::ProbeChannelNeverOpened`] if no probe
    /// succeeds before `timeout` elapses.
    pub fn await_probe_channel(&mut self, timeout: Duration) -> Result<(), CompatError> {
        let deadline = Instant::now() + timeout;
        loop {
            if self.probe("1").is_ok() {
                return Ok(());
            }
            if Instant::now() >= deadline {
                return Err(CompatError::ProbeChannelNeverOpened(timeout));
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    /// Blocks until the pty's rendered screen stops changing for `silence`,
    /// or `deadline` elapses -- a bounded, positively-confirmed settle
    /// check (each poll re-reads the real screen; two consecutive
    /// identical reads is the confirmation), never a fixed sleep. Used
    /// before [`prime_probe_channel`](Self::prime_probe_channel) on a
    /// fixture-less scenario: unlike every committed fixture (whose
    /// `init.lua` calls `serverstart` as its very first statement, well
    /// before any redraw), a daily config's own startup content is
    /// unknown to this harness, so there is no fixed needle to `wait_for`
    /// instead.
    ///
    /// Returns whether the screen was observed to settle before `deadline`.
    #[must_use]
    pub fn wait_for_screen_quiescence(&mut self, silence: Duration, deadline: Duration) -> bool {
        let start = Instant::now();
        let mut last = self.pty.screen();
        let mut quiet_since = Instant::now();
        loop {
            std::thread::sleep(Duration::from_millis(50));
            let current = self.pty.screen();
            if current == last {
                if Instant::now().duration_since(quiet_since) >= silence {
                    return true;
                }
            } else {
                last = current;
                quiet_since = Instant::now();
            }
            if start.elapsed() >= deadline {
                return false;
            }
        }
    }

    /// Drives one [`Step`]. Every `WaitFor`/`WaitForCell` step already
    /// carries its own resolved `timeout` (the scenario loader applies the
    /// schema default when a TOML step omits `timeout_ms`), so this method
    /// takes no separate timeout parameter of its own.
    ///
    /// # Errors
    ///
    /// Returns the [`CompatError`] variant matching whichever check failed.
    pub fn drive_step(&mut self, step: &Step) -> Result<(), CompatError> {
        match step {
            Step::Send(keys) => {
                self.pty.send(&resolve_send_keys(keys))?;
                Ok(())
            }
            Step::WaitFor { needle, timeout } => {
                if self.pty.wait_for(needle, *timeout) {
                    Ok(())
                } else {
                    Err(CompatError::WaitTimedOut {
                        needle: needle.clone(),
                        timeout: *timeout,
                    })
                }
            }
            Step::WaitForCell {
                row,
                col,
                expected,
                timeout,
            } => {
                if self.pty.wait_for_cell(*row, *col, expected, *timeout) {
                    Ok(())
                } else {
                    Err(CompatError::WaitForCellTimedOut {
                        row: *row,
                        col: *col,
                        expected: expected.clone(),
                        timeout: *timeout,
                    })
                }
            }
            Step::AssertAbsent(needle) => {
                if self.pty.screen().contains(needle.as_str()) {
                    Err(CompatError::ForbiddenTextPresent {
                        needle: needle.clone(),
                    })
                } else {
                    Ok(())
                }
            }
            Step::Probe { expr, expect } => {
                let actual = self.probe(expr)?;
                if &actual == expect {
                    Ok(())
                } else {
                    Err(CompatError::ProbeMismatch {
                        expr: expr.clone(),
                        expected: expect.clone(),
                        actual,
                    })
                }
            }
            Step::WaitForProbe {
                expr,
                expect,
                timeout,
            } => {
                let deadline = Instant::now() + *timeout;
                loop {
                    if self.probe(expr).is_ok_and(|actual| &actual == expect) {
                        return Ok(());
                    }
                    if Instant::now() >= deadline {
                        return Err(CompatError::WaitForProbeTimedOut {
                            expr: expr.clone(),
                            expect: expect.clone(),
                            timeout: *timeout,
                        });
                    }
                    std::thread::sleep(Duration::from_millis(200));
                }
            }
        }
    }

    /// The implicit epilogue every scenario gets after its scripted steps,
    /// per the design brief: probes `:messages` and `v:errmsg` over the
    /// probe channel and fails if either carries an E-numbered error or a
    /// Lua traceback. Two separate probes (not one combined expression):
    /// `v:errmsg` only ever holds the *last* error, while `:messages`
    /// accumulates the whole session's log, so a scenario that triggers and
    /// then clears an error (a later successful command overwrites
    /// `v:errmsg`) would otherwise slip past a `v:errmsg`-only check with
    /// the error still sitting in `:messages`.
    ///
    /// # Errors
    ///
    /// Returns [`CompatError::ZeroErrorViolation`] if either probe's content
    /// contains an `E`-numbered error code or the literal string
    /// `"traceback"`, or any [`CompatError`] the two probes themselves can
    /// raise.
    pub fn zero_error_check(&self) -> Result<(), CompatError> {
        let messages = self.probe("execute('messages')")?;
        if let Some(detail) = error_marker(&messages) {
            return Err(CompatError::ZeroErrorViolation {
                origin: "messages",
                detail,
            });
        }
        let errmsg = self.probe("v:errmsg")?;
        if let Some(detail) = error_marker(&errmsg) {
            return Err(CompatError::ZeroErrorViolation {
                origin: "v:errmsg",
                detail,
            });
        }
        Ok(())
    }
}

/// Scans `text` for an E-numbered Vim error (`E` followed by a digit,
/// nvim's own error-code convention) or a Lua traceback marker, returning
/// the offending line if found. A line-oriented scan (not a single
/// whole-text substring search) so [`CompatError::ZeroErrorViolation`]'s
/// `detail` names the actual offending line rather than the entire
/// `:messages` buffer, which can span an unrelated scenario's whole
/// scripted history.
fn error_marker(text: &str) -> Option<String> {
    text.lines()
        .find(|line| {
            line.contains("traceback")
                || line.as_bytes().windows(2).enumerate().any(|(i, w)| {
                    w[0] == b'E' && w[1].is_ascii_digit() && starts_error_token(line, i)
                })
        })
        .map(str::to_string)
}

/// True if the `E<digit>` found at byte offset `i` in `line` starts a token
/// (line start, or preceded by whitespace/`:`/`(`) rather than sitting
/// mid-word (e.g. the "E" in a plugin name or a file path), which would
/// otherwise false-positive [`error_marker`] on ordinary, error-free
/// content.
fn starts_error_token(line: &str, i: usize) -> bool {
    match line.as_bytes().get(i.wrapping_sub(1)) {
        None => true,
        Some(b) => !(b.is_ascii_alphanumeric() || *b == b'_'),
    }
}

/// Translates vim key-notation embedded in a [`Step::Send`] string (or
/// [`CompatSession::prime_probe_channel`]'s own typed command) into the
/// literal bytes a real keypress would send -- `"ihello<Esc>"` becomes the
/// text `ihello` followed by one real `0x1b` byte, not the four literal
/// characters `<Esc>`. Without this, a pty write of `keys.as_bytes()`
/// verbatim types `<Esc>`/`<CR>` as on-screen text instead of pressing the
/// key, which leaves the session stuck in whatever mode it started in --
/// silently, since most scenario assertions are substring checks that
/// still pass with the stray literal text present. The one case that
/// cannot silently tolerate it is `prime_probe_channel`'s own `<CR>`: an
/// ex command typed but never submitted never opens the probe channel,
/// which is how this bug was actually caught (a fixture-less scenario's
/// priming step timing out rather than any visible corruption).
///
/// Only the small notation set real compat scenarios need is recognized;
/// an unrecognized `<...>` token (or a lone `<` with no matching `>`)
/// passes through as literal text rather than erroring, since a scenario
/// author writing literal angle brackets into typed text is also a valid
/// use of `send`.
fn resolve_send_keys(text: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(text.len());
    let bytes = text.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'<' {
            if let Some(rel_end) = text[i..].find('>') {
                let token = &text[i + 1..i + rel_end];
                if let Some(resolved) = resolve_key_token(token) {
                    out.extend_from_slice(resolved);
                    i += rel_end + 1;
                    continue;
                }
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    out
}

/// The notation table [`resolve_send_keys`] recognizes, case-insensitive
/// (vim notation itself is case-insensitive for named keys: `<esc>` and
/// `<Esc>` are the same key).
fn resolve_key_token(token: &str) -> Option<&'static [u8]> {
    match token.to_ascii_lowercase().as_str() {
        "esc" | "escape" => Some(b"\x1b"),
        "cr" | "enter" | "return" => Some(b"\r"),
        "tab" => Some(b"\t"),
        "bs" | "backspace" => Some(b"\x08"),
        "space" => Some(b" "),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    #[test]
    fn error_marker_finds_an_e_numbered_line() {
        let text = "Press ENTER\nE5108: Error executing lua ...\nsome other line";
        assert_eq!(
            error_marker(text),
            Some("E5108: Error executing lua ...".to_string())
        );
    }

    #[test]
    fn error_marker_finds_a_lua_traceback() {
        let text = "stack traceback:\n\t[C]: in function 'foo'";
        assert_eq!(error_marker(text), Some("stack traceback:".to_string()));
    }

    #[test]
    fn error_marker_ignores_clean_content() {
        assert_eq!(error_marker(""), None);
        assert_eq!(error_marker("-- INSERT --"), None);
    }

    #[test]
    fn error_marker_does_not_false_positive_on_a_word_containing_e_and_a_digit() {
        // "CVE2024" contains an upper-case "E" immediately followed by a
        // digit, the same shape a real E-numbered error starts with -- but
        // it sits mid-word (preceded by "V", alphanumeric), so it must not
        // be mistaken for one.
        assert_eq!(error_marker("see CVE2024 for details"), None);
    }

    #[test]
    fn error_marker_matches_e_number_at_start_of_line() {
        assert_eq!(
            error_marker("E121: Undefined variable: foo"),
            Some("E121: Undefined variable: foo".to_string())
        );
    }

    #[test]
    fn resolve_send_keys_translates_esc_to_a_real_escape_byte() {
        assert_eq!(resolve_send_keys("ihello<Esc>"), b"ihello\x1b".to_vec());
    }

    #[test]
    fn resolve_send_keys_translates_cr_to_a_real_carriage_return() {
        assert_eq!(
            resolve_send_keys(":q<CR>"),
            b":q\r".to_vec(),
            "a typed ex command with a literal '<CR>' suffix instead of a \
             real carriage-return byte never submits, which is exactly the \
             priming-channel bug this translator fixes"
        );
    }

    #[test]
    fn resolve_send_keys_is_case_insensitive_on_known_notation() {
        assert_eq!(resolve_send_keys("x<esc>"), b"x\x1b".to_vec());
        assert_eq!(resolve_send_keys("x<ESC>"), b"x\x1b".to_vec());
    }

    #[test]
    fn resolve_send_keys_passes_unrecognized_notation_through_literally() {
        assert_eq!(resolve_send_keys("a<Nonsense>b"), b"a<Nonsense>b".to_vec());
    }

    #[test]
    fn resolve_send_keys_passes_plain_text_through_unchanged() {
        assert_eq!(resolve_send_keys("plain text"), b"plain text".to_vec());
    }

    #[test]
    fn resolve_send_keys_handles_an_unterminated_angle_bracket() {
        assert_eq!(resolve_send_keys("a < b"), b"a < b".to_vec());
    }
}
