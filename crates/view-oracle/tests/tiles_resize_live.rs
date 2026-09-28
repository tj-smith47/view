//! A terminal resize under tiles, read off the screen view paints while
//! nvim is still between the two halves of it.
//!
//! nvim announces the new window slots (`win_pos`) as soon as the screen is
//! resized, runs the config's `VimResized` handlers, and only then sizes and
//! redraws the window grids. A config whose handlers take time leaves view
//! holding new slots and old grids for that long, which is what a person
//! sees after dragging a terminal wider under a plugin-heavy config. The
//! handler here is a sleep, so the gap is long enough to sample.
#![cfg(unix)]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use std::time::{Duration, Instant};

use view_oracle::PtySession;
use view_test_support::host_deadline;

const COLS: u16 = 220;
const ROWS: u16 = 50;
const SHRUNK: (u16, u16) = (150, 38);
const BUDGET: Duration = Duration::from_secs(20);
const SAMPLE: Duration = Duration::from_millis(50);

/// How long a screen may show a tile's text and its frame disagreeing: one
/// redraw, the silence `window_fit_live.rs` settles a session on.
const REDRAW: Duration = Duration::from_millis(200);

/// Every glyph a frame is drawn with, under either look.
const FRAME_GLYPHS: &str = "─│╭╮╰╯┬┴├┤┼";

/// A tile's frame, as the rows and columns of its box.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Frame {
    top: u16,
    left: u16,
    bottom: u16,
    right: u16,
}

fn glyph(screen: &vt100::Screen, row: u16, col: u16) -> String {
    screen
        .cell(row, col)
        .map_or_else(String::new, |cell| cell.contents().to_string())
}

/// Every frame on screen, found from its top-left corner along its top and
/// left edges. A gapped tile's corners are its own; a gapless tile shares
/// the corners where its edges meet a neighbour's.
fn frames(screen: &vt100::Screen, cols: u16, rows: u16) -> Vec<Frame> {
    let is = |row: u16, col: u16, set: &str| {
        let g = glyph(screen, row, col);
        !g.is_empty() && set.contains(g.as_str())
    };
    let mut found = Vec::new();
    for top in 0..rows {
        for left in 0..cols {
            if !is(top, left, "╭┬") {
                continue;
            }
            let right = (left + 1..cols).find(|&c| is(top, c, "╮┬"));
            let bottom = (top + 1..rows).find(|&r| is(r, left, "╰┴"));
            if let (Some(right), Some(bottom)) = (right, bottom) {
                found.push(Frame {
                    top,
                    left,
                    bottom,
                    right,
                });
            }
        }
    }
    found
}

/// Where the screen shows a tile's text and its frame disagreeing, or
/// `None` where every cell inside a frame is buffer text, no buffer text
/// stands outside one, and nothing but a frame line stands outside every
/// frame.
///
/// Every buffer line is wider than any tile and `nowrap` is set, so a
/// tile whose grid fills its frame has a `#` in every cell inside it; no
/// title or status segment writes one.
fn mismatch(screen: &vt100::Screen, cols: u16, rows: u16) -> Option<String> {
    let frames = frames(screen, cols, rows);
    if frames.len() < 2 {
        return Some(format!("{} frames on screen", frames.len()));
    }
    let inside = |row: u16, col: u16| {
        frames
            .iter()
            .any(|f| row > f.top && row < f.bottom && col > f.left && col < f.right)
    };
    let on_a_frame = |row: u16, col: u16| {
        frames
            .iter()
            .any(|f| (f.top..=f.bottom).contains(&row) && (f.left..=f.right).contains(&col))
    };
    for row in 0..rows {
        for col in 0..cols {
            let g = glyph(screen, row, col);
            let text = g == "#";
            if inside(row, col) && !text {
                return Some(format!("a blank inside a frame at row {row} col {col}"));
            }
            if !inside(row, col) && text {
                return Some(format!("text outside every frame at row {row} col {col}"));
            }
            let blank = g.trim().is_empty() || FRAME_GLYPHS.contains(g.as_str());
            if !on_a_frame(row, col) && !blank {
                return Some(format!("{g:?} outside every frame at row {row} col {col}"));
            }
        }
    }
    None
}

