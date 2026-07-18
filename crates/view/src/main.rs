//! `view [FILE] --nvim-bin <path>`: the wired terminal frontend for an
//! embedded Neovim engine.

use anyhow::{Context, Result};
use clap::Parser;
use std::sync::mpsc::RecvTimeoutError;
use std::time::{Duration, Instant};
use view_core::grid::{Grid, GridOp};
use view_engine::process::{Engine, EngineConfig};
use view_engine::ui_events::{decode_redraw, UiEvent};
use view_tui::paint::{clamp_dim, saturate_u16, HlAttr, HlTable};
use view_tui::terminal::{drain_input, InputEvent, Term};

/// Upper bound on time spent in one pass of the notification-drain loop
/// before falling through to painting and input polling.
///
/// Without a budget, a sustained redraw stream (e.g. scrolling a large
/// file emits one `redraw` notification per rendered line) keeps the inner
/// loop's 4ms silence-timeout from ever firing, so input is never drained
/// and keystrokes queue up indefinitely behind the flood. 8ms keeps each
/// pass comfortably under one frame at typical terminal repaint rates, so
/// painting still reads as continuous while input polling is guaranteed a
/// turn every pass.
const DRAIN_BUDGET: Duration = Duration::from_millis(8);

/// Whether the notification-drain loop should keep pulling queued events
/// rather than yield to painting and input polling, given it started
/// draining at `started`.
fn should_keep_draining(started: Instant, budget: Duration) -> bool {
    started.elapsed() < budget
}

/// Maps a child's raw exit status to the code `view` itself should exit
/// with, so an abnormally terminated engine is never reported as success.
///
/// On Unix, a process killed by a signal has no exit code at all
/// (`status.code()` is `None`); naively defaulting that to `0` reports a
/// crashed engine as a clean exit. `128 + signal` is the conventional
/// mapping shells themselves use (`$?` after a `SIGKILL`ed process is 137),
/// so scripts inspecting `view`'s exit code see the same convention they
/// already know.
#[cfg(unix)]
fn exit_code_for(status: std::process::ExitStatus) -> i32 {
    use std::os::unix::process::ExitStatusExt;
    status
        .code()
        .unwrap_or_else(|| status.signal().map_or(1, |sig| 128 + sig))
}

/// Non-Unix fallback: there is no signal concept to map, so a missing exit
/// code (which `std::process::ExitStatus::code` can still return on other
/// platforms for an abnormal termination) becomes a plain nonzero failure
/// rather than the misleading default of success.
#[cfg(not(unix))]
fn exit_code_for(status: std::process::ExitStatus) -> i32 {
    status.code().unwrap_or(1)
}

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
        // engine events: drain whatever is queued (bounded by DRAIN_BUDGET),
        // then paint once on flush
        let drain_started = Instant::now();
        loop {
            if !should_keep_draining(drain_started, DRAIN_BUDGET) {
                break;
            }
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
                    // engine exited (:q, :cq, or a crash). Restore terminal
                    // via guards, then propagate nvim's exit code so :cq
                    // flows work and an abnormal death is never reported as
                    // success. shutdown() consumes engine and returns the
                    // real exit status; Drop alone (kill+reap, no return
                    // value) cannot surface it. The graceful qa! shutdown()
                    // sends is a harmless no-op here since the connection is
                    // already closed - the child has typically already
                    // exited by this point, so try_wait picks it up on the
                    // first poll.
                    term.restore_now();
                    let status = engine.shutdown()?;
                    std::process::exit(exit_code_for(status));
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_draining_within_budget() {
        let started = Instant::now();
        assert!(should_keep_draining(started, Duration::from_millis(50)));
    }

    #[test]
    fn stops_draining_once_budget_elapses() {
        let started = Instant::now() - Duration::from_millis(50);
        assert!(!should_keep_draining(started, Duration::from_millis(8)));
    }

    #[cfg(unix)]
    #[test]
    fn exit_code_for_signal_death_maps_to_128_plus_signal() {
        use std::os::unix::process::ExitStatusExt;
        // raw wait-status encoding: a nonzero low 7 bits with no exit code
        // means "terminated by signal N", here SIGKILL (9)
        let status = std::process::ExitStatus::from_raw(9);
        assert_eq!(exit_code_for(status), 137);
    }

    #[cfg(unix)]
    #[test]
    fn exit_code_for_normal_exit_preserves_code() {
        use std::os::unix::process::ExitStatusExt;
        // raw wait-status encoding: exit code lives in bits 8-15
        let status = std::process::ExitStatus::from_raw(5 << 8);
        assert_eq!(exit_code_for(status), 5);
    }
}
