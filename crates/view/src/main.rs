//! `view [FILE] --nvim-bin <path>`: CLI parsing and wiring for the terminal
//! frontend over an embedded Neovim engine. The startup sequence (shell
//! paint, background attach, pre-attach key buffering) lives in
//! [`startup`]; the steady-state loop itself lives in [`runtime`].

mod bridge;
mod clipboard;
mod engine_ops;
mod native;
mod recovery;
mod remote_guard;
mod runtime;
mod speculate;
mod startup;
mod theme_cache;
mod vlog;
mod wake;

use anyhow::{Context, Result};
use clap::Parser;
use std::sync::mpsc;
use std::time::Instant;
use view_core::model::{Model, Tier};
use view_core::msg::Effect;
use view_core::theme::Theme;
use view_engine::process::{stdin_operands, EngineConfig, RemoteSpec};
use view_tui::terminal::Term;

/// What this build calls itself, in `--version` and in its own startup
/// diagnostics.
///
/// Plain for the shipped editor; suffixed for either of the bench matrix's
/// counterfactual arms -- the one whose engine runs no heartbeat prober and
/// therefore notices no read-side hang, and the one that predicts nothing
/// so the echo row keeps measuring the round trip rather than the predicted
/// paint. The compile guard in `view-engine` is what keeps the first from
/// being made by accident; this is what makes either one that exists say so
/// wherever it is read from -- including from a shell, without a session to
/// start or a log to enable.
///
/// Both at once is representable and named as such: a build stripped of two
/// behaviours must not read as either single arm, since each row's number
/// would then be attributed to the wrong absence.
const VERSION: &str = match (
    cfg!(feature = "bench-no-heartbeat"),
    cfg!(feature = "bench-no-speculate"),
) {
    (true, true) => concat!(
        env!("CARGO_PKG_VERSION"),
        "+bench-no-heartbeat+bench-no-speculate"
    ),
    (true, false) => concat!(env!("CARGO_PKG_VERSION"), "+bench-no-heartbeat"),
    (false, true) => concat!(env!("CARGO_PKG_VERSION"), "+bench-no-speculate"),
    (false, false) => env!("CARGO_PKG_VERSION"),
};

/// What a session with no terminal on any descriptor tells the log and the
/// user, in one place so the two cannot come to say different things.
#[cfg(unix)]
const NO_TERMINAL_NOTICE: &str = "no terminal on stdin, stdout, stderr or /dev/tty: \
     this session takes no input; start view with a terminal on one of them";

/// `--tier`'s value vocabulary. A separate `clap`-derived enum rather than
/// deriving `ValueEnum` on `view_core::model::Tier` directly: `clap` is a
/// CLI/`main.rs`-boundary concern (per this crate's own convention of
/// keeping `anyhow`/CLI parsing out of library crates), and `Tier` is
/// `#[non_exhaustive]` in `view-core` regardless.
#[derive(Copy, Clone, clap::ValueEnum)]
enum TierArg {
    Full,
    Standard,
    Basic,
}

impl From<TierArg> for Tier {
    fn from(arg: TierArg) -> Self {
        match arg {
            TierArg::Full => Tier::Full,
            TierArg::Standard => Tier::Standard,
            TierArg::Basic => Tier::Basic,
        }
    }
}

#[derive(Parser)]
#[command(
    name = "view",
    version = VERSION,
    disable_version_flag = true,
    about = "A modern terminal editor powered by Neovim",
    after_help = "view's own flags (--tier, --clean, --appname, --config, --nvim-bin, --remote, ...) \
                  must appear before the first argument meant for nvim: once a token does \
                  not match one of view's flags, every remaining token -- including a later \
                  view flag -- is forwarded to nvim verbatim."
)]
struct Cli {
    /// Path to the nvim binary (defaults to PATH lookup)
    ///
    /// With `--remote`, this names the editor on the far side instead, and
    /// is resolved against the remote user's own `PATH` rather than this
    /// host's: a remote session runs no local nvim at all, so a value read
    /// here as a local path would name a binary nothing ever executes. It
    /// must be valid UTF-8 in that case, since the remote command line
    /// crosses to the far side as text.
    #[arg(long)]
    nvim_bin: Option<std::path::PathBuf>,
    /// `[user@]host[:path]`, ssh's own syntax. Spawns the engine on `host`
    /// over SSH instead of locally; `path` opens the same way a bare `view
    /// path` would locally, defaulting to the remote `$HOME` when omitted --
    /// matching plain `ssh host` + `nvim` with no arguments.
    ///
    /// The destination reaches the ssh client unparsed, so an alias from the
    /// user's own `~/.ssh/config` resolves exactly as it does on their
    /// command line. Only the first colon that follows the destination
    /// separates it from the path, and colons inside a bracketed address
    /// literal (`[2001:db8::1]:notes.md`) belong to the address.
    ///
    /// `path` is passed to the remote editor exactly as typed, with no local
    /// resolution: a relative path is relative to the remote login
    /// directory, which is the only host where the question has an answer.
    /// Every other file argument is opened there too -- a remote session
    /// runs one editor, on the far side, and it can reach no file of this
    /// host's. `--remote host:first.md second.md` opens both on `host`, with
    /// `first.md` current.
    ///
    /// A connection that drops is reconnected on a doubling backoff, with the
    /// attempt view is on named on screen while it waits, and the unsaved
    /// work recovered from the remote editor's own swap file. After the last
    /// attempt the session is handed back with the choice a dead engine has
    /// always offered: restart, or quit.
    #[arg(long, value_name = "[USER@]HOST[:PATH]")]
    remote: Option<String>,
    /// Overrides the port `~/.ssh/config` would otherwise resolve for
    /// `--remote`'s host.
    ///
    /// Requires `--remote`: with no destination to apply it to, this is a
    /// parse error rather than an ignored setting.
    #[arg(long, requires = "remote", value_name = "PORT")]
    ssh_port: Option<u16>,
    /// `-o KEY=VALUE`, forwarded verbatim to the underlying `ssh`
    /// invocation. Repeatable. The generic escape hatch for any SSH option
    /// this flag set does not name directly -- `ProxyJump`, `IdentityFile`,
    /// `ConnectTimeout`, and anything future.
    ///
    /// Requires `--remote`, the same way `--ssh-port` does. An entry naming
    /// an option view sets for itself (`BatchMode`, `RequestTTY`, and `Port`
    /// alongside `--ssh-port`) is refused rather than silently discarded:
    /// view's own value leads on the command line and a client keeps the
    /// first it obtains, so such an entry could never have taken effect.
    #[arg(long, requires = "remote", value_name = "KEY=VALUE")]
    ssh_opt: Vec<String>,
    /// Override auto-detected terminal capabilities instead of probing
    #[arg(long)]
    tier: Option<TierArg>,
    /// Spawns the bundled engine with no user config at all: `view.toml`
    /// and `init.lua` are both skipped, and every native feature stays on.
    /// This is view's own triage tool, and asks for something different from
    /// `nvim --clean`.
    #[arg(long)]
    clean: bool,
    /// Sets `NVIM_APPNAME` in the spawned engine's own environment, so it
    /// reads `$XDG_CONFIG_HOME/<name>` instead of `$XDG_CONFIG_HOME/nvim`.
    #[arg(long)]
    appname: Option<String>,
    /// An explicit `view.toml` path, replacing the one view would otherwise
    /// resolve for this platform.
    #[arg(long)]
    config: Option<std::path::PathBuf>,
    /// Prints the system clipboard's current text for `register` ('+' or
    /// '*') to stdout and exits, bypassing the editor entirely. Exists for
    /// the compat oracle's own out-of-band verification: every other check
    /// that harness can run reads state inside the same nvim/view session
    /// under test, but the system clipboard can only be checked
    /// independently of view's own claims by a second, freshly spawned
    /// process reading it directly (see `NO_DISPLAY_EXIT`'s doc for the
    /// no-clipboard-available case). Conflicts with `--remote`: this reads
    /// the clipboard of the host it runs on and starts no editor at all, so
    /// a destination combined with it would be accepted and never used.
    ///
    /// Hidden, so this whole comment stays maintainer-facing: nothing here
    /// is rendered to a user typing `--help`.
    #[arg(long, hide = true, conflicts_with = "remote")]
    print_clipboard: Option<char>,
    /// Print version. Long form only: `-V` starts a verbose engine session,
    /// the way it does for nvim itself.
    // declared by hand because that is the only way to drop the short form
    // clap's own generated version flag always carries: a bare `view -V`
    // must reach the engine as its `-V[N][file]`, not print a version string
    #[arg(long, action = clap::ArgAction::Version)]
    version: Option<bool>,
    /// Everything not claimed by a flag above, forwarded to the engine
    /// exactly as typed: `+42`, `-c 'set nu'`, `-R`, `-d`, `-O`, `-u NONE`,
    /// file paths, `-` for stdin. Every nvim argument is accepted, including
    /// ones this build has never heard of.
    ///
    /// Because this is a trailing var-arg, view's own long flags above must
    /// appear before the first passthrough token on the command line --
    /// once this field starts matching, it swallows every remaining token,
    /// `--tier`/`--clean` included.
    // `allow_hyphen_values` is what lets an nvim short flag like `-c` start
    // this catch-all instead of erroring as an unrecognized argument naming
    // view itself. Enumerating each nvim flag as an argument of view's own
    // instead was rejected as a maintenance treadmill that silently breaks
    // on every engine-pin bump that adds one
    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    passthrough: Vec<std::ffi::OsString>,
}

/// `print_clipboard`'s exit code when `arboard::Clipboard::new()` itself
/// fails (no display, e.g. a headless CI host or an SSH session with no
/// forwarded X11/Wayland): distinct from a normal failure so a caller can
/// treat it as "skip this check, not a regression" rather than a hard
/// failure of the content it expected to find.
const NO_DISPLAY_EXIT: i32 = 3;

