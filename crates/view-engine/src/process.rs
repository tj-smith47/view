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

/// A remote target an engine spawn is routed to over the system `ssh`
/// client, resolved from the CLI's remote flags into the arguments the
/// spawn path needs.
///
/// Kept as a small, explicit struct rather than a pre-built argument vector
/// so a caller can inspect what was resolved (auth-failure guidance, a
/// remote-session introspector) without re-parsing an argument vector.
///
/// Assumes a POSIX remote shell: the constructed command line is
/// POSIX-shell-shaped and is not designed for a `cmd.exe` or PowerShell
/// remote target.
///
/// `#[non_exhaustive]`: cross-crate callers build one from
/// [`new`](Self::new) and the `with_*` methods below, so a field added here
/// arrives with a default rather than breaking every construction site.
#[non_exhaustive]
#[derive(Debug, Clone)]
pub struct RemoteSpec {
    /// The destination, in ssh's own `[user@]host` syntax verbatim: it is
    /// handed to the client unparsed, so an alias defined in the user's own
    /// `~/.ssh/config` resolves exactly as it does on their command line.
    ///
    /// It must name a host: an empty destination, or one beginning with a
    /// dash (which a client reads as one of its own options), is refused by
    /// [`Engine::spawn`].
    pub target: String,
    /// The `nvim` binary on the far side, resolved against the remote
    /// user's own `PATH`. `"nvim"` by default.
    ///
    /// This is the binary a remote spawn runs; the local
    /// [`EngineConfig::nvim_bin`] has no bearing on one.
    pub remote_nvim_bin: String,
    /// The port passed to the client as `-p`, or `None` to leave the
    /// client's own resolution (`~/.ssh/config`, the default 22) alone.
    pub port: Option<u16>,
    /// `KEY=VALUE` pairs, each passed to the client as its own `-o`
    /// argument, in the order given.
    ///
    /// They follow view's own options rather than preceding them, and a
    /// client takes the *first* value it obtains for an option. An entry
    /// here that names an option view already set is therefore accepted and
    /// has no effect: `BatchMode=no` cannot re-arm the interactive prompt an
    /// embedded editor has no way to answer, and a `-o Port=` entry does not
    /// displace [`port`](Self::port). Everything else applies normally.
    pub extra_ssh_opts: Vec<String>,
    /// The local ssh client to run. `"ssh"` by default, resolved on the
    /// spawning process's own `PATH`.
    ///
    /// A field rather than a hardcoded program name because a remote spawn
    /// applies no environment to the local client at all (its plan belongs
    /// to the editor on the far side, and an override such as `HOME` or
    /// `PATH` applied here would redirect the client's own key and
    /// configuration lookup instead). With no environment to configure,
    /// there is nothing to select a different client through short of the
    /// spawning process's own `PATH`, which is process-global. An operator
    /// pinning one build of the client, and a test double standing in for
    /// it, both go here.
    pub ssh_bin: PathBuf,
}

impl RemoteSpec {
    /// A spec for `target`, in ssh's own `[user@]host` syntax, with every
    /// other setting at the client's own default.
    #[must_use]
    pub fn new(target: impl Into<String>) -> Self {
        Self {
            target: target.into(),
            remote_nvim_bin: String::from("nvim"),
            port: None,
            extra_ssh_opts: vec![],
            ssh_bin: PathBuf::from("ssh"),
        }
    }

    /// The `nvim` binary to run on the far side, replacing the remote
    /// `PATH` lookup of `nvim`.
    #[must_use]
    pub fn with_remote_nvim_bin(mut self, bin: impl Into<String>) -> Self {
        self.remote_nvim_bin = bin.into();
        self
    }

    /// The port the client connects to, replacing its own resolution.
    #[must_use]
    pub fn with_port(mut self, port: u16) -> Self {
        self.port = Some(port);
        self
    }

    /// Appends one `KEY=VALUE` client option, passed through as `-o`. An
    /// option view sets for itself wins over one appended here (see
    /// [`extra_ssh_opts`](Self::extra_ssh_opts)).
    #[must_use]
    pub fn with_ssh_opt(mut self, opt: impl Into<String>) -> Self {
        self.extra_ssh_opts.push(opt.into());
        self
    }

    /// The local ssh client to run, replacing the `PATH` lookup of `ssh`
    /// (see the [`ssh_bin`](Self::ssh_bin) field).
    #[must_use]
    pub fn with_ssh_bin(mut self, bin: impl Into<PathBuf>) -> Self {
        self.ssh_bin = bin.into();
        self
    }

    /// The arguments the client receives ahead of the remote command: view's
    /// own two connection options, the port when one was resolved, every
    /// entry from [`extra_ssh_opts`](Self::extra_ssh_opts), and finally the
    /// destination. Everything the client reads for itself, and nothing it
    /// sends.
    ///
    /// `BatchMode=yes` is the transport analogue of the rule that no paint
    /// ever waits on RPC: an embedded, headless client has no way to render
    /// a password or host-key prompt and no keyboard to answer one with, so
    /// a client permitted to ask would hang the handshake with the question
    /// invisible. `-T` is the same reason from the other direction -- a pty
    /// on the far side would put a terminal discipline in the middle of a
    /// binary msgpack stream.
    ///
    /// Public because a caller that must open a *second* connection to the
    /// same destination -- diagnosing why the spawn's own connection failed
    /// -- has to configure it exactly as the spawn's was configured, and a
    /// second derivation of these arguments is free to drift from the one
    /// the spawn actually used. It answers a different question than the
    /// spawn's if it connects on a different port, under different options,
    /// or with prompting re-armed.
    #[must_use]
    pub fn connection_args(&self) -> Vec<String> {
        // view's own options lead, and a client takes the first value it
        // obtains for an option: a caller's `BatchMode=no` therefore cannot
        // re-arm the interactive prompt an embedded editor has no way to
        // answer
        let mut args = vec![
            String::from("-T"),
            String::from("-o"),
            String::from("BatchMode=yes"),
        ];
        if let Some(port) = self.port {
            args.push(String::from("-p"));
            args.push(port.to_string());
        }
        for opt in &self.extra_ssh_opts {
            args.push(String::from("-o"));
            args.push(opt.clone());
        }
        // every argument from here on is the client's to send, not to read
        args.push(self.target.clone());
        args
    }
}

/// Backoff applied between successive reconnect attempts when the dead
/// engine's config carries a `RemoteSpec`. A local engine restarts
/// immediately since a crashed local process is not going to become
/// reachable by waiting; a remote engine's unreachability is often
/// transient (network blip, host reboot window), so retrying immediately
/// every time would just spin `ssh` uselessly.
pub const REMOTE_RECONNECT_BACKOFF_BASE: Duration = Duration::from_secs(1);

/// How many reconnect attempts a dropped remote connection is given before
/// the failure is handed back to the user. The waits double from
/// [`REMOTE_RECONNECT_BACKOFF_BASE`], so five of them spend about 31s in
/// total before giving up.
pub const REMOTE_RECONNECT_MAX_ATTEMPTS: u32 = 5;

