//! `:View ui panes` through a real session, read off the terminal.
//!
//! The flip's own effects are pinned in `view-core` and the engine's half
//! in `ext_tabline_toggle.rs`, and neither says what a person sees. This
//! drives the binary a user runs and reads the cells of row 0 on both
//! sides of the switch.
//!
//! Unix only, like every other leg that spawns the binary in a pty: the
//! hermetic search path the pty funnel gives the child leaves it no engine
//! to find on Windows.
#![cfg(unix)]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use std::time::Duration;

use view_oracle::PtySession;

const COLS: u16 = 80;
const ROWS: u16 = 24;
const BUDGET: Duration = Duration::from_secs(20);

/// The text of row `row`, blanks included.
fn row_text(screen: &vt100::Screen, row: u16) -> String {
    (0..COLS)
        .filter_map(|col| screen.cell(row, col).map(vt100::Cell::contents))
        .collect()
}

/// Starts view on its scratch file under the planted `panes = "nvim"`
/// config with `env` set, and waits for the startup screen. Returns the
/// scratch paths, which have to outlive the session, the session, and the
/// file's name.
fn launch(label: &str, env: &[(&str, &str)]) -> (common::ScratchPaths, PtySession, String) {
    let paths = common::ScratchPaths::new(label);
    let mut cmd = portable_pty::CommandBuilder::new(common::view_bin_path());
    cmd.cwd(
        paths
            .scratch
            .parent()
            .expect("the scratch file always sits inside the scratch root"),
    );
    let name = paths
        .scratch
        .file_name()
        .expect("the scratch file always has a file name")
        .to_string_lossy()
        .to_string();
    cmd.arg(&name);
    common::isolate_xdg_first_launch(&mut cmd, &paths.isolated_home);
    for (var, value) in env {
        cmd.env(var, value);
    }

    let mut session = PtySession::spawn_configured(cmd, COLS, ROWS)
        .expect("PtySession::spawn_configured against target/debug/view");
    assert!(
        session.wait_for("~", BUDGET),
        "view never painted its startup shell; screen:\n{}",
        session.screen()
    );
    (paths, session, name)
}

/// Under tiles the row carries only what the frames do not, so one tabpage
/// draws none and a second one brings it.
#[test]
fn a_tiled_top_row_comes_with_a_second_tabpage_and_leaves_with_it() {
    let (_paths, mut session, name) = launch("panes-flip", &[]);

    // the planted config is `panes = "nvim"`, where one tabpage reserves
    // no row at all
    assert!(
        !session
            .with_screen(|screen| row_text(screen, 0))
            .contains(&name),
        "the row was already there under nvim mode with one tabpage; screen:\n{}",
        session.screen()
    );

    // the tile's frame names the file on row 1, and row 0 stays empty
    let framed_alone = |screen: &vt100::Screen| {
        row_text(screen, 1).contains(&name) && !row_text(screen, 0).contains(&name)
    };
    session.send(b"\x1b:View ui panes tiles\r").unwrap();
    assert!(
        session.wait_for_screen(BUDGET, framed_alone),
        "one tabpage under tiles drew a top row; row 0: {:?}\nscreen:\n{}",
        session.with_screen(|screen| row_text(screen, 0)),
        session.screen()
    );

    session.send(b"\x1b:tabnew\r").unwrap();
    assert!(
        session.wait_for_screen(BUDGET, |screen| row_text(screen, 0).contains(&name)),
        "a second tabpage never put the first one's name on row 0; row 0: {:?}\nscreen:\n{}",
        session.with_screen(|screen| row_text(screen, 0)),
        session.screen()
    );

    session.send(b"\x1b:tabclose\r").unwrap();
    assert!(
        session.wait_for_screen(BUDGET, framed_alone),
        "closing the second tabpage left the row up; row 0: {:?}\nscreen:\n{}",
        session.with_screen(|screen| row_text(screen, 0)),
        session.screen()
    );
}

/// A flip to `panes = "nvim"` takes a drawn row off the screen. Two listed
/// buffers under `tabline_shows = "buffers"` keep view's row up under
/// tiles on one tabpage, where nvim, handed the row back, draws no tab
/// line of its own.
#[test]
fn a_panes_flip_takes_a_drawn_top_row_away_on_screen() {
    let (_paths, mut session, name) = launch(
        "panes-flip-buffers",
        &[("VIEW_NATIVE_TABLINE_SHOWS", "buffers")],
    );

    session.send(b"\x1b:View ui panes tiles\r").unwrap();
    assert!(
        session.wait_for_screen(BUDGET, |screen| row_text(screen, 1).contains(&name)),
        "the flip to tiles never framed the file; screen:\n{}",
        session.screen()
    );

    session.send(b"\x1b:badd second.txt\r").unwrap();
    assert!(
        session.wait_for_screen(BUDGET, |screen| row_text(screen, 0).contains(&name)),
        "a second listed buffer never put the names on row 0; row 0: {:?}\nscreen:\n{}",
        session.with_screen(|screen| row_text(screen, 0)),
        session.screen()
    );

    session.send(b"\x1b:View ui panes nvim\r").unwrap();
    assert!(
        session.wait_for_screen(BUDGET, |screen| !row_text(screen, 0).contains(&name)),
        "the flip back never handed row 0 over; row 0: {:?}\nscreen:\n{}",
        session.with_screen(|screen| row_text(screen, 0)),
        session.screen()
    );

    // a flip re-targets `laststatus` from view's own prior hold to its
    // own next one, which is not a foreign write; a notice box saying so
    // is the false positive `HOLD_OPTION_CHUNK`'s own-value check exists
    // to refuse
    let conflict_box = session.with_screen(|screen| {
        (0..ROWS).any(|row| row_text(screen, row).contains("your config also draws"))
    });
    assert!(
        !conflict_box,
        "a surface-conflict notice is on screen after the flip; screen:\n{}",
        session.screen()
    );
}
