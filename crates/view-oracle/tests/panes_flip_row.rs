//! `:View ui panes` through a real session, read off the terminal.
//!
//! The flip's own effects are pinned in `view-core` and the engine's half
//! in `ext_tabline_toggle.rs`, and neither says what a person sees. This
//! drives the binary a user runs and reads the cells of row 0 on both
//! sides of the switch.
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

/// Under tiles the row carries only what the frames do not, so one tabpage
/// draws none and a second one brings it.
#[test]
fn a_tiled_top_row_comes_with_a_second_tabpage_and_leaves_with_it() {
    let paths = common::ScratchPaths::new("panes-flip");
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

    let mut session = PtySession::spawn_configured(cmd, COLS, ROWS)
        .expect("PtySession::spawn_configured against target/debug/view");
    assert!(
        session.wait_for("~", BUDGET),
        "view never painted its startup shell; screen:\n{}",
        session.screen()
    );

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

    session.send(b"\x1b:View ui panes nvim\r").unwrap();
    assert!(
        session.wait_for_screen(BUDGET, |screen| !row_text(screen, 0).contains(&name)),
        "the flip back never handed row 0 over; row 0: {:?}\nscreen:\n{}",
        session.with_screen(|screen| row_text(screen, 0)),
        session.screen()
    );
}
