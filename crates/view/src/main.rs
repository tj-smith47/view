//! `view [FILE] --nvim-bin <path>`: CLI parsing and wiring for the terminal
//! frontend over an embedded Neovim engine. The startup sequence (shell
//! paint, background attach, pre-attach key buffering) lives in
//! [`startup`]; the steady-state loop itself lives in [`runtime`].

mod bridge;
mod clipboard;
mod native;
mod runtime;
mod startup;
mod theme_cache;
mod vlog;

use anyhow::{Context, Result};
use clap::Parser;
use std::sync::mpsc;
use std::time::Instant;
use view_core::model::{Model, Tier};
use view_core::theme::Theme;
use view_engine::process::EngineConfig;
use view_tui::terminal::Term;

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
#[command(name = "view", about = "A modern terminal editor powered by Neovim")]
struct Cli {
    /// Path to the nvim binary (defaults to PATH lookup)
    #[arg(long)]
    nvim_bin: Option<std::path::PathBuf>,
    /// Override auto-detected terminal capabilities instead of probing
    #[arg(long)]
    tier: Option<TierArg>,
    /// Spawns the bundled engine with no user config at all: `view.toml`
    /// and `init.lua` are both skipped, and every native feature stays on.
    /// This is view's own triage tool, distinct from `nvim --clean` --
    /// see `engine_config`'s doc comment for why the two must never share
    /// a constructor.
    #[arg(long)]
    clean: bool,
    /// Sets `NVIM_APPNAME` in the spawned engine's own environment, so it
    /// reads `$XDG_CONFIG_HOME/<name>` instead of `$XDG_CONFIG_HOME/nvim`.
    #[arg(long)]
    appname: Option<String>,
    /// An explicit `view.toml` path, replacing the platform default
    /// [`view_native::paths::config_path`] would otherwise resolve. Feeds
    /// `NativeConfig::load` directly.
    #[arg(long)]
    config: Option<std::path::PathBuf>,
    /// Prints the system clipboard's current text for `register` ('+' or
    /// '*') to stdout and exits, bypassing the editor entirely. Exists for
    /// the compat oracle's own out-of-band verification: every other check
    /// that harness can run reads state inside the same nvim/view session
    /// under test, but the system clipboard can only be checked
    /// independently of view's own claims by a second, freshly spawned
    /// process reading it directly (see `NO_DISPLAY_EXIT`'s doc for the
    /// no-clipboard-available case).
    #[arg(long, hide = true)]
    print_clipboard: Option<char>,
    /// Everything not claimed by a flag above, forwarded to the engine
    /// exactly as typed: `+42`, `-c 'set nu'`, `-R`, `-d`, `-O`, `-u NONE`,
    /// file paths, `-` for stdin. clap must not try to interpret any of
    /// this; `allow_hyphen_values` is what lets an nvim short flag like
    /// `-c` start this catch-all instead of erroring as an unrecognized
    /// argument naming *view*. Enumerating each nvim flag here instead was
    /// rejected as a maintenance treadmill that silently breaks on every
    /// engine-pin bump that adds one.
    ///
    /// Because this is a trailing var-arg, view's own long flags above must
    /// appear before the first passthrough token on the command line --
    /// once this field starts matching, it swallows every remaining token,
    /// `--tier`/`--clean` included.
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

/// The engine config `cli` asks for: the ordinary spawn, every passthrough
/// argument forwarded verbatim, and `--clean`/`--appname`/the stdin relay
/// layered on top of it.
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
    if let Some(bin) = &cli.nvim_bin {
        cfg = cfg.with_nvim_bin(bin.clone());
    }
    if cli.clean {
        cfg = cfg.with_arg("--clean");
    }
    if let Some(appname) = &cli.appname {
        cfg = cfg.with_env("NVIM_APPNAME", appname.clone());
    }
    for arg in &cli.passthrough {
        cfg = cfg.with_arg(arg.clone());
    }
    maybe_relay_stdin(cfg, &cli.passthrough)
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

    let wants_stdin = passthrough.iter().any(|arg| arg == "-");
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
    let wants_stdin = passthrough.iter().any(|arg| arg == "-");
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
    // startup's own debug-build stderr log (see startup::paint_shell_frame)
    // measures the shell-paint budget from this instant, not from
    // Term::init: the capability probe that init() runs is itself startup
    // work the design spec's 50ms target is meant to cover
    let process_start = Instant::now();
    vlog::init(process_start);
    let cli = Cli::parse();
    if let Some(register) = cli.print_clipboard {
        return print_clipboard(register);
    }
    deny_unsupported_stdin_relay(&cli.passthrough)?;
    let cfg = engine_config(&cli);

