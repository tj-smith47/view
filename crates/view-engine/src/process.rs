//! Spawning `nvim --embed` and performing the API-info handshake.
//!
//! `Engine` owns the child process, its RPC handle, and the damage pump for
//! the process's lifetime. Its `Drop` impl attempts a graceful
//! shutdown (`qa!` sent over the writer thread, then a bounded wait) before
//! falling back to `SIGKILL`, so a normally-responsive nvim gets the chance
//! to flush shada and remove its swap file instead of leaving behind a
//! recovery prompt on the next open.

use crate::damage::{DamagePump, PumpShared, SinkCutover};
use crate::handle::{EngineError, EngineHandle};
use rmpv::Value;
use std::ffi::{OsStr, OsString};
use std::path::PathBuf;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::mpsc::SyncSender;
use std::sync::Arc;
use std::time::{Duration, Instant};
use view_core::msg::{ExitInfo, Msg};

/// Configuration for spawning an embedded Neovim process.
///
/// `#[non_exhaustive]`: the hermetic environment plan an isolated spawn
/// applies is a private field rather than a seventh public one, so a caller
/// cannot construct a config that looks isolated and is not.
/// Cross-crate callers build one from [`default`](Self::default) or
/// [`isolated`](Self::isolated) and the `with_*` methods below.
#[non_exhaustive]
pub struct EngineConfig {
    /// Path to the `nvim` binary. Defaults to `"nvim"`, resolved via `PATH`;
    /// release packaging replaces this default with a bundled binary path.
    pub nvim_bin: PathBuf,
    /// Additional arguments passed to `nvim` after `--embed`.
    pub extra_args: Vec<OsString>,
    /// Environment overrides applied on top of the environment the child
    /// inherits. Empty by default: an engine spawned on a user's behalf
    /// must see the environment that user's own `nvim` would see.
    pub env: Vec<(OsString, OsString)>,
    /// Environment variables removed from the environment the child
    /// inherits. Applied after [`env`](Self::env), so a name in both is
    /// removed: an override pushed later must not reinstate a variable a
    /// caller asked to be rid of.
    pub env_remove: Vec<OsString>,
    /// Maximum time to wait for the `nvim_get_api_info` handshake response
    /// during [`Engine::spawn`]. Defaults to 5 seconds. A process that
    /// spawns but never replies (wedged, wrong binary, hung under a
    /// debugger) fails `spawn()` with `EngineError::Timeout` instead of
    /// blocking the caller forever; the child is reaped before the error
    /// is returned.
    pub handshake_timeout: Duration,
    /// Maximum time to wait for the child to exit on its own after a
    /// graceful `qa!` is sent during shutdown ([`Engine::shutdown`] or
    /// `Drop`). Defaults to 500 milliseconds. A child still running once
    /// this elapses is force-killed instead.
    pub shutdown_timeout: Duration,
    /// Whether [`crate::env`]'s hermetic environment plan is applied to the
    /// child. Private, and applied strictly after both public vectors (see
    /// [`env_plan`](Self::env_plan)): hermeticity that a caller's own
    /// `env` entry or a `env_remove.clear()` could discard is hermeticity
    /// that holds only while nobody touches the config, and a child that
    /// lost it looks exactly like one that kept it.
    hermetic: bool,
    /// A duplicate of the caller's own stdin, relayed into the child at a
    /// fixed descriptor (`crate::nvim_api::STDIN_RELAY_CHILD_FD`) rather
    /// than child fd 0, which `--embed` already claims for the
    /// msgpack-RPC channel (see `:help ui-startup-stdin`). Private: a
    /// caller arms it through [`with_stdin_relay`](Self::with_stdin_relay)
    /// and reads it back only through
    /// [`stdin_relay_requested`](Self::stdin_relay_requested), never the
    /// fd itself, since [`build_command`] is the only place that must ever
    /// touch it directly.
    #[cfg(unix)]
    stdin_relay: Option<std::os::fd::OwnedFd>,
}

impl Default for EngineConfig {
    fn default() -> Self {
        Self {
            nvim_bin: PathBuf::from("nvim"),
            extra_args: vec![],
            env: vec![],
            env_remove: vec![],
            handshake_timeout: Duration::from_secs(5),
            shutdown_timeout: Duration::from_millis(500),
            hermetic: false,
            #[cfg(unix)]
            stdin_relay: None,
        }
    }
}

impl EngineConfig {
    /// A config whose child ignores every nvim setting the host carries: no
    /// user config, plugins or shada (`--clean`), no swap file (`-n`), and
    /// none of the environment variables that reach past those flags to
    /// redirect the child's configuration anyway (see [`crate::env`]).
    ///
    /// For everything that measures the engine rather than a user's editor
    /// (the oracle's reference sessions, the engine's own tests), whose
    /// results must not turn on which machine happens to run them.
    ///
    /// Never for an engine spawned on a user's behalf, and never for one
    /// whose configuration is the thing being measured. `--clean` discards
    /// the user's config and plugins outright, which is the point here and
    /// exactly wrong there: the `view` binary spawns through
    /// [`EngineConfig::default`], and the measurement matrix measures that
    /// same binary against pinned fixture configurations it delivers
    /// through `XDG_CONFIG_HOME`. Routing that spawn through this
    /// constructor would measure a plugin-free editor against baselines
    /// recorded with the fixture's full plugin set, and the resulting
    /// number would pass its gate as a large improvement. Nothing about a
    /// stripped-down child fails loudly, so the invariant is pinned by test
    /// on every side: that `default` carries neither arguments nor an
    /// environment plan, that the config the `view` binary's own CLI builds
    /// is that `default` one, and that the specs the matrix builds carry no
    /// `--clean`.
    ///
    /// It also keeps the child's exit path open, which a host config can
    /// close off outright: any message emitted during startup parks `qa!`
    /// in nvim's `wait_return` prompt waiting for the keypress that
    /// acknowledges it, and an embedded nvim with no UI attached has no
    /// source for that keypress. Such a child never exits on its own at
    /// all, on any host, at any speed, and only the force-kill fallback
    /// ends it.
    #[must_use]
    pub fn isolated() -> Self {
        Self {
            extra_args: vec![OsString::from("--clean"), OsString::from("-n")],
            hermetic: true,
            ..Self::default()
        }
    }