/// `view --print-clipboard <register>`'s body: reads the system clipboard
/// directly via `arboard`, the same backend the clipboard worker
/// (`clipboard::run`) uses. Never touches nvim, the terminal, or any other
/// part of the editor: a check that the worker's own shadow-register
/// fallback could otherwise satisfy without ever reaching the real system
/// clipboard needs a read from a wholly separate process to be a genuine
/// proof, not an internally-consistent one.
fn print_clipboard(register: char) -> Result<()> {
    let mut clip = match arboard::Clipboard::new() {
        Ok(clip) => clip,
        Err(err) => {
            eprintln!("view: no system clipboard available for register '{register}': {err}");
            std::process::exit(NO_DISPLAY_EXIT);
        }
    };
    let text = clip
        .get_text()
        .with_context(|| format!("reading the system clipboard for register '{register}'"))?;
    print!("{text}");
    Ok(())
}

/// A `--remote` value split at the one colon that separates ssh's own
/// `[user@]host` destination from the path the remote editor opens.
///
/// Borrowed rather than owned: both halves are substrings of the value as
/// typed, which is what keeps the pass-through guarantee checkable -- there
/// is no owned copy anywhere on this path for a resolution step to have
/// rewritten.
struct RemoteTarget<'a> {
    /// `[user@]host`, handed to the ssh client exactly as typed.
    destination: &'a str,
    /// The path the remote editor opens, or `None` when the value named no
    /// path at all (`host`) or named an empty one (`host:`, scp's own
    /// spelling of the remote login directory). Both mean the remote
    /// `$HOME`, which is where an editor started with no file argument
    /// already opens.
    path: Option<&'a str>,
}

/// Splits `[user@]host[:path]` at the first colon that follows the
/// destination.
///
/// Not a hostname parser, and deliberately not one: the destination is ssh's
/// to interpret (an alias, an address, a name), and every syntax it accepts
/// must survive this function unchanged. Only two ambiguities are resolved,
/// both structural rather than syntactic:
///
/// - an `@` after the first colon belongs to the path (`host:/srv/a@b`), so
///   it never shifts where the destination is looked for;
/// - a bracketed address literal owns the colons inside it, so the scan for
///   the separator resumes after the closing bracket (`[2001:db8::1]:x`),
///   matching how scp reads the same syntax. A literal never closed
///   (`[::1`) names no path at all, which is what both ssh and scp do with
///   it: the whole value goes to the client, which reports the host it
///   could not resolve as the user typed it.
fn split_remote_target(value: &str) -> RemoteTarget<'_> {
    let host_start = match value.find('@') {
        Some(at) if !value[..at].contains(':') => at + 1,
        _ => 0,
    };
    let scan_from = if value[host_start..].starts_with('[') {
        match value[host_start..].find(']') {
            Some(close) => host_start + close + 1,
            None => {
                return RemoteTarget {
                    destination: value,
                    path: None,
                }
            }
        }
    } else {
        host_start
    };
    match value[scan_from..].find(':') {
        Some(rel) => {
            let (destination, rest) = value.split_at(scan_from + rel);
            let path = &rest[1..];
            RemoteTarget {
                destination,
                path: (!path.is_empty()).then_some(path),
            }
        }
        None => RemoteTarget {
            destination: value,
            path: None,
        },
    }
}

/// The [`RemoteSpec`] `cli`'s remote flags describe, for a `target` already
/// split out of `--remote`'s value.
///
/// The destination is not validated here beyond the split: an empty one, or
/// one a client reads as its own option, is refused by `Engine::spawn`
/// before a connection is attempted, and everything else is the client's own
/// to resolve and to report on.
fn remote_spec(cli: &Cli, target: &RemoteTarget<'_>) -> RemoteSpec {
    let mut spec = RemoteSpec::new(target.destination);
    // a non-UTF-8 path cannot cross to the far side as text and is refused
    // by `deny_incoherent_remote` before this runs; falling back to the
    // remote `PATH` lookup keeps this total without inventing a name
    if let Some(bin) = cli.nvim_bin.as_deref().and_then(std::path::Path::to_str) {
        spec = spec.with_remote_nvim_bin(bin);
    }
    if let Some(port) = cli.ssh_port {
        spec = spec.with_port(port);
    }
    for opt in &cli.ssh_opt {
        spec = spec.with_ssh_opt(opt.clone());
    }
    spec
}

/// The ssh options view sets for itself on every remote spawn, each with
/// what asking for it again would be asking for.
///
/// A client keeps the first value it obtains for an option and view's own
/// lead the command line, so an entry naming one of these could never take
/// effect. Refused rather than accepted-and-discarded: a connection flag
/// that reads as applied and is not is worth more to a user as an error.
const RESERVED_SSH_OPTS: [(&str, &str); 2] = [
    (
        "BatchMode",
        "view sets `BatchMode=yes` so a connection that needs a password or a \
         host-key answer fails fast: an embedded editor owns the terminal and \
         has none to spare for a prompt. Arrange the credential the connection \
         needs -- an agent, a key, a known_hosts entry -- rather than re-arming \
         a prompt nothing can answer",
    ),
    (
        "RequestTTY",
        "view runs the client with `-T`, and the remote command's own standard \
         input and output are the RPC channel this session talks to the editor \
         over: a pty placed between them would rewrite those bytes in flight",
    ),
];

/// Refuses a `--ssh-opt` entry that names an option view has already set for
/// itself, and one that names the port `--ssh-port` is setting.
///
/// `Port` is refused only alongside `--ssh-port`: on its own it is an
/// ordinary client option that applies normally, and only the pair is
/// ambiguous about which value the connection uses.
fn deny_inert_ssh_opts(cli: &Cli) -> Result<()> {
    for opt in &cli.ssh_opt {
        // ssh_config keywords take their argument separated by `=` or by
        // whitespace; splitting on `=` alone lets `-o 'BatchMode no'` walk
        // straight past this refusal with the same effect as `=no`. Leading
        // whitespace must be stripped before that split, not just after it:
        // splitting first would read the leading space itself as the
        // separator and leave an empty key that matches nothing.
        let trimmed = opt.trim_start();
        let key = trimmed
            .split_once(|c: char| c == '=' || c.is_ascii_whitespace())
            .map_or(trimmed, |(key, _)| key)
            .trim();
        if let Some((_, reason)) = RESERVED_SSH_OPTS
            .iter()
            .find(|(name, _)| key.eq_ignore_ascii_case(name))
        {
            anyhow::bail!(
                "view: `--ssh-opt {opt}` cannot take effect, and is refused \
                 rather than accepted and discarded: {reason}."
            );
        }
        if key.eq_ignore_ascii_case("Port") {
            if let Some(port) = cli.ssh_port {
                anyhow::bail!(
                    "view: `--ssh-opt {opt}` and `--ssh-port {port}` both set \
                     the port, and view's own `-p` leads the ssh command line, \
                     so the client would keep {port} and discard this entry. \
                     Set the port once, through either flag."
                );
            }
        }
    }
    Ok(())
}

/// Refuses the `--remote` combinations that cannot be honoured, ahead of the
/// terminal setup and the spawn, with a message naming both halves of the
/// conflict.
///
/// `-` is refused whatever stdin happens to be, unlike
/// [`deny_unsupported_stdin_relay`]'s local check: the ssh client's own
/// standard input is the RPC channel a remote session talks to the editor
/// over, so there is no second descriptor to carry piped content, and a `-`
/// left in the arguments would have the remote editor read that channel as
/// buffer text rather than merely open an empty buffer.
fn deny_incoherent_remote(cli: &Cli) -> Result<()> {
    let Some(remote) = &cli.remote else {
        return Ok(());
    };
    // the same two destinations `Engine::spawn` refuses, refused here as
    // well because here is the only place they can be refused before the
    // terminal is taken over: a spawn-time refusal is correct but arrives
    // after a full alternate-screen enter and exit
    if remote.is_empty() {
        anyhow::bail!(
            "view: `--remote` was given an empty destination, so there is no \
             host to start the editor on. Name one as `[user@]host[:path]`."
        );
    }
    if remote.starts_with('-') {
        anyhow::bail!(
            "view: `--remote {remote}` names no host: a destination beginning \
             with a dash is read by the ssh client as one of its own options, \
             so it would configure the connection instead of naming its far \
             end. Give the destination as `[user@]host[:path]`, and pass \
             client options through --ssh-opt."
        );
    }
    deny_inert_ssh_opts(cli)?;
    if !stdin_operands(&cli.passthrough).is_empty() {
        anyhow::bail!(
            "view: `--remote {remote}` and `-` (read piped stdin into the \
             first buffer) cannot be combined: the ssh client's own standard \
             input is already the RPC channel this session talks to the \
             remote editor over, so there is no descriptor left to carry the \
             piped content to the far side. Pipe into a local session \
             (`... | view -`), or send the content over first (`... | ssh \
             {remote} 'cat > /tmp/piped'`) and open it with `view --remote \
             {remote}:/tmp/piped`."
        );
    }
    if let Some(bin) = &cli.nvim_bin {
        if bin.to_str().is_none() {
            anyhow::bail!(
                "view: `--nvim-bin {}` is not valid UTF-8, and `--remote \
                 {remote}` sends the editor's name to the far side as text: \
                 an ssh command line carries no encoding a byte sequence \
                 like this survives. Name the remote editor with a UTF-8 \
                 path, or drop --nvim-bin to run the `nvim` on the remote \
                 PATH.",
                bin.display()
            );
        }
    }
    Ok(())
}