    let mut term =
        Term::init(cli.tier.map(Tier::from)).context("failed to initialize terminal backend")?;
    let (width, height) = term.size()?;
    let residue = term.take_residue();

    let mut model = Model::with_term_size(width, height);
    model.caps = term.caps();
    vlog::log_with("startup", || {
        format!(
            "version={} caps tier={:?} sync={} truecolor={} kitty_kbd={} term={width}x{height}",
            env!("CARGO_PKG_VERSION"),
            model.caps.tier,
            model.caps.sync,
            model.caps.truecolor,
            model.caps.kitty_kbd
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
    let config_path = resolve_config_path(&cli);
    match &config_path {
        Some(path) => {
            let cached = theme_cache::load(path);
            vlog::log_with("theme", || {
                format!(
                    "cache {} path={}",
                    if cached.is_some() { "hit" } else { "miss" },
                    path.display()
                )
            });
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
            eprintln!(
                "view: cannot resolve a config path (no XDG_CONFIG_HOME, HOME, or APPDATA set); theme cache disabled this run"
            );
        }
    }

    // painted before the engine even spawns, themed from the cache just
    // seeded above, so a slow-starting nvim can never delay the terminal's
    // first visible content
    startup::paint_shell_frame(&mut term, &model, process_start)
        .context("failed to paint the startup shell frame")?;

    // created here, not inside runtime::run: the input thread has to start
    // capturing keystrokes immediately after the shell paints, well before
    // the engine exists to send them to, or anything typed during attach
    // would be lost to a not-yet-existing channel
    let (msg_tx, msg_rx) = mpsc::sync_channel(startup::MSG_CHANNEL_CAPACITY);
    let term_size = view_tui::terminal::TermSizeCell::default();
    view_tui::terminal::spawn_input_thread(msg_tx.clone(), term_size.clone());

    let engine_rx = startup::attach_in_background(cfg, width, height, residue, msg_tx.clone());
    let drained = startup::drain_pre_attach(&msg_rx, &mut model, &mut term);
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
    let mut engine = attach_result.map_err(|failure| match failure {
        startup::AttachFailure::Spawn(err) => anyhow::Error::new(err)
            .context("failed to spawn the nvim process (check --nvim-bin / PATH)"),
        startup::AttachFailure::Attach(err) => {
            anyhow::Error::new(err).context("engine attach failed or timed out after nvim started")
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
    // no executor existed at all, in `drain_pre_attach`) and `load`'s own
    // broken-config notice below both need a real toast-expiry timer the
    // moment they run, which is here -- strictly before `runtime::run`'s
    // loop -- not deferred any further than "the first executor that
    // exists."
    let executor = runtime::Executor::new(engine.handle.clone()).with_toast_timer(msg_tx.clone());
    for eff in drained.toast_effects {
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
        persist_theme(&model, &config_path);
        vlog::log_with("engine", || format!("exit code={code}"));
        // nvim already reported its own exit (a presink Msg::EngineStopped,
        // translated by run_cutover): drop explicitly so Engine's Drop
        // graceful-shutdown sequence still runs, since process::exit below
        // would otherwise skip every destructor on this stack
        drop(engine);
        term.restore_now();
        report_fatal_reason(&model);
        std::process::exit(code);
    }

    let (model, exit_code) = runtime::run(
        model,
        engine,
        pump,
        runtime::MsgChannel {
            tx: msg_tx.clone(),
            rx: msg_rx,
        },
        term_size,
        &mut follow_ups,
        &mut term,
    )?;
    persist_theme(&model, &config_path);
    vlog::log_with("engine", || format!("exit code={exit_code}"));
    // std::process::exit bypasses destructors, so the terminal must be
    // restored explicitly first; every other return path (an error
    // propagated via `?` above) is covered by `Drop` on `term`.
    term.restore_now();
    report_fatal_reason(&model);
    std::process::exit(exit_code);
}

/// Persists `model`'s current theme to `config_path`'s cache slot, on
/// every exit path that reaches one (both quit shapes call this identically
/// rather than each carrying its own copy, so a future third exit path
/// cannot copy one and silently miss the store).
fn persist_theme(model: &Model, config_path: &Option<std::path::PathBuf>) {
    if let Some(path) = config_path {
        theme_cache::store(Theme::from_hl(model.engine.hl()), path);
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
    use std::ffi::OsString;

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
        let cfg = engine_config(&Cli::parse_from(["view", "-"]));
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