    /// The `nvim` binary to spawn, replacing the `PATH` lookup.
    #[must_use]
    pub fn with_nvim_bin(mut self, bin: impl Into<PathBuf>) -> Self {
        self.nvim_bin = bin.into();
        self
    }

    /// Appends one argument after `--embed`.
    #[must_use]
    pub fn with_arg(mut self, arg: impl Into<OsString>) -> Self {
        self.extra_args.push(arg.into());
        self
    }

    /// Appends one environment override. Overridden in turn by a hermetic
    /// config's own plan, which applies last (see [`env_plan`](Self::env_plan)).
    #[must_use]
    pub fn with_env(mut self, name: impl Into<OsString>, value: impl Into<OsString>) -> Self {
        self.env.push((name.into(), value.into()));
        self
    }

    /// Appends one environment removal, which outranks any override of the
    /// same name.
    #[must_use]
    pub fn with_env_remove(mut self, name: impl Into<OsString>) -> Self {
        self.env_remove.push(name.into());
        self
    }

    /// The handshake timeout, replacing the 5 second default.
    #[must_use]
    pub fn with_handshake_timeout(mut self, timeout: Duration) -> Self {
        self.handshake_timeout = timeout;
        self
    }

    /// The graceful-shutdown timeout, replacing the 500 millisecond default.
    #[must_use]
    pub fn with_shutdown_timeout(mut self, timeout: Duration) -> Self {
        self.shutdown_timeout = timeout;
        self
    }

    /// Arranges for `fd` -- a duplicate of the caller's own stdin -- to
    /// reach the child at a descriptor `--embed` has not already claimed
    /// (see the `stdin_relay` field doc), so a caller whose own stdin is
    /// piped (`ls | view -`) can still deliver that content to nvim once it
    /// also attaches through
    /// [`EngineHandle::ui_attach_with_stdin_relay`](crate::nvim_api),
    /// rather than the plain [`ui_attach`](crate::nvim_api), which nvim's
    /// `stdin_fd` option depends on.
    #[cfg(unix)]
    #[must_use]
    pub fn with_stdin_relay(mut self, fd: std::os::fd::OwnedFd) -> Self {
        self.stdin_relay = Some(fd);
        self
    }

    /// Whether a caller armed [`with_stdin_relay`](Self::with_stdin_relay).
    /// Always `false` off Unix, where no relay mechanism exists yet.
    ///
    /// The caller must read this *before* passing `self` to
    /// [`Engine::spawn`], which consumes the config by value: the choice
    /// between `ui_attach` and `ui_attach_with_stdin_relay` depends on the
    /// answer, and there is no config left to ask once `spawn` has it.
    #[must_use]
    pub fn stdin_relay_requested(&self) -> bool {
        #[cfg(unix)]
        {
            self.stdin_relay.is_some()
        }
        #[cfg(not(unix))]
        {
            false
        }
    }

    /// The environment plan this config applies to a child: `Some(value)`
    /// for an override, `None` for a removal, in application order and with
    /// one entry per name.
    ///
    /// [`env`](Self::env) first, then [`env_remove`](Self::env_remove), then
    /// -- for a config from [`isolated`](Self::isolated) -- [`crate::env`]'s
    /// hermetic plan, which therefore cannot be clobbered by anything a
    /// caller pushed. Public so the plan can be asserted on without spawning
    /// anything: an override or removal that silently stopped being applied
    /// would leave a child reading the host's own editor configuration, and
    /// nothing about that fails visibly.
    ///
    /// The hermetic plan itself is two layers, in this order:
    ///
    /// 1. [`crate::env::hermetic_sweep`], which removes every host variable
    ///    the allowlist does not name, and skips a name a caller already
    ///    planned: the sweep drops what the host merely happens to export,
    ///    and a variable a caller asked for is not that.
    /// 2. [`crate::env::HOST_REDIRECT_VARS`],
    ///    [`crate::env::HOST_SEARCH_PATH_VARS`],
    ///    [`crate::env::HOST_SUBPROCESS_CONFIG_VARS`] and
    ///    [`crate::env::HERMETIC_HOME_VAR`], unconditionally. The
    ///    sweep already covers a host that exports them; this layer also
    ///    covers the caller who set one deliberately, which is what an
    ///    isolated config must refuse whoever asks.
    #[must_use]
    pub fn env_plan(&self) -> Vec<(OsString, Option<OsString>)> {
        let mut plan: Vec<(OsString, Option<OsString>)> = Vec::new();
        for (name, value) in &self.env {
            plan_set(&mut plan, name, Some(value.clone()));
        }
        for name in &self.env_remove {
            plan_set(&mut plan, name, None);
        }
        if self.hermetic {
            for (name, _) in crate::env::hermetic_sweep() {
                plan_sweep(&mut plan, &name);
            }
            for name in crate::env::HOST_REDIRECT_VARS {
                plan_set(&mut plan, OsStr::new(name), None);
            }
            let empty = crate::env::empty_search_path().into_os_string();
            for name in crate::env::HOST_SEARCH_PATH_VARS {
                plan_set(&mut plan, OsStr::new(name), Some(empty.clone()));
            }
            let absent = crate::env::absent_config_file().into_os_string();
            for name in crate::env::HOST_SUBPROCESS_CONFIG_VARS {
                plan_set(&mut plan, OsStr::new(name), Some(absent.clone()));
            }
            plan_set(
                &mut plan,
                OsStr::new(crate::env::HERMETIC_HOME_VAR),
                Some(crate::env::hermetic_home().into_os_string()),
            );
        }
        plan
    }
}