/// The engine config `cli` asks for: the ordinary spawn, every passthrough
/// argument forwarded verbatim, and `--clean`/`--appname` layered on top of
/// it. A pure constructor of `cli` alone -- it dups no file descriptor and
/// touches no process state, unlike [`maybe_relay_stdin`], which a caller
/// composes on afterward when the stdin relay is wanted (see `main`'s own
/// call site).
///
/// A function rather than a few lines inside `main` so the constructor it
/// starts from is assertable. This is the editor a user's own session runs
/// and the one the measurement matrix measures a pinned fixture
/// configuration through, so it must keep starting from
/// [`EngineConfig::default`]: `EngineConfig::isolated` compiles here just as
/// well, and would spawn a child with `--clean` and a hermetic environment,
/// discarding the very config being measured. A matrix run recording that
/// child's numbers reports a large improvement and gates green. `--clean`
/// therefore appends the flag itself via `with_arg` rather than switching
/// constructors -- see [`Cli`]'s `clean` field for the rest of that
/// distinction.
fn engine_config(cli: &Cli) -> EngineConfig {
    let mut cfg = EngineConfig::default();
    let target = cli.remote.as_deref().map(split_remote_target);
    match &target {
        // `--nvim-bin` names the remote editor here and the local
        // `nvim_bin` is left at its default: a remote spawn runs no local
        // binary, so a path applied there would be a setting nothing reads
        Some(target) => cfg = cfg.with_remote(remote_spec(cli, target)),
        None => {
            if let Some(bin) = &cli.nvim_bin {
                cfg = cfg.with_nvim_bin(bin.clone());
            }
        }
    }
    if cli.clean {
        cfg = cfg.with_arg("--clean");
    }
    if let Some(appname) = &cli.appname {
        cfg = cfg.with_env("NVIM_APPNAME", appname.clone());
    }
    // ahead of `passthrough`, because the first file operand is the one nvim
    // makes current (and the left-hand side of a `-d` diff), and `--remote
    // host:path` promises that path opens the way a local `view path` would.
    // Nothing is at risk in that position: nvim reads its options wherever
    // they sit relative to the operands, verified against the pinned engine
    // for `-c`, `-u`, `+cmd`, `-O` and `-d`, and one whole token inserted at
    // the front of a whole token list can split no option from its value
    if let Some(path) = target.as_ref().and_then(|target| target.path) {
        cfg = cfg.with_arg(path);
    }
    for arg in &cli.passthrough {
        cfg = cfg.with_arg(arg.clone());
    }
    cfg
}

/// The config a *replacement* engine is spawned from, when the one this
/// session started with died.
///
/// Deliberately not [`engine_config`] threaded through
/// [`maybe_relay_stdin`] the way the first spawn is. The piped stdin a
/// `cmd | view -` session began with was consumed by the engine that died:
/// re-arming the relay hands the replacement a descriptor already at EOF,
/// and leaving `-` in the arguments *without* a relay armed is worse still,
/// since nvim would then read view's own `--embed` RPC channel as buffer
/// text. The dash goes.
///
/// What that costs is stated rather than hidden: the piped content comes
/// back only as far as nvim's own swap file holds it. view keeps no copy of
/// buffer text to replay -- nvim owns it -- so there is nothing else to
/// recover a `[No Name]` buffer from here.
///
/// Only a dash nvim reads as a file operand goes: one an option is carrying
/// (`-c -`) is that option's value, and dropping it would leave the option
/// holding whatever word came next.
fn respawn_config(cli: &Cli) -> EngineConfig {
    let mut cfg = engine_config(cli);
    let dropped = stdin_operands(&cfg.extra_args);
    if !dropped.is_empty() {
        cfg.extra_args = std::mem::take(&mut cfg.extra_args)
            .into_iter()
            .enumerate()
            .filter(|(index, _)| !dropped.contains(index))
            .map(|(_, arg)| arg)
            .collect();
    }
    cfg
}

/// Arms `cfg`'s stdin relay when `passthrough` names `-` and the process's
/// own stdin is not a terminal (`ls | view -`): duplicates it onto the
/// fixed descriptor `startup::spawn_and_attach` tells nvim to read via
/// `stdin_fd` (`EngineHandle::ui_attach_with_stdin_relay`), since child fd
/// 0 is already `--embed`'s own RPC channel and cannot double as the piped
/// content's descriptor.
///
/// A no-op everywhere else, including `-` typed at an interactive
/// terminal: there is no piped content to relay, so nvim reads its own
/// controlling terminal exactly like an ordinary `nvim -` invocation
/// would.
#[cfg(unix)]
fn maybe_relay_stdin(cfg: EngineConfig, passthrough: &[std::ffi::OsString]) -> EngineConfig {
    use std::io::IsTerminal;
    use std::os::fd::AsFd;

    let wants_stdin = !stdin_operands(passthrough).is_empty();
    if !wants_stdin || std::io::stdin().is_terminal() {
        return cfg;
    }
    match std::io::stdin().as_fd().try_clone_to_owned() {
        Ok(fd) => cfg.with_stdin_relay(fd),
        Err(_) => cfg,
    }
}

/// No relay mechanism exists off Unix yet: `-` still reaches nvim as a
/// literal passthrough argument, unchanged from `engine_config`'s ordinary
/// forwarding. Safe only because `main`'s own `deny_unsupported_stdin_relay`
/// has already refused to start at all whenever `-` is combined with a
/// non-tty stdin on this platform: `build_command` pipes the child's fd 0
/// unconditionally as the `--embed` RPC channel, so nvim has no inherited
/// stdin of its own left to fall back to here the way a plain `nvim -`
/// invocation would -- a `-` this function let through undefended would
/// have nvim read that RPC stream itself as buffer text instead.
#[cfg(not(unix))]
fn maybe_relay_stdin(cfg: EngineConfig, _passthrough: &[std::ffi::OsString]) -> EngineConfig {
    cfg
}

/// Refuses to start when `-` is combined with a non-tty stdin on a platform
/// with no stdin-relay mechanism ([`maybe_relay_stdin`]'s `cfg(not(unix))`
/// arm): `build_command` pipes the child's fd 0 unconditionally as the
/// `--embed` RPC channel (`process::build_command`), so unlike a plain
/// `nvim -` invocation, there is no inherited stdin left for nvim to read on
/// its own here -- letting the session start anyway would have nvim
/// consume the RPC stream itself as buffer text, corrupting the very
/// channel `view` talks to it over rather than merely doing nothing.
///
/// A no-op on Unix, where [`maybe_relay_stdin`]'s own `cfg(unix)` arm gives
/// `-` a real fd instead, and a no-op everywhere `-` is typed at an
/// interactive terminal, since there is no piped content to protect nvim
/// from reading in the first place.
#[cfg(not(unix))]
fn deny_unsupported_stdin_relay(passthrough: &[std::ffi::OsString]) -> Result<()> {
    use std::io::IsTerminal;
    let wants_stdin = !stdin_operands(passthrough).is_empty();
    if wants_stdin && !std::io::stdin().is_terminal() {
        anyhow::bail!(
            "view: `-` (read piped stdin into the first buffer) is not \
             supported on this platform yet: nvim's own fd 0 here is \
             already the --embed RPC channel this process spawns it over, \
             and there is no relay mechanism to give nvim a separate \
             descriptor for the caller's own piped content the way the \
             Unix build does. Redirect the piped content to a file and \
             open that instead."
        );
    }
    Ok(())
}

#[cfg(unix)]
fn deny_unsupported_stdin_relay(_passthrough: &[std::ffi::OsString]) -> Result<()> {
    Ok(())
}

/// The `view.toml` path this session reads: `None` for `--clean` (no user
/// config at all, matching [`Cli`]'s `clean` field doc), `cli.config`
/// verbatim when given, otherwise the platform default
/// [`view_native::paths::config_path`] resolves.
///
/// A pure function of `cli` alone, hoisted out of `main` so the three-way
/// choice is assertable without a process environment or filesystem.
#[must_use]
fn resolve_config_path(cli: &Cli) -> Option<std::path::PathBuf> {
    if cli.clean {
        return None;
    }
    cli.config.clone().or_else(view_native::paths::config_path)
}

