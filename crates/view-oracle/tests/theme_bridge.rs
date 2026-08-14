//! Proof, through the real `view` binary in a real terminal, that a
//! colorscheme changed mid-session reaches both surfaces it has to reach:
//! the colors on screen, and the cold-start theme cache on disk.
//!
//! Neither is expressible in the differential corpus. Both of its legs
//! consume the same event stream, so a session that never learned a scheme
//! had changed would leave the two sides agreeing exactly as before; what a
//! colorscheme switch produces that nothing else does is an announcement
//! outside the redraw stream, and the only place its consequence becomes
//! visible is a rendered cell and a written file.
//!
//! The scheme is planted rather than borrowed from nvim's builtins so the
//! assertion can be an exact color rather than "something moved": a test
//! that only checks for movement passes just as happily when the wrong
//! colors arrive.
#![cfg(unix)]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use view_oracle::PtySession;

/// The terminal the session is driven at. Row 0 is the tabline once a
/// second tab is open, which is the chrome this samples.
const COLS: u16 = 80;
const ROWS: u16 = 24;

/// The planted scheme's name, and the colors it pins on every tabline
/// group. One color pair across all three groups so the assertion holds at
/// any column of the row, whatever the tab labels happen to be named.
const SCHEME: &str = "viewbridge";
const CHROME_FG: vt100::Color = vt100::Color::Rgb(0xff, 0x00, 0xff);
const CHROME_BG: vt100::Color = vt100::Color::Rgb(0x00, 0xff, 0xff);

/// The same two colors as the cache serializes them: packed 24-bit ints,
/// which is what `Theme`'s resolved styles carry.
const CHROME_FG_PACKED: u32 = 0x00ff_00ff;
const CHROME_BG_PACKED: u32 = 0x0000_ffff;

/// Budget for anything the session has to do: a cold `view` spawn plus an
/// nvim spawn on a loaded box is the slow part, and every wait here is
/// satisfied in milliseconds by a healthy session.
const BUDGET: Duration = Duration::from_secs(10);

/// Writes `colors/<SCHEME>.vim` into the isolated nvim config directory.
///
/// No `highlight clear`: clearing resets the default colors too, which
/// makes nvim re-announce them and opens a fresh background probe the cache
/// write then has to wait on. Pinning only the three tabline groups keeps
/// what this test measures -- a named group's colors reaching the screen
/// and the cache -- separate from the probe round trip.
fn plant_scheme(home: &Path) {
    let dir = common::xdg_home(home, "XDG_CONFIG_HOME")
        .join("nvim")
        .join("colors");
    std::fs::create_dir_all(&dir).expect("the isolated nvim config home must be creatable");
    let body = format!(
        "let g:colors_name = '{SCHEME}'\n\
         hi TabLine     guifg=#ff00ff guibg=#00ffff gui=NONE\n\
         hi TabLineSel  guifg=#ff00ff guibg=#00ffff gui=NONE\n\
         hi TabLineFill guifg=#ff00ff guibg=#00ffff gui=NONE\n"
    );
    std::fs::write(dir.join(format!("{SCHEME}.vim")), body)
        .expect("the planted colorscheme must be writable");
}

/// The theme cache `view` writes for this isolated home, if it has written
/// one yet. Located by shape rather than by name: the file is named for a
/// hash of the config path, and recomputing that hash here would pin the
/// key rather than read it.
fn cache_file(home: &Path) -> Option<PathBuf> {
    let dir = common::xdg_home(home, "XDG_STATE_HOME").join("view");
    std::fs::read_dir(dir).ok()?.flatten().find_map(|entry| {
        let path = entry.path();
        let name = path.file_name()?.to_str()?;
        (name.starts_with("theme-") && name.ends_with(".toml")).then_some(path)
    })
}

/// The body of `text`'s `[header]` table: everything after its header line
/// up to the next one.
fn table<'a>(text: &'a str, header: &str) -> Option<&'a str> {
    let rest = text.split_once(&format!("[{header}]\n"))?.1;
    Some(rest.split_once("\n[").map_or(rest, |(body, _)| body))
}

/// Whether the cache on disk already carries the planted scheme's chrome.
fn cache_carries_scheme(home: &Path) -> bool {
    let Some(path) = cache_file(home) else {
        return false;
    };
    let Ok(text) = std::fs::read_to_string(path) else {
        return false;
    };
    table(&text, "chrome.TabLineFill").is_some_and(|body| {
        body.contains(&format!("fg = {CHROME_FG_PACKED}"))
            && body.contains(&format!("bg = {CHROME_BG_PACKED}"))
    })
}