fn dump(screen: &vt100::Screen, cols: u16, rows: u16) -> String {
    (0..rows)
        .map(|row| {
            (0..cols)
                .map(|col| {
                    let g = glyph(screen, row, col);
                    if g.is_empty() {
                        " ".to_string()
                    } else {
                        g
                    }
                })
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Resizes to `cols`x`rows`, samples the screen until `watch` has passed,
/// and answers the longest stretch `check` found something wrong on it,
/// with the first screen of that stretch.
fn resize_and_watch(
    session: &mut PtySession,
    cols: u16,
    rows: u16,
    watch: Duration,
    check: fn(&vt100::Screen, u16, u16) -> Option<String>,
) -> (Duration, String) {
    session.resize(cols, rows).unwrap();
    let started = Instant::now();
    let mut run: Option<(Instant, String)> = None;
    let mut worst = (Duration::ZERO, String::new());
    while started.elapsed() < watch {
        std::thread::sleep(host_deadline(SAMPLE));
        let seen = session.with_screen(|screen| {
            check(screen, cols, rows).map(|why| (why, dump(screen, cols, rows)))
        });
        match (seen, &run) {
            (Some((why, screen)), None) => {
                run = Some((Instant::now(), format!("{why}\n{screen}")));
            }
            (Some(_), Some((since, first))) => {
                if since.elapsed() > worst.0 {
                    worst = (since.elapsed(), first.clone());
                }
            }
            (None, _) => run = None,
        }
    }
    worst
}

/// A vsplit under a slow `VimResized` handler, shrunk and grown back, with
/// the look `gaps` names.
fn resize_under(gaps: bool) {
    let look = if gaps { "gapped" } else { "gapless" };
    let redraw = host_deadline(REDRAW);
    // long enough that a disagreement lasting the whole handler is several
    // redraw bounds past the one this test allows
    let handler = (redraw * 4).max(Duration::from_millis(1500));
    let watch = handler + host_deadline(Duration::from_secs(2));

    let paths = common::ScratchPaths::new(&format!("tiles-resize-{look}"));
    let line = "#".repeat(400);
    let text: Vec<&str> = std::iter::repeat_n(line.as_str(), 80).collect();
    std::fs::write(&paths.scratch, text.join("\n") + "\n").unwrap();
    let mut cmd = portable_pty::CommandBuilder::new(common::view_bin_path());
    cmd.cwd(paths.scratch.parent().unwrap());
    cmd.args(["--panes", "tiles"]);
    cmd.arg(paths.scratch.file_name().unwrap());
    common::isolate_xdg_first_launch(&mut cmd, &paths.isolated_home);
    let config = common::xdg_home(&paths.isolated_home, "XDG_CONFIG_HOME").join("view/view.toml");
    std::fs::write(config, format!("[ui]\ngaps = {gaps}\n")).unwrap();
    let mut session = PtySession::spawn_configured(cmd, COLS, ROWS).unwrap();
    assert!(
        session.wait_for("####", BUDGET),
        "{look}: view never showed the file; screen:\n{}",
        session.screen()
    );

    session.send(b"\x1b:set nowrap | vsplit\r").unwrap();
    // equalizing on a resize is the common handler, and it moves the right
    // window's slot away from the separator and status row the old layout
    // left in the global grid
    session
        .send(
            format!(
                "\x1b:autocmd VimResized * wincmd = | sleep {}m\r",
                handler.as_millis()
            )
            .as_bytes(),
        )
        .unwrap();
    // the launch notices stand over a tile until dismissed
    let settled = |screen: &vt100::Screen| mismatch(screen, COLS, ROWS).is_none();
    let mut dismissed = 0;
    while !session.wait_for_screen(host_deadline(Duration::from_millis(500)), settled) {
        assert!(
            dismissed < 8,
            "{look}: the two tiles never settled before the resize; screen:\n{}",
            session.screen()
        );
        session.send(b"\x1b:View notifications dismiss\r").unwrap();
        dismissed += 1;
    }
    let before = session.with_screen(|screen| frames(screen, COLS, ROWS));

    for (cols, rows) in [SHRUNK, (COLS, ROWS)] {
        let (longest, first) = resize_and_watch(&mut session, cols, rows, watch, mismatch);
        assert!(
            longest <= redraw,
            "{look}: after the resize to {cols}x{rows} the tiles' text and \
             frames disagreed for {longest:?}, past one redraw ({redraw:?}); \
             the first screen of that stretch:\n{first}"
        );
        let now = session.with_screen(|screen| mismatch(screen, cols, rows));
        assert_eq!(
            now,
            None,
            "{look}: the tiles never settled at {cols}x{rows}; screen:\n{}",
            session.screen()
        );
    }
    let after = session.with_screen(|screen| frames(screen, COLS, ROWS));
    assert_eq!(
        after,
        before,
        "{look}: the frames came back somewhere other than where the grow \
         restores the layout; screen:\n{}",
        session.screen()
    );
}

/// Where two frames on screen touch or overlap, or `None` where a tile's
/// and the panel's both stand and every two side by side have a blank
/// column between them. A tile the panel covers whole is under it, which
/// is why the shrunk vsplit may show one tile.
fn apart(screen: &vt100::Screen, cols: u16, rows: u16) -> Option<String> {
    let frames = frames(screen, cols, rows);
    if frames.len() < 2 {
        return Some(format!("{} frames on screen", frames.len()));
    }
    for (index, a) in frames.iter().enumerate() {
        for b in &frames[index + 1..] {
            let rows_meet = a.top <= b.bottom && b.top <= a.bottom;
            let cols_meet = a.left <= b.right + 1 && b.left <= a.right + 1;
            if rows_meet && cols_meet {
                return Some(format!("{a:?} and {b:?} touch"));
            }
        }
    }
    None
}

/// The gapped vsplit with the agent panel docked on the right, through the
/// same shrink and grow: the tile the panel covers part of closes its own
/// frame one gap short of the panel's at every size. A gapless tile shares
/// that edge with the panel, which the paint tests read.
fn panel_beside_the_tiles() {
    let paths = common::ScratchPaths::new("tiles-resize-panel");
    let line = "#".repeat(400);
    let text: Vec<&str> = std::iter::repeat_n(line.as_str(), 80).collect();
    std::fs::write(&paths.scratch, text.join("\n") + "\n").unwrap();
    let mut cmd = portable_pty::CommandBuilder::new(common::view_bin_path());
    cmd.cwd(paths.scratch.parent().unwrap());
    cmd.args(["--panes", "tiles"]);
    cmd.arg(paths.scratch.file_name().unwrap());
    common::isolate_xdg_first_launch(&mut cmd, &paths.isolated_home);
    let config = common::xdg_home(&paths.isolated_home, "XDG_CONFIG_HOME").join("view/view.toml");
    let stub = common::built_bin("view-ai", "view-ai-stub-agent", &["test-support"]);
    // the stub's second argument only holds a turn nobody starts here
    let resume = paths.isolated_home.join("resume");
    std::fs::write(
        config,
        format!(
            "[ui]\ngaps = true\n\n[ai]\nagent = [{:?}, {:?}]\n",
            stub.to_string_lossy(),
            resume.to_string_lossy()
        ),
    )
    .unwrap();
    let mut session = PtySession::spawn_configured(cmd, COLS, ROWS).unwrap();
    assert!(
        session.wait_for("####", BUDGET),
        "panel: view never showed the file; screen:\n{}",
        session.screen()
    );
    session.send(b"\x1b:set nowrap | vsplit\r").unwrap();
    // a launch notice is a frame of its own standing over a tile
    let settled = |screen: &vt100::Screen| mismatch(screen, COLS, ROWS).is_none();
    let mut dismissed = 0;
    while !session.wait_for_screen(host_deadline(Duration::from_millis(500)), settled) {
        assert!(
            dismissed < 8,
            "panel: the two tiles never settled; screen:\n{}",
            session.screen()
        );
        session.send(b"\x1b:View notifications dismiss\r").unwrap();
        dismissed += 1;
    }
    session.send(b"\x1b:View ai open\r").unwrap();
    assert!(
        session.wait_for("Trust ", BUDGET),
        "panel: a fresh workspace raised no trust prompt; screen:\n{}",
        session.screen()
    );
    session.send(b"y").unwrap();
    assert!(
        session.wait_for_screen(BUDGET, |screen| {
            let contents = screen.contents();
            !contents.contains("Trust ") && contents.contains("view-ai-stub-agent")
        }),
        "panel: the panel never opened; screen:\n{}",
        session.screen()
    );
    let redraw = host_deadline(REDRAW);
    let watch = host_deadline(Duration::from_secs(2));
    for (cols, rows) in [(COLS, ROWS), SHRUNK, (COLS, ROWS)] {
        let (longest, first) = resize_and_watch(&mut session, cols, rows, watch, apart);
        assert!(
            longest <= redraw,
            "panel: at {cols}x{rows} two frames touched for {longest:?}, past \
             one redraw ({redraw:?}); the first screen of that stretch:\n{first}"
        );
        let now = session.with_screen(|screen| apart(screen, cols, rows));
        assert_eq!(
            now,
            None,
            "panel: at {cols}x{rows}; screen:\n{}",
            session.screen()
        );
    }
}

/// Where a tile beside the tree shows anything but its own buffer inside
/// its frame, or `None` where every frame right of the leftmost one holds
/// its buffer from the first column: `@` there and `#` in every other
/// cell. The leftmost frame is the tree's once it is open.
fn text_beside_the_tree(screen: &vt100::Screen, cols: u16, rows: u16) -> Option<String> {
    let frames = frames(screen, cols, rows);
    let tree = frames.iter().min_by_key(|frame| frame.left)?;
    let tiles: Vec<&Frame> = frames.iter().filter(|f| f.left > tree.right).collect();
    if tiles.is_empty() {
        return Some(format!("no tile beside {tree:?} among {frames:?}"));
    }
    for tile in tiles {
        for row in tile.top + 1..tile.bottom {
            for col in tile.left + 1..tile.right {
                let want = if col == tile.left + 1 { "@" } else { "#" };
                let g = glyph(screen, row, col);
                if g != want {
                    return Some(format!("{g:?} at row {row} col {col} inside {tile:?}"));
                }
            }
        }
    }
    None
}

/// The gapped vsplit with the tree docked on the left over part of the
/// left tile, through the same shrink and grow: that tile shows its
/// buffer from the first column, inside the frame it closes beside the
/// tree, at every size.
fn tree_beside_the_tiles() {
    let paths = common::ScratchPaths::new("tiles-resize-tree");
    let line = format!("@{}", "#".repeat(399));
    let text: Vec<&str> = std::iter::repeat_n(line.as_str(), 80).collect();
    std::fs::write(&paths.scratch, text.join("\n") + "\n").unwrap();
    let mut cmd = portable_pty::CommandBuilder::new(common::view_bin_path());
    cmd.cwd(paths.scratch.parent().unwrap());
    cmd.args(["--panes", "tiles"]);
    cmd.arg(paths.scratch.file_name().unwrap());
    common::isolate_xdg_first_launch(&mut cmd, &paths.isolated_home);
    let config = common::xdg_home(&paths.isolated_home, "XDG_CONFIG_HOME").join("view/view.toml");
    std::fs::write(config, "[ui]\ngaps = true\n").unwrap();
    let mut session = PtySession::spawn_configured(cmd, COLS, ROWS).unwrap();
    assert!(
        session.wait_for("@###", BUDGET),
        "tree: view never showed the file; screen:\n{}",
        session.screen()
    );
    session.send(b"\x1b:set nowrap | vsplit\r").unwrap();
    // a launch notice is a frame of its own standing over a tile
    let settled = |screen: &vt100::Screen| text_beside_the_tree(screen, COLS, ROWS).is_none();
    let mut dismissed = 0;
    while !session.wait_for_screen(host_deadline(Duration::from_millis(500)), settled) {
        assert!(
            dismissed < 8,
            "tree: the two tiles never settled; screen:\n{}",
            session.screen()
        );
        session.send(b"\x1b:View notifications dismiss\r").unwrap();
        dismissed += 1;
    }
    session.send(b"\x1b:View tree\r").unwrap();
    assert!(
        session.wait_for_screen(BUDGET, |screen| {
            frames(screen, COLS, ROWS).len() >= 3 && settled(screen)
        }),
        "tree: the tile beside the tree never showed its first column; {:?}; \
         screen:\n{}",
        session.with_screen(|screen| text_beside_the_tree(screen, COLS, ROWS)),
        session.screen()
    );
    let redraw = host_deadline(REDRAW);
    let watch = host_deadline(Duration::from_secs(2));
    for (cols, rows) in [SHRUNK, (COLS, ROWS)] {
        let (longest, first) =
            resize_and_watch(&mut session, cols, rows, watch, text_beside_the_tree);
        assert!(
            longest <= redraw,
            "tree: at {cols}x{rows} a tile beside the tree showed something \
             other than its text for {longest:?}, past one redraw ({redraw:?}); \
             the first screen of that stretch:\n{first}"
        );
        let now = session.with_screen(|screen| text_beside_the_tree(screen, cols, rows));
        assert_eq!(
            now,
            None,
            "tree: at {cols}x{rows}; screen:\n{}",
            session.screen()
        );
    }
}

/// Both looks in one session after another, then the panel and the tree
/// beside the tiles: two live sessions sampled at once would each slow the
/// redraw the other is timing.
#[test]
fn a_tile_frame_stays_on_its_text_while_nvim_redraws_a_resize() {
    resize_under(true);
    resize_under(false);
    panel_beside_the_tiles();
    tree_beside_the_tiles();
}
