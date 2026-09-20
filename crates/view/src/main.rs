//! `view [FILE] --nvim-bin <path>`: CLI parsing and wiring for the terminal
//! frontend over an embedded Neovim engine. The startup sequence (shell
//! paint, background attach, pre-attach key buffering) lives in
//! [`startup`]; the steady-state loop itself lives in [`runtime`].

mod ai_context_worker;
mod ai_worker;
mod bridge;
mod clipboard;
mod engine_ops;
mod loop_msgs;
mod native;
mod osc52;
mod recovery;
mod remote_guard;
mod runtime;
mod speculate;
mod spinner;
mod startup;
mod theme_cache;
mod vlog;
mod wake;

use anyhow::{Context, Result};
use clap::Parser;
use std::sync::mpsc;
use std::time::Instant;
use view_core::model::{Look, Model, Panes, TermCaps, Tier};
use view_core::msg::Effect;
use view_core::theme::Theme;
use view_engine::process::{stdin_operands, BundledEngine, EngineConfig, RemoteSpec};
use view_native::config::{
    Overrides, Resolved, ResolvedConfig, ResolvedEngine, Source, TierChoice, ViewConfig,
};
use view_tui::terminal::Term;
use view_tui::tiers::CapsSource;

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

/// `--panes`'s value vocabulary, a `clap`-derived enum for the reason
/// [`TierArg`] is one. `Auto` is spelled here because a flag has no absence
/// to fall through to: writing it is how a user overrides a file that named
/// a mode.
#[derive(Copy, Clone, clap::ValueEnum)]
enum PanesArg {
    Auto,
    Tiles,
    Nvim,
}

impl From<PanesArg> for Option<Panes> {
    fn from(arg: PanesArg) -> Self {
        match arg {
            PanesArg::Auto => None,
            PanesArg::Tiles => Some(Panes::Tiles),
            PanesArg::Nvim => Some(Panes::Nvim),
        }
    }
}

impl From<TierArg> for TierChoice {
    fn from(arg: TierArg) -> Self {
        match arg {
            TierArg::Full => TierChoice::Full,
            TierArg::Standard => TierChoice::Standard,
            TierArg::Basic => TierChoice::Basic,
        }
    }
}

impl From<&Cli> for Overrides {
    fn from(cli: &Cli) -> Self {
        Self {
            tier: cli.tier.map(TierChoice::from),
            theme: cli.theme.clone(),
            nvim_bin: cli.nvim_bin.clone(),
            appname: cli.appname.clone(),
            // a bare switch can only say yes: multigrid comes back by
            // leaving it off, not by writing `--single-grid false`
            single_grid: cli.single_grid.then_some(true),
            panes: cli.panes.map(Option::<Panes>::from),
        }
    }
}

#[derive(Parser)]
#[command(
    name = "view",
    version = VERSION,
    disable_version_flag = true,
    about = "A modern terminal editor powered by Neovim",
    after_help = "view's own flags (--tier, --theme, --panes, --single-grid, --clean, --appname, \
                  --config, --nvim-bin, --remote, ...) \
                  must appear before the first argument meant for nvim: once a token does \
                  not match one of view's flags, every remaining token, a later \
                  view flag included, is forwarded to nvim verbatim."
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
    /// The colorscheme this session runs, as `:colorscheme` names it.
    /// Absent derives view's own chrome from whatever the user's config
    /// ended on, which is also what `auto` asks for.
    #[arg(long, value_name = "NAME")]
    theme: Option<String>,
    /// How window layout is drawn for this session: `tiles` gives every
    /// window a frame of view's own, `nvim` leaves the picture nvim paints
    /// for itself, and `auto` reads the session the way an absent key does.
    #[arg(long, value_name = "MODE")]
    panes: Option<PanesArg>,
    /// Attaches without `ext_multigrid`, so nvim composites its own window
    /// layout into one grid. The triage flag for a layout view draws
    /// differently than nvim would.
    #[arg(long)]
    single_grid: bool,
    /// Report the terminal capabilities this session resolved and where
    /// they came from, as a notice inside the session rather than a line on
    /// the screen it is about to take over. Implied by `--tier`, whose whole
    /// point is to change what that notice says.
    #[arg(long)]
    print_caps: bool,
    /// Spawns the bundled engine with no user config at all: `view.toml`
    /// and `init.lua` are both skipped, and every native feature resolves
    /// to its own shipped default.
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

/// The host the pill names for this session, or `None` for a local one.
///
/// ssh's own destination as the user typed it, path and all else split off:
/// a person with three windows open on three machines has nothing else on
/// screen that says which is which.
fn pill_host(remote: Option<&str>) -> Option<String> {
    remote.map(|value| split_remote_target(value).destination.to_string())
}