/// The wait owed before reconnect attempt `attempt`, counted from one:
/// `base` doubled once per attempt already spent.
///
/// `base` is a parameter rather than [`REMOTE_RECONNECT_BACKOFF_BASE`] read
/// directly, so the doubling rule has one implementation and a caller
/// proving the sequence against waits it can afford to wait out proves the
/// shipped rule rather than a second copy of it.
///
/// Saturating rather than wrapping at the top: the cap is
/// [`REMOTE_RECONNECT_MAX_ATTEMPTS`], and an attempt number past it is a
/// caller's arithmetic error, which must read as a very long wait rather
/// than as a zero-length one that spins.
#[must_use]
pub fn remote_reconnect_backoff(base: Duration, attempt: u32) -> Duration {
    let doublings = attempt.saturating_sub(1);
    base.saturating_mul(1u32.checked_shl(doublings).unwrap_or(u32::MAX))
}

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
    ///
    /// A remote spawn does not use it: the editor runs on the far side, and
    /// [`RemoteSpec::remote_nvim_bin`] names it there.
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
    /// The remote target [`build_command`] routes the spawn through, or
    /// `None` for a local child. Private for the same reason `hermetic` is:
    /// where the child runs decides what its whole environment plan means,
    /// and a caller that could set this field directly on a config built by
    /// [`isolated`](Self::isolated) would produce one describing a local
    /// isolation the far side never receives.
    remote: Option<RemoteSpec>,
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
            remote: None,
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

    /// The `nvim` binary to spawn, replacing the `PATH` lookup. A remote
    /// spawn resolves its editor through
    /// [`RemoteSpec::remote_nvim_bin`] instead and ignores this one.
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

    /// Arms a remote spawn: [`Engine::spawn`] routes through the system
    /// `ssh` client instead of the local [`nvim_bin`](Self::nvim_bin), and
    /// [`env_plan`](Self::env_plan) rides the single command string that
    /// client hands the remote shell, rather than the local child's own
    /// environment, which is the `ssh` process itself and forwards nothing.
    ///
    /// Compatible with [`isolated`](Self::isolated), on terms
    /// [`env_plan`](Self::env_plan) spells out entry by entry: the plan's
    /// removals cross as they stand, two of its overrides name a substitute
    /// that needs no local directory behind it, and `HOME` is exempt and
    /// stays the remote user's own. The local directory preparation
    /// [`Engine::spawn`] performs for an isolated child is skipped, since a
    /// remote child is pointed at none of it.
    ///
    /// The local [`nvim_bin`](Self::nvim_bin) is unused once this is armed:
    /// the editor runs on the far side, and
    /// [`RemoteSpec::remote_nvim_bin`] names it there.
    ///
    /// Also mutually exclusive with
    /// [`with_stdin_relay`](Self::with_stdin_relay), which describes a
    /// descriptor of this host's own that a remote child cannot be handed,
    /// and refused the same way.
    #[must_use]
    pub fn with_remote(mut self, remote: RemoteSpec) -> Self {
        self.remote = Some(remote);
        self
    }

    /// The remote target a caller armed with [`with_remote`](Self::with_remote),
    /// or `None` for a local spawn.
    ///
    /// Readable so a caller can report what was resolved (which client,
    /// which destination, which port) when a spawn fails, without keeping a
    /// second copy of the spec alongside the config that owns it.
    #[must_use]
    pub fn remote(&self) -> Option<&RemoteSpec> {
        self.remote.as_ref()
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
    ///
    /// # A hermetic plan for a child on another machine
    ///
    /// Where the child runs decides both what the first layer may enumerate
    /// and what the second layer's value-bearing entries may name, so a
    /// config that is both hermetic and [`remote`](Self::remote) plans four
    /// of them differently.
    ///
    /// - The first layer is [`crate::env::REMOTE_SWEEP_VARS`], a named list,
    ///   rather than [`crate::env::hermetic_sweep`]'s inversion of this
    ///   host's own environment. That constant's documentation carries the
    ///   reasoning; the consequence here is that the remote command line is
    ///   the same string on every machine that builds it.
    /// - [`crate::env::HOST_REDIRECT_VARS`] crosses unchanged, each name as
    ///   its own `env -u` on the remote command line.
    /// - `XDG_CONFIG_DIRS`/`XDG_DATA_DIRS` and
    ///   `GIT_CONFIG_GLOBAL`/`GIT_CONFIG_SYSTEM` are pointed at
    ///   [`crate::env::REMOTE_UNPLANTABLE_PATH`] rather than at the two
    ///   prepared local directories, which name nothing on the far side.
    ///   The substitute neutralizes the same lookups and needs no
    ///   preparation to do it.
    /// - `HOME` is the exemption: it is not planned at all, and the remote
    ///   child keeps the one its own login environment gives it. A hermetic
    ///   home is *written* by its holder -- an embedded Neovim creates
    ///   `$HOME/.local/state/nvim` as it starts -- so the value has to be a
    ///   writable directory that exists, which is a preparation, and a
    ///   command string handed to a far-side shell performs none. Removal
    ///   is not the fallback: unset, libc and LuaJIT resolve no home at all
    ///   and the child's own `expand('~')` fails (see
    ///   [`crate::env::HERMETIC_HOME_VAR`]).
    ///
    /// ## What a remote hermetic plan does not reach
    ///
    /// Both directions are load-bearing, and only one of them is a list of
    /// names. What crosses is stated above. What does not:
    ///
    /// - Everything the far side's own login environment exports that no
    ///   list here enumerates. `LD_PRELOAD` or `LD_LIBRARY_PATH` from the
    ///   remote `/etc/environment`, `SSH_AUTH_SOCK` from agent forwarding,
    ///   an `XDG_*` name outside [`crate::env::REMOTE_SWEEP_VARS`] -- each
    ///   reaches the far-side child intact, so the remote editor can link
    ///   different libraries or a subprocess it spawns can authenticate
    ///   against a forwarded agent while the config it started from says
    ///   `isolated`. Closing that class needs the neutralization to *run on
    ///   the far side* (an `env -i` plus an allowlist, executed there), and
    ///   a command line built here cannot be one: it can only name what
    ///   somebody enumerated, and no enumeration written on this host can be
    ///   complete about a shell it has never seen.
    /// - A name this host exports that the local client is configured to
    ///   forward and the remote server to accept, beyond the standard set
    ///   [`crate::env::CLIENT_FORWARDED_VARS`] enumerates. That set is
    ///   accounted for -- each of its names is either hermetic passthrough
    ///   or on [`crate::env::REMOTE_SWEEP_VARS`] -- but a `SendEnv` line
    ///   somebody added locally, met by an `AcceptEnv *` server, carries a
    ///   name no list here knows, and it arrives *before* this plan runs.
    ///   That direction is the mirror of the one above: the value is this
    ///   host's rather than the far side's, and it is equally out of reach
    ///   of a command line.
    /// - The layer `HOME` closes locally -- everything a subprocess resolves
    ///   through it without an environment variable of its own (`.netrc`,
    ///   `.ssh/`, the `core.excludesFile` default) -- still resolves out of
    ///   the remote user's own home, so a fetch that authenticates there is
    ///   authenticating as that user.
    ///
    /// Neither is a defect of the plan; both are the boundary of what a
    /// single command string can do, and a caller relying on `isolated`
    /// across a hop is relying on exactly this much.
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
            let far_side = self.remote.is_some();
            if far_side {
                for name in crate::env::REMOTE_SWEEP_VARS {
                    plan_sweep(&mut plan, OsStr::new(name));
                }
            } else {
                for (name, _) in crate::env::hermetic_sweep() {
                    plan_sweep(&mut plan, &name);
                }
            }
            for name in crate::env::HOST_REDIRECT_VARS {
                plan_set(&mut plan, OsStr::new(name), None);
            }
            let (empty, absent) = if far_side {
                let unplantable = OsString::from(crate::env::REMOTE_UNPLANTABLE_PATH);
                (unplantable.clone(), unplantable)
            } else {
                (
                    crate::env::empty_search_path().into_os_string(),
                    crate::env::absent_config_file().into_os_string(),
                )
            };
            for name in crate::env::HOST_SEARCH_PATH_VARS {
                plan_set(&mut plan, OsStr::new(name), Some(empty.clone()));
            }
            for name in crate::env::HOST_SUBPROCESS_CONFIG_VARS {
                plan_set(&mut plan, OsStr::new(name), Some(absent.clone()));
            }
            if !far_side {
                plan_set(
                    &mut plan,
                    OsStr::new(crate::env::HERMETIC_HOME_VAR),
                    Some(crate::env::hermetic_home().into_os_string()),
                );
            }
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
    /// Whether the child is an ssh client rather than a local editor (see
    /// [`is_remote`](Self::is_remote)).
    remote: bool,
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
/// [`Engine::start_pump`] leaves the watch paused to match, since an armed
/// watch whose anchor nothing advances asks the runtime loop for a wakeup
/// every threshold forever -- a periodic wakeup on the arm whose whole
/// premise is that it has none.
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
    /// A remote `cfg` returns `EngineError::Io` before anything is prepared
    /// or started when it carries a setting a remote spawn cannot honour (a
    /// stdin relay) or a destination that does not name a host (empty, or
    /// beginning with a dash).
    ///
    /// An isolated `cfg` for a *local* child also returns `EngineError::Io`
    /// when the hermetic search path cannot be established empty (see
    /// [`crate::env::prepare_empty_search_path`]) or the hermetic home holds
    /// a planted entry (see [`crate::env::prepare_hermetic_home`]), before
    /// any process is started: a child pointed at a directory somebody
    /// planted a plugin or credential file in is not isolated, and refusing
    /// the spawn is the only way that says so. An isolated remote `cfg`
    /// prepares neither, because it points its child at neither: the plan
    /// that crosses names what a far-side shell can be handed in one command
    /// string, and [`EngineConfig::env_plan`] states which entries that
    /// changes.
    pub fn spawn(cfg: EngineConfig) -> Result<Self, EngineError> {
        refuse_incoherent_remote(&cfg)?;
        let remote = cfg.remote.is_some();
        if cfg.hermetic && cfg.remote.is_none() {
            crate::env::prepare_empty_search_path()?;
            crate::env::prepare_hermetic_home()?;
        }
        let mut command = build_command(&cfg)?;
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
            remote,
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
        Self::spawn_recovering(cfg)
    }

    /// The replacement half of [`restart`](Self::restart) on its own: a
    /// fresh engine carrying nvim's own recovery flag, with nothing torn
    /// down here.
    ///
    /// For the caller that has already resolved its own teardown and must
    /// keep holding the connection it is replacing while the replacement is
    /// attempted -- a reconnect over a transport that may refuse the
    /// attempt, where the alternative is a session left with no engine at
    /// all and nothing to report the failure through. The ownership rule
    /// [`restart`](Self::restart) enforces is the caller's to keep here:
    /// nothing may be brought up alongside a connection that is still live.
    ///
    /// # Errors
    ///
    /// The same shapes [`spawn`](Self::spawn) returns, for the same reasons.
    pub fn spawn_recovering(cfg: EngineConfig) -> Result<Self, EngineError> {
        Self::spawn(with_recovery(cfg))
    }

    /// Whether this engine's child is the ssh client of a remote spawn
    /// rather than a local editor, as resolved from the config it was
    /// spawned with.
    ///
    /// Read off the spawn rather than re-derived from the command line: a
    /// second derivation is free to drift from the config that actually
    /// routed the spawn, and the question decides how a death is recovered
    /// from.
    #[must_use]
    pub fn is_remote(&self) -> bool {
        self.remote
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
        // and not armed at all under `bench-no-heartbeat`, where no prober
        // was spawned to advance the cadence: an armed watch over a frozen
        // anchor reads as a cadence that stopped and keeps asking the
        // runtime loop to look again one threshold later, forever. The
        // counterfactual arm is the absence of the cadence including the
        // wakeups it costs, so the watch that measures it stays paused and
        // the loop's wait stays unbounded (see `spawn_prober`).
        #[cfg(not(feature = "bench-no-heartbeat"))]
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
/// # The second autocommand, and why the errors are scoped
///
/// `v:errmsg` holds the last error raised at any point in the session, which
/// makes it useless for "did *this* go wrong" unless something establishes
/// where "this" starts. `UIEnter` is that boundary, measured on the pinned
/// engine: it fires after the user's config has been sourced and before nvim
/// opens the files it was given, so clearing `v:errmsg` there leaves the
/// variable saying only what the file-opening -- the recovery included --
/// raised. Without it, a config that errors on its own (`:preserve` under
/// `set noswapfile` raises `E313`, which auto-save plugins reach) leaves its
/// error standing for [`SWAP_RECOVERY_PROBE`] to read as a recovery's.
///
/// It does not fire on the one startup that never finishes -- a startup
/// error parks nvim ahead of both `UIEnter` and `VimEnter` -- and there it is
/// not needed: the park comes *from* the file-opening, so the error standing
/// when it happens is that file-opening's own.
const SWAP_RECOVERY_CMD: &str = "lua \
     local group = vim.api.nvim_create_augroup('view_swap_recovery', { clear = true }) \
     vim.api.nvim_create_autocmd('SwapExists', { \
     group = group, \
     pattern = '*', \
     desc = 'Recover a swap file no live process still owns', \
     callback = function() \
     if vim.v.swapchoice ~= '' then return end \
     vim.v.swapchoice = 'r' \
     vim.g.view_swap_recovered = (vim.g.view_swap_recovered or 0) + 1 \
     end, \
     }) \
     vim.api.nvim_create_autocmd('UIEnter', { \
     group = group, \
     desc = 'Scope v:errmsg to the files this session is about to open', \
     callback = function() vim.v.errmsg = '' end, \
     })";

/// Everything a started engine can say about a swap recovery it performed,
/// as one vimscript expression answering
/// `[recovered, reported, failure, empty]`.
///
/// Asked twice per connection, and the two readings answer different halves.
///
/// The recovery itself is not final until that connection's own `VimEnter`:
/// nvim opens the files it was given -- and answers their swap prompts --
/// after config sourcing. So the first two fields are gated on
/// `v:vim_did_enter` and read as "nothing yet" before it, which is what makes
/// the earlier of the two readings harmless.
///
/// The earlier reading exists because `VimEnter` is not guaranteed to arrive
/// at all. A startup that raised an error parks nvim at its own "press any
/// key" prompt *before* `VimEnter` fires (measured on the pinned engine:
/// `v:vim_did_enter` is still 0 while the prompt stands), and a recovery that
/// failed is exactly such a startup -- so a chain hung only off `VimEnter`
/// would go silent on the one case that most needs a voice. Asking as soon as
/// the connection is attached reaches it: nvim answers RPC while it waits at
/// that prompt.
///
/// # Two recoveries, one reading
///
/// [`SWAP_RECOVERY_CMD`]'s counter alone cannot answer this. It counts
/// `SwapExists` prompts, and the two ways a swap gets replayed reach it
/// differently: an ordinary spawn over a stale swap meets the prompt and is
/// counted, while a restart carries [`RECOVERY_ARG`] and nvim replays the
/// swap directly, without ever asking. Measured against the pinned engine
/// rather than assumed -- a `-r` restart of a crashed session reads the
/// counter back as `0`.
///
/// So `-r`'s own recovery is read off two facts instead: that the argument
/// is in `v:argv` at all, and that the buffer it produced is modified, which
/// is nvim's own way of saying the swap held work the file on disk does not.
/// `&modified` is the current buffer's, so a session recovering several
/// files at once undercounts rather than overclaims.
///
/// `reported` is the wider question and the one the redraw hangs off: nvim
/// writes its multi-line recovery report whenever it replays a swap, whether
/// or not anything came back changed, so a recovery that restored nothing
/// still leaves a report sitting over the buffer.
///
/// # A recovery that failed reads nothing like one that worked
///
/// `reported` alone cannot tell the two apart, and the difference is the
/// user's file. Measured against the pinned engine: a `-r` restart whose swap
/// file is gone comes up live, opens an **empty** buffer where the file's
/// contents should be, and raises nvim's own `E305`; a `-r` restart that
/// replayed a swap holding nothing new comes up with the file's contents and
/// no error. Both answer `reported`.
///
/// `failure` is what separates them: nvim's own error text, under three
/// conditions that together make it a statement about a recovery rather than
/// about the session.
///
/// 1. **A recovery was asked for at all** -- [`RECOVERY_ARG`] is in `v:argv`.
///    A session that never asked for one cannot report one failing, however
///    its startup went.
/// 2. **The error is a memline or swap-file error** -- `E300`-`E312`, and
///    not one line further. `E313`/`E314` are `:preserve` refusing, which
///    ordinary configs and auto-save plugins raise with no recovery in
///    sight; `E315`-`E319` are internal `ml_get` faults and a version check.
///    Enumerated out of the pinned engine, not assumed from the prefix.
/// 3. **The error belongs to this session's file-opening** -- everything the
///    config said is cleared at `UIEnter` (see [`SWAP_RECOVERY_CMD`]), so
///    what stands here was raised after it.
///
/// Empty on every recovery that went through, and empty on every session
/// that never asked for one.
///
/// `empty` is read rather than inferred, because a failure is not one shape.
/// `E305` leaves the buffer empty where the file's contents should be, which
/// is the destructive case and worth saying out loud; an `E309` mid-recovery
/// leaves the disk contents in place, and a notice claiming an empty buffer
/// there would be telling the user their intact text is gone. The reading
/// answers what is actually in the buffer and the wording follows it.
///
/// A session with no swap file to recover is not exotic -- `set noswapfile`
/// in the user's own config, an unwritable `'directory'`, or a `-n` reaching
/// the engine's arguments all reach a restart with nothing to replay -- and
/// supervision restarts the first few deaths unattended, so the user never
/// opts into it.
///
/// Public because the readings are not all consumed in this crate: what a
/// recovered session says to its user is decided in `view-core` off the
/// values this produces, and a second hand-written copy of the expression
/// would be free to drift from the autocommand that feeds it.
pub const SWAP_RECOVERY_PROBE: &str = "[\
     v:vim_did_enter * (get(g:, 'view_swap_recovered', 0) + \
     (index(v:argv, '-r') >= 0 && &modified)), \
     v:vim_did_enter && (index(v:argv, '-r') >= 0 || \
     get(g:, 'view_swap_recovered', 0) > 0), \
     index(v:argv, '-r') >= 0 && \
     (v:errmsg =~# '^E30[0-9]:' || v:errmsg =~# '^E31[0-2]:') ? v:errmsg : '', \
     line('$') == 1 && getline(1) == '']";

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
    !file_operands(args).is_empty()
}

/// The positions in `args` at which nvim reads `-` as a file operand -- the
/// buffer fed from piped stdin -- rather than as some option's own value.
///
/// `-c -` names no such operand: the dash there is the command `-c` carries.
/// A scan for a bare `-` that does not track option arity arms a stdin relay
/// for a session that reads no stdin, and drops a value the engine still
/// needs when the same scan is used to strip the operand back out.
#[must_use]
pub fn stdin_operands(args: &[OsString]) -> Vec<usize> {
    file_operands(args)
        .into_iter()
        .filter(|(_, arg)| *arg == "-")
        .map(|(index, _)| index)
        .collect()
}

/// Every token in `args` nvim opens as a file, paired with its position, an
/// option's own value never mistaken for one.
///
/// Errs towards naming fewer in every case this build cannot read
/// confidently, and each caller's use of the answer is what makes that the
/// safe direction: a missed file costs a restart the recovery flag, and the
/// `SwapExists` autocommand every spawn carries ([`SWAP_RECOVERY_CMD`]) still
/// recovers the swap when the file is opened, while a value mistaken for a
/// file costs the replacement engine its life.
fn file_operands(args: &[OsString]) -> Vec<(usize, &OsStr)> {
    let mut operands: Vec<(usize, &OsStr)> = Vec::new();
    let mut expect_value = false;
    for (index, arg) in args.iter().enumerate() {
        let text = arg.to_string_lossy();
        // nvim's own rule: only file names after this
        if text == "--" {
            operands.extend(
                args[index + 1..]
                    .iter()
                    .enumerate()
                    .map(|(offset, arg)| (index + 1 + offset, arg.as_os_str())),
            );
            return operands;
        }
        if expect_value {
            expect_value = false;
            continue;
        }
        // `-l <script> [args...]` hands everything that follows to a Lua
        // script, so nothing after it is a file nvim opens
        if text == "-l" {
            return operands;
        }
        // nvim's `-` is an operand, not an option: it names the buffer fed
        // from piped stdin. Reading it as an option would consume whatever
        // followed it as that option's value, so `cmd | view - notes.md`
        // would lose the real file too
        if text == "-" || !(text.starts_with('-') || text.starts_with('+')) {
            operands.push((index, arg.as_os_str()));
            continue;
        }
        expect_value = !takes_no_value(&text);
    }
    operands
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

/// Builds the command `cfg` describes: the local `nvim --embed` child, or
/// the `ssh` client that starts that same child on a remote host when
/// [`EngineConfig::with_remote`] armed one.
///
/// The stdio shape is one shape for both, set here rather than in either
/// branch: two pipes and a discarded stderr, which is what the RPC channel
/// needs and all it needs. An `ssh` client's own standard input and output
/// are the remote command's, so the branch that wraps one hands back a
/// child indistinguishable from a local `nvim` to everything downstream.
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
///
/// # Errors
///
/// Only the remote branch can fail, and only over a token it cannot forward
/// without altering it (see [`token_bytes`]). The local branch is
/// infallible: `Command` carries an `OsStr` to the child verbatim.
fn build_command(cfg: &EngineConfig) -> Result<Command, EngineError> {
    let mut command = match &cfg.remote {
        None => local_command(cfg),
        Some(remote) => remote_command(remote, cfg)?,
    };
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
    Ok(command)
}

/// Refuses a remote config whose other settings describe a spawn that
/// cannot happen, before anything is prepared and before any process runs.
///
/// Every case here is one that is otherwise accepted and silently
/// ineffective, and a remote editor that quietly lost a setting looks
/// exactly like one that kept it.
///
/// A stdin relay is a duplicate of this caller's own descriptor, handed to
/// the child at a fixed fd number. A client's own standard input is already
/// the remote command's, and `--embed` has claimed that for the RPC
/// channel, so there is no second descriptor at the far end for the relay
/// to arrive on: the fd would reach the local client and die there while
/// the remote editor was told to read its startup content from a
/// descriptor belonging to whatever opened it on that host.
///
/// A destination beginning with a dash is read by a client as one of its
/// own options rather than as a host, which turns a target into local
/// option injection (`-o ProxyCommand=...` runs a command on this
/// machine). OpenSSH refuses such a destination itself, but that refusal
/// belongs to whichever client [`RemoteSpec::ssh_bin`] names, and this one
/// does not.
fn refuse_incoherent_remote(cfg: &EngineConfig) -> Result<(), EngineError> {
    let Some(remote) = &cfg.remote else {
        return Ok(());
    };
    let refuse = |reason: &str| Err(EngineError::Io(std::io::Error::other(reason.to_string())));
    if cfg.stdin_relay_requested() {
        return refuse(
            "a stdin relay hands the child a descriptor of this host's own, \
             which a remote spawn has no way to deliver: the ssh client's \
             standard input is already the RPC channel. Relay stdin to a \
             local engine, or open the remote one without it",
        );
    }
    if remote.target.is_empty() {
        return refuse("a remote spawn needs a destination, and this one is empty");
    }
    if remote.target.starts_with('-') {
        return refuse(
            "a remote destination beginning with a dash is read by the ssh \
             client as one of its own options, not as a host, so it would \
             configure the connection instead of naming its far end",
        );
    }
    Ok(())
}

/// The local half of [`build_command`]: `nvim --embed` run on this host,
/// with [`EngineConfig::env_plan`] applied entry by entry so the environment
/// the child gets is the plan a caller can inspect, never a second
/// derivation of it.
fn local_command(cfg: &EngineConfig) -> Command {
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
}

/// The remote half of [`build_command`]: the system `ssh` client, carrying
/// its own connection options and then the one string the far side's shell
/// re-parses into the same `nvim --embed` invocation the local half runs
/// directly.
///
/// The environment plan is deliberately *not* applied to this `Command`.
/// `Command::env` would set it on the local `ssh` process, which is not the
/// process the plan describes and does not forward it: an override landing
/// there is at best inert and at worst a redirection of the client's own
/// key and configuration lookup (`HOME`, `PATH`). It rides the remote
/// command line instead (see [`remote_command_line`]).
///
/// The connection half comes from [`RemoteSpec::connection_args`], which
/// documents `BatchMode=yes` and `-T` and is the one place either is
/// spelled: a diagnostic connection opened after this one fails has to
/// carry the same options to be describing the same connection at all.
fn remote_command(remote: &RemoteSpec, cfg: &EngineConfig) -> Result<Command, EngineError> {
    let mut command = Command::new(&remote.ssh_bin);
    command.args(remote.connection_args());
    // the destination ends the client's own arguments; the single command
    // string the far side's shell re-parses follows it
    command.arg(remote_command_line(remote, cfg)?);
    Ok(command)
}

/// Builds the single string `ssh` hands to the remote shell for re-parsing.
///
/// `ssh` does not preserve [`Command::arg`] boundaries across the wire:
/// every argument trailing the destination is joined with spaces into one
/// string before the remote side ever sees it, so the string is constructed
/// here rather than left to an argv passthrough that does not exist. A
/// value carrying a space would otherwise word-split into extra remote
/// shell tokens, and one carrying `$`, a backtick or `;` would run as
/// remote shell syntax. Path-shaped variables that plausibly contain spaces
/// ([`crate::env::HOST_REDIRECT_VARS`]) are exactly what an engine forwards,
/// so both are reachable rather than theoretical.
///
/// Every token is single-quote-escaped uniformly, not only the ones judged
/// to need it: classifying which tokens are safe is the mistake that leaves
/// the one space-, quote- or `$`-bearing value unescaped.
///
/// Both halves of the plan cross, whatever put them in it -- a caller's own
/// overrides and removals, and a hermetic config's whole layer, which
/// [`EngineConfig::env_plan`] shapes for a far-side child before it gets
/// here. A `Some` entry becomes a `KEY=value`
/// assignment; a `None` entry becomes `env -u KEY`. A removal names a
/// variable that must be absent from the *child*, which is not the same
/// claim as undoing this host's own export: a remote editor started over
/// `ssh` inherits the remote user's login environment (sshd and PAM, then
/// the login shell's non-interactive startup files), and a redirect
/// variable set there reaches it exactly as one set here would.
///
/// # Errors
///
/// Off Unix, a token that is not valid Unicode has no byte view to forward
/// and is refused by name rather than transcoded (see [`token_bytes`]).
fn remote_command_line(remote: &RemoteSpec, cfg: &EngineConfig) -> Result<OsString, EngineError> {
    let mut tokens: Vec<Vec<u8>> = vec![b"env".to_vec()];
    let mut assignments: Vec<Vec<u8>> = Vec::new();
    for (name, value) in cfg.env_plan() {
        match value {
            Some(value) => {
                let mut assignment = token_bytes(&name)?;
                assignment.push(b'=');
                assignment.extend_from_slice(&token_bytes(&value)?);
                assignments.push(assignment);
            }
            // ahead of the `--` below, because `-u` is one of `env`'s own
            // options and `--` is what ends its option parsing
            None => {
                tokens.push(b"-u".to_vec());
                tokens.push(token_bytes(&name)?);
            }
        }
    }
    // `--` ends `env`'s own option parsing, so a remote nvim path beginning
    // with a dash is a program name rather than a flag. It goes here, ahead
    // of the assignments, and not between them and the program: every `env`
    // implementation measured (GNU, uutils, the BSD one macOS ships) reads
    // an operand after the assignments as the utility to run, and rejects
    // this one with `--: No such file or directory`.
    tokens.push(b"--".to_vec());
    tokens.append(&mut assignments);
    tokens.push(remote.remote_nvim_bin.clone().into_bytes());
    // same order and the same reason as the local half: the swap answer
    // rides every spawn, ahead of the files it opens
    tokens.push(b"--embed".to_vec());
    tokens.push(b"--cmd".to_vec());
    tokens.push(SWAP_RECOVERY_CMD.as_bytes().to_vec());
    for arg in &cfg.extra_args {
        tokens.push(token_bytes(arg)?);
    }
    let mut line: Vec<u8> = Vec::new();
    for token in &tokens {
        if !line.is_empty() {
            line.push(b' ');
        }
        line.extend_from_slice(&shell_quote(token));
    }
    line_into_os_string(line)
}

/// The bytes `token` contributes to a remote command line.
///
/// On Unix this is the `OsStr`'s own byte content, so a path or value the
/// local child would receive byte for byte crosses to the remote one
/// unchanged. Encoding is a property of the far side's filesystem and
/// locale, and a byte sequence that is not valid UTF-8 is an ordinary
/// filename there, not an error: decoding it lossily would silently point
/// the remote editor at a *different* path than the caller named, and
/// creating a file under that path on write is the same defect with a
/// permanent result.
///
/// # Errors
///
/// Off Unix there is no byte view of an `OsStr` at all, so a token that is
/// not valid Unicode is refused by name. A remote spawn from such a host is
/// rejected rather than silently altered.
fn token_bytes(token: &OsStr) -> Result<Vec<u8>, EngineError> {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        Ok(token.as_bytes().to_vec())
    }
    #[cfg(not(unix))]
    {
        match token.to_str() {
            Some(text) => Ok(text.as_bytes().to_vec()),
            None => Err(EngineError::Io(std::io::Error::other(format!(
                "a remote command line cannot carry {token:?}: it is not valid \
                 Unicode, and this platform offers no byte view of it to \
                 forward unchanged"
            )))),
        }
    }
}

/// Wraps the assembled command line back up as the single argument `ssh`
/// receives.
///
/// # Errors
///
/// Off Unix the bytes came from `str` and reassemble infallibly; the error
/// arm exists so no platform reaches for a lossy conversion.
fn line_into_os_string(line: Vec<u8>) -> Result<OsString, EngineError> {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStringExt;
        Ok(OsString::from_vec(line))
    }
    #[cfg(not(unix))]
    {
        String::from_utf8(line)
            .map(OsString::from)
            .map_err(|err| EngineError::Io(std::io::Error::other(err)))
    }
}

