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
use crate::heartbeat::{HeartbeatProber, HeartbeatWatch};
use rmpv::Value;
use std::ffi::{OsStr, OsString};
use std::path::PathBuf;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};
use view_core::msg::ExitInfo;
use view_core::sink::MsgSink;

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
    /// The read side's liveness watch, fed by a prober thread this engine
    /// spawned and folded by whichever thread observes it. A public field
    /// rather than an accessor because the one caller reads it in the same
    /// expression as [`handle`](Self::handle), which an accessor borrowing
    /// all of `Engine` would forbid.
    pub heartbeat: HeartbeatWatch,
    command_line: Vec<OsString>,
}

// A comment saying the feature must never be shipped is not a mechanism, so
// an optimized build carrying it has to name the campaign it exists for:
// `task heartbeat-ab` sets VIEW_BENCH_NO_HEARTBEAT for the two
// counterfactual binaries it builds, and nothing else in the tree sets it,
// so every other release build with the prober compiled out fails to
// compile instead of becoming an artifact with no supervision in it.
// `debug_assertions` is what separates the two cases: a debug build cannot
// be mistaken for a shipped editor, so a lint or test leg may compile the
// arm below with no ceremony, while the builds that could plausibly leave
// the machine must be deliberate. `option_env!` is tracked by cargo, so
// flipping the variable rebuilds rather than reusing a cached verdict.
#[cfg(all(feature = "bench-no-heartbeat", not(debug_assertions)))]
const _: () = assert!(
    option_env!("VIEW_BENCH_NO_HEARTBEAT").is_some(),
    "bench-no-heartbeat compiles out read-side hang supervision and must never ship: set \
     VIEW_BENCH_NO_HEARTBEAT=1 in the build environment (as `task heartbeat-ab` does) if this \
     optimized build really is the paired campaign's counterfactual arm"
);