/// Records `name`'s disposition in `plan`, replacing any earlier one rather
/// than appending a second entry for the same variable: the plan is applied
/// to a process environment, where a name has one value or none, so a later
/// entry silently winning over an earlier one in `Command`'s own map would
/// make the inspectable plan disagree with the spawned child.
fn plan_set(plan: &mut Vec<(OsString, Option<OsString>)>, name: &OsStr, value: Option<OsString>) {
    match plan
        .iter_mut()
        .find(|(known, _)| crate::env::env_names_eq(known, name))
    {
        Some(entry) => entry.1 = value,
        None => plan.push((name.to_os_string(), value)),
    }
}

/// Records `name` as removed by the hermetic sweep, leaving any entry the
/// plan already carries for it alone.
///
/// The sweep's subject is what the host happens to export, which a caller's
/// own entry for the same name is not: a caller that set a variable
/// deliberately gets to keep it, and one that asked for something an
/// isolated spawn refuses outright loses it to the layer applied after this
/// one instead.
///
/// The name is the whole test here, and that is deliberately *not* the test
/// the pty and plain-`Command` funnel applies, which compares the builder's
/// value against the host's because those builders cannot report whether a
/// caller set a name at all. The two rules agree everywhere except one
/// edge: a caller that sets a swept name to exactly the value the host
/// already holds is kept here and dropped there. This side is the one with
/// the caller's intent in hand -- `env` and `env_remove` say so outright --
/// so it is the side that answers correctly, and matching the weaker rule
/// to make the two identical would mean discarding a variable a caller
/// asked for because of what some unrelated machine happens to export.
fn plan_sweep(plan: &mut Vec<(OsString, Option<OsString>)>, name: &OsStr) {
    if !plan
        .iter()
        .any(|(known, _)| crate::env::env_names_eq(known, name))
    {
        plan.push((name.to_os_string(), None));
    }
}

/// Which of the two shutdown paths ended the child: it exited on its own
/// after `qa!`, or it was still running at the deadline and was killed.
///
/// Recorded where the branch is taken rather than inferred afterwards from
/// the exit status, because the status does not carry the distinction. A
/// child that exits between the last poll and the `kill` is signalled only
/// after it is already gone, so `wait` reports the ordinary exit code it
/// chose for itself and the forced path looks graceful; a child that dies
/// of its own fault while processing `qa!` reports a signal though nothing
/// forced it, and the graceful path looks forced. Reading the path off the
/// status therefore both misses forced kills and invents them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum ShutdownPath {
    /// The child exited on its own before `shutdown_timeout` elapsed.
    /// Nothing was signalled to it.
    Graceful,
    /// The child was still running when `shutdown_timeout` elapsed, so it
    /// was force-killed and reaped.
    Forced,
}

/// How a child's shutdown ended: which path ran, and the exit status the
/// child produced.
#[derive(Debug)]
#[non_exhaustive]
pub struct ShutdownOutcome {
    /// The path taken. See [`ShutdownPath`] for why it is a recorded fact
    /// rather than something `status` can be read for.
    pub path: ShutdownPath,
    /// The child's exit status, as `wait` reported it.
    pub status: ExitStatus,
}

/// The engine's reported API version and RPC channel id, from
/// `nvim_get_api_info`.
pub struct ApiInfo {
    /// The msgpack-RPC channel id assigned to this connection.
    pub channel_id: u64,
    /// Major version component of the running Neovim's API.
    pub version_major: u64,
    /// Minor version component of the running Neovim's API.
    pub version_minor: u64,
}

/// A spawned embedded Neovim process with its RPC handle and damage pump.
///
/// `Engine` owns the child process for its entire lifetime: once
/// `Engine::spawn` returns `Ok`, dropping the `Engine` always shuts the
/// child down (`Drop` attempts a graceful `qa!` first, then force-kills and
/// reaps; see [`shutdown`](Self::shutdown) for the same sequence with an
/// observable exit status). The child itself is a private field: callers
/// cannot block on it directly, only read its pid, attach the runtime
/// loop's damage pump, or consume the `Engine` to shut it down explicitly.
pub struct Engine {
    /// The RPC client for issuing requests to the engine. `Clone` and
    /// `Send`, so requests can be issued from other threads while the
    /// runtime loop owns the [`DamagePump`] returned by
    /// [`start_pump`](Self::start_pump).
    pub handle: EngineHandle,
    child: Child,
    shutdown_timeout: Duration,
    /// The engine's API version and channel id, captured at handshake time.
    pub api_info: ApiInfo,
    /// Damage/request pump state, live from `spawn` so redraws and known
    /// requests arriving before [`start_pump`](Self::start_pump) attaches a
    /// sink are staged rather than lost. See `crate::damage` for the full
    /// contract.
    pump: Arc<PumpShared>,
}

/// Owns a spawned child during [`Engine::spawn`] so every early-return path
/// (pipe capture failure, handshake error or timeout) reaps it instead of
/// leaking a zombie. Disarmed via `.0.take()` once `spawn` has everything it
/// needs to build the long-lived `Engine`, which then owns reaping itself.
///
/// This guard only covers the pre-handshake window, where the child has
/// never answered anything, so there is no session state worth saving; it
/// always force-kills rather than attempting the graceful shutdown
/// `Engine`'s own `Drop` uses once a connection is actually live.
struct ChildGuard(Option<Child>);