/// POSIX single-quote escaping: wraps `token` in `'...'` and replaces every
/// literal `'` inside it with `'\''` (close the quote, escape the quote,
/// reopen), the standard rule for a string a POSIX shell parses back as
/// exactly one word whatever its contents.
///
/// Byte-wise, never over `str`: single quoting is byte-transparent, and a
/// token that is not valid UTF-8 must survive it unchanged rather than be
/// decoded on the way through.
fn shell_quote(token: &[u8]) -> Vec<u8> {
    let mut quoted = Vec::with_capacity(token.len() + 2);
    quoted.push(b'\'');
    for byte in token {
        if *byte == b'\'' {
            quoted.extend_from_slice(br"'\''");
        } else {
            quoted.push(*byte);
        }
    }
    quoted.push(b'\'');
    quoted
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
            .expect("a local config always builds a command")
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

    /// The program and argument vector the spawn path would hand the
    /// operating system, read back off the built `Command` for the same
    /// reason [`spawned_env`] reads the environment off it.
    fn built(cfg: &EngineConfig) -> (OsString, Vec<OsString>) {
        let command = build_command(cfg).expect("this config must build a command");
        (
            command.get_program().to_os_string(),
            command.get_args().map(OsStr::to_os_string).collect(),
        )
    }

    /// The one string the client joins everything trailing the destination
    /// into, which is the whole of what the remote shell re-parses.
    fn remote_line(cfg: &EngineConfig) -> String {
        let (_, args) = built(cfg);
        args.last()
            .expect("a remote command carries at least the command string")
            .to_string_lossy()
            .into_owned()
    }

    /// `token` as it appears in the assembled command line: the quoting
    /// under test, rendered back as text the assertions can search for.
    fn quoted(token: &str) -> String {
        String::from_utf8_lossy(&shell_quote(token.as_bytes())).into_owned()
    }

    fn osv(items: &[&str]) -> Vec<OsString> {
        items.iter().map(OsString::from).collect()
    }

    #[test]
    fn a_remote_config_spawns_the_ssh_client_instead_of_the_local_nvim() {
        let cfg = EngineConfig::default()
            .with_nvim_bin("/local/bin/nvim")
            .with_remote(RemoteSpec::new("user@host"));
        let (program, args) = built(&cfg);
        assert_eq!(
            program,
            OsString::from("ssh"),
            "a remote config must run the ssh client; running the local nvim \
             instead edits this host's files while reporting the remote's"
        );
        assert!(
            args.contains(&OsString::from("user@host")),
            "the destination never reached the client; args {args:?}"
        );
        assert!(
            !args.iter().any(|arg| arg == OsStr::new("/local/bin/nvim")),
            "the local nvim path leaked into the remote command; args {args:?}"
        );
    }

    /// Everything the client parses for itself comes before the
    /// destination, and exactly one argument comes after it: the client
    /// joins every trailing argument into one string regardless, so a
    /// second one here would be a boundary the far side never sees.
    #[test]
    fn a_remote_spawn_carries_the_clients_own_options_ahead_of_the_destination() {
        let spec = RemoteSpec::new("host")
            .with_port(2222)
            .with_ssh_opt("ConnectTimeout=4")
            .with_ssh_opt("StrictHostKeyChecking=yes");
        let cfg = EngineConfig::default().with_remote(spec);
        let (_, args) = built(&cfg);
        let destination = args
            .iter()
            .position(|arg| arg == OsStr::new("host"))
            .expect("the destination must appear in the argument vector");
        assert_eq!(
            args[..destination],
            osv(&[
                "-T",
                "-o",
                "BatchMode=yes",
                "-p",
                "2222",
                "-o",
                "ConnectTimeout=4",
                "-o",
                "StrictHostKeyChecking=yes",
            ])[..],
            "a pty on the far side would corrupt a binary msgpack stream, \
             and a client permitted to prompt hangs a handshake nothing can \
             answer; args {args:?}"
        );
        assert_eq!(
            args.len(),
            destination + 2,
            "everything trailing the destination is joined into one string \
             by the client, so a second trailing argument is an argv \
             boundary the remote shell never receives; args {args:?}"
        );
    }

    /// What a caller opening a second connection to the same destination
    /// builds it from is the spawn's own connection, argument for argument.
    /// The two drifting apart is silent by construction: a diagnosis made
    /// over a connection configured differently -- another port, without
    /// the caller's own options, with prompting re-armed -- reads exactly
    /// like a diagnosis of the connection that failed.
    #[test]
    fn a_diagnostic_connection_is_built_from_the_spawns_own_arguments() {
        let spec = RemoteSpec::new("host")
            .with_port(2222)
            .with_ssh_opt("ConnectTimeout=4");
        let cfg = EngineConfig::default().with_remote(spec.clone());
        let (_, args) = built(&cfg);
        let connection: Vec<OsString> =
            spec.connection_args().into_iter().map(Into::into).collect();
        assert_eq!(
            args[..connection.len()],
            connection[..],
            "the client's own arguments must be the built command's own \
             prefix; args {args:?}"
        );
        assert_eq!(
            args.len(),
            connection.len() + 1,
            "only the single remote command string follows them; args {args:?}"
        );
    }

    /// The environment plan reaches the far side on the command line, ahead
    /// of the binary it applies to.
    #[test]
    fn a_planned_environment_override_rides_the_single_remote_command_line() {
        let cfg = EngineConfig::default()
            .with_env("NVIM_APPNAME", "work")
            .with_remote(RemoteSpec::new("host").with_remote_nvim_bin("/opt/nvim/bin/nvim"));
        let line = remote_line(&cfg);
        let planned = line
            .find("'NVIM_APPNAME=work'")
            .expect("the override must ride the remote command line");
        let binary = line
            .find("'/opt/nvim/bin/nvim'")
            .expect("the remote binary must appear in the command line");
        assert!(
            planned < binary,
            "an assignment after the program name is an argument to it, not \
             an environment entry; line {line}"
        );
        let end_of_options = line
            .find("'--'")
            .expect("the command line must end env's own option parsing");
        assert!(
            end_of_options < planned,
            "every env implementation measured reads an operand after the \
             assignments as the program to run, and refuses this one with \
             `--: No such file or directory`; line {line}"
        );
    }

    /// `Command::env` on the remote branch would configure the local client,
    /// not the editor: at best inert, at worst a redirection of the client's
    /// own key and configuration lookup.
    #[test]
    fn a_remote_spawn_leaves_the_local_ssh_clients_own_environment_alone() {
        let cfg = EngineConfig::default()
            .with_env("HOME", "/planted")
            .with_remote(RemoteSpec::new("host"));
        assert!(
            spawned_env(&cfg).is_empty(),
            "the plan was applied to the ssh client's own environment, which \
             is not the process it describes; env {:?}",
            spawned_env(&cfg)
        );
        assert!(
            remote_line(&cfg).contains("'HOME=/planted'"),
            "the plan reached neither the client nor the far side; line {}",
            remote_line(&cfg)
        );
    }

    /// Uniform escaping, asserted on the values that break a design which
    /// escapes only what it judges to need it: the client hands the remote
    /// shell one string, so a bare space word-splits and bare shell syntax
    /// runs.
    #[test]
    fn every_remote_token_is_escaped_whatever_it_carries() {
        let cfg = EngineConfig::default()
            .with_env("SPACED", "/home/a user/init.lua")
            .with_env("QUOTED", "it's here")
            .with_env("SHELLY", "$(id); `id`; rm -rf /")
            .with_remote(RemoteSpec::new("host"));
        let line = remote_line(&cfg);
        assert!(
            line.contains("'SPACED=/home/a user/init.lua'"),
            "a space-bearing value must survive as one word; line {line}"
        );
        assert!(
            line.contains(r"'QUOTED=it'\''s here'"),
            "a quote-bearing value must close, escape and reopen the quote; \
             line {line}"
        );
        assert!(
            line.contains("'SHELLY=$(id); `id`; rm -rf /'"),
            "shell syntax in a value must reach the far side as text, never \
             as syntax; line {line}"
        );
    }

    /// A removal names a variable that must be absent from the child, which
    /// is a different claim from undoing this host's own export: a remote
    /// editor inherits the remote user's login environment, so a redirect
    /// variable set in an `~/.zshenv` there reaches it exactly as one set
    /// here would.
    #[test]
    fn a_removal_reaches_the_remote_editor_as_an_unset() {
        let cfg = EngineConfig::default()
            .with_env_remove("VIMINIT")
            .with_remote(RemoteSpec::new("host"));
        let line = remote_line(&cfg);
        let unset = line
            .find("'-u' 'VIMINIT'")
            .expect("a removal must cross as an unset, not be dropped");
        let end_of_options = line
            .find("'--'")
            .expect("the command line must end env's own option parsing");
        assert!(
            unset < end_of_options,
            "`-u` is one of env's own options, so an unset placed after the \
             `--` that ends option parsing is read as a program to run; line \
             {line}"
        );
    }

    /// The remote path must be as faithful as the local one, which carries
    /// an `OsStr` to the child untouched. A filename or a path-shaped value
    /// that is not valid UTF-8 is ordinary on a POSIX filesystem, and
    /// decoding it lossily would point the remote editor at a different
    /// path than the caller named, silently, and create a file there on
    /// write.
    #[cfg(unix)]
    #[test]
    fn every_byte_of_a_token_crosses_unchanged() {
        use std::os::unix::ffi::{OsStrExt, OsStringExt};
        // a latin-1 e-acute: a filename on countless real filesystems, and
        // not valid UTF-8
        let raw = b"caf\xe9.md";
        let value = OsString::from_vec(b"/home/caf\xe9/init.lua".to_vec());
        let cfg = EngineConfig::default()
            .with_env("VIEW_PROBE", &value)
            .with_arg(OsString::from_vec(raw.to_vec()))
            .with_remote(RemoteSpec::new("host"));
        let (_, args) = built(&cfg);
        let line = args
            .last()
            .expect("a remote command carries at least the command string")
            .as_bytes()
            .to_vec();
        for needle in [raw.as_slice(), value.as_bytes()] {
            assert!(
                line.windows(needle.len()).any(|window| window == needle),
                "{:?} did not survive into the command line, so the remote \
                 editor is pointed somewhere the caller did not name; line \
                 {:?}",
                OsStr::from_bytes(needle),
                OsStr::from_bytes(&line)
            );
        }
        assert!(
            !line.windows(3).any(|window| window == [0xef, 0xbf, 0xbd]),
            "a replacement character reached the command line, which is the \
             lossy decode this forbids; line {:?}",
            OsStr::from_bytes(&line)
        );
    }

    /// The other side of that guarantee, on a platform with no byte view of
    /// an `OsStr` to forward: the token is refused by name rather than
    /// transcoded. Windows filenames are UTF-16 sequences that need not be
    /// well-formed, so a lone surrogate is a name a real file can carry and
    /// no UTF-8 encoding exists for. Silently replacing it would send the
    /// remote editor to a different path than the caller named -- the same
    /// defect the Unix arm above forbids, reached from the opposite
    /// direction.
    #[cfg(windows)]
    #[test]
    fn a_token_with_no_byte_view_is_refused_rather_than_transcoded() {
        use std::os::windows::ffi::OsStringExt;
        // `s`, a lone high surrogate, `h`: well-formed UTF-16 it is not, a
        // valid Windows filename it is
        let lone = OsString::from_wide(&[0x0073, 0xD800, 0x0068]);
        let cfg = EngineConfig::default()
            .with_arg(lone)
            .with_remote(RemoteSpec::new("host"));
        let refused = build_command(&cfg)
            .expect_err("a token with no byte view must be refused, not forwarded")
            .to_string();
        assert!(
            refused.contains("not valid Unicode"),
            "the refusal must say what it could not carry: {refused}"
        );
        assert!(
            !refused.contains('\u{fffd}'),
            "the refusal itself must not be the lossy decode it refuses: \
             {refused}"
        );
    }

    /// The same guarantee the local path pins, on the branch that builds its
    /// command line as text: nvim runs `--cmd` commands before it opens any
    /// file, so the swap answer must precede the caller's own arguments.
    #[test]
    fn a_remote_spawn_carries_the_swap_answer_ahead_of_the_files_it_opens() {
        let cfg = EngineConfig::default()
            .with_arg("notes.md")
            .with_remote(RemoteSpec::new("host"));
        let line = remote_line(&cfg);
        let answer = line
            .find(&quoted(SWAP_RECOVERY_CMD))
            .expect("the swap answer must ride a remote spawn too");
        let file = line
            .find("'notes.md'")
            .expect("the caller's file must reach the far side");
        assert!(
            answer < file,
            "an autocommand registered after the file it guards is already \
             open guards nothing; line {line}"
        );
    }

    #[test]
    fn with_remote_records_the_target_a_caller_armed() {
        let cfg = EngineConfig::default().with_remote(RemoteSpec::new("user@host").with_port(2222));
        let remote = cfg.remote().expect("with_remote must record the spec");
        assert_eq!(remote.target, "user@host");
        assert_eq!(remote.port, Some(2222));
        assert_eq!(remote.remote_nvim_bin, "nvim");
        assert!(
            EngineConfig::default().remote().is_none(),
            "a config nobody armed must stay local"
        );
    }

    /// The hermetic plan a remote config carries, entry by entry: the two
    /// override groups that name a local directory locally must name the
    /// substitute instead, because the local paths mean nothing on the far
    /// side and would point the child at whatever sits there.
    #[test]
    fn a_remote_hermetic_plan_replaces_the_paths_only_this_host_prepares() {
        let plan = EngineConfig::isolated()
            .with_remote(RemoteSpec::new("host"))
            .env_plan();
        for name in crate::env::HOST_SEARCH_PATH_VARS
            .iter()
            .chain(crate::env::HOST_SUBPROCESS_CONFIG_VARS)
        {
            let entry = plan
                .iter()
                .find(|(known, _)| crate::env::env_names_eq(known, OsStr::new(name)));
            assert_eq!(
                entry.map(|(_, value)| value.clone()),
                Some(Some(OsString::from(crate::env::REMOTE_UNPLANTABLE_PATH))),
                "{name} rode a remote plan pointing at a directory only this \
                 host prepares; plan {plan:?}"
            );
        }
    }

    /// The exemption, pinned from both ends: the remote child keeps its own
    /// login `HOME`, and the sweep must not take it either. Unset, the child
    /// resolves no home at all -- and a plan that removed it would look
    /// exactly like this one until somebody ran `expand('~')` on the far
    /// side.
    #[test]
    fn a_remote_hermetic_plan_leaves_the_far_sides_own_home_alone() {
        let plan = EngineConfig::isolated()
            .with_remote(RemoteSpec::new("host"))
            .env_plan();
        assert!(
            !plan.iter().any(|(name, _)| crate::env::env_names_eq(
                name,
                OsStr::new(crate::env::HERMETIC_HOME_VAR)
            )),
            "a remote hermetic plan carries an entry for {}, so the far side \
             receives a home this host chose or no home at all; plan {plan:?}",
            crate::env::HERMETIC_HOME_VAR
        );
        let local = EngineConfig::isolated().env_plan();
        assert!(
            local.contains(&(
                OsString::from(crate::env::HERMETIC_HOME_VAR),
                Some(crate::env::hermetic_home().into_os_string())
            )),
            "the local plan stopped pointing {} at the prepared home, so the \
             exemption above is no longer an exemption from anything; plan \
             {local:?}",
            crate::env::HERMETIC_HOME_VAR
        );
    }

    /// The removals are the half that crosses unchanged, and they are what
    /// the far side's own login environment is neutralized by.
    #[test]
    fn a_remote_hermetic_plan_still_removes_every_redirect_variable() {
        let plan = EngineConfig::isolated()
            .with_remote(RemoteSpec::new("host"))
            .env_plan();
        for name in crate::env::HOST_REDIRECT_VARS {
            assert!(
                plan.contains(&(OsString::from(*name), None)),
                "{name} is not removed on the remote path, so a remote login \
                 environment that exports it redirects the editor; plan {plan:?}"
            );
        }
    }

    /// A caller's own removal of the exempt name is still the caller's:
    /// holding the sweep off `HOME` must not also override somebody who
    /// asked for it to go.
    #[test]
    fn a_callers_own_home_removal_survives_the_remote_exemption() {
        let plan = EngineConfig::isolated()
            .with_env_remove(crate::env::HERMETIC_HOME_VAR)
            .with_remote(RemoteSpec::new("host"))
            .env_plan();
        assert!(
            plan.contains(&(OsString::from(crate::env::HERMETIC_HOME_VAR), None)),
            "the exemption swallowed a removal the caller asked for; plan {plan:?}"
        );
    }

    /// The far-side child's own standard paths, which a remote login profile
    /// sets and which redirect every `stdpath()` lookup it makes.
    #[test]
    fn a_remote_hermetic_plan_removes_the_editors_own_standard_paths() {
        let plan = EngineConfig::isolated()
            .with_remote(RemoteSpec::new("host"))
            .env_plan();
        for name in crate::env::REMOTE_SWEEP_VARS {
            assert!(
                plan.contains(&(OsString::from(*name), None)),
                "{name} is not removed on the remote path, so a remote login \
                 profile that exports it moves every standard path the far-side \
                 editor resolves; plan {plan:?}"
            );
        }
    }

    /// The property the named list exists for: a remote plan is a function
    /// of these constants and the caller's own entries alone, never of what
    /// the machine building it happens to export.
    ///
    /// Asserted as a closed set rather than by planting a variable, because
    /// planting one means mutating this process's environment while sibling
    /// tests read it. Every name outside the union is a name the invoking
    /// shell contributed, which is the whole failure: a command line that
    /// differs between two machines running the same command, carrying this
    /// host's list of variable names to somebody else's account.
    #[test]
    fn a_remote_hermetic_plan_names_only_what_this_module_enumerates() {
        let plan = EngineConfig::isolated()
            .with_remote(RemoteSpec::new("host"))
            .env_plan();
        let enumerated: Vec<&str> = crate::env::REMOTE_SWEEP_VARS
            .iter()
            .chain(crate::env::HOST_REDIRECT_VARS)
            .chain(crate::env::HOST_SEARCH_PATH_VARS)
            .chain(crate::env::HOST_SUBPROCESS_CONFIG_VARS)
            .copied()
            .collect();
        for (name, _) in &plan {
            assert!(
                enumerated
                    .iter()
                    .any(|known| crate::env::env_names_eq(name, OsStr::new(known))),
                "{name:?} rode a remote plan without being named by any list \
                 here, so the command line depends on the environment of the \
                 machine that built it; plan {plan:?}"
            );
        }
        assert_eq!(
            plan.len(),
            enumerated.len(),
            "the remote plan and the lists it is built from no longer have one \
             entry each; plan {plan:?}"
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

    /// The outcome of a spawn, as text, so a refusal can be asserted on
    /// without an `Engine` ever existing to unwrap.
    fn spawn_outcome(cfg: EngineConfig) -> String {
        match Engine::spawn(cfg) {
            Ok(_) => String::from("the spawn was accepted"),
            Err(err) => err.to_string(),
        }
    }

    /// A relay is a descriptor of this host's own. The client's standard
    /// input is already the remote command's, and `--embed` has claimed
    /// that for the RPC channel, so there is no second descriptor at the
    /// far end for one to arrive on: accepting the pair points the remote
    /// editor at whatever happens to be open on that fd over there.
    #[test]
    fn a_remote_spawn_refuses_an_armed_stdin_relay() {
        let dev_null = std::fs::File::open("/dev/null").expect("/dev/null always opens");
        let outcome = spawn_outcome(
            EngineConfig::default()
                .with_stdin_relay(dev_null.into())
                .with_remote(RemoteSpec::new("host")),
        );
        assert!(
            outcome.contains("stdin relay"),
            "a relay a remote spawn cannot deliver must be refused, not \
             accepted and dropped, got: {outcome}"
        );
    }

    /// A destination is a host, and a client reads a leading dash as one of
    /// its own options: accepting one turns a target into local option
    /// injection, and only the client's own validation would stand in the
    /// way of it.
    #[test]
    fn a_remote_destination_that_is_not_a_hostname_is_refused() {
        let injected = spawn_outcome(
            EngineConfig::default().with_remote(RemoteSpec::new("-oProxyCommand=touch /tmp/view")),
        );
        assert!(
            injected.contains("dash"),
            "a destination the client would read as an option must be \
             refused, got: {injected}"
        );
        let empty = spawn_outcome(EngineConfig::default().with_remote(RemoteSpec::new("")));
        assert!(
            empty.contains("destination"),
            "an empty destination names no host and must be refused, got: {empty}"
        );
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

    /// The dash a caller may relay a descriptor for, told apart from the one
    /// an option is carrying. A caller that armed a relay for `-c -` would
    /// hand a live fd to a session that reads no stdin, and one that stripped
    /// that dash back out on a restart would leave `-c` holding whatever word
    /// came next.
    #[test]
    fn a_dash_is_an_operand_only_where_no_option_is_carrying_it() {
        assert_eq!(stdin_operands(&args(&["-"])), vec![0]);
        assert_eq!(stdin_operands(&args(&["--clean", "-"])), vec![1]);
        assert_eq!(stdin_operands(&args(&["-", "notes.md"])), vec![0]);
        assert_eq!(stdin_operands(&args(&["notes.md", "-"])), vec![1]);
        assert_eq!(stdin_operands(&args(&["--", "-"])), vec![1]);

        assert!(stdin_operands(&args(&[])).is_empty());
        assert!(stdin_operands(&args(&["notes.md"])).is_empty());
        assert!(
            stdin_operands(&args(&["-c", "-"])).is_empty(),
            "the dash is the command -c carries, not a buffer"
        );
        assert!(stdin_operands(&args(&["-u", "-"])).is_empty());
        assert!(
            stdin_operands(&args(&["-l", "script.lua", "-"])).is_empty(),
            "-l hands everything after it to a Lua script"
        );
        assert_eq!(
            stdin_operands(&args(&["-c", "-", "-"])),
            vec![2],
            "only the dash past the one -c consumed is an operand"
        );
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
        let command = build_command(&EngineConfig::default().with_arg("notes.md"))
            .expect("a local config always builds a command");
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
        let command = build_command(&EngineConfig::isolated())
            .expect("a local config always builds a command");
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
