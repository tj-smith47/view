//! `view [FILE] --nvim-bin <path>`: CLI parsing and wiring for the terminal
//! frontend over an embedded Neovim engine. The runtime loop itself lives in
//! [`runtime`].

mod runtime;
mod theme_cache;

use anyhow::{Context, Result};
use clap::Parser;
use view_core::model::{Model, Tier};
use view_core::theme::Theme;
use view_engine::process::{Engine, EngineConfig};
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
    let cli = Cli::parse();
    let mut cfg = EngineConfig::default();
    if let Some(bin) = cli.nvim_bin {
        cfg.nvim_bin = bin;
    }
    if let Some(file) = &cli.file {
        cfg.extra_args.push(file.as_os_str().to_owned());
    }
    let engine = Engine::spawn(cfg).context("failed to start nvim engine")?;

    let mut term =
        Term::init(cli.tier.map(Tier::from)).context("failed to initialize terminal backend")?;
    let (width, height) = term.size()?;
    let residue = term.take_residue();

    // the only request the setup path makes; once run() starts, every nvim
    // call goes through notify so a slow response never stalls a frame or a
    // keystroke
    // the underlying EngineError::Timeout variant's Display already names
    // the elapsed timeout, so this context only needs to name the call
    engine
        .handle
        .ui_attach(width, height)
        .context("ui attach failed or timed out")?;

    // anything the user typed before or during the startup capability probe
    // (see Term::take_residue) has to reach nvim before the runtime loop's
    // own input thread starts, or it is lost for good; errors are ignored
    // here rather than propagated, since a write failure on a
    // freshly-attached connection means the engine is already gone, which
    // run() below discovers and handles through its own EngineDown path
    // moments later
    for notation in view_tui::keys::encode_residue_bytes(&residue) {
        let _ = engine.handle.input(&notation);
    }

    let mut model = Model::with_term_size(width, height);
    model.caps = term.caps();

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

    let (model, exit_code) = runtime::run(model, engine, &mut term)?;
    if let Some(path) = &config_path {
        theme_cache::store(Theme::from_hl(&model.engine.hl), path);
    }
    // std::process::exit bypasses destructors, so the terminal must be
    // restored explicitly first; every other return path (an error
    // propagated via `?` above) is covered by `Drop` on `term`.
    term.restore_now();
    std::process::exit(exit_code);
}
