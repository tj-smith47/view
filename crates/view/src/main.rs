//! `view [FILE] --nvim-bin <path>`: CLI parsing and wiring for the terminal
//! frontend over an embedded Neovim engine. The startup sequence (shell
//! paint, background attach, pre-attach key buffering) lives in
//! [`startup`]; the steady-state loop itself lives in [`runtime`].

mod runtime;
mod startup;
mod theme_cache;

use anyhow::{Context, Result};
use clap::Parser;
use std::sync::mpsc;
use std::time::Instant;
use view_core::model::{Model, Tier};
use view_core::msg::Msg;
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

fn main() -> Result<()> {
    // startup's own debug-build stderr log (see startup::paint_shell_frame)
    // measures the shell-paint budget from this instant, not from
    // Term::init: the capability probe that init() runs is itself startup
    // work the design spec's 50ms target is meant to cover
    let process_start = Instant::now();
    let cli = Cli::parse();
    let mut cfg = EngineConfig::default();
    if let Some(bin) = cli.nvim_bin {
        cfg.nvim_bin = bin;
    }
    if let Some(file) = &cli.file {
        cfg.extra_args.push(file.as_os_str().to_owned());
    }

    let mut term =
        Term::init(cli.tier.map(Tier::from)).context("failed to initialize terminal backend")?;
    let (width, height) = term.size()?;
    let residue = term.take_residue();

    let mut model = Model::with_term_size(width, height);
    model.caps = term.caps();
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
            theme_cache::seed_hl_table(&mut model.engine.hl, &cached);
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
    let (msg_tx, msg_rx) = mpsc::sync_channel(64);
    view_tui::terminal::spawn_input_thread(msg_tx.clone());

    let engine_rx = startup::attach_in_background(cfg, width, height, residue, msg_tx.clone());
    let drained = startup::drain_pre_attach(&msg_rx, &mut model, &mut term);
    let mut engine = engine_rx
        .recv()
        .context("engine attach thread ended without a result")?
        .map_err(|failure| match failure {
            startup::AttachFailure::Spawn(err) => anyhow::Error::new(err)
                .context("failed to spawn the nvim process (check --nvim-bin / PATH)"),
            startup::AttachFailure::Attach(err) => anyhow::Error::new(err)
                .context("engine attach failed or timed out after nvim started"),
        })?;

    // Delivered directly through update()/Executor, never by re-enqueuing
    // onto msg_tx: msg_tx is a bounded sync_channel(64) with no consumer
    // running yet (runtime::run's loop starts below), so pushing
    // buffered-plus-still-queued keys back onto it can exceed capacity and
    // block `send` forever -- a permanent freeze in raw mode. Total order
    // survives without a second buffer: every key still sitting in msg_tx
    // at this point was typed by the input thread after drain_pre_attach
    // observed Msg::EngineReady (drain_pre_attach is msg_tx's only reader
    // up to that point), which is after every key in `drained.keys` was
    // already buffered -- so applying `drained` here, before anything
    // reads from msg_tx again, reproduces arrival order exactly. The
    // resize (if any) is applied first: nvim should see the final
    // pre-attach terminal size before it sees the keys typed at that size.
    let executor = runtime::Executor::new(engine.handle.clone());
    let mut engine_alive = true;
    if let Some((width, height)) = drained.resize {
        engine_alive = runtime::dispatch(&mut model, &executor, Msg::Resized { width, height })
            == runtime::Flow::Continue;
    }
    // skipped entirely once the resize write itself already reported the
    // engine gone: further replayed input would fail the same way, and
    // runtime::run's own loop discovers the same failure cleanly once its
    // pump is attached below
    if engine_alive {
        for key in drained.keys {
            if runtime::dispatch(&mut model, &executor, Msg::Key(key)) != runtime::Flow::Continue {
                break;
            }
        }
    }

    // attach_sink -- the only code path that connects the engine's pump to
    // msg_tx at all -- runs here, strictly after EngineReady was already
    // observed and the buffered window was already replayed above; see
    // startup::attach_in_background's doc comment for why that makes a
    // pump-originated message landing ahead of EngineReady structurally
    // impossible rather than merely unobserved.
    let pump = engine.start_pump(msg_tx.clone());

    let (model, exit_code) = runtime::run(model, engine, pump, msg_rx, &mut term)?;
    if let Some(path) = &config_path {
        theme_cache::store(Theme::from_hl(&model.engine.hl), path);
    }
    // std::process::exit bypasses destructors, so the terminal must be
    // restored explicitly first; every other return path (an error
    // propagated via `?` above) is covered by `Drop` on `term`.
    term.restore_now();
    std::process::exit(exit_code);
}