/// Starts the one thread that owns the heartbeat cadence, ticking `prober`
/// every [`crate::heartbeat::HEARTBEAT_PROBE_INTERVAL`] against `handle`.
///
/// A thread of its own because neither the request seam the probe rides nor
/// the reply seam its answer comes back on has any notion of a clock: they
/// send when told and deliver when answered. It cannot be the runtime
/// loop's thread, which must never originate the send, nor the reader
/// thread, which must never block -- and a timer is a block.
///
/// Detached, and ends itself: the first tick the connection refuses retires
/// it -- paused or armed alike, which is the whole reason
/// [`HeartbeatProber::tick`] answers for the connection before it answers
/// for the pause -- so a replaced engine leaves no prober behind and a
/// process shutting down waits for nothing. That costs at most one interval
/// of lag between the connection closing and the thread noticing, during
/// which the ticks it issues are refused rather than written.
///
/// Under `bench-no-heartbeat` there is no thread and no tick at all: the
/// measurement arm that binary exists for has to be the absence of this
/// cadence, not a paused copy of it, so the counterfactual is a compilation
/// without the prober rather than one that still wakes on the interval.
fn spawn_prober(prober: HeartbeatProber, handle: EngineHandle) {
    // consumed rather than returned around: the arm below is compiled out
    // in this configuration, so nothing follows this to skip
    #[cfg(feature = "bench-no-heartbeat")]
    let _ = (prober, handle);
    #[cfg(not(feature = "bench-no-heartbeat"))]
    std::thread::spawn(move || loop {
        std::thread::sleep(crate::heartbeat::HEARTBEAT_PROBE_INTERVAL);
        if prober.tick(&handle).is_err() {
            break;
        }
    });
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
    /// handshake. The child carries the swap-prompt autocommand every spawn
    /// is given (see [`SWAP_RECOVERY_CMD`]).
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
        // read back off the `Command` that is about to be spawned rather than
        // re-derived from `cfg`: a second derivation is free to drift from
        // what the child actually receives, and the whole point of exposing
        // this is to be able to assert on the real thing
        let command_line: Vec<OsString> = std::iter::once(command.get_program().to_os_string())
            .chain(command.get_args().map(std::ffi::OsStr::to_os_string))
            .collect();
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
        let heartbeat = HeartbeatWatch::default();
        spawn_prober(heartbeat.prober(), handle.clone());
        Ok(Self {
            handle,
            child,
            shutdown_timeout: cfg.shutdown_timeout,
            api_info,
            pump,
            heartbeat,
            command_line,
        })
    }

    /// Tears the current child down and brings a fresh one up from `cfg`,
    /// with nvim's own recovery flag applied whenever `cfg` names a file for
    /// it to act on (see [`with_recovery`]), so the replacement opens what
    /// its predecessor left in a swap file rather than what is on disk.
    ///
    /// The teardown is the existing `Drop` sequence, unchanged and not
    /// duplicated: `qa!`, a bounded wait, then `SIGKILL` and a reap. The old
    /// handle is consumed and the new one returned, matching
    /// [`spawn`](Self::spawn)'s own ownership shape -- there is never a
    /// moment at which two live engines exist for one session, and a caller
    /// cannot keep addressing the connection it just replaced.
    ///
    /// The replacement carries a fresh [`heartbeat`](Self::heartbeat),
    /// because it is a fresh [`spawn`](Self::spawn): a watch carried across
    /// the boundary would arrive holding the dead engine's unanswered
    /// probes and the silence they accumulated, and would read the healthy
    /// replacement as wedged on the first observation.
    ///
    /// Recovers exactly what nvim's own swap file holds and nothing else.
    /// view keeps no copy of buffer text to reconcile against -- nvim owns
    /// it -- so the guarantee here is nvim's own crash-recovery guarantee,
    /// no stronger: edits made since the last swap flush are gone, and the
    /// window layout is not recovered at all.
    ///
    /// # Errors
    ///
    /// The same shapes [`spawn`](Self::spawn) returns, for the same reasons.
    /// A restart that fails leaves no engine at all: the old child is
    /// already gone by then, so a caller holds nothing to retry with and
    /// owes the user a report rather than a silent second attempt.
    pub fn restart(self, cfg: EngineConfig) -> Result<Self, EngineError> {
        // explicit, and the whole teardown: `Drop` runs the graceful-then-
        // forced sequence on every drop path, so re-implementing it here
        // would be a second copy free to drift from the one every other
        // shutdown takes
        drop(self);
        Self::spawn(with_recovery(cfg))
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
    ///
    /// The caller owes `sink` a consumer for the engine's whole lifetime,
    /// and a draining one: an undeliverable `Msg::EngineRequest` is fatal to
    /// the reader thread, which stops reading the wire (see [`damage`]'s
    /// module doc), and a sink whose receiver is merely full stalls every
    /// message queued behind it. Dropping the receiver -- the shape a test
    /// falls into by binding it as `_rx` -- is therefore not a way to ignore
    /// the traffic; it is a way to lose the session.
    ///
    /// [`damage`]: crate::damage
    #[must_use]
    pub fn start_pump(
        &mut self,
        sink: impl MsgSink + Send + Sync + 'static,
    ) -> (DamagePump, SinkCutover) {
        // the heartbeat is armed here rather than at spawn because this is
        // the first moment a reply can reach the consumer that folds it:
        // anything the pump staged before this call is returned as
        // `SinkCutover` and replayed through the caller's own dispatch,
        // never through the acknowledgement path, so a probe issued before
        // now would be charged to the engine as silence it did not owe.
        // After the attach, not before: a tick landing in the window
        // between the two would have its reply staged rather than sunk, and
        // that generation would stay outstanding for the session's life.
        let attached = self.pump.attach_sink(sink);
        self.heartbeat.resume();
        attached
    }

    /// How long a stop resolution waits for the reader thread to publish
    /// what it found, once the child itself is already reaped. It is a
    /// drain of an at-EOF stream, not work: this is a ceiling on a thread
    /// that never wakes, not a budget anything is expected to spend.
    const READER_SETTLE: Duration = Duration::from_millis(250);

    /// Resolves the whole stop: the child's real exit status, and whether
    /// the engine announced it was leaving before the connection went
    /// ([`EngineHandle::announced_exit`](crate::handle::EngineHandle::announced_exit)).
    ///
    /// One call rather than two reads because the two are only meaningful
    /// together and only in this order. The status is resolved first, which
    /// reaps the child and so guarantees the reader's stream is at EOF; the
    /// wait that follows is what makes the announcement's absence mean
    /// "there was none" rather than "the reader had not got to it yet".
    ///
    /// A bounded (up to `shutdown_timeout` plus [`READER_SETTLE`]) block, on
    /// a transition that happens at most once per engine.
    pub fn stop_report(&mut self) -> (ExitInfo, bool) {
        let exit = self.wait_exit();
        self.handle.wait_until_settled(Self::READER_SETTLE);
        (exit, self.handle.announced_exit())
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

    /// This connection's generation: an id no other connection this process
    /// opens has held, carried by every `Msg::EngineStopped` its reader
    /// routes.
    ///
    /// One loop channel serves every engine a session opens, and the reader
    /// of an engine being replaced posts its stop *after* the replacement is
    /// live -- the teardown [`restart`](Self::restart) performs is what
    /// produces that stop. Comparing this against the stop's own stamp is
    /// how a caller separates a stop from the connection it currently runs
    /// from a stop belonging to one it already replaced.
    #[must_use]
    pub fn generation(&self) -> u64 {
        self.pump.generation()
    }

    /// The exact command line this engine's child was spawned with, program
    /// name first: read off the `Command` at spawn time, never re-derived
    /// from the config.
    ///
    /// The one place the argument-shaping rules in [`with_recovery`] can be
    /// asserted for what they actually delivered on every platform. Reading
    /// it back out of the OS process table instead is a Linux-only luxury
    /// (`/proc/<pid>/cmdline`), and a test written against that alone proves
    /// nothing anywhere else.
    #[must_use]
    pub fn command_line(&self) -> &[OsString] {
        &self.command_line
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

/// Answers an otherwise unanswered swap-file prompt with "recover", on every
/// spawn, before any file is opened.
///
/// Finding a swap file is a question nvim asks its user
/// (`[O]pen/[E]dit/[R]ecover/[D]elete/[Q]uit`) and then blocks its own main
/// loop until somebody answers. An embedded engine meets that question at
/// the two moments it can least afford to stop: during startup, before the
/// frontend has a frame to draw the question in, and inside the `:edit` a
/// later file open turns into, which then never returns. `v:swapchoice` is
/// nvim's own documented way for an autocommand to answer on the user's
/// behalf (`:help SwapExists`), and `r` is the answer that keeps the work --
/// recover from the swap, the same outcome [`RECOVERY_ARG`] gives a restart.
///
/// # Only the swap nobody still owns
///
/// The answer is given only when `v:swapchoice` is still empty, which is the
/// difference between recovering a dead session's work and stealing a live
/// one's. nvim ships its own `SwapExists` handler in the `nvim.swapfile`
/// group, it runs first, and against the pinned engine it decides the two
/// cases apart: a swap whose owning process is gone reads back
/// `swapinfo().pid == 0`, so nvim leaves `v:swapchoice` empty for a human to
/// answer -- the crash case, and the one this recovers. A swap whose owner is
/// still running is answered `e` with a `W325` warning, and a second view
/// started on that file must keep exactly that behavior: two sessions
/// recovering one swap end holding divergent unsaved copies of the file, with
/// nothing on screen to say so.
///
/// # In a group of its own
///
/// nvim passes `-u` through verbatim, so a migrated vimrc runs whatever it
/// already said, and a bare `autocmd!` at the top of one is a common way for
/// a config to claim the autocommands from there on. Ungrouped, that line
/// deletes this guard and silently reinstates both hangs. The named group is what survives
/// it, the same hardening nvim's own defaults use for the same reason.
///
/// # Why a startup argument
///
/// Measured against the pinned engine rather than assumed. An autocommand
/// registered over the RPC channel after the handshake and before
/// `nvim_ui_attach` does fire: it runs, and `v:swapchoice` reads back `r`
/// inside it from both Lua and Vimscript. nvim still shows `E325` and parks
/// the session on the dialog, then applies the dialog's own default, which
/// opens the file from disk and drops everything the swap held. The identical
/// autocommand given as `--cmd` recovers the swap and leaves an ordinary
/// session (mode `n`, nothing blocking), which is also what a real terminal
/// nvim does with it.
///
/// `g:view_swap_recovered` counts the prompts answered here, and only those:
/// a prompt nvim answered itself leaves the counter where it was. Answering
/// is what erases every other trace that one was asked: no `E325` notice, no
/// message to the UI, and a recovered buffer that looks exactly like a buffer
/// whose file simply held that text.
const SWAP_RECOVERY_CMD: &str = "lua vim.api.nvim_create_autocmd('SwapExists', { \
     group = vim.api.nvim_create_augroup('view_swap_recovery', { clear = true }), \
     pattern = '*', \
     desc = 'Recover a swap file no live process still owns', \
     callback = function() \
     if vim.v.swapchoice ~= '' then return end \
     vim.v.swapchoice = 'r' \
     vim.g.view_swap_recovered = (vim.g.view_swap_recovered or 0) + 1 \
     end, \
     })";

/// nvim's own crash-recovery flag: the replacement engine opens each file it
/// was given from that file's swap file instead of from disk.
///
/// Passed through [`EngineConfig::extra_args`] like any other engine
/// passthrough argument, so [`build_command`] needs no notion of recovery at
/// all.
const RECOVERY_ARG: &str = "-r";

/// Applies [`RECOVERY_ARG`] to a restart's config, but only for a spawn that
/// names a file for it to act on.
///
/// The condition is nvim's own, measured against the pinned engine rather
/// than assumed: `-r` with a file recovers that file's swap and leaves an
/// ordinary editable session behind (mode `n`, nothing blocking), while `-r`
/// with no file at all means "list every swap file you can find", which
/// prints that list to a UI that has just attached, parks the engine at the
/// prompt acknowledging it, and then exits. A restart is exactly the moment
/// an engine must come back up, so the flag is applied where it recovers
/// something and withheld where it would end the replacement.
///
/// Position is irrelevant to nvim, which reads options wherever they appear
/// ahead of `--`, so this appends rather than splicing ahead of the file
/// arguments a caller already put in `extra_args`.
fn with_recovery(mut cfg: EngineConfig) -> EngineConfig {
    // never twice: a session that restarts twice would otherwise hand nvim
    // `-r -r`, and a config a caller built with the flag already on it is a
    // config that means it once
    let already = cfg.extra_args.iter().any(|arg| arg == RECOVERY_ARG);
    if names_a_file(&cfg.extra_args) && !already {
        cfg.extra_args.push(OsString::from(RECOVERY_ARG));
    }
    cfg
}

/// nvim options that take no value of their own, so an ordinary word
/// following one of them is a file name rather than that option's argument.
///
/// The list is deliberately the *short* half of nvim's option set: an
/// argument this build does not recognise is assumed to take a value, which
/// is the reading that withholds [`RECOVERY_ARG`] (see [`names_a_file`]).
const OPTIONS_TAKING_NO_VALUE: [&str; 25] = [
    "--clean",
    "--embed",
    "--headless",
    "--api-info",
    "--version",
    "--help",
    "--noplugin",
    "-v",
    "-h",
    "-n",
    "-N",
    "-R",
    "-d",
    "-b",
    "-m",
    "-M",
    "-Z",
    "-e",
    "-E",
    "-es",
    "-Es",
    "-A",
    "-o",
    "-O",
    "-p",
];

/// Whether `args` names at least one file for nvim to open.
///
/// Errs towards "no" in every case this build cannot read confidently, and
/// the caller's use of the answer is what makes that the safe direction: a
/// missed file costs a restart the recovery flag, and the `SwapExists`
/// autocommand every spawn carries ([`SWAP_RECOVERY_CMD`]) still recovers the
/// swap when the file is opened, while a value mistaken for a file costs the
/// replacement engine its life.
fn names_a_file(args: &[OsString]) -> bool {
    let mut expect_value = false;
    for (index, arg) in args.iter().enumerate() {
        let arg = arg.to_string_lossy();
        // nvim's own rule: only file names after this
        if arg == "--" {
            return index + 1 < args.len();
        }
        if expect_value {
            expect_value = false;
            continue;
        }
        // `-l <script> [args...]` hands everything that follows to a Lua
        // script, so nothing after it is a file nvim opens
        if arg == "-l" {
            return false;
        }
        // nvim's `-` is an operand, not an option: it names the buffer fed
        // from piped stdin. Reading it as an option would consume whatever
        // followed it as that option's value, so `cmd | view - notes.md`
        // would lose the real file too
        if arg == "-" {
            return true;
        }
        if arg.starts_with('-') || arg.starts_with('+') {
            expect_value = !takes_no_value(&arg);
            continue;
        }
        return true;
    }
    false
}

/// Whether `arg` is an option this build knows carries its own value, or
/// none at all.
fn takes_no_value(arg: &str) -> bool {
    OPTIONS_TAKING_NO_VALUE.contains(&arg)
        // `-V[N][file]`, `-o[N]`, `-O[N]`, `-p[N]`: the value, when there is
        // one, is attached rather than separate
        || arg.strip_prefix("-V").is_some_and(|rest| rest.chars().all(char::is_numeric) || !rest.is_empty())
        || ["-o", "-O", "-p"]
            .iter()
            .any(|flag| arg.strip_prefix(flag).is_some_and(|n| !n.is_empty() && n.chars().all(|c| c.is_ascii_digit())))
        // `--name=value` carries its value inside the argument
        || (arg.starts_with("--") && arg.contains('='))
        // `+cmd` is a whole command in one argument
        || arg.starts_with('+')
}

/// Builds the `nvim --embed` command `cfg` describes, applying
/// [`EngineConfig::env_plan`] entry by entry so the environment the child
/// gets is the plan a caller can inspect, never a second derivation of it.
///
/// # The one `--cmd` this spends
///
/// [`SWAP_RECOVERY_CMD`] rides every spawn as a `--cmd`, and nvim caps how
/// many of those it accepts. Measured against the pinned engine (v0.12.4):
/// `--cmd` and `-c`/`+cmd` are budgeted separately at ten each, so a config
/// whose [`EngineConfig::extra_args`] carry ten `-c` arguments is unaffected
/// and one carrying ten `--cmd` arguments is one over. Going over is a hard
/// startup failure, never a dropped argument: nvim prints `Too many
/// "+command", "-c command" or "--cmd command" arguments` and exits 1 before
/// the RPC channel exists, which a caller sees as a handshake that never
/// happens. Nine `--cmd` arguments are what a config may still spend.
fn build_command(cfg: &EngineConfig) -> Command {
    let mut command = Command::new(&cfg.nvim_bin);
    // ahead of `extra_args`, which is where a caller's file arguments live:
    // nvim runs `--cmd` commands before it opens any of them, and an
    // autocommand registered after the file it is meant to guard is already
    // open guards nothing
    command
        .arg("--embed")
        .arg("--cmd")
        .arg(SWAP_RECOVERY_CMD)
        .args(&cfg.extra_args);
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
    use std::os::fd::{FromRawFd, OwnedFd};
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
    /// introspection (`get_envs`/`get_args` say nothing about it), so
    /// *whether the closure gets installed at all* is only provable
    /// end-to-end against a spawned child; what a unit test can pin here is
    /// the flag that decides that. `relay_stdin_fd`'s own fd-flag effect,
    /// once installed, is directly testable without a spawn at all -- see
    /// `relay_stdin_fd_clears_cloexec_in_place_when_source_already_is_fd_3`
    /// below.
    #[test]
    fn stdin_relay_requested_tracks_whether_a_relay_fd_was_armed() {
        assert!(!EngineConfig::default().stdin_relay_requested());
        let dev_null = std::fs::File::open("/dev/null").expect("/dev/null always opens");
        let cfg = EngineConfig::default().with_stdin_relay(dev_null.into());
        assert!(cfg.stdin_relay_requested());
    }

    /// `relay_stdin_fd`'s own fd-flag effect, isolated from the spawn
    /// plumbing above: calls it directly against a descriptor already
    /// forced onto `STDIN_RELAY_CHILD_FD` with `FD_CLOEXEC` explicitly SET
    /// beforehand, then reads the flag straight back with `fcntl`
    /// (`F_GETFD`).
    ///
    /// The explicit re-arm matters: `dup2` to a *different* destination fd
    /// unconditionally clears `FD_CLOEXEC` on arrival regardless of the
    /// source's own flags, so landing the probe on fd 3 and calling
    /// `relay_stdin_fd` right after -- without setting the flag back first
    /// -- would start from "already cleared" and could never tell the
    /// self-dup branch's own no-op-preserves-whatever-was-there behavior
    /// apart from the fix's explicit clear: both would land on the same
    /// end state by accident, not because the code under test did
    /// anything. Confirmed falsifiable by hand: reverting
    /// `relay_stdin_fd`'s self-dup branch to a plain
    /// `dup2(source_fd, &mut target)` (dup2-onto-self, a documented no-op
    /// that leaves whatever `FD_CLOEXEC` state was already there
    /// untouched) makes this test fail; the real fix
    /// (`fcntl_setfd(FdFlags::empty())`) passes.
    ///
    /// Seizing the process-global fd slot 3 can't happen in the shared
    /// `--lib` test binary's process: `task test` runs that binary
    /// multi-threaded, and sibling tests elsewhere in the crate (file
    /// opens in particular) can legitimately hold or want that slot at the
    /// same moment, racing this test's `dup2`/`fcntl` calls. So this
    /// function is a thin re-exec wrapper: absent the child marker env
    /// var, it spawns a *fresh* invocation of this same test binary
    /// filtered to itself alone (`--exact ... --test-threads=1`) and waits
    /// on it; only the resulting child process, which owns fd 3 with no
    /// other test running anywhere in it, does the actual seizure below.
    #[test]
    fn relay_stdin_fd_clears_cloexec_in_place_when_source_already_is_fd_3() {
        const CHILD_MARKER: &str = "VIEW_ENGINE_FD3_TEST_CHILD";
        const TEST_PATH: &str =
            "process::tests::relay_stdin_fd_clears_cloexec_in_place_when_source_already_is_fd_3";

        if std::env::var_os(CHILD_MARKER).is_none() {
            let exe = std::env::current_exe()
                .expect("the running test binary always resolves its own path");
            let status = std::process::Command::new(exe)
                .arg(TEST_PATH)
                .arg("--exact")
                .arg("--test-threads=1")
                .arg("--nocapture")
                .env(CHILD_MARKER, "1")
                .status()
                .expect("spawn the isolated child that owns fd 3 alone");
            assert!(
                status.success(),
                "the isolated fd-3 child test failed ({status:?}); its own \
                 stderr (inherited, not captured by this wrapper) carries \
                 the actual assertion failure"
            );
            return;
        }

        use rustix::fd::AsFd;
        use rustix::io::FdFlags;

        let content = std::env::temp_dir().join(format!(
            "view-engine-relay-stdin-fd-unit-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::write(&content, "probe").expect("scratch file for the fd under test");
        let opened = std::fs::File::open(&content).expect("just wrote it");
        std::fs::remove_file(&content).ok();

        // Land the probe on a scratch fd first, never fd 3 directly, so
        // nothing here ever holds two `OwnedFd`s pointing at the same raw
        // number at once -- std aborts on drop if it catches that.
        let source: OwnedFd =
            rustix::io::fcntl_dupfd_cloexec(opened.as_fd(), 64).expect("dup onto a scratch fd");
        drop(opened);

        // Force it onto fd 3 via dup2-to-a-different-destination (clears
        // `FD_CLOEXEC` on arrival, as noted above), then explicitly set the
        // flag back so the probe starts from the state this test needs.
        // Safe here specifically because this process was spawned solely
        // to run this one test: no sibling test anywhere in it holds or
        // expects anything at fd 3.
        #[allow(unsafe_code)]
        let mut relay_fd = unsafe { OwnedFd::from_raw_fd(crate::nvim_api::STDIN_RELAY_CHILD_FD) };
        rustix::io::dup2(source.as_fd(), &mut relay_fd).expect("force the probe onto fd 3");
        drop(source);
        rustix::io::fcntl_setfd(relay_fd.as_fd(), FdFlags::CLOEXEC)
            .expect("re-arm FD_CLOEXEC before the call under test");
        assert!(
            rustix::io::fcntl_getfd(relay_fd.as_fd())
                .expect("read the flag back")
                .contains(FdFlags::CLOEXEC),
            "test setup must start with FD_CLOEXEC set, or this test can't \
             discriminate the pre-fix self-dup2 no-op from the fix"
        );

        relay_stdin_fd(crate::nvim_api::STDIN_RELAY_CHILD_FD)
            .expect("relay_stdin_fd must succeed against fd 3 with itself as the source");

        let flags_after = rustix::io::fcntl_getfd(relay_fd.as_fd()).expect("read the flag back");
        assert!(
            !flags_after.contains(FdFlags::CLOEXEC),
            "relay_stdin_fd must clear FD_CLOEXEC on fd 3 in place when the \
             source already is fd 3, or the descriptor is silently closed \
             at exec and the piped content never reaches the child -- got \
             {flags_after:?}"
        );

        // Unlike the production `pre_exec` caller this mirrors (which
        // `mem::forget`s because `exec` immediately takes the process
        // over), no `exec` follows here -- this process's only job was
        // this one test, and it's done, so close the fd deterministically
        // rather than leaking it for whatever remains of the process.
        drop(relay_fd);
    }

    fn args(args: &[&str]) -> Vec<OsString> {
        args.iter().map(OsString::from).collect()
    }

    /// The reading the recovery flag turns on: a plain word nvim would open.
    #[test]
    fn a_plain_argument_is_the_file_that_makes_recovery_meaningful() {
        assert!(names_a_file(&args(&["notes.md"])));
        assert!(names_a_file(&args(&["--clean", "-n", "notes.md"])));
        assert!(names_a_file(&args(&["-R", "notes.md", "other.md"])));
        assert!(names_a_file(&args(&["--", "-not-an-option"])));
        assert!(names_a_file(&args(&["+42", "notes.md"])));
        assert!(names_a_file(&args(&["--cmd=set nu", "notes.md"])));
        // `cmd | view -`: the dash is the buffer, and the word after one is
        // never its value
        assert!(names_a_file(&args(&["-"])));
        assert!(names_a_file(&args(&["-", "notes.md"])));
        assert!(names_a_file(&args(&["--clean", "-"])));
    }

    /// Every reading that must withhold it. The value cases are the ones
    /// that matter: a restart that mistook `NONE` for a file would hand nvim
    /// `-r` with nothing to recover, which lists swap files and exits.
    #[test]
    fn an_option_value_is_never_mistaken_for_a_file() {
        assert!(!names_a_file(&args(&[])));
        assert!(!names_a_file(&args(&["--clean", "-n"])));
        assert!(!names_a_file(&args(&["-u", "NONE"])));
        assert!(!names_a_file(&args(&["--cmd", "set nu"])));
        assert!(!names_a_file(&args(&["-c", "q"])));
        assert!(!names_a_file(&args(&["--listen", "127.0.0.1:1234"])));
        assert!(!names_a_file(&args(&["-l", "script.lua", "notes.md"])));
        assert!(!names_a_file(&args(&["--"])));
        // an option this build has never heard of is assumed to consume the
        // next word, which is the reading that withholds the flag
        assert!(!names_a_file(&args(&["--future-option", "value"])));
    }

    #[test]
    fn recovery_is_applied_exactly_once_and_only_with_a_file_to_recover() {
        let recovered = with_recovery(EngineConfig::default().with_arg("notes.md"));
        assert_eq!(
            recovered.extra_args,
            args(&["notes.md", RECOVERY_ARG]),
            "a config naming a file must gain nvim's recovery flag"
        );

        let bare = with_recovery(EngineConfig::isolated());
        assert!(
            !bare.extra_args.iter().any(|arg| arg == RECOVERY_ARG),
            "a config naming no file must not gain a flag that lists swap \
             files and exits: {:?}",
            bare.extra_args
        );

        let twice = with_recovery(with_recovery(EngineConfig::default().with_arg("notes.md")));
        assert_eq!(
            twice
                .extra_args
                .iter()
                .filter(|arg| *arg == RECOVERY_ARG)
                .count(),
            1,
            "a second restart must not stack a second -r"
        );
    }

    /// Position is the whole guarantee here: nvim runs `--cmd` commands in
    /// the order it is given them and before it opens any file, so the
    /// autocommand must precede whatever the caller put in `extra_args`.
    #[test]
    fn every_spawn_carries_the_swap_answer_ahead_of_the_files_it_opens() {
        let command = build_command(&EngineConfig::default().with_arg("notes.md"));
        let spawned: Vec<OsString> = command
            .get_args()
            .map(std::ffi::OsStr::to_os_string)
            .collect();
        assert_eq!(
            spawned,
            args(&["--embed", "--cmd", SWAP_RECOVERY_CMD, "notes.md"]),
            "the swap answer must ride every spawn, ahead of its files"
        );
    }

    /// The two properties a live test proves end to end, pinned here as text
    /// as well, since either can be dropped by an edit that still leaves the
    /// autocommand firing and every command line unchanged: the group that
    /// survives a vimrc's `autocmd!`, and the emptiness check that leaves a
    /// live owner's swap to nvim.
    #[test]
    fn the_swap_answer_is_grouped_and_answers_only_an_unclaimed_prompt() {
        assert!(
            SWAP_RECOVERY_CMD.contains("nvim_create_augroup('view_swap_recovery'"),
            "an ungrouped autocommand is deleted by a vimrc's bare autocmd!: \
             {SWAP_RECOVERY_CMD}"
        );
        assert!(
            SWAP_RECOVERY_CMD.contains("if vim.v.swapchoice ~= '' then return end"),
            "an unconditional answer overrules nvim's own verdict on a swap \
             a live process still owns: {SWAP_RECOVERY_CMD}"
        );
    }

    /// One `--cmd`, never two: nvim caps them at ten (see [`build_command`]),
    /// and a second one spent here would come out of the caller's own budget.
    #[test]
    fn a_spawn_spends_exactly_one_cmd_argument() {
        let command = build_command(&EngineConfig::isolated());
        assert_eq!(
            command
                .get_args()
                .filter(|arg| *arg == std::ffi::OsStr::new("--cmd"))
                .count(),
            1,
            "the swap answer must cost the caller one --cmd slot, no more"
        );
    }
}
