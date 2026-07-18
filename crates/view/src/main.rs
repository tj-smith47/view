//! `view [FILE] --nvim-bin <path>`: CLI parsing and wiring for the terminal
//! frontend over an embedded Neovim engine. The runtime loop itself lives in
//! [`runtime`].

mod runtime;

use anyhow::{Context, Result};
use clap::Parser;
use view_core::model::Model;
use view_engine::process::{Engine, EngineConfig};
use view_tui::terminal::Term;

#[derive(Parser)]
#[command(name = "view", about = "A modern terminal editor powered by Neovim")]
struct Cli {
    /// File to open
    file: Option<std::path::PathBuf>,
    /// Path to the nvim binary (defaults to PATH lookup)
    #[arg(long)]
    nvim_bin: Option<std::path::PathBuf>,
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

    let mut term = Term::init().context("failed to initialize terminal backend")?;
    let (width, height) = term.size()?;

    // the only request the setup path makes; once run() starts, every nvim
    // call goes through notify so a slow response never stalls a frame or a
    // keystroke
    // the underlying EngineError::Timeout variant's Display already names
    // the elapsed timeout, so this context only needs to name the call
    engine
        .handle
        .ui_attach(width, height)
        .context("ui attach failed or timed out")?;

    let model = Model::new();
    let exit_code = runtime::run(model, engine, &mut term)?;
    // std::process::exit bypasses destructors, so the terminal must be
    // restored explicitly first; every other return path (an error
    // propagated via `?` above) is covered by `Drop` on `term`.
    term.restore_now();
    std::process::exit(exit_code);
}