/// The [`RemoteSpec`] `cli`'s remote flags describe, for a `target` already
/// split out of `--remote`'s value.
///
/// The destination is not validated here beyond the split: an empty one, or
/// one a client reads as its own option, is refused by `Engine::spawn`
/// before a connection is attempted, and everything else is the client's own
/// to resolve and to report on.
fn remote_spec(cli: &Cli, engine: &ResolvedEngine, target: &RemoteTarget<'_>) -> RemoteSpec {
    let mut spec = RemoteSpec::new(target.destination);
    // a non-UTF-8 path cannot cross to the far side as text and is refused
    // by `deny_incoherent_remote` before this runs; falling back to the
    // remote `PATH` lookup keeps this total without inventing a name
    if let Some(bin) = engine
        .nvim_bin
        .value
        .as_deref()
        .and_then(std::path::Path::to_str)
    {
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
         needs, whether an agent, a key or a known_hosts entry, rather than re-arming \
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
fn engine_config(cli: &Cli, engine: &ResolvedEngine) -> EngineConfig {
    let mut cfg = EngineConfig::default();
    let target = cli.remote.as_deref().map(split_remote_target);
    match &target {
        // the resolved `nvim_bin` names the remote editor here and the
        // local one is left at its default: a remote spawn runs no local
        // binary, so a path applied there would be a setting nothing reads
        Some(target) => cfg = cfg.with_remote(remote_spec(cli, engine, target)),
        // a release layout answers with its own binary and runtime, so the
        // child runs the pinned pair rather than whatever PATH resolves; a
        // development build has none beside its executable and the PATH
        // lookup is the honest fallback. A named binary outranks both, and
        // the bundled layout is what the derived answer means: only a
        // caller that can look beside its own executable can resolve it
        None => match &engine.nvim_bin.value {
            Some(bin) => cfg = cfg.with_nvim_bin(bin.clone()),
            None => {
                if let Some(layout) = BundledEngine::resolve() {
                    cfg = cfg.with_bundled(layout);
                }
            }
        },
    }
    if cli.clean {
        cfg = cfg.with_arg("--clean");
    }
    if let Some(appname) = &engine.appname.value {
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

/// What a local spawn that failed owes the user: which editor view tried to
/// run, and which layer named it.
///
/// A resolved value with no provenance in its error message leaves the
/// precedence chain unusable for triage. "check --nvim-bin / PATH" sends a
/// user to two places they may have touched neither of, when the answer is
/// that their own `view.toml` named an editor that is not there.
fn spawn_failure_context(engine: &ResolvedEngine) -> String {
    match &engine.nvim_bin.value {
        Some(bin) => format!(
            "failed to spawn `{}`, the editor named by the {}",
            bin.display(),
            engine.nvim_bin.source.label()
        ),
        // the derived answer, whose two halves are both absent: no engine
        // shipped beside this binary and no `nvim` on PATH, so naming a
        // path here would name one nothing chose
        None => "failed to spawn the nvim process: no engine ships beside this binary and \
                 `nvim` was not found on PATH (name one with --nvim-bin, VIEW_ENGINE_NVIM_BIN \
                 or [engine] nvim_bin)"
            .to_string(),
    }
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
fn respawn_config(cli: &Cli, engine: &ResolvedEngine) -> EngineConfig {
    let mut cfg = engine_config(cli, engine);
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

/// Every key this invocation resolves, and the layer each answer came
/// from: a flag on the command line over a `VIEW_*` variable over `file`
/// over the value view derives for itself.
///
/// `--clean` is view's own triage tool, and the question it asks is
/// whether view or a user's own configuration is at fault. It already
/// reads no file ([`resolve_config_path`] answers `None` for it), and it
/// reads no environment either: a mode that answered that question for the
/// file while leaving `VIEW_NATIVE_PICKER` in play would be answering a
/// different one.
#[must_use]
fn resolve_session_config(cli: &Cli, file: &ViewConfig) -> ResolvedConfig {
    let flags = Overrides::from(cli);
    if cli.clean {
        // the defaults rather than `file`, which `resolve_config_path` has
        // already left unread for a clean session: stating both halves
        // here makes the mode's contract this function's own rather than
        // an agreement between two functions
        return view_native::config::resolve_with(&ViewConfig::defaults(), &flags, &|_| None);
    }
    view_native::config::resolve(file, &flags)
}

/// Reads every table `view-native` owns out of `config_path`, once, ahead
/// of the spawn whose own editor is one of the answers the file is a layer
/// of, and ahead of the attach that the `[native]` answers decide the
/// `ext_*` set for.
///
/// A config that cannot be read or parsed falls back to every default and
/// hands the error back rather than reporting it: this runs before the
/// terminal, the model and the effect executor exist, and the user is told
/// by [`note_unread_config`] once they do. Falling back rather than
/// refusing to start matches the loader's own contract that an absent file
/// is the full experience -- an editor does not decline to open a file over
/// a typo in an optional table -- but the user is told, because a silently
/// ignored `picker = false` is a feature they turned off still taking their
/// keys.
///
/// The fallback is deliberately the *full* set of surfaces, not the safest
/// one: a mistyped table must not also cost the user the palette and the
/// message overlay, which is what a fail-closed answer here would do.
fn load_view_config(
    config_path: Option<&std::path::Path>,
) -> (
    view_native::config::ViewConfig,
    Option<view_native::config::NativeConfigError>,
) {
    match view_native::config::ViewConfig::load(config_path) {
        Ok(file) => (file, None),
        Err(err) => (view_native::config::ViewConfig::defaults(), Some(err)),
    }
}

/// What a `view.toml` that could not be read owes the user, raised the
/// moment there is a model to raise it on.
///
/// A notice rather than a stderr write: by the time this runs the terminal
/// is in raw mode behind the alternate screen, where a bare stderr line is
/// invisible at best (see `TerminalGuard`'s doc comment).
fn note_unread_config(
    err: &view_native::config::NativeConfigError,
    model: &mut Model,
    notices: &mut Vec<Effect>,
) {
    vlog::log_with("native", || format!("config unreadable: {err}"));
    model.dirty = true;
    // the surfaces attached below are the fail-open default, not an answer
    // to what this user wrote, so nothing may later tell them to write a
    // `[native]` line they may already have written -- see
    // `Model::config_was_read`
    model.note_config_unread();
    notices.extend(model.engine.record_native_notice(
        format!("view: {err}; every native feature stays at its default this session"),
        false,
    ));
}

/// Resolves `[ai]` from `view.toml` into `model.ai_enabled` and the width
/// the panel opens at (`model.ai_panel_width_pct`), returning
/// whatever notice a broken config owes the user alongside which agent
/// `[ai]` names -- the same resolution `ai_worker::AiWorker` is later built
/// from, read once here rather than a second time at that call site so the
/// two can never disagree about what one `view.toml` said.
///
/// Hoisted out of `main` so the fail-closed leg -- the one that rots
/// silently, since a broken `[ai]` table is rare in practice -- is
/// assertable directly rather than only by reading the match arm. Diverges
/// from `AiConfig::resolve`'s own "no file is the full experience" contract
/// on one path: a file that exists but cannot be read or parsed fails
/// toward disabled rather than the enabled default the successful case
/// leaves it at, so a broken config can only ever narrow what a user's
/// untouched `view.toml` already granted, never silently widen it. The
/// agent spec returned on that path is `AiConfig::default`'s, which is
/// never acted on: `model.ai_enabled` is false, so nothing ever asks the
/// worker to spawn.
///
/// `clean` carries `--clean` across the crate boundary the key registry
/// cannot: `[ai]`'s two keys are resolved by the crate that parses that
/// table, and a triage mode that suppressed the file and the environment
/// for eleven keys and let `VIEW_AI_AGENT` through would be answering a
/// different question for the twelfth.
fn seed_ai_enabled(
    config_path: Option<&std::path::Path>,
    clean: bool,
    model: &mut Model,
) -> (Vec<Effect>, view_ai::AgentSpec) {
    match view_ai::AiConfig::resolve(config_path, clean) {
        Ok(cfg) => {
            model.ai_enabled = cfg.enabled();
            model.ai_panel_width_pct = cfg.panel_width();
            model.ai_review_open_target = cfg.review_open_target();
            let agent = cfg.agent_spec().clone();
            // a width or an open target that could not be read never fails
            // the table (see `resolve_panel_width`): failing it here would
            // answer a mistyped percentage by turning the agent off for
            // the run
            let effects = [cfg.panel_width_notice(), cfg.review_open_target_notice()]
                .into_iter()
                .flatten()
                .map(str::to_string)
                // and whatever the environment layer had to discard, on the
                // same terms: this table's notices reach a user through one
                // path, whichever layer produced them
                .chain(cfg.notices().iter().cloned())
                .flat_map(|notice| model.engine.record_native_notice(notice, false))
                .collect();
            (effects, agent)
        }
        Err(err) => {
            model.ai_enabled = false;
            let effects = model.engine.record_native_notice(
                format!(
                    "view: could not read [ai] from view.toml ({err}); the AI agent panel is disabled this run"
                ),
                false,
            );
            (effects, view_ai::AiConfig::default().agent_spec().clone())
        }
    }
}

/// The capability line for a session that asked to see it, or `None` for
/// one that did not.
///
/// The same facts the `"startup"` `VIEW_LOG` line carries, phrased for a
/// user rather than a log reader: a session is normally silent about its
/// own capabilities, and asks for this either outright (`--print-caps`) or
/// by overriding them, whose whole point is to change what this would have
/// said. The override is the *resolved* `[ui] tier` rather than the flag,
/// so a session that named its tier in the environment is shown what it
/// got exactly like one that named it on the command line -- and is told
/// which of the three layers named it, which is the half a user needs to
/// go and change it.
fn caps_notice(
    cli: &Cli,
    tier: &Resolved<Option<TierChoice>>,
    caps: TermCaps,
    source: CapsSource,
) -> Option<String> {
    (cli.print_caps || tier.value.is_some()).then(|| {
        // rendered from the capability register, so the line a user reads
        // and the table the build enforces are one thing: a capability that
        // gains a row is printed here with no edit, and one printed here
        // without a row cannot exist
        format!(
            "view: terminal capabilities: tier={:?} {} ({})",
            caps.tier,
            view_tui::tiers::resolved(&caps),
            caps_source_label(source, tier.source)
        )
    })
}

/// Where the capabilities came from, with an override named down to the
/// layer that set it.
///
/// [`CapsSource`] knows a resolved tier decided them and structurally
/// cannot know which layer resolved that tier -- `view-tui` is handed the
/// answer, not the chain. "tier override" alone leaves a user to guess
/// between a flag they typed, a variable they exported and a file they
/// wrote, which is the question provenance exists to answer.
fn caps_source_label(caps: CapsSource, tier: Source) -> String {
    if caps != CapsSource::Override {
        return caps.label().to_string();
    }
    match tier {
        Source::Flag => "--tier flag".to_string(),
        // the name the key registry generates, never a second copy of it
        Source::Env => view_native::config::keys()
            .iter()
            .find(|key| key.table == "ui" && key.key == "tier")
            .map_or_else(|| caps.label().to_string(), view_native::config::env_name),
        Source::File => "[ui] tier in your config file".to_string(),
        // an override with nothing above the derived layer to explain it is
        // a state the chain cannot produce, and so is a layer this build
        // has never heard of; naming the override as the override it is
        // beats inventing a layer that did not answer
        _ => caps.label().to_string(),
    }
}

/// Points fd 2 away from the terminal for the rest of the session and
/// returns the guard that hands it back, or `None` if there was nothing to
/// point it at.
///
/// The `VIEW_LOG` file when the session is capturing one, so a diagnostic a
/// library writes on its own -- macOS AppKit's, when the OS refuses a
/// pasteboard write -- is triage material beside the rest of the session's
/// record; the null device otherwise, because the alternative is the byte
/// landing on the alternate screen at whatever cell the emulator's cursor
/// sits on. Neither is a session-ending failure: an editor still runs with
/// its stderr wherever it started.
///
/// Returned rather than dropped here: the value restores fd 2, and it must
/// not do that while `main` still owns the terminal.
#[cfg(unix)]
#[must_use]
fn route_stderr_off_the_terminal() -> Option<view_tui::terminal::StderrGuard> {
    let sink = vlog::sink_dup().or_else(|| {
        std::fs::OpenOptions::new()
            .write(true)
            .open("/dev/null")
            .ok()
    })?;
    match view_tui::terminal::StderrGuard::redirect(&sink) {
        Ok(guard) => Some(guard),
        Err(e) => {
            vlog::log_with("startup", || format!("stderr stays on the terminal: {e}"));
            None
        }
    }
}

/// Writes down a tie the host refused, which view-proc cannot do for
/// itself: a leaf crate with no workspace dependency has no log to reach,
/// and a child running untied is otherwise indistinguishable from a tied
/// one until it is found reparented to init.
fn log_untied_child(note: &str) {
    vlog::log("proc", note);
}

fn main() -> Result<()> {
    // startup's own VIEW_LOG "startup" line (see startup::paint_shell_frame)
    // measures the shell-paint budget from this instant, not from
    // Term::init: the capability probe that init() runs is itself startup
    // work the design spec's 50ms target is meant to cover
    let process_start = Instant::now();
    vlog::init(process_start);
    // the writer first: preparing the tie can itself be refused (a pipe,
    // a thread, a /bin/sh), and a refusal raised with nowhere to write it
    // is the one that says no child of this session is tied
    view_proc::record_refusals_with(log_untied_child);
    // before any other thread exists, and as far from the engine spawn as
    // the body allows: off Linux the tie is a watcher process, and this is
    // what keeps forking it out of the spawn the startup budget measures
    view_proc::prepare_to_tie_children();
    let cli = Cli::parse();
    if let Some(register) = cli.print_clipboard {
        return print_clipboard(register);
    }
    deny_incoherent_remote(&cli)?;
    deny_unsupported_stdin_relay(&cli.passthrough)?;
    // read and resolved ahead of the spawn, not after it: the editor the
    // child runs is one of the keys the chain answers, so every layer has
    // to be in hand before there is a child. One file read of a few
    // hundred bytes and one environment sweep sit in front of the spawn
    // for it, both far below the first-paint budget the spawn itself
    // dominates
    let config_path = resolve_config_path(&cli);
    let (file, config_error) = load_view_config(config_path.as_deref());
    let resolved = resolve_session_config(&cli, &file);
    let view_config = resolved.tables.clone();
    let cfg = engine_config(&cli, &resolved.engine);
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

    // after every `is_terminal` question anything has left to ask of fd 2,
    // and before the child below can inherit it: from here the descriptor
    // answers for the sink, and a library's own diagnostic lands there
    // instead of on cells the painter believes it still owns
    #[cfg(unix)]
    let _stderr = route_stderr_off_the_terminal();

    // ahead of the spawn, and one ioctl: the child is started `--headless`
    // and lays its own windows out while this thread is still resolving
    // what kind of terminal it is talking to, so it has to be told the size
    // at spawn or it would source the user's config against nvim's 80x24
    // default and reflow every window at the attach.
    let (width, height) =
        view_tui::terminal::size_now().context("failed to read the terminal size")?;

    // ahead of `Term::init` rather than after it: the child sources
    // `init.lua` and opens its files without waiting for a UI
    // (`EngineConfig::with_late_attach`), so everything this thread does
    // between here and the attach is work nvim performs underneath rather
    // than behind. What sits in that window is the capability probe's first
    // window -- up to `tiers::PROBE_DEADLINE`, and a full network round
    // trip of it over ssh. The probed tier reaches the screen on the first
    // frame anyway: it is read off `term` below, before anything is
    // painted.
    //
    // The guard, not the call, is what makes this safe: from here to
    // `engine_result` this process owns a live nvim it has no other handle
    // on, so every `?` in between must kill it -- and must not park on a
    // channel while doing so, which is why the guard owns every sender the
    // attach can wait for rather than leaving one as a local here (see
    // `AttachGuard`).
    // the `ext_*` set `nvim_ui_attach` requests follows the `[native]`
    // switches, so a surface a user turned off is never taken from their
    // plugins in the first place, and `[engine] single_grid` for whether
    // nvim addresses each window's grid separately. Resolved once here and
    // handed to `NativeSession` afterwards rather than read again there --
    // two reads of one file can answer differently, and the attach would
    // then have externalized a surface the rest of the session believes it
    // declined
    let surfaces = view_native::config::ext_surfaces(&resolved);
    // the size the attach will ask for, never the terminal's own: the child
    // lays every window out against what this `--cmd` tells it, so a spawn
    // seeded a row taller than `Model::grid_target` makes the attach a
    // relayout of every window on screen
    // the ring tiles mode frames the screen with comes off the spawn's own
    // geometry, so the child lays its windows out against the grid the
    // attach will ask for
    let look = Look::new(resolved.ui.panes.value, resolved.ui.gaps.value);
    let ring = look.ring();
    // through `Look::bar_rows` rather than off the switch alone: under
    // tiles the segments sit in each frame's own bottom edge, so no row is
    // reserved for a bar and `Model::statusline_rows` answers the same
    let statusline = look.bar_rows(resolved.tables.native.enabled("statusline")) > 0;
    // the pill's own row, reserved in the spawn's geometry for the reason
    // the ring is: a child laid out a row taller than the first frame
    // leaves makes the attach a relayout of every window on screen
    let chrome = u16::from(
        resolved.tables.native.enabled("tabline") && look.panes == view_core::model::Panes::Tiles,
    );
    let spawn_size = view_core::model::grid_target_for((width, height), chrome, statusline, ring);
    // what the chrome alone would have left, so this is true for every
    // geometry the engine would have refused -- a zero floored to
    // `view_core::model::SIZE_FLOOR`, an axis clamped to
    // `view_core::model::ENGINE_MIN_SIZE` -- and for none it accepted. A
    // session clamped on either axis paints clipped against a terminal
    // smaller than its grid, and nothing else on screen says why
    let clamped_geometry = spawn_size
        != (
            width.saturating_sub(ring),
            height
                .saturating_sub(chrome)
                .saturating_sub(u16::from(statusline))
                .saturating_sub(ring),
        );
    // the log line here and the notice below are the same report at the two
    // points it can be made: this runs before the terminal is entered, where
    // `VIEW_LOG` is the only sink a session has, and the notice needs a
    // model and an executor that do not exist until the attach has landed
    if clamped_geometry {
        vlog::log_with("startup", || {
            format!(
                "terminal reported {width}x{height}; engine spawned at {}x{}",
                spawn_size.0, spawn_size.1
            )
        });
    }
    // read before the config is consumed: `update()` builds the attach and
    // has no config left to ask which descriptor the child's piped stdin is
    // on, nor whether the child was already attached on its way up
    let cfg = cfg.with_late_attach(spawn_size.0, spawn_size.1);
    let stdin_relay = cfg.stdin_relay_requested();
    let attaches_late = cfg.attaches_late();
    let mut attach = startup::attach_in_background(cfg);

    let (raw_tx, msg_rx) = mpsc::sync_channel(startup::MSG_CHANNEL_CAPACITY);
    let term_size = view_tui::terminal::TermSizeCell::default();
    #[cfg(unix)]
    let msg_tx = wake::LoopSender::with_waker(
        raw_tx,
        wake::LoopWaker::new().context("failed to create the runtime loop's wake pipe")?,
    );
    #[cfg(not(unix))]
    let msg_tx = wake::LoopSender::new(raw_tx.clone());
    // the spawn's own geometry, never the terminal's reading: the one child
    // this releases an attach for is the one `spawn_and_attach` attaches
    // itself (a relayed stdin, a swap recovery), and `nvim_ui_attach` is
    // refused outright below `view_core::model::ENGINE_MIN_SIZE` -- while
    // off that path the raw reading is a row taller than the `--cmd`
    // already told the child, which is the relayout the shared arithmetic
    // exists to prevent
    attach.release(msg_tx.clone(), spawn_size.0, spawn_size.1, surfaces.clone());

    let mut term = Term::init(resolved.ui.tier.value.map(Tier::from))
        .context("failed to initialize terminal backend")?;
    // after the terminal is in raw mode, unlike the unix path above, which
    // only builds a wake pipe: this thread reads the console itself
    #[cfg(not(unix))]
    view_tui::terminal::spawn_input_thread(raw_tx, term_size.clone());

    // the cwd is resolved once at startup, before any picker ever opens:
    // `Source::Files` with no root override searches from here
    let mut model = Model::with_term_size(width, height)
        .with_cwd(std::env::current_dir().unwrap_or_default())
        // the same look the spawn's geometry was seeded from, so the first
        // frame reserves the ring the child was already laid out inside
        .with_look(look)
        // ssh's own destination as the user typed it, which is the only
        // thing on screen that says which machine the session is on
        .with_remote(pill_host(cli.remote.as_deref()))
        .with_tabline_shows(resolved.tables.native.tabline_shows());
    // the accent the user named, ahead of the two syntax groups the theme
    // probes for when they named none
    model
        .engine
        .set_accent_token(resolved.ui.tokens.value.accent);
    // what `"auto"` answered, kept beside the resolved mode so
    // `:View ui panes` can report the marker and switch back to it
    model.detected_look = view_core::model::Detected {
        panes: Some(resolved.ui.detected_panes),
        marker: resolved.ui.panes_marker,
    };
    // what the probe's first window resolved, and what every frame is
    // painted at until the terminal says otherwise: a terminal slower than
    // that window revises this upward through `Msg::CapsUpgraded`, on
    // whatever frame its answer reaches the input path
    model.caps = term.caps();
    // opts into startup's shell frame (a themed statusline bar over an
    // empty grid) instead of Model's ordinary already-running default;
    // update() flips this back to true for good on the first grid Flush
    model.chrome_painted = false;

    // ahead of the trust and theme reads below rather than after them, for
    // the same reason the spawn is ahead of `Term::init`: until the attach
    // thread has the size, the child it started is still sitting in front
    // of its own `init.lua`. The `[native]` read between them is the one
    // that cannot wait behind the attach: it decides the surfaces the
    // attach asks for.
    // Any notice the reads below owe the user is buffered as an effect
    // rather than printed: the terminal is already raw-mode/alternate-screen
    // owned by `Term::init` above, where a bare stderr write is invisible at
    // best (see `TerminalGuard`'s doc comment) and no effect executor exists
    // yet to run a native notice through. `run_cutover`'s own toast timer
    // picks these up the same way it already does for
    // `drained.toast_effects` -- see that binding's construction below.
    let mut pre_executor_effects: Vec<Effect> = Vec::new();

    // the read itself happened before the spawn; this is the first point
    // there is a model to raise its failure on
    if let Some(err) = &config_error {
        note_unread_config(err, &mut model, &mut pre_executor_effects);
    }
    // same point, same reason, one layer up: a `VIEW_*` view could not read
    // was resolved past before the terminal existed, and this is the first
    // place there is a model to say so on
    for notice in resolved.notices() {
        pre_executor_effects.extend(model.engine.record_native_notice(notice.clone(), false));
    }
    // the on-screen half of the geometry report above, raised here for the
    // same reason: the clamp was decided before the spawn, where the
    // terminal was not yet view's to write on. Both readings are named
    // because a user with a 5-column terminal sees a 12-column grid clipped
    // against it and has nothing else on screen to explain the mismatch.
    // Worded as the startup fact the `VIEW_LOG` line above states, never as
    // a present-tense claim about the grid: the entry stays in the history
    // after the user widens the terminal, where "paints clipped" is false
    if clamped_geometry {
        pre_executor_effects.extend(model.engine.record_native_notice(
            format!(
                "view: terminal reported {width}x{height} at start, below the minimum; grid laid out at {}x{}",
                spawn_size.0, spawn_size.1
            ),
            false,
        ));
    }

    // the same set the attach is given: `Model::owns` answers about the
    // surfaces nvim is actually asked for, so the two can never differ
    model.attach_surfaces(surfaces);
    model.stdin_relay = stdin_relay;
    if !attaches_late {
        // a child that had to be attached before it could read its piped
        // stdin is attached already (`startup::spawn_and_attach`), and its
        // `VimEnter` must not send a second one
        let _ = model.takes_attach();
    }

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

    // the theme cache is keyed on the same path and on the resolved theme
    // choice, so cold start can already paint last session's colors --
    // last session under *this* colorscheme's -- before nvim answers
    // `ui_attach` with its own `default_colors_set`
    let (ai_seed_effects, ai_agent) =
        seed_ai_enabled(config_path.as_deref(), cli.clean, &mut model);
    pre_executor_effects.extend(ai_seed_effects);

    // seeded before the cache read below, which is keyed on it, and read
    // again at `VimEnter` -- the first moment the user's own config has
    // finished having its say over the colorscheme (see
    // `RpcCall::Colorscheme`). `update()` reads no config file itself, so
    // the choice has to arrive on `Model` as already-resolved state, the
    // same way `cwd` and `ai_trusted` do
    model.colorscheme = resolved.ui.theme.value.clone();

    match &config_path {
        Some(path) => {
            let (cached, notice) = theme_cache::load(path, model.colorscheme.as_deref());
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

    // themed from the cache just seeded above, and painted on this thread
    // while the engine spawned above is still coming up, so a slow-starting
    // nvim can never delay the terminal's first visible content
    startup::paint_shell_frame(&mut term, &model, process_start)
        .context("failed to paint the startup shell frame")?;

    // the probe stops owning the terminal here, with whatever it has heard
    // by now and no wait for the rest: the tier decides how a frame is
    // painted, never whether it can be, so nothing below -- the residue the
    // attach's last step blocks on, the attach, the first content -- is
    // held for a terminal that answers a network round trip late. What it
    // still owes arrives on the input path instead (`open_after_probe`).
    let mut probe = term
        .settle_probe()
        .context("failed to take the terminal capability probe off the terminal")?;
    attach.send_residue(std::mem::take(&mut probe.residue));
    model.caps = probe.caps;
    // `fence_seen` is on the line because the tier on it is not necessarily
    // final: false means the terminal still owes replies, and a later
    // `caps tier=` line supersedes this one. Without it a reader tailing
    // `VIEW_LOG` takes the first verdict for the session's.
    vlog::log_with("startup", || {
        format!(
            "version={} caps tier={:?} {} fence_seen={} source={} \
             term={width}x{height}",
            VERSION,
            model.caps.tier,
            view_tui::tiers::resolved(&model.caps),
            probe.fence_seen,
            caps_source_label(term.caps_source(), resolved.ui.tier.source)
        )
    });
    // a message, not a write: this runs with the alternate screen up, where
    // a bare stderr line is invisible until teardown scrolls it back --
    // which is where the unconditional capability line used to surface,
    // long after the session it described
    if let Some(notice) = caps_notice(&cli, &resolved.ui.tier, model.caps, term.caps_source()) {
        pre_executor_effects.extend(model.engine.record_native_notice(notice, false));
    }

    // strictly after the probe has stopped reading the terminal, and
    // strictly before the pre-attach wait: two readers of one tty cannot
    // both have the first bytes, and until this exists it is the probe that
    // is capturing what the user types (see `ProbeOutcome::residue`).
    //
    // Whether anything the terminal still owes has to be kept off
    // crossterm's parser is the callee's to decide, and it decides from the
    // outcome whole rather than from fields picked out here: a missing
    // fence, a reply cut in half by the settle, a box-glyph question the
    // probe already had answered. Whatever the terminal does answer reaches
    // the loop as `Msg::CapsUpgraded` rather than being waited for.
    #[cfg(unix)]
    let mut input_source = view_tui::input::InputSource::open_after_probe(&probe)
        .context("failed to open the pollable terminal input handle")?;
    // after the handle exists rather than beside the line above, because the
    // guard's own deadline is computed inside the constructor: a reader that
    // sees this line knows the window it names has already started
    #[cfg(unix)]
    vlog::log_with("startup", || {
        format!(
            "input guard listening={} cap_ms={}",
            input_source.still_listening(),
            view_tui::tiers::PROBE_HARD_CAP.as_millis()
        )
    });
    #[cfg(not(unix))]
    let mut input_source = ();

    let drained = startup::drain_pre_attach(
        &msg_rx,
        &msg_tx,
        &mut model,
        &mut term,
        &mut input_source,
        &term_size,
    );
    let attach_result = attach
        .engine_result()
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
            startup::AttachFailure::Spawn(err) => {
                anyhow::Error::new(err).context(spawn_failure_context(&resolved.engine))
            }
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
        let (events, folded_at) = pump.take_damage_folded();
        crate::vlog::log_redraw_census(&events, folded_at);
        events
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
    let (mut native, load_effects) = native::NativeSession::load(
        view_config,
        config_path.clone(),
        engine.api_info.channel_id,
        &mut model,
    );
    for eff in load_effects {
        let _ = executor.run(eff);
    }
    // built alongside it, for the same reason: a config that sets a
    // colorscheme has already fired the bridge's own autocmd by now
    let mut theme_bridge =
        bridge::ThemeBridge::new(config_path.as_deref(), model.colorscheme.as_deref());
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
        vlog::log("exit", "children stopped");
        term.restore_now();
        vlog::log("exit", "terminal restored");
        // after restore_now, not before: persist_theme's own diagnostic (on
        // a cache-write failure) is a plain stderr write, and the terminal
        // is raw-mode/alternate-screen owned until the line above -- see
        // report_fatal_reason's doc comment for the same ordering
        // requirement on the read side.
        persist_theme(&model, &config_path);
        report_fatal_reason(&model);
        vlog::log_with("exit", || format!("leaving code={code}"));
        std::process::exit(code);
    }

    // built fresh per restart rather than stored once: `EngineConfig` is
    // consumed by the spawn it describes
    let respawn = || respawn_config(&cli, &resolved.engine);
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
        ai_agent,
    )?;
    vlog::log_with("engine", || format!("exit code={exit_code}"));
    // every child `run` owned -- the engine and the agent session -- was
    // signalled by its frame's teardown before this line; a log that goes
    // silent between the engine's exit and the process's own leaves a user
    // handing in VIEW_LOG after a stranded terminal unable to say whether
    // the teardown ran or the process was killed one instruction later
    vlog::log("exit", "children stopped");
    // std::process::exit bypasses destructors, so the terminal must be
    // restored explicitly first; every other return path (an error
    // propagated via `?` above) is covered by `Drop` on `term`. Also why
    // persist_theme runs after this line and not before: its own
    // diagnostic (on a cache-write failure) is a plain stderr write, valid
    // only once the terminal is no longer raw-mode/alternate-screen owned.
    term.restore_now();
    vlog::log("exit", "terminal restored");
    persist_theme(&model, &config_path);
    report_fatal_reason(&model);
    vlog::log_with("exit", || format!("leaving code={exit_code}"));
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
        if let Some(notice) = theme_cache::store(
            Theme::from_hl(model.engine.hl()),
            path,
            model.colorscheme.as_deref(),
        ) {
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

    /// Every key a command line resolves, against an empty environment and
    /// no config file: what the assertions below are about is the flag
    /// layer, and a host that happens to export a `VIEW_*` name must not
    /// be able to answer for it.
    fn resolved_for(cli: &Cli) -> ResolvedConfig {
        view_native::config::resolve_with(&ViewConfig::defaults(), &Overrides::from(cli), &|_| None)
    }

    /// [`super::engine_config`] for a command line alone, resolved the
    /// hermetic way [`resolved_for`] describes. Shadows the production
    /// name inside this module so every spawn assertion below runs through
    /// the same chain a session does, without each one restating it.
    fn engine_config(cli: &Cli) -> EngineConfig {
        super::engine_config(cli, &resolved_for(cli).engine)
    }

    /// [`super::respawn_config`] on the same terms as [`engine_config`].
    fn respawn_config(cli: &Cli) -> EngineConfig {
        super::respawn_config(cli, &resolved_for(cli).engine)
    }

    /// A spawn that failed names the editor it tried and the layer that
    /// chose it, at every layer that can choose one. Walked rather than
    /// sampled on the file arm alone: the whole point of the provenance is
    /// that a user reading the failure can tell which of their four places
    /// to go fix, and a message that named only the path would send someone
    /// editing `view.toml` to a flag they never typed.
    #[test]
    fn a_failed_spawn_names_the_editor_and_the_layer_that_named_it() {
        let file = ViewConfig::from_toml_str("[engine]\nnvim_bin = \"/nope/from-file\"\n")
            .expect("the fixture must parse");
        let env =
            |name: &str| (name == "VIEW_ENGINE_NVIM_BIN").then(|| "/nope/from-env".to_string());
        let flag = Overrides {
            nvim_bin: Some(std::path::PathBuf::from("/nope/from-flag")),
            ..Overrides::default()
        };
        for (flags, env, path, layer) in [
            (
                flag,
                &env as &dyn Fn(&str) -> Option<String>,
                "/nope/from-flag",
                Source::Flag,
            ),
            (Overrides::default(), &env, "/nope/from-env", Source::Env),
            (
                Overrides::default(),
                &(|_: &str| None),
                "/nope/from-file",
                Source::File,
            ),
        ] {
            let resolved = view_native::config::resolve_with(&file, &flags, env);
            let context = spawn_failure_context(&resolved.engine);
            assert!(
                context.contains(path) && context.contains(layer.label()),
                "a spawn failure from the {} names neither {path} nor its layer: {context}",
                layer.label()
            );
        }
    }

    /// The derived arm of the same message: nothing named an editor, so
    /// there is no path to print and the message says where one could be
    /// named instead.
    #[test]
    fn a_failed_spawn_with_nothing_named_points_at_every_place_one_could_be() {
        let resolved = resolved_for(&Cli::parse_from(["view"]));
        let context = spawn_failure_context(&resolved.engine);
        for place in ["--nvim-bin", "VIEW_ENGINE_NVIM_BIN", "nvim_bin"] {
            assert!(
                context.contains(place),
                "{place} is not offered by the derived failure: {context}"
            );
        }
    }

    /// Every flag the key registry claims is a flag this binary actually
    /// parses. Walked rather than sampled: a registry row naming a flag
    /// nobody can type is a promise the CLI never made.
    #[test]
    fn every_registry_flag_is_one_this_binary_accepts() {
        let command = Cli::command();
        let accepted: Vec<String> = command
            .get_arguments()
            .filter_map(clap::Arg::get_long)
            .map(|long| format!("--{long}"))
            .collect();
        for flag in view_native::config::keys()
            .iter()
            .filter_map(|key| key.flag)
        {
            assert!(
                accepted.iter().any(|long| long == flag),
                "the registry claims {flag}, which this binary does not accept: {accepted:?}"
            );
        }
    }

    /// The `after_help` sample is a hand-written list, and this is what
    /// keeps it from becoming a stale one: a flag the registry carries has
    /// to appear in the sentence that tells a user where view's flags may
    /// go, or the ordering rule reads as not applying to it. The trailing
    /// `...` stays, because the sentence also covers flags that name no
    /// config key at all (`--clean`, `--config`, `--remote`).
    #[test]
    fn the_ordering_note_names_every_flag_the_registry_carries() {
        let command = Cli::command();
        let after = command
            .get_after_help()
            .map(ToString::to_string)
            .unwrap_or_default();
        for flag in view_native::config::keys()
            .iter()
            .filter_map(|key| key.flag)
        {
            assert!(
                after.contains(flag),
                "{flag} is missing from the flag-ordering note: {after}"
            );
        }
        assert!(
            after.contains("..."),
            "the note lists a sample and must say so: {after}"
        );
    }

    /// The other direction of the same claim: a flag typed on the command
    /// line reaches the key it names, at the top of the chain.
    #[test]
    fn every_flag_the_registry_names_reaches_its_own_key() {
        let cli = Cli::parse_from([
            "view",
            "--tier",
            "basic",
            "--theme",
            "gruvbox",
            "--nvim-bin",
            "/opt/nvim/bin/nvim",
            "--appname",
            "work",
            "--single-grid",
            // nvim rather than tiles for the reason `resolve.rs`'s own flag
            // fixture states: the two flags contradict each other any other
            // way round, and view resolves that by ignoring `--panes`
            "--panes",
            "nvim",
        ]);
        let resolved = resolved_for(&cli);
        for (key, _, source) in resolved.rows() {
            let expected = if key.flag.is_some() {
                Source::Flag
            } else {
                Source::Derived
            };
            assert_eq!(
                source, expected,
                "[{}] {} answered from the wrong layer",
                key.table, key.key
            );
        }
        assert_eq!(resolved.ui.theme.value.as_deref(), Some("gruvbox"));
        assert!(resolved.engine.single_grid.value);
        assert_eq!(resolved.ui.panes.value, Panes::Nvim);
    }

    /// `--clean` is the triage tool, and its question -- view, or this
    /// user's configuration -- has one variable only while every other
    /// layer is suppressed.
    #[test]
    fn clean_answers_every_key_from_its_derived_default() {
        let file = ViewConfig::from_toml_str(
            "[native]\npicker = false\n\n[supervision]\nauto_restart = false\n",
        )
        .expect("the fixture must parse");
        let resolved = resolve_session_config(&Cli::parse_from(["view", "--clean"]), &file);
        for (key, _, source) in resolved.rows() {
            assert_eq!(
                source,
                Source::Derived,
                "[{}] {} survived --clean",
                key.table,
                key.key
            );
        }
        assert!(resolved.tables.native.enabled("picker"));
        assert!(resolved.tables.supervision.auto_restart);
    }

    /// The set a session with nothing to narrow it attaches under `panes`.
    ///
    /// The tab line is the one `[native]` switch whose default follows the
    /// look, so tiles attach every surface and nvim mode attaches the
    /// registry's own set.
    fn shipped_under(panes: view_core::model::Panes) -> Vec<view_core::native::ext::Ext> {
        if panes == view_core::model::Panes::Tiles {
            view_core::native::ext::ALL_MULTIGRID.to_vec()
        } else {
            view_core::native::ext::shipped_multigrid()
        }
    }

    /// `--clean` skips the file, but it must still reproduce the mode view
    /// ships rather than a second one of its own: a config-blind attach
    /// that fell back to single-grid would make the triage tool lie about
    /// which attach mode misbehaved.
    #[test]
    fn clean_attaches_the_default_set() {
        let file = ViewConfig::from_toml_str("[engine]\nsingle_grid = true\n")
            .expect("the fixture must parse");
        let resolved = resolve_session_config(&Cli::parse_from(["view", "--clean"]), &file);
        assert_eq!(
            view_native::config::ext_surfaces(&resolved),
            shipped_under(resolved.ui.panes.value),
            "--clean must attach the shipped set even when the file it ignores asked for the fallback"
        );
    }

    /// `fn main`'s own body, and nothing else in the file: the sequence
    /// these pins describe is a sequence only inside one function, so a
    /// step hoisted into a helper -- above `main` or below it -- must read
    /// as "this pin can no longer see it" rather than as an ordering that
    /// happens to still compare. Ends at the first brace in column zero,
    /// which is where every top-level item closes.
    fn startup_body() -> String {
        let source = include_str!("main.rs");
        let (_, body) = source
            .split_once("\nfn main() -> Result<()> {")
            .expect("main.rs still defines fn main");
        code_only(body.split_once("\n}\n").map_or(body, |(body, _)| body))
    }

    /// `source` with every comment gone -- `/* … */` spans, nesting
    /// included the way rustc reads them, and the `//` tail of every line,
    /// not only lines that open with one -- so a marker can resolve to
    /// code and nothing else. Every step the pins
    /// below name is named in the prose around its own call site too, and
    /// a pin measuring the mention rather than the call keeps passing with
    /// the call moved out of the window.
    ///
    /// Quote-blind, deliberately: `fn main` holds no string containing a
    /// comment introducer (only comments containing quotes, which go
    /// whole), and the failure mode of stripping too much is loud rather
    /// than silent. Deleting text never reorders what is left, so an
    /// over-strip can only make a marker missing -- which `offset_of`
    /// reports -- never make one compare in the wrong order.
    fn code_only(source: &str) -> String {
        let mut spanless = String::with_capacity(source.len());
        let mut rest = source;
        // depth-counted, because rust's block comments nest: taking the
        // first `*/` as the end of the first `/*` hands back the tail of an
        // outer comment as if it were code
        let mut depth = 0usize;
        loop {
            let open = rest.find("/*");
            let close = rest.find("*/");
            match (open, close) {
                (Some(open), close) if close.is_none_or(|close| open < close) => {
                    if depth == 0 {
                        spanless.push_str(&rest[..open]);
                    }
                    depth += 1;
                    rest = &rest[open + 2..];
                }
                (_, Some(close)) => {
                    // at depth 0 this closes nothing -- the source is
                    // unbalanced, and dropping the text around it would
                    // hide code rather than prose
                    if depth == 0 {
                        spanless.push_str(&rest[..close + 2]);
                    }
                    depth = depth.saturating_sub(1);
                    rest = &rest[close + 2..];
                }
                // no delimiter left: an arm guard does not count towards
                // exhaustivity, so this also spells `(Some(_), None)`,
                // which the opener arm above has already taken
                _ => break,
            }
        }
        if depth == 0 {
            spanless.push_str(rest);
        }
        spanless
            .lines()
            .map(|line| line.split_once("//").map_or(line, |(code, _)| code))
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// The pins locate a step by its text, and prose quotes those steps
    /// constantly; this is what keeps the two apart.
    #[test]
    fn a_step_named_only_in_a_comment_is_not_a_step_the_pins_can_find() {
        let body = "    let probe = term\n        .other_call(); // .settle_probe() in prose\n\
                    /* .engine_result() spanning\n   two lines */\n    let x = 1;\n\
                    /* outer /* inner */ .drain_pre_attach() still inside */\n    let y = 2;\n";
        let code = code_only(body);

        for buried in [".settle_probe()", ".engine_result()", ".drain_pre_attach()"] {
            assert!(
                !code.contains(buried),
                "`{buried}` survived in a comment, so a pin naming it can \
                 measure prose instead of the call it guards"
            );
        }
        assert!(
            code.contains(".other_call();")
                && code.contains("let x = 1;")
                && code.contains("let y = 2;"),
            "the code around the comments has to survive, or every pin \
             reads as \"this step is gone\""
        );
    }

    fn offset_of(marker: &str) -> usize {
        startup_body()
            .find(marker)
            .expect("fn main no longer performs this step itself")
    }

    /// `nvim --embed` sources nothing and opens nothing until a UI
    /// attaches, so everything `main` performs before the attach thread is
    /// started is prepended whole to the engine's own startup and lands in
    /// `first_paint`'s marker -- the terminal capability handshake above
    /// all, whose first window runs to `tiers::PROBE_DEADLINE` and which a
    /// terminal answering across an ssh hop spends in full.
    ///
    /// Two boundaries rather than a list of reads: `Term::init` opens the
    /// terminal half of startup and `pre_executor_effects` opens the half
    /// where this process reads its own state (trust store, theme cache,
    /// and whatever is added next to them), so a read added to either half
    /// is behind the spawn by construction instead of by a reviewer
    /// noticing. Nothing but a source-order check can hold this: every one
    /// of those reads still has to happen before the shell frame is
    /// rendered, so hoisting one above the spawn breaks no other test.
    ///
    /// The config prologue is the one read that deliberately sits *ahead*
    /// of the spawn, because the editor the child runs is one of the keys
    /// the chain answers -- there is no spawn to order until it has run.
    /// That exception has its own pin
    /// ([`only_the_config_prologue_runs_before_the_engine_spawn`]) holding
    /// it to the three calls it is, so it cannot quietly become the place
    /// new startup reads accumulate.
    ///
    /// Measured on dev-linux over two alternating pairs of cold spawns,
    /// uninstrumented builds from one source path and one target dir: the
    /// `first_paint.minimal` marker came in a little earlier with the spawn
    /// ahead of the terminal handshake than behind it, for a shell frame
    /// that did not move. A terminal that never answers the probe at all
    /// gains nothing and loses nothing: its content was held by the probe's
    /// own second window, not by anything nvim is doing -- a wait
    /// `settle_probe` has since removed, which is what leaves this ordering
    /// measurable at all on such a terminal.
    ///
    /// That reading measures the boundary this test pins -- the spawn
    /// against the terminal handshake -- and was taken before the config
    /// prologue moved ahead of the spawn. The prologue's own cost (one file
    /// read of a few hundred bytes and one environment sweep) is unmeasured
    /// since that move, so nothing here states a current end-to-end
    /// `first_paint` figure.
    #[test]
    fn the_engine_spawn_precedes_both_halves_of_the_startup_only_this_process_needs() {
        let spawn = offset_of("startup::attach_in_background(");
        for (half, cost) in [
            ("Term::init(", "the terminal capability handshake"),
            (
                "let mut pre_executor_effects",
                "this process reading its own trust, config and theme state",
            ),
        ] {
            assert!(
                spawn < offset_of(half),
                "`{half}` runs before the engine spawn, so nvim's whole \
                 startup waits on {cost}"
            );
        }
    }

    /// Every call `fn main` performs before the engine spawn, in source
    /// order, paths and method calls alike.
    ///
    /// Deliberately unfiltered past the two things that are not calls at
    /// all -- a `#[cfg(...)]` attribute and the keywords that take a
    /// parenthesis -- because any filter is a hole: a read hoisted above
    /// the spawn is exactly the thing that would have been "obviously not
    /// worth listing".
    fn calls_before_the_spawn() -> Vec<String> {
        let body = startup_body();
        let spawn = body
            .find("startup::attach_in_background(")
            .expect("fn main no longer spawns the engine itself");
        let code = code_only(&body[..spawn]);
        let bytes = code.as_bytes();
        code.match_indices('(')
            .filter_map(|(at, _)| {
                let start = bytes[..at]
                    .iter()
                    .rposition(|b| !(b.is_ascii_alphanumeric() || *b == b'_' || *b == b':'))
                    .map_or(0, |before| before + 1);
                let name = &code[start..at];
                let attribute = start >= 2 && &code[start - 2..start] == "#[";
                let keyword = matches!(name, "let" | "if" | "while" | "for" | "match" | "return");
                (!name.is_empty() && !attribute && !keyword).then(|| name.to_string())
            })
            .collect()
    }

    /// The one startup read that runs ahead of the engine spawn is the
    /// config chain, and it is the three calls that chain takes.
    ///
    /// The exception is load-bearing -- `--nvim-bin` and `[engine]
    /// nvim_bin` decide *which* editor the spawn spawns, so there is no
    /// spawn to defer until the chain has answered -- and an exception with
    /// no bound on it is where every later startup read ends up. Everything
    /// else here is argument handling that reads nothing and the launch
    /// shapes that never reach a spawn at all (`--print-clipboard`, a
    /// refused remote, a relayed stdin), so a new entry in this list is
    /// either one of those or a read that owes the engine's startup its
    /// latency and must move below the spawn instead.
    ///
    /// The terminal size is the second such exception, and the same shape as
    /// the first: the child sources the user's config against the geometry
    /// the spawn hands it, so a size read after the spawn would be a size the
    /// config never saw. It is one `ioctl` on a descriptor this process
    /// already holds.
    ///
    /// The `ext_*` set is the third: the attach that carries it is built
    /// from the model rather than from the config, so the set has to be
    /// folded in before the loop exists. It reads nothing further -- the
    /// config chain above has already been resolved, and this walks the
    /// `[native]` table it produced.
    ///
    /// The grid the spawn is seeded with is the fourth, and rides the size
    /// read above: the rows view's own chrome takes are not the child's to
    /// lay windows out in, so the spawn is handed the grid the attach will
    /// ask for rather than the terminal's own. The ring tiles mode frames
    /// the screen with is part of that grid, as is the row the pill takes,
    /// and both come out of the `[ui]` and `[native]` answers the chain
    /// above already resolved. Every call is arithmetic over values in
    /// hand.
    ///
    /// The geometry notice is the fifth, and it is not a read at all: it
    /// reports the reading the line above stood a geometry in for, on every
    /// branch where the two differ -- a zero answered with
    /// `view_core::model::SIZE_FLOOR`, an axis held up to
    /// `view_core::model::ENGINE_MIN_SIZE` -- and there is nothing left to
    /// name either pair by once the spawn has consumed the geometry. Its
    /// own comparison is the arithmetic the chrome takes, which the line
    /// above has already performed on values in hand. Only the log line
    /// runs here: the notice the user reads is raised with the other
    /// startup notices, below the spawn, where a model and an executor
    /// exist to carry it.
    ///
    /// The sixth reads nothing and forks nothing: it hands view-proc this
    /// process's log, so that an arm of the tie the host refuses says so
    /// instead of leaving a child running untied and indistinguishable
    /// from a tied one. It stands above the line below it because
    /// preparing the tie is itself refusable, and view-proc holds such a
    /// refusal only as far as its own small buffer reaches.
    ///
    /// The tie is the seventh, and it is here to keep a cost *off* the
    /// spawn rather than to take one: off Linux a tied child is tied by a
    /// watcher process, and the first tied spawn is the one that forks it
    /// -- which is the engine spawn below. The fork is made by a thread
    /// while the config chain runs, and the call has to be here rather
    /// than lower
    /// because the pipe that watcher reads is two syscalls where there is
    /// no `pipe2`: a fork on another thread landing between them inherits
    /// the write end and holds it open for the life of the session. This is
    /// the line before this process has a second thread of any kind.
    ///
    /// Both have to precede the first tied spawn, and the first tied spawn
    /// is the engine's.
    #[test]
    fn only_the_config_prologue_runs_before_the_engine_spawn() {
        assert_eq!(
            calls_before_the_spawn(),
            vec![
                "Instant::now",
                "vlog::init",
                "view_proc::record_refusals_with",
                "view_proc::prepare_to_tie_children",
                "Cli::parse",
                "Some",
                "print_clipboard",
                "deny_incoherent_remote",
                "deny_unsupported_stdin_relay",
                "resolve_config_path",
                "load_view_config",
                "as_deref",
                "resolve_session_config",
                "clone",
                "engine_config",
                "remote",
                "cloned",
                "Some",
                "remote_guard::deny_absent_ssh",
                "maybe_relay_stdin",
                "view_tui::input::adopt_terminal_stdin",
                "vlog::log",
                "route_stderr_off_the_terminal",
                "view_tui::terminal::size_now",
                "context",
                "view_native::config::ext_surfaces",
                "Look::new",
                "ring",
                "bar_rows",
                "enabled",
                "u16::from",
                "enabled",
                "view_core::model::grid_target_for",
                "saturating_sub",
                "saturating_sub",
                "saturating_sub",
                "u16::from",
                "saturating_sub",
                "vlog::log_with",
                "with_late_attach",
                "stdin_relay_requested",
                "attaches_late",
            ],
            "a call added before the engine spawn prepends its own latency \
             to nvim's whole startup: move it below the spawn, or state \
             here why it cannot run there"
        );
    }

    /// The attach carries the geometry the spawn's own `--cmd` gave the
    /// child, never the terminal's raw reading: the one child released
    /// here that `startup::spawn_and_attach` attaches itself (a relayed
    /// stdin, a swap recovery) is attached at exactly this pair, and the
    /// engine refuses `nvim_ui_attach` below
    /// `view_core::model::ENGINE_MIN_SIZE` outright -- so a terminal still
    /// negotiating its size would fail that attach where an ordinary start
    /// floors. Off the zero path the raw reading is a row taller than the
    /// `--cmd` whenever the statusline is on, which is the relayout the
    /// shared arithmetic exists to prevent. Both pairs are in scope at the
    /// call, and the release itself reports nothing, so nothing but the
    /// source says which one was spent.
    #[test]
    fn the_attach_is_released_with_the_size_the_spawn_was_seeded_with() {
        let body = startup_body();
        let (_, call) = body
            .split_once("attach.release(")
            .expect("fn main no longer releases the attach itself");
        let call = call.split_once(';').map_or(call, |(call, _)| call);
        assert!(
            call.contains("spawn_size.0") && call.contains("spawn_size.1"),
            "the attach is released with `{call}`, not with the geometry the \
             spawn's `--cmd` gave the child"
        );
    }

    /// The window the spawn opened: between it and `engine_result` this
    /// process holds a live `nvim --embed` and no other handle on it, so
    /// every fallible step in between exits leaving a stray editor behind
    /// unless `AttachGuard` is still alive to kill it. Each of these is a
    /// `?`; the guard is what makes a new one safe by construction, and
    /// this is what catches the reordering that would put one outside it.
    ///
    /// The terminal size is read ahead of the window rather than inside it,
    /// and owes it nothing: the spawn it feeds has not happened yet, so a
    /// failure there has no child to leave behind.
    #[test]
    fn every_fallible_startup_step_runs_inside_the_attach_guards_window() {
        let spawn = offset_of("startup::attach_in_background(");
        let result = offset_of(".engine_result()");
        assert!(
            offset_of("view_tui::terminal::size_now()") < spawn,
            "the geometry the spawn is armed with must be read before it"
        );
        for step in [
            "Term::init(",
            "startup::paint_shell_frame(",
            ".settle_probe()",
            "InputSource::open",
            "startup::drain_pre_attach(",
        ] {
            let at = offset_of(step);
            assert!(
                spawn < at && at < result,
                "`{step}` runs outside the attach guard's window, so failing \
                 there leaves the spawned nvim running with nothing left to \
                 kill it"
            );
        }
    }

    /// fd 2 answers for the log or the null device from the redirect
    /// onwards, so everything that asks whether a *terminal* is on it has
    /// to have asked already -- `adopt_terminal_stdin`'s third fallback
    /// reads exactly that -- and the one notice a session with no terminal
    /// anywhere can still be read on has to be printed before the sink
    /// exists, not into it. The far side is the screen: a redirect landing
    /// after `Term::init` leaves the capability probe's own window open for
    /// a library to write across.
    #[test]
    fn stderr_leaves_the_terminal_after_every_probe_of_it_and_before_the_screen() {
        let redirect = offset_of("route_stderr_off_the_terminal()");
        for (earlier, why) in [
            (
                "view_tui::input::adopt_terminal_stdin()",
                "the stdin fallback asks whether a terminal is on fd 2, and \
                 would adopt the sink",
            ),
            (
                "eprintln!(\"view: {NO_TERMINAL_NOTICE}\")",
                "the notice a terminal-less session is told goes into the \
                 sink nobody has been told about",
            ),
        ] {
            assert!(
                offset_of(earlier) < redirect,
                "`{earlier}` runs after the stderr redirect, so {why}"
            );
        }
        assert!(
            redirect < offset_of("Term::init("),
            "the stderr redirect runs after the terminal is entered, so a \
             library writing to fd 2 during capability detection still \
             paints over the screen"
        );
    }

    /// The one thing the guard cannot own, pinned because nothing else in
    /// the file says the order matters.
    ///
    /// The attach thread's last act is a blocking `send` of
    /// `Msg::EngineReady` on the loop's bounded channel, so an armed
    /// `Drop`'s join returns only once that send completes or fails. What
    /// makes it fail rather than park, when nothing is draining the channel
    /// yet, is the receiver being gone -- and `msg_rx` is a local declared
    /// after the guard, so it drops first. A guard can hold every sender
    /// the thread waits on (that is what closed the residue deadlock), but
    /// it cannot hold the receiver the runtime loop reads from, so this one
    /// stays ownership by declaration order and gets a pin instead.
    #[test]
    fn the_loops_channel_is_declared_after_the_attach_so_its_receiver_drops_first() {
        assert!(
            offset_of("startup::attach_in_background(") < offset_of("let (raw_tx, msg_rx)"),
            "the loop channel is declared before the attach guard, so its \
             receiver outlives the guard: an abandoned startup joins a \
             thread parked in the `EngineReady` send, holding the child"
        );
    }

    /// `[ui] theme` reaches the session through exactly one assignment, and
    /// dropping it fails nothing: the cache read and the store below it
    /// would agree on `None`, the `VimEnter` arm would send no
    /// `RpcCall::Colorscheme`, and every test in the tree would still pass
    /// while the key silently did nothing. So the assignment is pinned, and
    /// so is its position -- the cache slot is keyed on the resolved choice,
    /// so a seed after the read would look the answer up under the wrong
    /// scheme's hash on every cold start.
    #[test]
    fn the_resolved_theme_is_seeded_onto_the_model_before_the_cache_is_read() {
        let seed = offset_of("model.colorscheme = resolved.ui.theme.value");
        assert!(
            seed < offset_of("theme_cache::load("),
            "the theme cache is read before the choice it is keyed on is \
             seeded, so a named scheme reads back the unnamed slot"
        );
    }

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

    /// A build with no release layout beside its executable -- this test
    /// binary, and every development build -- spawns the `nvim` its user's
    /// own `PATH` names, with nothing exported at it. The bundled branch is
    /// pinned by `BundledEngine::resolve_from`'s own tests, which can plant
    /// a layout; what this holds is the fallback, which no released binary
    /// exercises and which would otherwise be provable only by absence.
    #[test]
    fn a_build_with_no_layout_beside_it_spawns_the_path_engine() {
        let cfg = engine_config(&Cli::parse_from(["view"]));
        assert_eq!(cfg.nvim_bin, std::path::PathBuf::from("nvim"));
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
    fn print_caps_flag_emits_exactly_one_notice() {
        let mut model = Model::new();
        let cli = Cli::parse_from(["view", "--print-caps"]);
        let notice = caps_notice(
            &cli,
            &resolved_for(&cli).ui.tier,
            model.caps,
            CapsSource::Probed,
        )
        .expect("--print-caps asks for the capability line");
        assert!(
            notice.contains("tier=") && notice.contains("(probed)"),
            "the notice must carry the tier and where it came from, got {notice:?}"
        );
        assert!(
            !notice.contains('\n'),
            "the notice is one message line, got {notice:?}"
        );
        model.engine.record_native_notice(notice, false);
        assert_eq!(
            model.engine.messages.entries.len(),
            1,
            "a session that asked for the capability line gets it once, got {:?}",
            model.engine.messages.entries
        );

        // the second way to ask: `--tier` changes what this line would have
        // said, so the session that overrides is shown what it got
        let cli = Cli::parse_from(["view", "--tier", "basic"]);
        let overridden = caps_notice(
            &cli,
            &resolved_for(&cli).ui.tier,
            model.caps,
            CapsSource::Override,
        )
        .expect("--tier implies the capability line");
        assert!(
            overridden.contains("(--tier flag)"),
            "an overridden session is told which layer overrode it, got {overridden:?}"
        );
    }

    /// A tier override names the layer that set it, not the fact that one
    /// was set: `--tier`, `VIEW_UI_TIER` and a config file are three
    /// different things to go and change, and "tier override" is the same
    /// word for all three.
    #[test]
    fn an_overridden_tier_names_the_layer_that_set_it() {
        let model = Model::new();
        let cli = Cli::parse_from(["view", "notes.md"]);
        let from_env = view_native::config::resolve_with(
            &ViewConfig::defaults(),
            &Overrides::from(&cli),
            &|name| (name == "VIEW_UI_TIER").then(|| "basic".to_string()),
        );
        let notice = caps_notice(
            &cli,
            &from_env.ui.tier,
            model.caps,
            view_tui::tiers::CapsSource::Override,
        )
        .expect("a resolved tier implies the capability line");
        assert!(
            notice.contains("(VIEW_UI_TIER)"),
            "an environment override must name the variable, got {notice:?}"
        );

        // and a probed session says what it always said: the layer only
        // stands in for the word `override`, never for `probed`
        let probed = caps_notice(
            &Cli::parse_from(["view", "--print-caps"]),
            &from_env.ui.tier,
            model.caps,
            view_tui::tiers::CapsSource::Probed,
        )
        .expect("--print-caps asks for the capability line");
        assert!(
            probed.contains("(probed)"),
            "capabilities that were probed are still probed, got {probed:?}"
        );
    }

    #[test]
    fn print_caps_is_silent_without_the_flag() {
        let model = Model::new();
        let cli = Cli::parse_from(["view", "notes.md"]);
        let notice = caps_notice(
            &cli,
            &resolved_for(&cli).ui.tier,
            model.caps,
            CapsSource::Probed,
        );
        assert!(
            notice.is_none(),
            "an ordinary session says nothing about its own capabilities, got {notice:?}"
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

    /// The fail-open leg the attach now depends on. A `view.toml` that
    /// cannot be parsed must cost the user a notice, never a surface: the
    /// set handed to `nvim_ui_attach` moments later is derived from what
    /// this returns, so a fail-closed answer here would answer a typo by
    /// taking away the palette and the message overlay for the session.
    #[test]
    fn an_unreadable_config_attaches_every_ext() {
        let dir = view_test_support::ScratchDir::new("main-config-fail-open").unwrap();
        let path = dir.join("view.toml");
        std::fs::write(&path, "[native]\nthis is not toml\n").unwrap();

        let mut model = Model::with_term_size(80, 24);
        let mut notices = Vec::new();
        let (file, err) = load_view_config(Some(&path));
        // through the same empty-environment chain `resolved_for` states,
        // so a host exporting a `VIEW_*` name cannot answer for the layer
        // this leg is about
        let resolved = view_native::config::resolve_with(&file, &Overrides::default(), &|_| None);
        note_unread_config(
            &err.expect("a file that is not TOML must be reported"),
            &mut model,
            &mut notices,
        );

        assert_eq!(
            view_native::config::ext_surfaces(&resolved),
            shipped_under(resolved.ui.panes.value),
            "a config that could not be read keeps every surface, and attaches \
             the mode view ships"
        );
        assert!(
            !notices.is_empty(),
            "the user must be told the file was ignored"
        );
        let shown = format!("{:?}", model.engine.messages.entries);
        assert!(
            shown.contains("every native feature stays at its default this session"),
            "and told what that cost them: {shown}"
        );
        assert!(
            !model.config_was_read(),
            "the session has to know these surfaces are the fail-open default \
             rather than this user's answer: a notice that later names a \
             `[native]` remedy would be telling them to write a line they may \
             have written into the very file view could not read"
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

    /// The pill's left edge names the machine, so the destination the model
    /// carries is ssh's own, with the path and nothing else split off.
    #[test]
    fn the_remote_destination_reaches_the_model_at_startup() {
        let model = view_core::model::Model::with_term_size(80, 24)
            .with_remote(pill_host(Some("deploy@prod-box:/etc/app.conf")));
        assert_eq!(model.remote.as_deref(), Some("deploy@prod-box"));
        assert_eq!(
            view_core::native::pill::PillView::from_model(&model).host,
            "deploy@prod-box"
        );
        assert!(
            startup_body().contains(".with_remote(pill_host(cli.remote.as_deref()))"),
            "fn main builds its model without the destination, so the pill \
             would name no machine however well the helper answers"
        );
        let local = view_core::model::Model::with_term_size(80, 24).with_remote(pill_host(None));
        assert_eq!(local.remote, None);
        assert!(
            view_core::native::pill::PillView::from_model(&local)
                .host
                .is_empty(),
            "a local session spends no columns saying so"
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

    #[test]
    fn a_broken_ai_config_seeds_disabled_and_names_the_file() {
        let dir = view_test_support::ScratchDir::new("seed-ai-enabled-broken").unwrap();
        let path = dir.join("view.toml");
        std::fs::write(&path, "[ai]\nenabled = \"not a bool\"\n").unwrap();
        let mut model = Model::with_term_size(80, 24);
        let (effects, _agent) = seed_ai_enabled(Some(&path), false, &mut model);
        assert!(
            !model.ai_enabled,
            "an unreadable/invalid [ai] table must fail closed, not silently widen the surface"
        );
        assert!(
            !effects.is_empty(),
            "a broken config owes the user a notice effect"
        );
        let shown: String = model
            .engine
            .messages
            .entries
            .iter()
            .flat_map(|e| e.content.iter().map(|(_, t)| t.as_str()))
            .collect();
        assert!(
            shown.contains(&path.display().to_string()),
            "the toast must name the file that failed to parse: {shown}"
        );
    }

    #[test]
    fn a_readable_ai_config_seeds_the_configured_value() {
        let dir = view_test_support::ScratchDir::new("seed-ai-enabled-ok").unwrap();
        let path = dir.join("view.toml");
        std::fs::write(&path, "[ai]\nenabled = false\n").unwrap();
        let mut model = Model::with_term_size(80, 24);
        let (effects, _agent) = seed_ai_enabled(Some(&path), false, &mut model);
        assert!(
            !model.ai_enabled,
            "a valid [ai] enabled = false must seed the configured value, not the default"
        );
        assert!(
            effects.is_empty(),
            "a valid config owes no notice: {effects:?}"
        );

        let mut absent = Model::with_term_size(80, 24);
        let (effects, agent) = seed_ai_enabled(None, false, &mut absent);
        assert!(
            absent.ai_enabled,
            "no config path at all is the documented default-on case"
        );
        assert!(effects.is_empty(), "{effects:?}");
        assert_eq!(
            agent,
            view_ai::AgentSpec::Id("claude-code".to_string()),
            "the absent-config default must resolve the same agent AiConfig::default names"
        );
    }

    /// The agent `ai_worker::AiWorker` is later built from is the same one
    /// `[ai]` named, not a second, independent read of the file: a
    /// `command = [...]` line resolves to `AgentSpec::Command`, carrying the
    /// exact argv this build will spawn.
    #[test]
    fn a_command_agent_spec_resolves_to_the_configured_argv() {
        let dir = view_test_support::ScratchDir::new("seed-ai-enabled-command").unwrap();
        let path = dir.join("view.toml");
        std::fs::write(&path, "[ai]\nagent = [\"my-agent\", \"--flag\"]\n").unwrap();
        let mut model = Model::with_term_size(80, 24);
        let (effects, agent) = seed_ai_enabled(Some(&path), false, &mut model);
        assert!(effects.is_empty(), "{effects:?}");
        assert!(model.ai_enabled);
        assert_eq!(
            agent,
            view_ai::AgentSpec::Command(vec!["my-agent".to_string(), "--flag".to_string()])
        );
    }
}
