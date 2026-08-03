//! `view [FILE] --nvim-bin <path>`: CLI parsing and wiring for the terminal
//! frontend over an embedded Neovim engine. The startup sequence (shell
//! paint, background attach, pre-attach key buffering) lives in
//! [`startup`]; the steady-state loop itself lives in [`runtime`].

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
    /// File to open
    file: Option<std::path::PathBuf>,
    /// Path to the nvim binary (defaults to PATH lookup)
    #[arg(long)]
    nvim_bin: Option<std::path::PathBuf>,
    /// Override auto-detected terminal capabilities instead of probing
    #[arg(long)]
    tier: Option<TierArg>,
}

/// The engine config `cli` asks for: the ordinary spawn, plus the binary
/// path and file argument the user named.
///
/// A function rather than four lines inside `main` so the constructor it
/// starts from is assertable. This is the editor a user's own session runs
/// and the one the measurement matrix measures a pinned fixture
/// configuration through, so it must keep starting from
/// [`EngineConfig::default`]: `EngineConfig::isolated` compiles here just as
/// well, and would spawn a child with `--clean`, discarding the very config
/// being measured. A matrix run recording that child's numbers reports a
/// large improvement and gates green.
fn engine_config(cli: &Cli) -> EngineConfig {
    let mut cfg = EngineConfig::default();
    if let Some(bin) = &cli.nvim_bin {
        cfg = cfg.with_nvim_bin(bin.clone());
    }
    if let Some(file) = &cli.file {
        cfg = cfg.with_arg(file.as_os_str());
    }
    cfg
}

fn main() -> Result<()> {
    // startup's own debug-build stderr log (see startup::paint_shell_frame)
    // measures the shell-paint budget from this instant, not from
    // Term::init: the capability probe that init() runs is itself startup
    // work the design spec's 50ms target is meant to cover
    let process_start = Instant::now();
    vlog::init(process_start);
    let cli = Cli::parse();
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

    // config loading itself lands with the config system (view.toml
    // sourcing is out of this crate's scope so far); resolving just the
    // path identity here is enough to key the theme cache, so cold start
    // can already paint last session's colors before nvim answers
    // `ui_attach` with its own `default_colors_set`.
    let config_path = theme_cache::resolved_config_path();
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

    let executor = runtime::Executor::new(engine.handle.clone());
    // Resolves the presink messages, the pending redraw, and the pre-attach
    // input buffer directly through update()/Executor -- never by touching
    // msg_tx, whose only reader (runtime::run's loop) has not started yet.
    // See run_cutover's doc comment for the full ordering and
    // no-blocking-send argument.
    let outcome = startup::run_cutover(
        &mut model,
        &executor,
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

    let (model, exit_code) = runtime::run(model, engine, pump, msg_rx, term_size, &mut term)?;
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
    fn a_file_argument_is_the_only_argument_the_cli_adds() {
        let cfg = engine_config(&Cli::parse_from(["view", "notes.txt"]));
        assert_eq!(cfg.extra_args, vec![OsString::from("notes.txt")]);
        assert!(cfg.env_plan().is_empty(), "{:?}", cfg.env_plan());
    }

    #[test]
    fn nvim_bin_replaces_the_path_lookup() {
        let cfg = engine_config(&Cli::parse_from(["view", "--nvim-bin", "/opt/nvim"]));
        assert_eq!(cfg.nvim_bin, std::path::PathBuf::from("/opt/nvim"));
    }
}