fn main() -> Result<()> {
    // startup's own VIEW_LOG "startup" line (see startup::paint_shell_frame)
    // measures the shell-paint budget from this instant, not from
    // Term::init: the capability probe that init() runs is itself startup
    // work the design spec's 50ms target is meant to cover
    let process_start = Instant::now();
    vlog::init(process_start);
    let cli = Cli::parse();
    if let Some(register) = cli.print_clipboard {
        return print_clipboard(register);
    }
    deny_incoherent_remote(&cli)?;
    deny_unsupported_stdin_relay(&cli.passthrough)?;
    let cfg = engine_config(&cli);
    // read off the config rather than re-derived from `cli`: the client this
    // resolves is the client the spawn below runs, and the spec is gone once
    // `attach_in_background` consumes the config it belongs to
    let remote = cfg.remote().cloned();
    if let Some(remote) = &remote {
        // ahead of `Term::init`: a session with no client to run has nothing
        // to show, and a refusal printed after the alternate screen has been
        // entered and left is a refusal the user watches flash past
        remote_guard::deny_absent_ssh(remote)?;
    }
    let cfg = maybe_relay_stdin(cfg, &cli.passthrough);
    // strictly after the relay, which is the one consumer of the piped fd 0
    // this replaces, and strictly before the capability probe, crossterm and
    // `InputSource` each go looking for a terminal of their own
    #[cfg(unix)]
    if !view_tui::input::adopt_terminal_stdin() {
        vlog::log("startup", NO_TERMINAL_NOTICE);
        // also to stderr, and before the alternate screen exists: a session
        // shaped this way has no terminal to read the log on, and stderr is
        // exactly where its redirect puts the one file the user can still
        // read afterwards
        eprintln!("view: {NO_TERMINAL_NOTICE}");
    }

    let mut term =
        Term::init(cli.tier.map(Tier::from)).context("failed to initialize terminal backend")?;
    let (width, height) = term.size()?;
    let residue = term.take_residue();

    // the cwd is resolved once at startup, before any picker ever opens:
    // `Source::Files` with no root override searches from here
    let mut model =
        Model::with_term_size(width, height).with_cwd(std::env::current_dir().unwrap_or_default());
    model.caps = term.caps();
    vlog::log_with("startup", || {
        format!(
            "version={} caps tier={:?} sync={} truecolor={} kitty_kbd={} term={width}x{height}",
            VERSION, model.caps.tier, model.caps.sync, model.caps.truecolor, model.caps.kitty_kbd
        )
    });
    // opts into startup's placeholder shell (statusline bar plus a static
    // "waiting for nvim" indicator) instead of Model's ordinary
    // already-running default; update() flips this back to true for good
    // on the first grid Flush
    model.content_painted = false;

    // resolved before the engine exists because the theme cache is keyed on
    // it, so cold start can already paint last session's colors before nvim
    // answers `ui_attach` with its own `default_colors_set`. The `[native]`
    // table behind the same path is read later, once there is a channel for
    // the features it enables to notify back over (see `NativeSession`).
    //
    // Any notice this block owes the user is buffered as an effect rather
    // than printed: the terminal is already raw-mode/alternate-screen owned
    // by `Term::init` above, where a bare stderr write is invisible at best
    // (see `TerminalGuard`'s doc comment) and no effect executor exists yet
    // to run a native notice through. `run_cutover`'s own toast timer picks
    // these up the same way it already does for `drained.toast_effects` --
    // see that binding's construction below.
    let mut pre_executor_effects: Vec<Effect> = Vec::new();

    // seeded here, once, before the engine exists: `update()` has no
    // filesystem access, so whether this project is trusted for AI agent
    // access has to arrive as already-resolved state on `Model` (see
    // `view_ai::TrustStore` and the `Msg::FeatureInvoke` gate that reads
    // `model.ai_trusted`). A store that cannot be read at all fails closed
    // -- `model.ai_trusted` stays `false` (`Model::new`'s own default)
    // rather than risking a stale or corrupt read being treated as trust.
    match view_ai::TrustStore::load() {
        Ok(store) => model.ai_trusted = store.is_trusted(&model.cwd),
        Err(err) => {
            pre_executor_effects.extend(model.engine.record_native_notice(
                format!(
                    "view: could not read the AI trust store ({err}); this project will be \
                     asked to trust AI agent access again this run"
                ),
                false,
            ));
        }
    }

    let config_path = resolve_config_path(&cli);

    // seeded here, once, before the engine exists, for the same reason
    // `model.ai_trusted` is above: `update()` cannot read `view.toml`, so
    // whether the feature is on at all has to arrive as already-resolved
    // state (see the `Msg::FeatureInvoke` gate that reads `model.ai_enabled`
    // ahead of the trust gate -- a disabled feature must not prompt for
    // trust either). Diverges from `AiConfig::load`'s own "no file is the
    // full experience" contract on one path: a file that exists but cannot
    // be read or parsed fails toward disabled rather than the enabled
    // default the successful case leaves it at, so a broken config can only
    // ever narrow what a user's untouched `view.toml` already granted,
    // never silently widen it.
    match view_ai::AiConfig::load(config_path.as_deref()) {
        Ok(cfg) => model.ai_enabled = cfg.enabled(),
        Err(err) => {
            model.ai_enabled = false;
            pre_executor_effects.extend(model.engine.record_native_notice(
                format!("view: could not read [ai] from view.toml ({err}); the AI agent panel is disabled this run"),
                false,
            ));
        }
    }

    match &config_path {
        Some(path) => {
            let (cached, notice) = theme_cache::load(path);
            vlog::log_with("theme", || {
                format!(
                    "cache {} path={}",
                    if cached.is_some() { "hit" } else { "miss" },
                    path.display()
                )
            });
            if let Some(notice) = notice {
                pre_executor_effects.extend(model.engine.record_native_notice(notice, false));
            }
            // only seeds on a genuine cache hit: seeding from a miss's
            // Theme::default() would register TabLineSel/PmenuSel with
            // all-false attrs, permanently defeating Theme::from_hl's
            // emphasis fallback for the pre-attach frame (see
            // theme_cache::load's doc comment)
            if let Some(cached) = cached {
                model
                    .engine
                    .replace_hl(theme_cache::seeded_hl_table(&cached));
            }
        }
        // --clean asked for exactly this: no config path at all, so there
        // is nothing to warn about
        None if cli.clean => {}
        None => {
            pre_executor_effects.extend(model.engine.record_native_notice(
                "view: cannot resolve a config path (no XDG_CONFIG_HOME, HOME, or APPDATA set); theme cache disabled this run".to_string(),
                false,
            ));
        }
    }

    // painted before the engine even spawns, themed from the cache just
    // seeded above, so a slow-starting nvim can never delay the terminal's
    // first visible content
    startup::paint_shell_frame(&mut term, &model, process_start)
        .context("failed to paint the startup shell frame")?;

    // created here, not inside runtime::run: input capture has to be live
    // immediately after the shell paints, well before the engine exists,
    // or anything typed during attach would be lost. On unix that means
    // opening the pollable input handle (keys wait in the kernel's tty
    // queue until the pre-attach wait drains them inline); off unix it
    // means starting the input thread against a channel that exists.
    let (raw_tx, msg_rx) = mpsc::sync_channel(startup::MSG_CHANNEL_CAPACITY);
    let term_size = view_tui::terminal::TermSizeCell::default();
    #[cfg(unix)]
    let mut input_source = view_tui::input::InputSource::open()
        .context("failed to open the pollable terminal input handle")?;
    #[cfg(not(unix))]
    let mut input_source = ();
    #[cfg(unix)]
    let msg_tx = wake::LoopSender::with_waker(
        raw_tx,
        wake::LoopWaker::new().context("failed to create the runtime loop's wake pipe")?,
    );
    #[cfg(not(unix))]
    let msg_tx = {
        view_tui::terminal::spawn_input_thread(raw_tx.clone(), term_size.clone());
        wake::LoopSender::new(raw_tx)
    };

    let engine_rx = startup::attach_in_background(cfg, width, height, residue, msg_tx.clone());
    let drained = startup::drain_pre_attach(
        &msg_rx,
        &msg_tx,
        &mut model,
        &mut term,
        &mut input_source,
        &term_size,
    );
    let attach_result = engine_rx
        .recv()
        .context("engine attach thread ended without a result")?;
    match &attach_result {
        Ok(engine) => vlog::log_with("engine", || {
            format!(
                "attach ok pid={} channel={} api={}.{}",
                engine.pid(),
                engine.api_info.channel_id,
                engine.api_info.version_major,
                engine.api_info.version_minor
            )
        }),
        Err(failure) => vlog::log_with("engine", || format!("attach failed: {failure:?}")),
    }
    let mut engine = attach_result.map_err(|failure| {
        // a remote session never runs a local nvim, so the local hints would
        // send the user to a binary and a PATH that had no part in it; what
        // it gets instead names the client, the connection, or the far
        // side's editor, whichever the failure was actually about
        if let Some(remote) = &remote {
            let context = remote_guard::attach_failure_context(remote, &failure);
            let err = match failure {
                startup::AttachFailure::Spawn(err) | startup::AttachFailure::Attach(err) => err,
            };
            return anyhow::Error::new(err).context(context);
        }
        match failure {
            startup::AttachFailure::Spawn(err) => anyhow::Error::new(err)
                .context("failed to spawn the nvim process (check --nvim-bin / PATH)"),
            startup::AttachFailure::Attach(err) => anyhow::Error::new(err)
                .context("engine attach failed or timed out after nvim started"),
        }
    })?;

    // attach_sink -- the only code path that connects the engine's pump to
    // msg_tx at all -- runs here, strictly after EngineReady was already
    // observed above; see startup::attach_in_background's doc comment for
    // why that makes a pump-originated message reaching msg_tx ahead of
    // EngineReady structurally impossible rather than merely unobserved. It
    // returns what it found staged instead of sending it: msg_tx has no
    // guaranteed consumer yet at this point (runtime::run's loop starts
    // below), so a send performed here has no bound on how long it could
    // block -- see damage::PumpShared::attach_sink's doc comment.
    let (pump, cutover) = engine.start_pump(msg_tx.clone());
    let pending_redraw = if cutover.redraw_pending {
        pump.take_damage()
    } else {
        Vec::new()
    };

    // `.with_toast_timer` wired on this executor too, not only the one
    // `runtime::run` builds later: `drained.toast_effects` (buffered while
    // no executor existed at all, in `drain_pre_attach`), `pre_executor_effects`
    // (buffered the same way for the theme-cache/config-path notices above,
    // built even earlier), and `load`'s own broken-config notice below all
    // need a real toast-expiry timer the moment they run, which is here --
    // strictly before `runtime::run`'s loop -- not deferred any further than
    // "the first executor that exists."
    let executor = runtime::Executor::new(engine.handle.clone()).with_toast_timer(msg_tx.clone());
    for eff in pre_executor_effects
        .into_iter()
        .chain(drained.toast_effects)
    {
        let _ = executor.run(eff);
    }
    // built before the cutover, not after: a config that sources quickly has
    // already fired `VimEnter` into the presink by now, and that message is
    // what triggers this session's takeover and key registration
    let (mut native, load_effects) =
        native::NativeSession::load(config_path.clone(), engine.api_info.channel_id, &mut model);
    for eff in load_effects {
        let _ = executor.run(eff);
    }
    // built alongside it, for the same reason: a config that sets a
    // colorscheme has already fired the bridge's own autocmd by now
    let mut theme_bridge = bridge::ThemeBridge::new(config_path.as_deref());
    let mut follow_ups = runtime::FollowUps {
        native: &mut native,
        theme: &mut theme_bridge,
        speculate: crate::speculate::SpeculationClock::default(),
    };
    // Resolves the presink messages, the pending redraw, and the pre-attach
    // input buffer directly through update()/Executor -- never by touching
    // msg_tx, whose only reader (runtime::run's loop) has not started yet.
    // See run_cutover's doc comment for the full ordering and
    // no-blocking-send argument.
    let outcome = startup::run_cutover(
        &mut model,
        &executor,
        &mut follow_ups,
        startup::CutoverInput {
            presink: cutover.presink,
            pending_redraw,
            resize: drained.resize,
            keys: drained.keys,
        },
        || engine.wait_exit(),
    );
    if let startup::CutoverOutcome::Quit(code) = outcome {
        vlog::log_with("engine", || format!("exit code={code}"));
        // nvim already reported its own exit (a presink Msg::EngineStopped,
        // translated by run_cutover): drop explicitly so Engine's Drop
        // graceful-shutdown sequence still runs, since process::exit below
        // would otherwise skip every destructor on this stack
        drop(engine);
        term.restore_now();
        // after restore_now, not before: persist_theme's own diagnostic (on
        // a cache-write failure) is a plain stderr write, and the terminal
        // is raw-mode/alternate-screen owned until the line above -- see
        // report_fatal_reason's doc comment for the same ordering
        // requirement on the read side.
        persist_theme(&model, &config_path);
        report_fatal_reason(&model);
        std::process::exit(code);
    }

    // built fresh per restart rather than stored once: `EngineConfig` is
    // consumed by the spawn it describes
    let respawn = || respawn_config(&cli);
    let (model, exit_code) = runtime::run(
        model,
        recovery::EngineSession {
            engine,
            pump,
            respawn: &respawn,
        },
        runtime::MsgChannel {
            tx: msg_tx.clone(),
            rx: msg_rx,
        },
        runtime::InputHandles {
            term_size,
            input: &mut input_source,
        },
        &mut follow_ups,
        &mut term,
    )?;
    vlog::log_with("engine", || format!("exit code={exit_code}"));
    // std::process::exit bypasses destructors, so the terminal must be
    // restored explicitly first; every other return path (an error
    // propagated via `?` above) is covered by `Drop` on `term`. Also why
    // persist_theme runs after this line and not before: its own
    // diagnostic (on a cache-write failure) is a plain stderr write, valid
    // only once the terminal is no longer raw-mode/alternate-screen owned.
    term.restore_now();
    persist_theme(&model, &config_path);
    report_fatal_reason(&model);
    std::process::exit(exit_code);
}