/// Blocks until the cache carries the planted scheme, returning whether it
/// did within [`BUDGET`].
///
/// A wait rather than a single read because the write can trail the frame
/// that shows the new colors: `view` writes the cache from the same
/// dispatch that applied the highlight batch, but a scheme touching the
/// default colors defers it to the background probe's reply, one round trip
/// later. Waiting covers both without asserting which one happened.
fn wait_for_cached_scheme(home: &Path) -> bool {
    let deadline = Instant::now() + BUDGET;
    while Instant::now() < deadline {
        if cache_carries_scheme(home) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    false
}

/// The text of `row`, for failure messages that show what was on screen
/// instead of what was expected.
fn row_text(screen: &vt100::Screen, row: u16) -> String {
    (0..COLS)
        .filter_map(|col| screen.cell(row, col).map(vt100::Cell::contents))
        .collect()
}

/// Every cell of `row` as a foreground/background pair.
fn row_colors(screen: &vt100::Screen, row: u16) -> Vec<(vt100::Color, vt100::Color)> {
    (0..COLS)
        .map(|col| {
            screen
                .cell(row, col)
                .map_or((vt100::Color::Default, vt100::Color::Default), |cell| {
                    (cell.fgcolor(), cell.bgcolor())
                })
        })
        .collect()
}

/// Whether every cell of `row` is wearing the planted scheme's colors.
fn row_wears_scheme(screen: &vt100::Screen, row: u16) -> bool {
    row_colors(screen, row)
        .iter()
        .all(|pair| *pair == (CHROME_FG, CHROME_BG))
}

/// A `view` session on an isolated home with the scheme already planted,
/// paused at the point where a second tab has opened and the tabline is
/// therefore on screen.
fn tabline_session(paths: &common::ScratchPaths) -> PtySession {
    plant_scheme(&paths.isolated_home);

    let mut cmd = portable_pty::CommandBuilder::new(common::view_bin_path());
    // the file's own name, from its own directory, rather than the absolute
    // path: nvim labels the first tab with the buffer's name, and an
    // absolute one is exactly as long as this checkout's location happens to
    // be. A deeper one (`/Users/<name>/repos/view` against `/opt/repos/view`
    // is enough) pushes the second tab's label off the right edge of an
    // 80-column row, leaving the wait below nothing to find on a host that
    // differs from this test's author's in nothing but its home directory.
    cmd.cwd(
        paths
            .scratch
            .parent()
            .expect("the scratch file always sits inside the scratch root"),
    );
    cmd.arg(
        paths
            .scratch
            .file_name()
            .expect("the scratch file always has a file name"),
    );
    common::isolate_xdg_native_off(&mut cmd, &paths.isolated_home);

    let mut session = PtySession::spawn_configured(cmd, COLS, ROWS)
        .expect("PtySession::spawn_configured against target/debug/view");

    assert!(
        session.wait_for("~", BUDGET),
        "view never painted its startup shell; screen:\n{}",
        session.screen()
    );

    // bare nvim only shows a tabline past one tab, and view reserves the
    // row on the same threshold, so a second tab is what puts the chrome
    // this samples on screen at all
    session.send(b"\x1b:tabnew\r").unwrap();
    // the label, not the whole `[No Name]`: the first tab is named for the
    // file argument's full path, which can push the second tab's name off
    // the right edge of an 80-column row
    assert!(
        session.wait_for_screen(BUDGET, |screen| row_text(screen, 0).contains("[No Nam")),
        "the tabline never appeared on the reserved row; screen:\n{}",
        session.screen()
    );
    session
}

/// The whole path, end to end, in the binary a user runs: `:colorscheme`
/// mid-session repaints chrome in the new scheme's colors without a
/// restart, and persists them for the next launch's first frame while the
/// session is still running.
///
/// Both halves are asserted against one session on purpose. The rendered
/// half alone cannot say the switch was ever recorded, and the cache half
/// alone cannot say the user saw anything change.
#[test]
fn a_colorscheme_set_mid_session_repaints_chrome_and_caches_it() {
    let paths = common::ScratchPaths::new("theme-bridge");
    let mut session = tabline_session(&paths);

    let before = session.with_screen(|screen| row_colors(screen, 0));
    assert!(
        !before.iter().all(|pair| *pair == (CHROME_FG, CHROME_BG)),
        "the session was already wearing the planted scheme before it was asked for, \
         so nothing below could distinguish a switch from a no-op"
    );
    assert!(
        !cache_carries_scheme(&paths.isolated_home),
        "a cache carrying the planted scheme already existed before the switch"
    );

    session
        .send(format!("\x1b:colorscheme {SCHEME}\r").as_bytes())
        .unwrap();

    assert!(
        session.wait_for_screen(BUDGET, |screen| row_wears_scheme(screen, 0)),
        "the tabline never repainted in the new scheme's colors: the theme a painter \
         reads did not re-derive from the colors the switch applied.\nrow 0 text: {:?}\n\
         row 0 colors: {:?}",
        session.with_screen(|screen| row_text(screen, 0)),
        session.with_screen(|screen| row_colors(screen, 0)),
    );

    let after = session.with_screen(|screen| row_colors(screen, 0));
    assert_ne!(
        before, after,
        "the rendered chrome is identical across the switch"
    );

    // read while the session is still running, which is what makes this the
    // bridge's own observable: the only other writer runs on the way out
    assert!(
        wait_for_cached_scheme(&paths.isolated_home),
        "no theme cache carrying the new scheme was written while the session was alive: \
         the switch reached the screen but nothing recorded it for the next cold start"
    );

    session.send(b"\x1b:qa!\r").unwrap();
    let _ = session.wait_for_exit(BUDGET);
}
