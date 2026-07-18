//! `view [FILE] --nvim-bin <path>`: the wired terminal frontend for an
//! embedded Neovim engine.

use anyhow::{Context, Result};
use clap::Parser;
use std::sync::mpsc::RecvTimeoutError;
use std::time::Duration;
use view_core::grid::{Grid, GridOp};
use view_engine::process::{Engine, EngineConfig};
use view_engine::ui_events::{decode_redraw, UiEvent};
use view_tui::paint::{clamp_dim, saturate_u16, HlAttr, HlTable};
use view_tui::terminal::{drain_input, InputEvent, Term, TerminalGuard};

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
    let mut engine = Engine::spawn(cfg).context("failed to start nvim engine")?;
    // take the receiver once, up front: a Receiver field is !Sync and would
    // make Engine, and Arc<Engine>, not even Send
    let notifications = engine
        .take_notifications()
        .context("notification receiver already taken")?;

    let _guard = TerminalGuard::enter().context("failed to enter raw mode")?;
    let mut term = Term::init().context("failed to initialize terminal backend")?;
    let (width, height) = term.size()?;

    // the only request the setup path makes; once the loop below starts,
    // every nvim call goes through notify so a slow response never stalls a
    // frame or a keystroke
    engine
        .handle
        .ui_attach(width, height)
        .context("ui attach failed")?;

    let mut grid = Grid::new();
    let mut hl = HlTable {
        default_fg: None,
        default_bg: None,
        attrs: std::collections::HashMap::new(),
    };
    let mut dirty = false;

    loop {
        // engine events: drain whatever is queued, then paint once on flush
        loop {
            match notifications.recv_timeout(Duration::from_millis(4)) {
                Ok(note) if note.method == "redraw" => {
                    for ev in decode_redraw(&note.params) {
                        match ev {
                            UiEvent::GridResize { width, height, .. } => {
                                // clamp untrusted wire dimensions: a desynced or
                                // malformed grid_resize must not allocate
                                // unboundedly, and a plain `as u16` cast would
                                // silently truncate 65536 to 0
                                grid.apply(GridOp::Resize {
                                    width: clamp_dim(width),
                                    height: clamp_dim(height),
                                });
                            }
                            UiEvent::GridLine {
                                row,
                                col_start,
                                cells,
                                ..
                            } => {
                                grid.apply(GridOp::PutLine {
                                    row: saturate_u16(row),
                                    col_start: saturate_u16(col_start),
                                    cells: cells
                                        .into_iter()
                                        .map(|c| (c.text, c.hl_id, c.repeat))
                                        .collect(),
                                });
                            }
                            UiEvent::GridCursorGoto { row, col, .. } => {
                                grid.apply(GridOp::CursorGoto {
                                    row: saturate_u16(row),
                                    col: saturate_u16(col),
                                });
                            }
                            UiEvent::GridScroll {
                                top,
                                bot,
                                left,
                                right,
                                rows,
                                ..
                            } => {
                                grid.apply(GridOp::Scroll {
                                    top: saturate_u16(top),
                                    bot: saturate_u16(bot),
                                    left: saturate_u16(left),
                                    right: saturate_u16(right),
                                    rows: i32::try_from(rows).unwrap_or(if rows > 0 {
                                        i32::MAX
                                    } else {
                                        i32::MIN
                                    }),
                                });
                            }
                            UiEvent::GridClear { .. } => grid.apply(GridOp::Clear),
                            UiEvent::HlAttrDefine {
                                id,
                                fg,
                                bg,
                                bold,
                                italic,
                                underline: _,
                                reverse,
                            } => {
                                hl.attrs.insert(
                                    id,
                                    HlAttr {
                                        fg,
                                        bg,
                                        bold,
                                        italic,
                                        reverse,
                                    },
                                );
                            }
                            UiEvent::DefaultColorsSet { fg, bg, .. } => {
                                hl.default_fg = fg;
                                hl.default_bg = bg;
                            }
                            UiEvent::Flush => dirty = true,
                            UiEvent::Unknown { .. } => {}
                            // UiEvent is #[non_exhaustive]: a future nvim-side
                            // event kind must degrade to a no-op here rather
                            // than fail to compile on a version bump
                            _ => {}
                        }
                    }
                }
                Ok(_) => {}
                Err(RecvTimeoutError::Timeout) => break,
                Err(RecvTimeoutError::Disconnected) => {
                    // engine exited (:q). Restore terminal via guards, then
                    // propagate nvim's exit code so :cq flows work. shutdown()
                    // consumes engine and returns the real exit status; Drop
                    // alone (kill+reap, no return value) cannot surface it.
                    // The graceful qa! shutdown() sends is a harmless no-op
                    // here since the connection is already closed - the
                    // child has typically already exited by this point, so
                    // try_wait picks it up on the first poll.
                    term.restore_now();
                    let status = engine.shutdown()?;
                    std::process::exit(status.code().unwrap_or(0));
                }
            }
        }

        if dirty {
            term.draw(&grid, &hl)?;
            let (row, col) = grid.cursor();
            term.set_cursor(row, col)?;
            dirty = false;
        }

        // input: drain without blocking the paint path
        for ev in drain_input()? {
            match ev {
                // fire-and-forget: the paint loop must never await an RPC
                // response, or one slow keystroke stalls every frame queued
                // behind it
                InputEvent::Key(notation) => engine.handle.input(&notation)?,
                InputEvent::Resize(w, h) => engine.handle.try_resize(w, h)?,
            }
        }
    }
}