/// Persists `model`'s current theme to `config_path`'s cache slot, on
/// every exit path that reaches one (both quit shapes call this identically
/// rather than each carrying its own copy, so a future third exit path
/// cannot copy one and silently miss the store).
///
/// Callers must call this only after `term.restore_now()`: a write failure
/// here is reported with a plain `eprintln!`, which is only safe once the
/// terminal is no longer raw-mode/alternate-screen owned (see
/// `report_fatal_reason`'s doc comment for the same requirement on the
/// read side).
fn persist_theme(model: &Model, config_path: &Option<std::path::PathBuf>) {
    if let Some(path) = config_path {
        if let Some(notice) = theme_cache::store(Theme::from_hl(model.engine.hl()), path) {
            eprintln!("{notice}");
        }
    }
}

/// Reports `model.fatal_reason` (set by a `Msg::EngineStopped` whose reader
/// thread stopped for a reason other than an ordinary process exit) to
/// stderr. Called only after `term.restore_now()`: the reader thread that
/// originates this reason never writes it directly itself, since it runs
/// headless behind the terminal's raw-mode alternate screen, where a write
/// would be invisible or corrupt the screen (see `Msg::EngineStopped`'s doc
/// comment in `view-core`).
fn report_fatal_reason(model: &Model) {
    if let Some(reason) = &model.fatal_reason {
        vlog::log("fatal", reason);
        eprintln!("view: {reason}");
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;
    use clap::CommandFactory;
    use std::ffi::OsString;

    // The ordering rule lives only in `passthrough`'s own field doc, which
    // nothing renders to a user typing `--help`; a rule a user cannot see
    // until they hit it (`view notes.md --tier basic` reaching nvim as a
    // literal `--tier basic` and erroring inside it) is not documented in
    // any way that helps them, so this pins the rendered `--help` output
    // actually carries it.
    #[test]
    fn rendered_help_states_the_flags_before_passthrough_ordering_rule() {
        let help = Cli::command().render_long_help().to_string();
        assert!(
            help.contains("before the first argument meant for nvim"),
            "the rendered --help must state the ordering rule, got:\n{help}"
        );
    }

    /// A replacement engine is not a second first-spawn: the pipe that fed
    /// the original is drained, and `-` left in place with no relay armed
    /// would have nvim read the RPC channel itself as buffer text.
    #[test]
    fn a_replacement_engine_is_never_pointed_at_the_pipe_the_first_one_drank() {
        let cli = Cli::parse_from(["view", "-"]);
        assert_eq!(
            engine_config(&cli).extra_args,
            vec![std::ffi::OsString::from("-")],
            "the first spawn must still be handed the dash it was asked for"
        );
        assert!(
            respawn_config(&cli).extra_args.is_empty(),
            "the replacement was pointed back at a pipe with nothing left in \
             it: {:?}",
            respawn_config(&cli).extra_args
        );
        assert!(
            !respawn_config(&cli).stdin_relay_requested(),
            "the replacement re-armed the relay on a drained descriptor"
        );

        let mixed = Cli::parse_from(["view", "-", "notes.md"]);
        assert_eq!(
            respawn_config(&mixed).extra_args,
            vec![std::ffi::OsString::from("notes.md")],
            "dropping the dash must not cost the replacement the real file \
             the session also named"
        );
    }

    #[test]
    fn the_editor_spawns_the_users_own_environment_not_a_hermetic_one() {
        let cfg = engine_config(&Cli::parse_from(["view"]));
        assert!(
            cfg.extra_args.is_empty(),
            "the editor a user runs carries a spawn argument of its own: \
             --clean here discards the user's config and plugins, and the \
             measurement matrix would record a plugin-free baseline for a \
             fixture it believes it measured; got {:?}",
            cfg.extra_args
        );
        assert!(
            cfg.env_plan().is_empty(),
            "the editor a user runs rewrites their environment: a hermetic \
             plan here detaches every session from the config it is supposed \
             to load; got {:?}",
            cfg.env_plan()
        );
    }

    #[test]
    fn a_bare_positional_argument_is_the_only_argument_the_cli_adds() {
        let cfg = engine_config(&Cli::parse_from(["view", "notes.txt"]));
        assert_eq!(cfg.extra_args, vec![OsString::from("notes.txt")]);
        assert!(cfg.env_plan().is_empty(), "{:?}", cfg.env_plan());
    }

    #[test]
    fn nvim_bin_replaces_the_path_lookup() {
        let cfg = engine_config(&Cli::parse_from(["view", "--nvim-bin", "/opt/nvim"]));
        assert_eq!(cfg.nvim_bin, std::path::PathBuf::from("/opt/nvim"));
    }

    // A leading `+42` (an engine "go to line" argument) must reach the
    // engine byte-for-byte, not be rejected by clap or split from the file
    // that follows it.
    #[test]
    fn a_leading_plus_line_number_reaches_the_engine_verbatim() {
        let cfg = engine_config(&Cli::parse_from(["view", "+42", "notes.md"]));
        assert_eq!(
            cfg.extra_args,
            vec![OsString::from("+42"), OsString::from("notes.md")],
            "a +N argument must reach nvim exactly as typed, in order"
        );
    }

    #[test]
    fn short_flags_and_their_own_values_pass_through_untouched() {
        let cfg = engine_config(&Cli::parse_from(["view", "-c", "set nu", "-R", "notes.md"]));
        assert_eq!(
            cfg.extra_args,
            vec![
                OsString::from("-c"),
                OsString::from("set nu"),
                OsString::from("-R"),
                OsString::from("notes.md"),
            ]
        );
    }

    #[test]
    fn diff_mode_forwards_both_files_after_the_flag() {
        let cfg = engine_config(&Cli::parse_from(["view", "-d", "a.txt", "b.txt"]));
        assert_eq!(
            cfg.extra_args,
            vec![
                OsString::from("-d"),
                OsString::from("a.txt"),
                OsString::from("b.txt"),
            ]
        );
    }

    #[test]
    fn vertical_split_flag_forwards_both_files() {
        let cfg = engine_config(&Cli::parse_from(["view", "-O", "a.rs", "b.rs"]));
        assert_eq!(
            cfg.extra_args,
            vec![
                OsString::from("-O"),
                OsString::from("a.rs"),
                OsString::from("b.rs"),
            ]
        );
    }

    #[test]
    fn explicit_init_forwards_u_and_its_value() {
        let cfg = engine_config(&Cli::parse_from(["view", "-u", "NONE", "notes.md"]));
        assert_eq!(
            cfg.extra_args,
            vec![
                OsString::from("-u"),
                OsString::from("NONE"),
                OsString::from("notes.md"),
            ]
        );
    }

    // view's own long flags must still be parsed by view and never leak
    // into the engine's argument list -- the failure mode a trailing_var_arg
    // catch-all risks.
    #[test]
    fn tier_basic_is_parsed_by_view_and_never_forwarded_to_the_engine() {
        let cli = Cli::parse_from(["view", "--tier", "basic", "notes.md"]);
        assert!(matches!(cli.tier, Some(TierArg::Basic)));
        let cfg = engine_config(&cli);
        assert_eq!(
            cfg.extra_args,
            vec![OsString::from("notes.md")],
            "--tier and its value must never reach the engine, got {:?}",
            cfg.extra_args
        );
    }

    #[test]
    fn nvim_bin_before_passthrough_is_claimed_by_view_not_forwarded() {
        let cfg = engine_config(&Cli::parse_from([
            "view",
            "--nvim-bin",
            "/opt/nvim/bin/nvim",
            "--tier",
            "basic",
            "notes.md",
        ]));
        assert_eq!(cfg.nvim_bin, std::path::PathBuf::from("/opt/nvim/bin/nvim"));
        assert_eq!(cfg.extra_args, vec![OsString::from("notes.md")]);
    }

    #[test]
    fn appname_sets_nvim_appname_in_the_childs_environment() {
        let cfg = engine_config(&Cli::parse_from(["view", "--appname", "work", "notes.md"]));
        assert_eq!(
            cfg.env_plan(),
            vec![(OsString::from("NVIM_APPNAME"), Some(OsString::from("work")))],
            "got {:?}",
            cfg.env_plan()
        );
        assert_eq!(cfg.extra_args, vec![OsString::from("notes.md")]);
    }

    // --clean is view's own triage tool: bundled engine, no user config,
    // native defaults on. It must append the bare flag through `with_arg`,
    // never route through `EngineConfig::isolated`, whose extra `-n` and
    // hermetic environment plan are reserved for the oracle/measurement
    // matrix (see `engine_config`'s doc comment).
    #[test]
    fn clean_appends_only_the_clean_flag_never_isolateds_extra_n_or_hermetic_env() {
        let cfg = engine_config(&Cli::parse_from(["view", "--clean"]));
        assert_eq!(cfg.extra_args, vec![OsString::from("--clean")]);
        assert!(
            cfg.env_plan().is_empty(),
            "--clean must not carry isolated()'s hermetic environment plan, got {:?}",
            cfg.env_plan()
        );
    }

    #[test]
    fn clean_forces_no_config_path_even_when_config_is_also_given() {
        let cli = Cli::parse_from(["view", "--clean", "--config", "./off.toml"]);
        assert_eq!(
            resolve_config_path(&cli),
            None,
            "--clean means no user config at all, overriding --config"
        );
    }

    #[test]
    fn an_explicit_config_flag_is_used_verbatim() {
        let cli = Cli::parse_from(["view", "--config", "./off.toml"]);
        assert_eq!(
            resolve_config_path(&cli),
            Some(std::path::PathBuf::from("./off.toml"))
        );
    }

    #[test]
    fn with_neither_clean_nor_config_the_platform_default_is_used() {
        let cli = Cli::parse_from(["view"]);
        assert_eq!(resolve_config_path(&cli), view_native::paths::config_path());
    }

    // `-` must both reach the engine as a literal passthrough argument (so
    // nvim itself still sees the flag it interprets as "read stdin") and
    // arm `maybe_relay_stdin`'s clone: cargo test's own stdin is never a
    // controlling terminal, so this exercises the same `is_terminal() ==
    // false` branch `ls | view -` takes.
    //
    // `#[cfg(unix)]`-gated: `stdin_relay_requested()` is hardcoded `false`
    // off Unix (`process.rs`'s `#[cfg(not(unix))]` arm -- no relay
    // mechanism exists there), so this exact assertion would fail on
    // windows-latest, which is in `ci.yml`'s matrix. The non-Unix half of
    // this behavior is covered by its sibling immediately below.
    #[cfg(unix)]
    #[test]
    fn a_bare_dash_reaches_the_engine_and_arms_the_stdin_relay() {
        let cli = Cli::parse_from(["view", "-"]);
        let cfg = maybe_relay_stdin(engine_config(&cli), &cli.passthrough);
        assert_eq!(cfg.extra_args, vec![OsString::from("-")]);
        assert!(
            cfg.stdin_relay_requested(),
            "`view -` with a non-tty stdin must arm the relay, or piped \
             content silently reaches nvim as an empty stream instead"
        );
    }

    // Runs only where `deny_unsupported_stdin_relay`'s real (non-Unix) arm
    // exists: on Unix the function is an unconditional `Ok(())`, so this
    // would assert nothing there. Exercised by the Windows CI mirror this
    // project already runs (`winserver`); cargo test's own stdin is not a
    // terminal, matching `ls | view -`'s shape.
    #[cfg(not(unix))]
    #[test]
    fn a_bare_dash_off_unix_refuses_to_start_against_a_piped_stdin() {
        let err = deny_unsupported_stdin_relay(&[OsString::from("-")])
            .expect_err("no relay mechanism exists off Unix; starting anyway would have nvim read its own RPC channel as buffer text");
        assert!(
            err.to_string().contains('-'),
            "the error must name the flag it is refusing, got {err}"
        );
    }

    // nvim's `-V[N][file]` and clap's generated short version flag both want
    // `-V`, and the passthrough contract says the nvim reading wins: a user
    // with `view -V` in their fingers wants a verbose engine session, not a
    // version string. `-V` bare (nvim's own "verbose level 10") is the shape
    // that regresses silently if the short form is ever reclaimed -- the
    // attached-value shapes below never matched a clap short flag anyway.
    #[test]
    fn nvims_verbose_flag_reaches_the_engine_rather_than_printing_views_version() {
        for argv in [
            &["view", "-V"][..],
            &["view", "-V1"],
            &["view", "-V10", "notes.md"],
            &["view", "-V2/tmp/nvim.log"],
        ] {
            let cli = Cli::try_parse_from(argv.iter().copied())
                .map_err(|err| format!("{argv:?} was rejected or claimed by clap: {err}"))
                .expect("nvim's verbose flag must survive view's own parse");
            let expected: Vec<OsString> = argv[1..].iter().map(OsString::from).collect();
            assert_eq!(
                engine_config(&cli).extra_args,
                expected,
                "{argv:?} must reach nvim verbatim"
            );
        }
    }

    // The long form is the whole version surface, so it keeps working
    // unchanged after the short form was released to nvim.
    #[test]
    fn the_long_version_flag_still_reports_this_builds_version() {
        let err = Cli::try_parse_from(["view", "--version"])
            .err()
            .expect("--version exits through clap rather than returning a parsed Cli");
        assert_eq!(err.kind(), clap::error::ErrorKind::DisplayVersion);
        assert!(
            err.to_string().contains(VERSION),
            "--version must print this build's version, got {err}"
        );
    }

    // A short form left declared anywhere on the version argument is the
    // regression this pins: `--help` rendering it as `-V, --version` is the
    // user-visible tell, and it means clap claims the token before
    // `passthrough` can.
    #[test]
    fn no_short_form_is_declared_for_the_version_flag() {
        let version = Cli::command()
            .get_arguments()
            .find(|arg| arg.get_id().as_str() == "version")
            .map(|arg| arg.get_short())
            .expect("the CLI must still carry a version argument");
        assert_eq!(
            version, None,
            "the version flag reclaimed a short form; nvim's -V passthrough \
             breaks the moment it does"
        );
    }

    /// The remote surface's own helper: the spec a parsed `Cli` produced,
    /// or a failure naming what was missing, so every assertion below reads
    /// as one line about the flags rather than three about `Option`.
    fn spec_of(argv: &[&str]) -> RemoteSpec {
        let cli = Cli::try_parse_from(argv.iter().copied())
            .map_err(|err| format!("{argv:?} was rejected by clap: {err}"))
            .expect("the flags under test must parse");
        engine_config(&cli)
            .remote()
            .ok_or_else(|| format!("{argv:?} armed no remote spawn"))
            .expect("a destination must arm a remote spawn")
            .clone()
    }

    // A destination with no path is the whole value, and it opens no file:
    // an editor started with no file argument already opens the remote
    // login directory, which is what `view --remote host` promises.
    #[test]
    fn a_bare_destination_targets_the_host_and_opens_no_file() {
        let cli = Cli::parse_from(["view", "--remote", "prod-box"]);
        let cfg = engine_config(&cli);
        assert_eq!(
            cfg.remote().map(|remote| remote.target.as_str()),
            Some("prod-box")
        );
        assert!(
            cfg.extra_args.is_empty(),
            "a bare destination named no file, so none may be forwarded: {:?}",
            cfg.extra_args
        );
    }

    #[test]
    fn a_user_and_an_absolute_path_split_at_the_destinations_own_colon() {
        let cli = Cli::parse_from(["view", "--remote", "deploy@prod-box:/etc/app.conf"]);
        let cfg = engine_config(&cli);
        assert_eq!(
            cfg.remote().map(|remote| remote.target.as_str()),
            Some("deploy@prod-box"),
            "the user belongs to the destination ssh resolves, not to the path"
        );
        assert_eq!(cfg.extra_args, vec![OsString::from("/etc/app.conf")]);
    }

    // The pass-through guarantee: "relative to what" has an answer only on
    // the remote host, so a relative path must reach the remote editor
    // exactly as typed. Any local resolution shows up here as an absolute
    // path or one carrying this process's own cwd.
    #[test]
    fn a_relative_remote_path_is_never_resolved_against_the_local_cwd() {
        let cfg = engine_config(&Cli::parse_from([
            "view",
            "--remote",
            "prod-box:relative.txt",
        ]));
        assert_eq!(
            cfg.extra_args,
            vec![OsString::from("relative.txt")],
            "the remote path must reach the far side byte-for-byte"
        );
        let forwarded = std::path::Path::new(&cfg.extra_args[0]);
        assert!(
            forwarded.is_relative(),
            "a relative remote path was made absolute locally: {}",
            forwarded.display()
        );
        let cwd = std::env::current_dir().unwrap_or_default();
        assert!(
            !forwarded.starts_with(&cwd),
            "the remote path was resolved against this host's cwd ({}): {}",
            cwd.display(),
            forwarded.display()
        );
    }

    // A dotted relative path is the shape a local resolution step would
    // normalize away rather than merely prefix, so it is asserted
    // separately from the bare-filename case above.
    #[test]
    fn a_dotted_relative_remote_path_keeps_every_component_it_was_given() {
        let cfg = engine_config(&Cli::parse_from([
            "view",
            "--remote",
            "prod-box:../src/./main.rs",
        ]));
        assert_eq!(cfg.extra_args, vec![OsString::from("../src/./main.rs")]);
    }

    #[test]
    fn ssh_port_and_repeated_ssh_opts_reach_the_spec_in_order() {
        let spec = spec_of(&[
            "view",
            "--remote",
            "prod-box",
            "--ssh-port",
            "2222",
            "--ssh-opt",
            "ProxyJump=bastion",
            "--ssh-opt",
            "ConnectTimeout=4",
        ]);
        assert_eq!(spec.port, Some(2222));
        assert_eq!(
            spec.extra_ssh_opts,
            vec![
                String::from("ProxyJump=bastion"),
                String::from("ConnectTimeout=4"),
            ],
            "each --ssh-opt is its own -o, and order decides which value a \
             client keeps"
        );
    }

    // ssh_config's whitespace spelling of an option is not one of the
    // reserved keys, so it must clear the refusal untouched and reach the
    // client byte-for-byte -- neither re-spelled with `=` nor split apart.
    #[test]
    fn a_whitespace_spelled_ssh_opt_outside_the_refusal_set_is_forwarded_intact() {
        let cli = Cli::parse_from([
            "view",
            "--remote",
            "prod-box",
            "--ssh-opt",
            "ProxyJump bastion",
        ]);
        assert!(
            deny_incoherent_remote(&cli).is_ok(),
            "a whitespace-spelled option outside the reserved set is an \
             ordinary client option and applies normally"
        );
        let spec = spec_of(&[
            "view",
            "--remote",
            "prod-box",
            "--ssh-opt",
            "ProxyJump bastion",
        ]);
        assert_eq!(spec.extra_ssh_opts, vec![String::from("ProxyJump bastion")]);
    }

    // Trimming decides only whether the refusal fires; it must never rewrite
    // the value a passing entry hands to the client, leading whitespace
    // included.
    #[test]
    fn a_leading_whitespace_ssh_opt_outside_the_refusal_set_is_forwarded_with_its_whitespace_intact(
    ) {
        let cli = Cli::parse_from([
            "view",
            "--remote",
            "prod-box",
            "--ssh-opt",
            " ProxyJump bastion",
        ]);
        assert!(
            deny_incoherent_remote(&cli).is_ok(),
            "a leading-whitespace option outside the reserved set is an \
             ordinary client option and applies normally"
        );
        let spec = spec_of(&[
            "view",
            "--remote",
            "prod-box",
            "--ssh-opt",
            " ProxyJump bastion",
        ]);
        assert_eq!(
            spec.extra_ssh_opts,
            vec![String::from(" ProxyJump bastion")],
            "trimming for the refusal comparison must not rewrite the \
             forwarded value"
        );
    }

    // Silently ignoring a connection flag on a local session would let a
    // user believe a proxy or a port applied to a spawn that never opened a
    // connection at all.
    #[test]
    fn the_ssh_flags_are_a_parse_error_without_a_destination_to_apply_them_to() {
        for argv in [
            &["view", "--ssh-port", "2222"][..],
            &["view", "--ssh-opt", "ProxyJump=bastion"],
        ] {
            let err = Cli::try_parse_from(argv.iter().copied())
                .err()
                .ok_or_else(|| format!("{argv:?} was accepted with no --remote to apply it to"))
                .expect("a connection flag must be refused without a destination");
            assert_eq!(
                err.kind(),
                clap::error::ErrorKind::MissingRequiredArgument,
                "{argv:?} must fail as a missing --remote, got {err}"
            );
            assert!(
                err.to_string().contains("--remote"),
                "the error must name the flag that is missing, got {err}"
            );
        }
    }

    // A local session must stay exactly what it was: `remote()` is the one
    // switch between the two spawn paths, and a default-armed one would
    // route every ordinary `view notes.md` through an ssh client.
    #[test]
    fn a_session_without_the_flag_arms_no_remote_spawn() {
        assert!(engine_config(&Cli::parse_from(["view", "notes.md"]))
            .remote()
            .is_none());
    }

    // Colons inside a bracketed address literal belong to the address, the
    // same reading scp gives the same syntax. Without that, the first colon
    // of an IPv6 literal splits the destination mid-address.
    #[test]
    fn a_bracketed_address_literal_keeps_its_own_colons() {
        let cfg = engine_config(&Cli::parse_from([
            "view",
            "--remote",
            "[2001:db8::1]:notes.md",
        ]));
        assert_eq!(
            cfg.remote().map(|remote| remote.target.as_str()),
            Some("[2001:db8::1]")
        );
        assert_eq!(cfg.extra_args, vec![OsString::from("notes.md")]);

        let bare = engine_config(&Cli::parse_from(["view", "--remote", "deploy@[::1]"]));
        assert_eq!(
            bare.remote().map(|remote| remote.target.as_str()),
            Some("deploy@[::1]"),
            "a bracketed literal with no path is the whole destination"
        );
        assert!(bare.extra_args.is_empty(), "{:?}", bare.extra_args);
    }

    // An `@` after the separating colon is part of the path, not a user
    // delimiter: reading it as one would look for the destination's colon
    // past the path's own.
    #[test]
    fn an_at_sign_inside_the_path_does_not_move_the_destination() {
        let cfg = engine_config(&Cli::parse_from([
            "view",
            "--remote",
            "prod-box:/srv/a@b.txt",
        ]));
        assert_eq!(
            cfg.remote().map(|remote| remote.target.as_str()),
            Some("prod-box")
        );
        assert_eq!(cfg.extra_args, vec![OsString::from("/srv/a@b.txt")]);
    }

    // `host:` is scp's own spelling of the remote login directory. Passing
    // the empty string on to the editor instead would open a nameless
    // buffer no write can ever complete.
    #[test]
    fn an_empty_path_opens_the_remote_home_rather_than_a_nameless_buffer() {
        let cfg = engine_config(&Cli::parse_from(["view", "--remote", "prod-box:"]));
        assert_eq!(
            cfg.remote().map(|remote| remote.target.as_str()),
            Some("prod-box")
        );
        assert!(cfg.extra_args.is_empty(), "{:?}", cfg.extra_args);
    }

    // nvim reads its options wherever they sit relative to the file
    // operands, so the path costs nothing by leading them, and one whole
    // token at the front of a whole token list splits no option from its
    // value.
    #[test]
    fn the_remote_path_leads_the_options_it_is_opened_under() {
        let cfg = engine_config(&Cli::parse_from([
            "view",
            "--remote",
            "prod-box:notes.md",
            "-c",
            "set nu",
        ]));
        assert_eq!(
            cfg.extra_args,
            vec![
                OsString::from("notes.md"),
                OsString::from("-c"),
                OsString::from("set nu"),
            ]
        );
    }

    // `--remote host:path` promises the path opens the way a local `view
    // path` would, and locally the first file operand is the buffer nvim
    // makes current. Behind the passthrough it would be the other file that
    // opened, making that promise untrue for every session naming two.
    #[test]
    fn the_destinations_own_path_is_the_buffer_the_session_opens_on() {
        let cfg = engine_config(&Cli::parse_from([
            "view",
            "--remote",
            "prod-box:config.yaml",
            "notes.md",
        ]));
        assert_eq!(
            cfg.extra_args,
            vec![OsString::from("config.yaml"), OsString::from("notes.md")],
            "the path --remote named must be the first file operand, which is \
             the one nvim makes current"
        );
    }

    // The same ordering decides which file a diff puts on the left, so the
    // window layout follows the buffer identity rather than being a second
    // thing to reason about.
    #[test]
    fn a_remote_diff_opens_the_destinations_own_path_on_the_left() {
        let cfg = engine_config(&Cli::parse_from([
            "view",
            "--remote",
            "prod-box:mine.conf",
            "-d",
            "theirs.conf",
        ]));
        assert_eq!(
            cfg.extra_args,
            vec![
                OsString::from("mine.conf"),
                OsString::from("-d"),
                OsString::from("theirs.conf"),
            ]
        );
    }

    // --clean is view's own triage tool and stays available remotely: it is
    // a plain argument to the editor, unlike the hermetic environment plan
    // a remote spawn refuses outright.
    #[test]
    fn clean_reaches_the_remote_editor_as_an_ordinary_argument() {
        let cfg = engine_config(&Cli::parse_from([
            "view",
            "--clean",
            "--remote",
            "prod-box:notes.md",
        ]));
        assert!(cfg.remote().is_some());
        assert_eq!(
            cfg.extra_args,
            vec![OsString::from("--clean"), OsString::from("notes.md")]
        );
    }

    // The far side is the only host running an editor, so the flag that
    // names one must name that editor. Applying it to the local `nvim_bin`
    // instead is a setting a remote spawn never reads.
    #[test]
    fn nvim_bin_names_the_remote_editor_when_a_destination_is_given() {
        let cli = Cli::parse_from([
            "view",
            "--nvim-bin",
            "/opt/nvim/bin/nvim",
            "--remote",
            "prod-box",
        ]);
        let cfg = engine_config(&cli);
        assert_eq!(
            cfg.remote().map(|remote| remote.remote_nvim_bin.as_str()),
            Some("/opt/nvim/bin/nvim")
        );
        assert_eq!(
            cfg.nvim_bin,
            EngineConfig::default().nvim_bin,
            "a remote spawn runs no local binary, so the local one must stay \
             at its default rather than carry a value nothing reads"
        );
    }

    #[test]
    fn without_nvim_bin_the_remote_path_lookup_stands() {
        assert_eq!(
            spec_of(&["view", "--remote", "prod-box"]).remote_nvim_bin,
            "nvim",
            "the default must stay the remote PATH's own nvim"
        );
    }

    // A replacement engine reconnects to the same host and reopens the same
    // file: the remote spec and its path are part of what the session is,
    // not of the spawn that died.
    #[test]
    fn a_replacement_engine_reconnects_to_the_same_destination_and_file() {
        let cli = Cli::parse_from(["view", "--remote", "deploy@prod-box:/etc/app.conf"]);
        let cfg = respawn_config(&cli);
        assert_eq!(
            cfg.remote().map(|remote| remote.target.as_str()),
            Some("deploy@prod-box")
        );
        assert_eq!(cfg.extra_args, vec![OsString::from("/etc/app.conf")]);
    }

    // Handing a remote session a `-` would have the remote editor read the
    // ssh client's standard input, which is the RPC channel itself, as
    // buffer text. The engine refuses an armed relay of its own, but the
    // user-facing refusal is owed here: it is the only one that can name
    // the flags as typed and say what to do instead.
    #[test]
    fn a_piped_stdin_and_a_destination_are_refused_together_by_name() {
        let cli = Cli::parse_from(["view", "--remote", "prod-box", "-"]);
        let err = deny_incoherent_remote(&cli)
            .expect_err("a remote session has no descriptor to carry piped content");
        let text = err.to_string();
        for expected in ["--remote", "prod-box", "`-`", "| view -"] {
            assert!(
                text.contains(expected),
                "the refusal must name {expected}, got: {text}"
            );
        }
    }

    // The local guard fires only against a non-tty stdin, since a `-` typed
    // at a terminal reads that terminal. A remote session has no such
    // reading: the descriptor is the RPC channel whatever stdin is here.
    #[test]
    fn a_local_session_is_still_free_to_take_a_dash() {
        let cli = Cli::parse_from(["view", "-"]);
        assert!(
            deny_incoherent_remote(&cli).is_ok(),
            "the remote refusal must not touch a local piped session"
        );
    }

    // A remote command line crosses as text, so a name that is not text has
    // nothing to cross as. Refused by name rather than transcoded: a lossy
    // conversion would run some other binary and report success.
    #[cfg(unix)]
    #[test]
    fn a_non_utf8_editor_name_is_refused_rather_than_transcoded() {
        use std::os::unix::ffi::OsStringExt;
        let bin = OsString::from_vec(vec![b'/', b'o', b'p', b't', b'/', 0xff, b'v']);
        let cli = Cli::parse_from([OsString::from("view"), OsString::from("--nvim-bin"), bin]);
        let cli = Cli {
            remote: Some(String::from("prod-box")),
            ..cli
        };
        let err = deny_incoherent_remote(&cli)
            .expect_err("a name that is not text cannot cross to the far side");
        assert!(
            err.to_string().contains("--nvim-bin"),
            "the refusal must name the flag it refuses, got {err}"
        );
        assert_eq!(
            engine_config(&cli)
                .remote()
                .map(|remote| remote.remote_nvim_bin.as_str()),
            Some("nvim"),
            "the fallback behind the refusal must be the remote PATH lookup, \
             never a transcoded name"
        );
    }

    // A destination with no closing bracket is handed to the client whole,
    // which is what ssh and scp both do with it. Splitting at the literal's
    // own first colon invents a file (`:1`) and a host (`[`) the user never
    // typed, and buries the client's own "could not resolve" message.
    #[test]
    fn an_unterminated_bracket_names_no_path_and_reaches_the_client_whole() {
        let cfg = engine_config(&Cli::parse_from(["view", "--remote", "[::1"]));
        assert_eq!(
            cfg.remote().map(|remote| remote.target.as_str()),
            Some("[::1")
        );
        assert!(
            cfg.extra_args.is_empty(),
            "half the destination was invented as a file: {:?}",
            cfg.extra_args
        );
    }

    // An option view sets for itself leads the ssh command line and a client
    // keeps the first value it obtains, so these entries could never apply.
    // Accepting them silently is what makes a user believe a connection was
    // configured the way they asked.
    #[test]
    fn an_ssh_opt_that_could_never_apply_is_refused_by_name() {
        for (argv, named) in [
            (
                &["view", "--remote", "prod-box", "--ssh-opt", "BatchMode=no"][..],
                "BatchMode=no",
            ),
            (
                &["view", "--remote", "prod-box", "--ssh-opt", "batchmode=NO"],
                "batchmode=NO",
            ),
            (
                &["view", "--remote", "prod-box", "--ssh-opt", "BatchMode no"],
                "BatchMode no",
            ),
            (
                &["view", "--remote", "prod-box", "--ssh-opt", " BatchMode no"],
                " BatchMode no",
            ),
            (
                &[
                    "view",
                    "--remote",
                    "prod-box",
                    "--ssh-opt",
                    "RequestTTY=yes",
                ],
                "RequestTTY=yes",
            ),
            (
                &[
                    "view",
                    "--remote",
                    "prod-box",
                    "--ssh-opt",
                    "\tRequestTTY=yes",
                ],
                "\tRequestTTY=yes",
            ),
        ] {
            let cli = Cli::parse_from(argv.iter().copied());
            let err = deny_incoherent_remote(&cli)
                .expect_err("an entry a client would discard must be refused");
            assert!(
                err.to_string().contains(named),
                "the refusal must quote the entry it refuses, got {err}"
            );
        }
    }

    #[test]
    fn a_port_entry_is_refused_only_against_the_flag_that_would_outrank_it() {
        for port_opt in ["Port=1234", "Port 1234", " Port 1234"] {
            let clash = Cli::parse_from([
                "view",
                "--remote",
                "prod-box",
                "--ssh-port",
                "2222",
                "--ssh-opt",
                port_opt,
            ]);
            let err = deny_incoherent_remote(&clash)
                .expect_err("two ports, one of which the client would discard");
            let text = err.to_string();
            assert!(
                text.contains(port_opt) && text.contains("2222"),
                "the refusal must name both values, got {text}"
            );
        }

        let alone = Cli::parse_from(["view", "--remote", "prod-box", "--ssh-opt", "Port=1234"]);
        assert!(
            deny_incoherent_remote(&alone).is_ok(),
            "on its own a Port entry is an ordinary client option and applies \
             normally, so refusing it would cost a working spelling"
        );
    }

    // Both are knowable before the terminal is taken over. The engine
    // refuses them too, but only after a full alternate-screen enter and
    // exit has flashed past the message.
    #[test]
    fn a_destination_that_names_no_host_is_refused_before_the_terminal_is_taken() {
        let empty = Cli::parse_from(["view", "--remote", ""]);
        assert!(
            deny_incoherent_remote(&empty)
                .expect_err("an empty destination names no host")
                .to_string()
                .contains("--remote"),
            "the refusal must name the flag"
        );

        let dashed = Cli::parse_from(["view", "--remote=-oProxyCommand=touch /tmp/pwn"]);
        let err = deny_incoherent_remote(&dashed)
            .expect_err("a dash-leading destination is read as a client option, not a host");
        assert!(
            err.to_string().contains("--ssh-opt"),
            "the refusal must point at the flag that does carry client \
             options, got {err}"
        );
    }

    // --print-clipboard reads this host's clipboard and starts no editor at
    // all, so a destination alongside it is accepted and never used.
    #[test]
    fn the_clipboard_probe_and_a_destination_are_refused_as_a_pair() {
        let err = Cli::try_parse_from(["view", "--print-clipboard", "+", "--remote", "prod-box"])
            .err()
            .ok_or("a local clipboard read cannot serve a remote destination")
            .expect("the combination must be refused at parse time");
        assert_eq!(err.kind(), clap::error::ErrorKind::ArgumentConflict);
    }

    // A dash an option is carrying is that option's value, not a buffer:
    // arming a relay for it hands a live descriptor to a session that reads
    // no stdin, and refusing a remote session over it refuses a coherent
    // command line.
    #[test]
    fn a_dash_an_option_carries_is_never_read_as_a_piped_buffer() {
        let cli = Cli::parse_from(["view", "--remote", "prod-box", "-c", "-"]);
        assert!(
            deny_incoherent_remote(&cli).is_ok(),
            "`-c -` names no stdin buffer, so a remote session over it is \
             coherent"
        );

        let local = Cli::parse_from(["view", "-c", "-"]);
        assert_eq!(
            respawn_config(&local).extra_args,
            vec![OsString::from("-c"), OsString::from("-")],
            "stripping a dash the option carries leaves -c holding whatever \
             word came next"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_dash_an_option_carries_arms_no_stdin_relay() {
        let cli = Cli::parse_from(["view", "-c", "-"]);
        let cfg = maybe_relay_stdin(engine_config(&cli), &cli.passthrough);
        assert!(
            !cfg.stdin_relay_requested(),
            "a relay was armed for a session that reads no stdin"
        );
    }

    // The value syntax is the whole surface of --remote, and a user reads
    // it in --help before anywhere else.
    #[test]
    fn rendered_help_states_the_remote_value_syntax_and_the_ssh_flags() {
        let help = Cli::command().render_long_help().to_string();
        for expected in [
            "[USER@]HOST[:PATH]",
            "--ssh-port",
            "KEY=VALUE",
            // every file argument is a far-side path, and the ssh flags are
            // a parse error rather than an ignored setting without a
            // destination: both are surprises a user must not have to hit
            "Every other file argument is opened there too",
            "Requires `--remote`",
        ] {
            assert!(
                help.contains(expected),
                "--help must show {expected}, got:\n{help}"
            );
        }
    }

    // A field's doc comment is this CLI's help text, so it is read by people
    // who will never open the source: a rustdoc intra-doc link renders to
    // them as literal brackets around a path they cannot follow, and a
    // parser knob or a private function name documents how view is built
    // rather than how it is run. The rationale still exists, in ordinary
    // comments and on the functions it belongs to; only the rendered surface
    // is guarded here.
    #[test]
    fn rendered_help_documents_the_tool_and_never_its_implementation() {
        let help = Cli::command().render_long_help().to_string();
        for leak in [
            "[`",
            "`]",
            "allow_hyphen_values",
            "trailing_var_arg",
            "clap",
            "view_native::",
            "constructor",
            "doc comment",
        ] {
            assert!(
                !help.contains(leak),
                "--help leaks {leak:?} to a user who only wants to run view:\n{help}"
            );
        }
    }

    #[test]
    fn print_clipboard_is_claimed_by_view_and_never_forwarded() {
        let cli = Cli::parse_from(["view", "--print-clipboard", "+"]);
        assert_eq!(cli.print_clipboard, Some('+'));
        assert!(
            cli.passthrough.is_empty(),
            "--print-clipboard must not leak its register into the \
             engine's own argument list, got {:?}",
            cli.passthrough
        );
    }
}