impl Drop for ChildGuard {
    fn drop(&mut self) {
        if let Some(mut child) = self.0.take() {
            // best-effort: the child may already be gone (e.g. it exited on
            // its own before the guard dropped), so errors here are not
            // actionable and are discarded rather than propagated
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

impl Engine {
    /// Spawns `nvim --embed` per `cfg` and performs the `nvim_get_api_info`
    /// handshake.
    ///
    /// # Errors
    ///
    /// Returns `EngineError::Io` if the process fails to spawn or its pipes
    /// cannot be captured, or the error returned by the handshake request
    /// itself (`EngineError::Rpc`, `Remote`, `Closed`, or `Timeout` if the
    /// engine does not answer within `cfg.handshake_timeout`). On any error
    /// after a successful process spawn, the child is killed and reaped
    /// before the error is returned; no zombie survives a failed `spawn`.
    ///
    /// An isolated `cfg` also returns `EngineError::Io` when the hermetic
    /// search path cannot be established empty (see
    /// [`crate::env::prepare_empty_search_path`]) or the hermetic home holds
    /// a planted entry (see [`crate::env::prepare_hermetic_home`]), before
    /// any process is started: a child pointed at a directory somebody
    /// planted a plugin or credential file in is not isolated, and refusing
    /// the spawn is the only way that says so.
    pub fn spawn(cfg: EngineConfig) -> Result<Self, EngineError> {
        if cfg.hermetic {
            crate::env::prepare_empty_search_path()?;
            crate::env::prepare_hermetic_home()?;
        }
        let mut command = build_command(&cfg);
        // the stdin `Stdio::piped()` would build on Windows is opened without
        // the right to read its own attributes, which is the right the
        // outbox's readiness query needs, and no later call can widen it. A
        // pipe built here answers that query; the child is handed its read
        // end and cannot tell the difference.
        #[cfg(windows)]
        let our_stdin = {
            let (theirs, ours) = crate::winpipe::child_stdin_pipe()?;
            command.stdin(Stdio::from(theirs));
            ours
        };
        let mut guard = ChildGuard(Some(command.spawn()?));
        // the child's own ends are the child's from here on. A `Command`
        // holds any handle it was configured with until it is dropped, so on
        // Windows this is what closes the parent's copy of the child's stdin
        // read end -- without it a child that died during the handshake could
        // not break its own stdin pipe, and detection would rest on the
        // stdout EOF and the handshake timeout alone. Unix closes the
        // child-side ends in the parent as part of the spawn, so there it
        // costs nothing and reads the same.
        drop(command);
        // unreachable ok_or: nothing clears guard.0 before this point
        let child = guard
            .0
            .as_mut()
            .ok_or_else(|| EngineError::Io(std::io::Error::other("child slot empty")))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| EngineError::Io(std::io::Error::other("stdout pipe not captured")))?;
        #[cfg(not(windows))]
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| EngineError::Io(std::io::Error::other("stdin pipe not captured")))?;
        // built before EngineHandle::start_pumped so the reader thread can
        // fold and stage from its very first message, before start_pump is
        // ever called
        let pump = PumpShared::new();
        // a second handle on the same pipe, so the outbox can ask whether a
        // small write completes now without borrowing the writer it would
        // then have to write through
        #[cfg(unix)]
        let handle = {
            use std::os::fd::AsFd;
            let pipe = stdin.as_fd().try_clone_to_owned().ok();
            EngineHandle::start_pumped(stdout, stdin, Arc::clone(&pump), pipe)
        };
        #[cfg(windows)]
        let handle = {
            let pipe = our_stdin.try_clone().ok();
            EngineHandle::start_pumped(
                stdout,
                std::fs::File::from(our_stdin),
                Arc::clone(&pump),
                pipe,
            )
        };
        #[cfg(not(any(unix, windows)))]
        let handle = EngineHandle::start_pumped(stdout, stdin, Arc::clone(&pump));
        let api_info = decode_api_info(handle.request_timeout(
            "nvim_get_api_info",
            vec![],
            cfg.handshake_timeout,
        )?)?;
        // handshake succeeded: disarm the guard and hand the child to the
        // long-lived Engine, which now owns reaping it via its own Drop
        // unreachable else: nothing clears guard.0 before this point
        let Some(child) = guard.0.take() else {
            return Err(EngineError::Io(std::io::Error::other("child slot empty")));
        };
        Ok(Self {
            handle,
            child,
            shutdown_timeout: cfg.shutdown_timeout,
            api_info,
            pump,
        })
    }

    /// Attaches the runtime loop's bounded `Msg` channel and returns the
    /// [`DamagePump`] handle for draining compacted damage from it, plus
    /// [`SinkCutover`]: everything that arrived between `spawn` and this
    /// call (a `view_vim_enter` firing during the window before this call,
    /// most notably, plus whether damage was already pending), returned
    /// rather than sent into `sink`. `sink` has no guaranteed consumer yet
    /// at the moment this call is made, so nothing here performs a send at
    /// all -- see [`PumpShared::attach_sink`]'s doc comment for why. The
    /// caller resolves the returned state through its own dispatch path
    /// once a consumer is guaranteed (see `view`'s `startup::run_cutover`).
    #[must_use]
    pub fn start_pump(&mut self, sink: SyncSender<Msg>) -> (DamagePump, SinkCutover) {
        self.pump.attach_sink(sink)
    }

    /// Resolves the engine's exit status into an [`ExitInfo`], for the
    /// runtime loop to call once its reader signals `Msg::EngineStopped`
    /// (the reader thread's stream ended, so the connection is already
    /// gone; this determines the child's real exit status).
    ///
    /// Reuses `graceful_kill`'s bounded-wait-then-kill sequence rather
    /// than duplicating it: sending `qa!` again here is a harmless no-op
    /// once the connection is already closed (`notify` just fails silently
    /// and the very next `try_wait` typically finds the child already
    /// exited). `code: None` means the exit status itself was unreadable
    /// (a `std::io::Error` from `try_wait`/`kill`/`wait`), which `update()`
    /// maps to exit code 1 rather than treating as success.
    #[must_use]
    pub fn wait_exit(&mut self) -> ExitInfo {
        match graceful_kill(&self.handle, &mut self.child, self.shutdown_timeout) {
            Ok(outcome) => exit_info_from_status(outcome.status),
            Err(_) => ExitInfo {
                code: None,
                by_signal: false,
            },
        }
    }

    /// The OS process id of the spawned child. For diagnostics and tests
    /// that need to verify the process was actually reaped; does not block
    /// or observe exit status (see [`shutdown`](Self::shutdown) for that).
    pub fn pid(&self) -> u32 {
        self.child.id()
    }

    /// Consumes the `Engine` and runs the same graceful-then-forced
    /// shutdown sequence as `Drop`, returning which path it took and the
    /// child's real exit status.
    ///
    /// `Drop` alone cannot surface this: it runs on every drop path
    /// (including a panic unwinding through an `Engine`), discards errors,
    /// and has no return value. Call `shutdown` explicitly to learn whether
    /// the child exited on its own or had to be killed
    /// ([`ShutdownOutcome::path`]) or to forward the real exit code, such
    /// as via `std::process::exit`.
    ///
    /// # Errors
    ///
    /// Returns the underlying `std::io::Error` if `try_wait`, `kill`, or
    /// `wait` on the child process fails.
    pub fn shutdown(mut self) -> std::io::Result<ShutdownOutcome> {
        graceful_kill(&self.handle, &mut self.child, self.shutdown_timeout)
    }
}

impl Drop for Engine {
    /// Attempts a graceful shutdown (`qa!`, then a bounded wait) and falls
    /// back to `SIGKILL` + reap on every drop path. Errors are discarded: a
    /// `kill()` on an already-exited process (e.g. one whose exit status a
    /// caller already collected via [`shutdown`](Self::shutdown)) is not
    /// actionable and must not panic or be surfaced from a `Drop` impl.
    fn drop(&mut self) {
        let _ = graceful_kill(&self.handle, &mut self.child, self.shutdown_timeout);
    }
}

/// Builds the `nvim --embed` command `cfg` describes, applying
/// [`EngineConfig::env_plan`] entry by entry so the environment the child
/// gets is the plan a caller can inspect, never a second derivation of it.
fn build_command(cfg: &EngineConfig) -> Command {
    let mut command = Command::new(&cfg.nvim_bin);
    command.arg("--embed").args(&cfg.extra_args);
    for (name, value) in cfg.env_plan() {
        match value {
            Some(value) => command.env(name, value),
            None => command.env_remove(name),
        };
    }
    command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    #[cfg(unix)]
    if let Some(relay) = &cfg.stdin_relay {
        use std::os::fd::AsRawFd;
        use std::os::unix::process::CommandExt;
        // copied out to a plain integer so the pre_exec closure below (which
        // must be `'static`) does not have to borrow `cfg`, which does not
        // outlive this function
        let source = relay.as_raw_fd();
        // SAFETY: `relay_stdin_fd` only calls `dup2` and `mem::forget`, both
        // async-signal-safe, and touches no heap allocator or lock -- the
        // constraint `pre_exec` imposes on code running between `fork` and
        // `exec` in the child. See its own doc comment for why the
        // `mem::forget` is required, not incidental.
        #[allow(unsafe_code)]
        unsafe {
            command.pre_exec(move || relay_stdin_fd(source));
        }
    }
    command
}

/// Duplicates `source` onto the child's fd
/// [`crate::nvim_api::STDIN_RELAY_CHILD_FD`], for a caller's own real stdin
/// to reach nvim over a descriptor `--embed`'s RPC channel does not already
/// claim (child fd 0 is the RPC write end nvim reads its own commands from;
/// see `:help ui-startup-stdin`).
///
/// Runs inside [`std::process::Command::pre_exec`], after `fork` and
/// before `exec`, where only async-signal-safe calls are sound:
/// `rustix::io::dup2`/`fcntl_setfd` are, wrapping the raw `dup2(2)`/
/// `fcntl(2)` syscalls directly. `OwnedFd::from_raw_fd` performs no syscall
/// of its own -- it only wraps an integer -- so wrapping the fixed
/// descriptor number here is exactly as safe as anywhere else; what makes
/// it a real, open descriptor is the `dup2` call itself. The `mem::forget`
/// right after is required, not optional: without it, this function's
/// return drops `target`, which closes the very descriptor `dup2` just
/// installed, before `exec` ever replaces the process image and gets a
/// chance to use it.
///
/// `source == STDIN_RELAY_CHILD_FD` is a real, reachable case, not a
/// theoretical one: `std`'s own `AsFd::try_clone_to_owned` (what
/// `main::maybe_relay_stdin` calls on the process's own stdin) allocates at
/// the lowest free descriptor via `F_DUPFD_CLOEXEC`, and with only stdio
/// open at the point `engine_config` runs -- before `Term::init`, before
/// any pipe exists -- that lowest free descriptor already IS fd 3. A
/// `dup2`/`dup3` of a descriptor onto itself is platform-inconsistent
/// (`dup2` is a documented no-op that leaves `FD_CLOEXEC` set, so `exec`
/// closes it anyway; `dup3` rejects equal descriptors with `EINVAL`
/// outright), so this case is handled separately: the descriptor is
/// already the right one, and only its close-on-exec flag needs clearing.
#[cfg(unix)]
fn relay_stdin_fd(source: std::os::fd::RawFd) -> std::io::Result<()> {
    use std::os::fd::{BorrowedFd, FromRawFd, OwnedFd};
    // SAFETY: `source` is a raw copy of the fd `EngineConfig::stdin_relay`
    // owns; that `EngineConfig` (and therefore the fd) outlives the whole
    // `Command::spawn()` call this closure runs inside of, in
    // `Engine::spawn`.
    #[allow(unsafe_code)]
    let source_fd = unsafe { BorrowedFd::borrow_raw(source) };
    if source == crate::nvim_api::STDIN_RELAY_CHILD_FD {
        rustix::io::fcntl_setfd(source_fd, rustix::io::FdFlags::empty())?;
        return Ok(());
    }
    // SAFETY: see this function's own doc comment.
    #[allow(unsafe_code)]
    let mut target = unsafe { OwnedFd::from_raw_fd(crate::nvim_api::STDIN_RELAY_CHILD_FD) };
    rustix::io::dup2(source_fd, &mut target)?;
    std::mem::forget(target);
    Ok(())
}

/// Sends `qa!` as a fire-and-forget notification, polls `try_wait` until
/// the child exits or `shutdown_timeout` elapses, then force-kills and
/// reaps it. Shared by [`Engine::shutdown`] and `Engine`'s `Drop` impl so
/// the two sequences can never drift apart.
///
/// Reports which branch it took in the returned [`ShutdownOutcome`],
/// tagged at the branch itself: that is the only place the two are
/// distinguishable, since by the time a status is in hand the kill and the
/// child's own exit have already merged into one value.
///
/// The `notify` call is best-effort: if the writer thread is already gone
/// (connection already closed, e.g. nvim crashed or the peer wrote garbage
/// mid-session), sending `qa!` fails and this falls straight through to the
/// poll loop, which sees the child has already exited on the very first
/// `try_wait`.
fn graceful_kill(
    handle: &EngineHandle,
    child: &mut Child,
    shutdown_timeout: Duration,
) -> std::io::Result<ShutdownOutcome> {
    let _ = handle.notify("nvim_command", vec![Value::from("qa!")]);
    let deadline = Instant::now() + shutdown_timeout;
    loop {
        if let Some(status) = child.try_wait()? {
            return Ok(ShutdownOutcome {
                path: ShutdownPath::Graceful,
                status,
            });
        }
        if Instant::now() >= deadline {
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    child.kill()?;
    Ok(ShutdownOutcome {
        path: ShutdownPath::Forced,
        status: child.wait()?,
    })
}

/// Maps a child's raw `ExitStatus` to [`ExitInfo`]: a normal exit passes its
/// code through unchanged; a signal death (no exit code at all on Unix) maps
/// to `128 + signal`, the conventional mapping shells already use (`$?`
/// after a `SIGKILL`ed process is 137), so `update()`'s `Effect::Quit` exit
/// code matches what a caller's shell would report for the same death.
#[cfg(unix)]
fn exit_info_from_status(status: ExitStatus) -> ExitInfo {
    use std::os::unix::process::ExitStatusExt;
    match status.code() {
        Some(code) => ExitInfo {
            code: Some(code),
            by_signal: false,
        },
        None => ExitInfo {
            code: status.signal().map(|sig| 128 + sig),
            by_signal: true,
        },
    }
}

/// Non-Unix fallback: there is no signal concept to map, so a missing exit
/// code becomes `None` (unreadable) rather than the misleading default of
/// success.
#[cfg(not(unix))]
fn exit_info_from_status(status: ExitStatus) -> ExitInfo {
    ExitInfo {
        code: status.code(),
        by_signal: false,
    }
}

fn decode_api_info(v: Value) -> Result<ApiInfo, EngineError> {
    let bad = || EngineError::Remote(Value::from("unexpected api_info shape"));
    let Value::Array(parts) = v else {
        return Err(bad());
    };
    let channel_id = parts.first().and_then(Value::as_u64).ok_or_else(bad)?;
    let meta = parts.get(1).ok_or_else(bad)?;
    let version = map_get(meta, "version").ok_or_else(bad)?;
    Ok(ApiInfo {
        channel_id,
        version_major: map_get(&version, "major")
            .and_then(|v| v.as_u64())
            .ok_or_else(bad)?,
        version_minor: map_get(&version, "minor")
            .and_then(|v| v.as_u64())
            .ok_or_else(bad)?,
    })
}

fn map_get(v: &Value, key: &str) -> Option<Value> {
    let Value::Map(pairs) = v else { return None };
    crate::wire::map_find(pairs, key).cloned()
}

#[cfg(test)]
mod config_tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    /// The environment the spawn path actually hands a child, read back off
    /// the built `Command` rather than off [`EngineConfig::env_plan`]: the
    /// plan is what the assertions below are about, and a plan that stopped
    /// reaching `Command` at all would satisfy every one of them.
    fn spawned_env(cfg: &EngineConfig) -> Vec<(OsString, Option<OsString>)> {
        build_command(cfg)
            .get_envs()
            .map(|(name, value)| (name.to_os_string(), value.map(OsStr::to_os_string)))
            .collect()
    }

    #[test]
    fn the_default_config_alters_neither_arguments_nor_environment() {
        let cfg = EngineConfig::default();
        assert!(
            cfg.extra_args.is_empty(),
            "the default spawn is the one the editor uses on a user's \
             behalf and the one the measurement matrix measures a fixture \
             config through; an argument added here (--clean above all) \
             would discard that config and still measure and gate green, \
             got {:?}",
            cfg.extra_args
        );
        assert!(spawned_env(&cfg).is_empty(), "{:?}", spawned_env(&cfg));
        assert!(cfg.env_plan().is_empty(), "{:?}", cfg.env_plan());
    }

    #[test]
    fn an_isolated_config_neutralizes_every_host_config_variable() {
        let plan = spawned_env(&EngineConfig::isolated());
        for name in crate::env::HOST_REDIRECT_VARS {
            assert!(
                plan.contains(&(OsString::from(*name), None)),
                "{name} survives into an isolated child's environment; plan {plan:?}"
            );
        }
        let empty = crate::env::empty_search_path().into_os_string();
        for name in crate::env::HOST_SEARCH_PATH_VARS {
            assert!(
                plan.contains(&(OsString::from(*name), Some(empty.clone()))),
                "{name} is not pointed at an empty directory, so the child \
                 searches a system-wide default; plan {plan:?}"
            );
        }
    }

    #[test]
    fn a_removal_outranks_an_override_of_the_same_variable() {
        let cfg = EngineConfig::default()
            .with_env("VIMINIT", "echo leaked")
            .with_env_remove("VIMINIT");
        assert!(
            spawned_env(&cfg).contains(&(OsString::from("VIMINIT"), None)),
            "an override reinstated a variable a caller asked to be rid of"
        );
    }

    /// The hermetic plan is not a set of entries a caller happens to find in
    /// the two public vectors: emptying both leaves it in force. Compiles
    /// either way, and a child that lost its isolation to it reports
    /// nothing at all.
    #[test]
    fn emptying_a_callers_vectors_leaves_an_isolated_config_isolated() {
        let mut cfg = EngineConfig::isolated();
        cfg.env.clear();
        cfg.env_remove.clear();
        let plan = spawned_env(&cfg);
        assert!(
            plan.contains(&(OsString::from("VIMINIT"), None)),
            "clearing the caller's vectors took the isolation with it; plan {plan:?}"
        );
        let empty = crate::env::empty_search_path().into_os_string();
        assert!(
            plan.contains(&(OsString::from("XDG_CONFIG_DIRS"), Some(empty))),
            "clearing the caller's vectors took the isolation's search path \
             with it; plan {plan:?}"
        );
    }

    /// The other half of the same guarantee: an entry a caller pushes for a
    /// name the hermetic plan covers loses to it, whichever vector it went
    /// into. The composed shape (`with_env` for a variable an isolated spawn
    /// clears) is the one that compiles most readily and silently.
    #[test]
    fn a_callers_own_entry_cannot_outrank_the_isolation() {
        let cfg = EngineConfig::isolated()
            .with_env("VIMINIT", "echo leaked")
            .with_env("XDG_CONFIG_DIRS", "/host/xdg")
            .with_env_remove("XDG_CONFIG_DIRS");
        let plan = spawned_env(&cfg);
        assert!(
            plan.contains(&(OsString::from("VIMINIT"), None)),
            "a caller's override reinstated a variable the isolation \
             removes; plan {plan:?}"
        );
        let empty = crate::env::empty_search_path().into_os_string();
        assert!(
            plan.contains(&(OsString::from("XDG_CONFIG_DIRS"), Some(empty))),
            "a caller's own entry outranked the isolation's search path, so \
             the child searches somewhere the isolation did not choose; plan \
             {plan:?}"
        );
    }

    /// One entry per variable, whichever vector named it: `Command` holds
    /// one disposition per name, so a plan carrying two would report a
    /// variable's fate differently from how the child receives it.
    #[test]
    fn the_plan_carries_one_entry_per_variable() {
        let cfg = EngineConfig::isolated()
            .with_env("VIMINIT", "echo leaked")
            .with_env("XDG_CONFIG_DIRS", "/host/xdg")
            .with_env_remove("XDG_CONFIG_DIRS");
        let mut names: Vec<OsString> = cfg.env_plan().into_iter().map(|(name, _)| name).collect();
        let total = names.len();
        names.sort_unstable();
        names.dedup();
        assert_eq!(total, names.len(), "the plan names a variable twice");
    }

    /// The enumerations are the second layer, not the whole rule: a variable
    /// nobody wrote down still must not reach an isolated child. A denylist
    /// cannot state this, and its incompleteness reports nothing -- the child
    /// starts, measures, and disagrees with another host only once somebody
    /// compares the two.
    #[test]
    fn an_isolated_plan_removes_every_host_variable_the_allowlist_does_not_name() {
        let plan = spawned_env(&EngineConfig::isolated());
        let swept = crate::env::hermetic_sweep();
        assert!(
            !swept.is_empty(),
            "this host exports nothing outside the allowlist, so nothing here \
             is actually asserted"
        );
        let overridden = crate::env::empty_search_path().into_os_string();
        let home = crate::env::hermetic_home().into_os_string();
        for (name, _) in swept {
            let expected = if crate::env::HOST_SEARCH_PATH_VARS
                .iter()
                .any(|search| crate::env::env_names_eq(&name, OsStr::new(search)))
            {
                Some(overridden.clone())
            } else if crate::env::env_names_eq(&name, OsStr::new(crate::env::HERMETIC_HOME_VAR)) {
                Some(home.clone())
            } else {
                None
            };
            assert!(
                plan.contains(&(name.clone(), expected.clone())),
                "{name:?} is exported by this host, is on no allowlist, and \
                 the isolated plan does not give it {expected:?}; plan {plan:?}"
            );
        }
    }

    /// The other half: the sweep is a filter, not an `env_clear`. A child
    /// handed nothing at all cannot resolve a shell, a home directory or its
    /// own runtime files, and every assertion about what the sweep drops
    /// would pass just as green against it.
    #[test]
    fn an_isolated_plan_leaves_the_allowlisted_variables_alone() {
        let plan = spawned_env(&EngineConfig::isolated());
        for name in crate::env::HERMETIC_PASSTHROUGH_VARS {
            assert!(
                !plan
                    .iter()
                    .any(|(planned, _)| crate::env::env_names_eq(planned, OsStr::new(name))),
                "{name} is allowlisted and the isolated plan touches it anyway; \
                 plan {plan:?}"
            );
        }
    }

    /// The programs an isolated child spawns resolve their own configuration
    /// and credentials through `HOME`, which the editor's `XDG_*_HOME`
    /// overrides do nothing about. An isolated plan closes that on two
    /// levels: the git configuration files are pointed at a missing path,
    /// and `HOME` itself at the hardened empty directory for everything no
    /// variable of git's diverts (`.netrc`, `.ssh/`, the ignore-file
    /// default).
    ///
    /// The two file layers are asserted by literal name rather than by
    /// iterating the const the code under test iterates: an entry dropped
    /// from that const would otherwise shrink this assertion along with the
    /// plan, and nothing can plant `/etc/gitconfig` to catch the system
    /// half's loss downstream.
    #[test]
    fn an_isolated_plan_closes_the_config_layers_a_child_subprocess_reads_from_home() {
        let plan = spawned_env(&EngineConfig::isolated());
        let absent = crate::env::absent_config_file().into_os_string();
        for name in ["GIT_CONFIG_GLOBAL", "GIT_CONFIG_SYSTEM"] {
            assert!(
                plan.contains(&(OsString::from(name), Some(absent.clone()))),
                "{name} does not select a missing configuration file, so a \
                 subprocess of an isolated child reads the operator's own; \
                 plan {plan:?}"
            );
        }
        let home = crate::env::hermetic_home().into_os_string();
        assert!(
            plan.contains(&(OsString::from("HOME"), Some(home))),
            "HOME still names the operator's own home, so a subprocess of an \
             isolated child resolves its credentials out of it; plan {plan:?}"
        );
    }

    /// One entry per variable means one entry per *variable*, not per
    /// spelling: a process environment folds names exactly where its host
    /// does, so a plan that kept two spellings apart where the child merges
    /// them would report a disposition the child does not apply.
    ///
    /// The expectation is `cfg!(windows)` rather than the comparison the
    /// code under test uses, so this states something on both platforms and
    /// cannot agree with a broken rule by consulting it: Windows must fold
    /// the two spellings into one entry, Unix must keep them as the two
    /// variables they are there.
    #[test]
    fn the_plan_folds_two_spellings_of_a_name_exactly_where_the_host_does() {
        let cfg = EngineConfig::default()
            .with_env("Path", "first")
            .with_env("PATH", "second");
        let plan = cfg.env_plan();
        let expected = if cfg!(windows) { 1 } else { 2 };
        assert_eq!(
            plan.len(),
            expected,
            "the plan carries {} entries for two spellings the host treats as \
             {expected}; plan {plan:?}",
            plan.len()
        );
    }

    /// The one edge on which this funnel and the pty/`Command` funnel
    /// disagree, pinned so the divergence stays a decision rather than an
    /// accident: a caller who sets a swept name to exactly the host's own
    /// value keeps it here, because this side knows the caller set it.
    #[test]
    fn a_callers_override_matching_the_hosts_value_survives_the_engine_sweep() {
        let (host, value) = crate::env::hermetic_sweep()
            .into_iter()
            .find(|(name, _)| {
                !crate::env::HOST_REDIRECT_VARS
                    .iter()
                    .chain(crate::env::HOST_SEARCH_PATH_VARS)
                    .chain(crate::env::HOST_SUBPROCESS_CONFIG_VARS)
                    .chain(std::iter::once(&crate::env::HERMETIC_HOME_VAR))
                    .any(|fixed| crate::env::env_names_eq(name, OsStr::new(fixed)))
            })
            .expect("this host exports nothing the sweep would drop");
        let plan = spawned_env(&EngineConfig::isolated().with_env(&host, &value));
        assert!(
            plan.contains(&(host.clone(), Some(value.clone()))),
            "{host:?} was swept though the caller planned it; the value \
             matching the host's own is not what decides it on this side; \
             plan {plan:?}"
        );
    }

    /// A caller's own override of a name the sweep would otherwise drop
    /// survives it. The variables a measurement delivers to its child --
    /// fixture `XDG_*_HOME` directories, a tap descriptor, a probe socket --
    /// are all of this shape, and a sweep that took them would leave the
    /// child measuring something other than what the row named.
    #[test]
    fn a_callers_own_override_survives_the_sweep() {
        // taken from the host's own environment rather than invented: a name
        // the sweep never visits would demonstrate nothing about surviving it
        let host = crate::env::hermetic_sweep()
            .into_iter()
            .map(|(name, _)| name)
            .find(|name| {
                !crate::env::HOST_REDIRECT_VARS
                    .iter()
                    .chain(crate::env::HOST_SEARCH_PATH_VARS)
                    .chain(std::iter::once(&crate::env::HERMETIC_HOME_VAR))
                    .any(|fixed| crate::env::env_names_eq(name, OsStr::new(fixed)))
            })
            .expect("this host exports nothing the sweep would drop");
        let cfg = EngineConfig::isolated().with_env(&host, "caller-chose-this");
        let plan = spawned_env(&cfg);
        assert!(
            plan.contains(&(host.clone(), Some(OsString::from("caller-chose-this")))),
            "the sweep dropped {host:?} though the caller set it deliberately; \
             plan {plan:?}"
        );
    }
}

#[cfg(all(test, unix))]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;
    use std::os::unix::process::ExitStatusExt;

    #[test]
    fn wait_exit_mapping_passes_normal_exit_code_through() {
        // raw wait-status encoding: exit code lives in bits 8-15
        let status = ExitStatus::from_raw(5 << 8);
        let info = exit_info_from_status(status);
        assert_eq!(info.code, Some(5));
        assert!(!info.by_signal);
    }

    #[test]
    fn wait_exit_mapping_maps_signal_death_to_128_plus_signal() {
        // raw wait-status encoding: a nonzero low 7 bits with no exit code
        // means "terminated by signal N", here SIGKILL (9)
        let status = ExitStatus::from_raw(9);
        let info = exit_info_from_status(status);
        assert_eq!(info.code, Some(137));
        assert!(info.by_signal);
    }

    /// `build_command`'s `pre_exec` closure is opaque to `Command`'s own
    /// introspection (`get_envs`/`get_args` say nothing about it), so the
    /// fd-duplication path itself is only provable end-to-end against a
    /// spawned child; what a unit test can pin is the flag that decides
    /// whether `build_command` installs it at all.
    #[test]
    fn stdin_relay_requested_tracks_whether_a_relay_fd_was_armed() {
        assert!(!EngineConfig::default().stdin_relay_requested());
        let dev_null = std::fs::File::open("/dev/null").expect("/dev/null always opens");
        let cfg = EngineConfig::default().with_stdin_relay(dev_null.into());
        assert!(cfg.stdin_relay_requested());
    }
}
