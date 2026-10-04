//! A terminal resize under tiles, read off the screen view paints while
//! nvim is still between the two halves of it.
//!
//! nvim announces the new window slots (`win_pos`) as soon as the screen is
//! resized, runs the config's `VimResized` handlers, and only then sizes and
//! redraws the window grids. A config whose handlers take time leaves view
//! holding new slots and old grids for that long, which is what a person
//! sees after dragging a terminal wider under a plugin-heavy config. The
//! handler here is a sleep, or a wait on a key the test sends, and every
//! frame view paints for the new size is replayed and judged.
#![cfg(unix)]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use std::time::Duration;

use view_oracle::{PtySession, QueryPolicy};
use view_test_support::host_deadline;

const COLS: u16 = 220;
const ROWS: u16 = 50;
const SHRUNK: (u16, u16) = (150, 38);
const BUDGET: Duration = Duration::from_secs(20);

/// How long the slow `VimResized` handler a plugin-heavy config runs takes.
const HANDLER: Duration = Duration::from_millis(1500);

/// The close of the synchronized-output bracket. view writes it at the end
/// of every frame that writes anything once the terminal admits the
/// bracket, and a terminal applies each bracketed frame whole, so the
/// sessions here answer the probe for it.
const FRAME_END: &[u8] = b"\x1b[?2026l";

/// The terminal answers every view session here spawns with.
const POLICY: QueryPolicy = QueryPolicy::AnswerFullTier;

/// The cursor position the status row shows once each `VimResized` handler
/// has moved the cursor a column right on the first line as its last act,
/// so the frame that carries it is the last one the resize paints. The
/// first line's first columns stay on screen at every size, so the move
/// scrolls nothing.
fn resized_mark(count: u32) -> impl Fn(&vt100::Screen) -> bool {
    let mark = mark_text(count);
    move |screen| screen.contents().contains(&mark)
}

/// The status row's text [`resized_mark`] looks for.
fn mark_text(count: u32) -> String {
    format!(" 1:{} ", count + 1)
}

/// The command that counts resizes into [`resized_mark`], after `first`.
fn mark_resizes(first: &str) -> String {
    format!(
        "\x1b:let g:resizes = 0 | autocmd VimResized * {first}let g:resizes += 1 \
         | call cursor(1, g:resizes + 1)\r"
    )
}

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

/// Whether a whole frame reaches the right edge of a `cols`-wide screen,
/// past the gap column a gapped look leaves there: a layout painted for
/// another width shows none, the old one cut at the edge or short of it.
fn spans(screen: &vt100::Screen, cols: u16, rows: u16) -> bool {
    frames(screen, cols, rows)
        .iter()
        .any(|frame| frame.right + 2 >= cols)
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

/// What a screen check answers: `None` where the screen is right, else why
/// it is not.
type Check = fn(&vt100::Screen, u16, u16) -> Option<String>;

/// Whether a screen shows the layout nvim moved its windows to for the
/// size, which no frame painted for the old layout does.
type Layout = fn(&vt100::Screen, u16, u16) -> bool;

/// One screen a painted frame left.
#[derive(Debug, Clone)]
struct Painted {
    /// The byte the frame ended at.
    at: usize,
    screen: String,
    /// What the check found wrong on the screen.
    why: Option<String>,
    /// What the [`Layout`] read off the screen.
    new_layout: bool,
}

/// Blocks until the recording ends where a painted frame ends, so a replay
/// cut there holds no half of one.
fn settle_on_a_frame_boundary(session: &mut PtySession) {
    while !session.raw_output().ends_with(FRAME_END) {
        let mut absorbed = false;
        assert!(
            session.wait_for_screen(BUDGET, |_| std::mem::replace(&mut absorbed, true)),
            "the output stopped partway through a frame; screen:\n{}",
            session.screen()
        );
    }
}

/// What is wrong with the frames a resize painted, or `None` where the last
/// one agrees and every one before it that disagrees was painted for the
/// old layout, before nvim moved its windows. A frame that shows the new
/// slots shows them over the grids sized for them, however many frames
/// view paints before nvim answers.
fn judge(screens: &[Painted]) -> Option<String> {
    let Some(last) = screens.last() else {
        return Some("no frame was painted".to_string());
    };
    if let Some(why) = &last.why {
        return Some(format!(
            "the last of {} frames (ending at byte {}): {why}; the screen \
             it left:\n{}",
            screens.len(),
            last.at,
            last.screen
        ));
    }
    let wrong: Vec<(usize, &Painted)> = screens
        .iter()
        .enumerate()
        .filter(|(_, painted)| painted.why.is_some() && painted.new_layout)
        .collect();
    if wrong.is_empty() {
        return None;
    }
    let shown: Vec<String> = wrong
        .iter()
        .map(|(index, painted)| {
            format!(
                "frame {index} (ending at byte {}): {}; the screen it left:\n{}",
                painted.at,
                painted.why.as_deref().unwrap_or_default(),
                painted.screen
            )
        })
        .collect();
    Some(format!(
        "{} of {} frames show the new layout and disagree\n{}",
        wrong.len(),
        screens.len(),
        shown.join("\n")
    ))
}

/// The erase view writes at the head of the first frame it paints for a new
/// size.
const CLEAR: &[u8] = b"\x1b[2J";

/// Blocks until the recording past byte `from` holds a whole frame painted
/// for a new size.
fn wait_for_a_frame_at_the_new_size(session: &mut PtySession, from: usize) {
    let painted = |raw: &[u8]| {
        raw.windows(CLEAR.len())
            .position(|w| w == CLEAR)
            .is_some_and(|at| raw[at..].windows(FRAME_END.len()).any(|w| w == FRAME_END))
    };
    while !painted(&session.raw_output()[from..]) {
        let mut absorbed = false;
        assert!(
            session.wait_for_screen(BUDGET, |_| std::mem::replace(&mut absorbed, true)),
            "view painted no frame for the new size; screen:\n{}",
            session.screen()
        );
    }
}

/// Resizes to `cols`x`rows`, runs `between` with the recording's length at
/// the resize, and answers every frame view painted for the new size until
/// the screen passes both `done` and `check`, each replayed through `term`
/// at the size it was painted for and read by `check` and `layout`.
/// `replayed` is how far into the recording `term` has been brought, and
/// moves to its end.
///
/// The first frame view paints for a new size clears the screen. A frame
/// already on its way when the resize lands was painted for the old size,
/// so everything before that clear is replayed at the old size and judged
/// by nothing.
fn resize_and_replay(
    session: &mut PtySession,
    term: &mut vt100::Parser,
    replayed: &mut usize,
    (cols, rows): (u16, u16),
    between: impl FnOnce(&mut PtySession, usize),
    done: impl Fn(&vt100::Screen) -> bool,
    (check, layout): (Check, Layout),
) -> Vec<Painted> {
    settle_on_a_frame_boundary(session);
    let from = session.raw_output().len();
    term.process(&session.raw_output()[*replayed..from]);
    session.resize(cols, rows).unwrap();
    between(session, from);
    assert!(
        session.wait_for_screen(BUDGET, |screen| {
            done(screen) && check(screen, cols, rows).is_none()
        }),
        "at {cols}x{rows} the resize never settled; {:?}; screen:\n{}",
        session.with_screen(|screen| check(screen, cols, rows)),
        session.screen()
    );
    settle_on_a_frame_boundary(session);
    *replayed = session.raw_output().len();
    let raw = session.raw_output()[from..*replayed].to_vec();
    let cleared = raw
        .windows(CLEAR.len())
        .position(|w| w == CLEAR)
        .unwrap_or_else(|| panic!("view never cleared the screen for {cols}x{rows}"));
    term.process(&raw[..cleared]);
    term.screen_mut().set_size(rows, cols);
    painted_screens(term, &raw[cleared..], check, layout)
}

/// A vsplit under a slow `VimResized` handler, shrunk and grown back, with
/// the look `gaps` names.
fn resize_under(gaps: bool) {
    let look = if gaps { "gapped" } else { "gapless" };
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
    let mut session = PtySession::spawn_configured_with(cmd, COLS, ROWS, POLICY).unwrap();
    session.record_raw_output_up_to(64 << 20);
    assert!(
        session.wait_for("####", BUDGET),
        "{look}: view never showed the file; screen:\n{}",
        session.screen()
    );

    session.send(b"\x1b:set nowrap | vsplit\r").unwrap();
    // equalizing on a resize is the common handler, and it moves the right
    // window's slot away from the separator and status row the old layout
    // left in the global grid
    let handler = format!("wincmd = | sleep {}m | ", HANDLER.as_millis());
    session.send(mark_resizes(&handler).as_bytes()).unwrap();
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

    let mut term = vt100::Parser::new(ROWS, COLS, 0);
    let mut replayed = 0;
    for (count, size) in (1..).zip([SHRUNK, (COLS, ROWS)]) {
        let screens = resize_and_replay(
            &mut session,
            &mut term,
            &mut replayed,
            size,
            |_, _| {},
            resized_mark(count),
            (mismatch, spans),
        );
        if let Some(why) = judge(&screens) {
            panic!("{look}: after the resize to {size:?} the tiles' text and frames: {why}");
        }
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

/// The digit at column `index` of the wrapped line: it steps by one every
/// column and once more every ten, so a line wrapped at any width short of
/// a hundred columns off the frame's shows a different digit where its
/// second screen row starts.
fn wrapped_digit(index: u16) -> String {
    ((index % 10 + index / 10) % 10).to_string()
}

/// Whether a whole tile closes its frame one gap short of the panel's, the
/// rightmost frame. The panel is view's own and stands at the new size from
/// the first frame view paints for it, so it tells nothing about where nvim
/// has put the tiles.
fn beside_the_panel(screen: &vt100::Screen, cols: u16, rows: u16) -> bool {
    let frames = frames(screen, cols, rows);
    let Some(panel) = frames.iter().max_by_key(|frame| frame.left) else {
        return false;
    };
    frames
        .iter()
        .any(|tile| tile.right < panel.left && tile.right + 2 >= panel.left)
}

/// Where a tile left of the panel shows the first line anywhere but from
/// its first column to its frame, wrapped there onto the row under it, or
/// `None` where every such tile does. The rightmost frame is the panel's.
fn wraps_beside_the_panel(screen: &vt100::Screen, cols: u16, rows: u16) -> Option<String> {
    let frames = frames(screen, cols, rows);
    let panel = frames.iter().max_by_key(|frame| frame.left)?;
    let tiles: Vec<&Frame> = frames.iter().filter(|f| f.right <= panel.left).collect();
    if tiles.is_empty() {
        return Some(format!("no tile beside {panel:?} among {frames:?}"));
    }
    for tile in tiles {
        let width = tile.right - tile.left - 1;
        for (row, first) in [(tile.top + 1, 0), (tile.top + 2, width)] {
            for col in tile.left + 1..tile.right {
                let want = wrapped_digit(first + col - tile.left - 1);
                let g = glyph(screen, row, col);
                if g != want {
                    return Some(format!(
                        "{g:?} at row {row} col {col} inside {tile:?}, where {want} wraps"
                    ));
                }
            }
        }
    }
    None
}

/// The vsplit with the agent panel docked on the right, through the same
/// shrink and grow. The tile the panel covers part of wraps its first line
/// at its own frame at every size, and a gapped tile closes its frame one
/// gap short of the panel's. A gapless tile shares that edge with the
/// panel, which the paint tests read.
fn panel_beside_the_tiles(gaps: bool) {
    let look = if gaps { "gapped" } else { "gapless" };
    let paths = common::ScratchPaths::new(&format!("tiles-resize-panel-{look}"));
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
            "[ui]\ngaps = {gaps}\n\n[ai]\nagent = [{:?}, {:?}]\n",
            stub.to_string_lossy(),
            resume.to_string_lossy()
        ),
    )
    .unwrap();
    let mut session = PtySession::spawn_configured_with(cmd, COLS, ROWS, POLICY).unwrap();
    session.record_raw_output_up_to(64 << 20);
    assert!(
        session.wait_for("####", BUDGET),
        "{look} panel: view never showed the file; screen:\n{}",
        session.screen()
    );
    session.send(b"\x1b:set nowrap | vsplit\r").unwrap();
    // a launch notice is a frame of its own standing over a tile
    let settled = |screen: &vt100::Screen| mismatch(screen, COLS, ROWS).is_none();
    let mut dismissed = 0;
    while !session.wait_for_screen(host_deadline(Duration::from_millis(500)), settled) {
        assert!(
            dismissed < 8,
            "{look} panel: the two tiles never settled; screen:\n{}",
            session.screen()
        );
        session.send(b"\x1b:View notifications dismiss\r").unwrap();
        dismissed += 1;
    }
    session.send(b"\x1b:View ai open\r").unwrap();
    assert!(
        session.wait_for("Trust ", BUDGET),
        "{look} panel: a fresh workspace raised no trust prompt; screen:\n{}",
        session.screen()
    );
    session.send(b"y").unwrap();
    assert!(
        session.wait_for_screen(BUDGET, |screen| {
            let contents = screen.contents();
            !contents.contains("Trust ") && contents.contains("view-ai-stub-agent")
        }),
        "{look} panel: the panel never opened; screen:\n{}",
        session.screen()
    );
    let digits: String = (0..400).map(wrapped_digit).collect();
    session
        .send(format!("\x1b:call setline(1, '{digits}') | windo set wrap\r").as_bytes())
        .unwrap();
    // `windo` leaves the cursor in the right tile, which the panel covers
    // whole at the shrunk size, status row and mark with it
    session.send(b"\x1b:1wincmd w\r").unwrap();
    session.send(mark_resizes("").as_bytes()).unwrap();
    let mut term = vt100::Parser::new(ROWS, COLS, 0);
    let mut replayed = 0;
    for (count, (cols, rows)) in (0..).zip([(COLS, ROWS), SHRUNK, (COLS, ROWS)]) {
        let marked = resized_mark(count);
        let wrapped = |screen: &vt100::Screen| {
            spans(screen, cols, rows)
                && wraps_beside_the_panel(screen, cols, rows).is_none()
                && marked(screen)
        };
        // the first size is the one the session opened at
        if count > 0 && gaps {
            let screens = resize_and_replay(
                &mut session,
                &mut term,
                &mut replayed,
                (cols, rows),
                |_, _| {},
                wrapped,
                (apart, beside_the_panel),
            );
            if let Some(why) = judge(&screens) {
                panic!("panel: at {cols}x{rows} two frames touched: {why}");
            }
        } else if count > 0 {
            session.resize(cols, rows).unwrap();
        }
        assert!(
            session.wait_for_screen(BUDGET, wrapped),
            "{look} panel: at {cols}x{rows} the tile beside the panel never \
             wrapped its first line at its frame under the handler's mark; \
             {:?}; screen:\n{}",
            session.with_screen(|screen| wraps_beside_the_panel(screen, cols, rows)),
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
    let mut session = PtySession::spawn_configured_with(cmd, COLS, ROWS, POLICY).unwrap();
    session.record_raw_output_up_to(64 << 20);
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
    let mut term = vt100::Parser::new(ROWS, COLS, 0);
    let mut replayed = 0;
    for (cols, rows) in [SHRUNK, (COLS, ROWS)] {
        let screens = resize_and_replay(
            &mut session,
            &mut term,
            &mut replayed,
            (cols, rows),
            |_, _| {},
            |screen| spans(screen, cols, rows),
            (text_beside_the_tree, spans),
        );
        if let Some(why) = judge(&screens) {
            panic!("tree: at {cols}x{rows} a tile beside the tree: {why}");
        }
    }
}

/// Every frame on screen as [`frames`] finds it, and a frame the screen's
/// edge cuts off, which a layout nvim has yet to move to a shrunk terminal
/// leaves for a frame or two, reaching that edge.
fn frames_or_cut(screen: &vt100::Screen, cols: u16, rows: u16) -> Vec<Frame> {
    let is = |row: u16, col: u16, set: &str| {
        let g = glyph(screen, row, col);
        !g.is_empty() && set.contains(g.as_str())
    };
    let mut found = Vec::new();
    for top in 0..rows {
        for left in 0..cols {
            if !is(top, left, "╭┬") || !is(top + 1, left, "│├┤┼") {
                continue;
            }
            let right = (left + 1..cols)
                .find(|&c| is(top, c, "╮┬"))
                .unwrap_or(cols - 1);
            let bottom = (top + 1..rows)
                .find(|&r| is(r, left, "╰┴"))
                .unwrap_or(rows - 1);
            found.push(Frame {
                top,
                left,
                bottom,
                right,
            });
        }
    }
    found
}

/// Where a digit of the wrapped text stands off every frame, edges
/// included, or `None` where each one is on a tile. A status row carries
/// its own digits on a frame's bottom edge.
fn strays(screen: &vt100::Screen, cols: u16, rows: u16) -> Option<String> {
    let frames = frames_or_cut(screen, cols, rows);
    if frames.is_empty() {
        return Some("no frame on screen".to_string());
    }
    for row in 0..rows {
        for col in 0..cols {
            let g = glyph(screen, row, col);
            if !g.chars().all(|ch| ch.is_ascii_digit()) || g.is_empty() {
                continue;
            }
            let on_a_frame = frames
                .iter()
                .any(|f| (f.top..=f.bottom).contains(&row) && (f.left..=f.right).contains(&col));
            if !on_a_frame {
                return Some(format!("{g:?} off every frame at row {row} col {col}"));
            }
        }
    }
    None
}

/// Every screen a painted frame in `raw` leaves `term` showing, with what
/// `check` finds wrong on it and whether `layout` reads the new layout
/// there, once `term` has taken the size the frames were painted for.
fn painted_screens(
    term: &mut vt100::Parser,
    raw: &[u8],
    check: Check,
    layout: Layout,
) -> Vec<Painted> {
    let (rows, cols) = term.screen().size();
    let mut screens = Vec::new();
    let mut from = 0;
    while let Some(at) = raw[from..]
        .windows(FRAME_END.len())
        .position(|w| w == FRAME_END)
    {
        let end = from + at + FRAME_END.len();
        term.process(&raw[from..end]);
        let screen = term.screen();
        screens.push(Painted {
            at: end,
            screen: dump(screen, cols, rows),
            why: check(screen, cols, rows),
            new_layout: layout(screen, cols, rows),
        });
        from = end;
    }
    term.process(&raw[from..]);
    screens
}

/// Blocks until the `VimResized` handler that writes into the named pipe
/// `held` has written `count - 1` there, which it does as it starts to wait
/// on a key.
fn wait_until_held(held: &std::path::Path, count: u32) {
    use std::os::unix::fs::OpenOptionsExt;
    let (tx, rx) = std::sync::mpsc::channel();
    let reading = held.to_path_buf();
    std::thread::spawn(move || {
        let _ = tx.send(std::fs::read_to_string(&reading).map_err(|e| e.kind()));
    });
    let read = rx.recv_timeout(host_deadline(BUDGET));
    if read.is_err() {
        // a writer of the test's own completes the reader thread's open, and
        // dropping it hands that read an end of file, so no thread outlives
        // the test
        let _ = std::fs::OpenOptions::new()
            .write(true)
            .custom_flags(nix::fcntl::OFlag::O_NONBLOCK.bits())
            .open(held);
    }
    assert_eq!(
        read,
        Ok(Ok(format!("{}\n", count - 1))),
        "the handler never wrote that it waits on a key for resize {count}"
    );
}

/// A vsplit whose windows wrap one long line of digits, shrunk and grown
/// back under a slow `VimResized` handler, with the look `gaps` names: in
/// every frame view paints, no digit stands off a tile's frame, the rows a
/// shrinking tile vacates included.
fn wrapped_text_stays_inside_its_frame(gaps: bool) {
    let look = if gaps { "gapped" } else { "gapless" };
    let paths = common::ScratchPaths::new(&format!("tiles-resize-wrap-{look}"));
    let digits: String = (0..400).map(wrapped_digit).collect();
    let text: Vec<&str> = std::iter::repeat_n(digits.as_str(), 80).collect();
    std::fs::write(&paths.scratch, text.join("\n") + "\n").unwrap();
    let mut cmd = portable_pty::CommandBuilder::new(common::view_bin_path());
    cmd.cwd(paths.scratch.parent().unwrap());
    cmd.args(["--panes", "tiles"]);
    cmd.arg(paths.scratch.file_name().unwrap());
    common::isolate_xdg_first_launch(&mut cmd, &paths.isolated_home);
    let config = common::xdg_home(&paths.isolated_home, "XDG_CONFIG_HOME").join("view/view.toml");
    std::fs::write(config, format!("[ui]\ngaps = {gaps}\n")).unwrap();
    let mut session = PtySession::spawn_configured_with(cmd, COLS, ROWS, POLICY).unwrap();
    session.record_raw_output_up_to(64 << 20);
    assert!(
        session.wait_for("01234567", BUDGET),
        "{look} wrap: view never showed the file; screen:\n{}",
        session.screen()
    );
    session.send(b"\x1b:set wrap | vsplit\r").unwrap();
    // the handler holds nvim short of the grid resize until the test has
    // seen view paint the gap, so that frame is one of its own however long
    // the host keeps view off the CPU; it says it holds through a pipe the
    // test reads, because a key sent before nvim enters it is typeahead
    // nvim reads as a normal-mode command ahead of the queued resize
    let held = paths.isolated_home.join("held");
    nix::unistd::mkfifo(&held, nix::sys::stat::Mode::S_IRWXU).unwrap();
    let handler = format!(
        "wincmd = | call writefile([g:resizes], '{}') | call getchar() | ",
        held.display()
    );
    session.send(mark_resizes(&handler).as_bytes()).unwrap();
    // a launch notice and the command line are frames of their own; the
    // split has landed once two frames stand side by side
    let settled = |screen: &vt100::Screen| {
        let frames = frames(screen, COLS, ROWS);
        matches!(frames.as_slice(), [a, b] if a.top == b.top && a.bottom == b.bottom)
            && strays(screen, COLS, ROWS).is_none()
    };
    let mut dismissed = 0;
    while !session.wait_for_screen(host_deadline(Duration::from_millis(500)), settled) {
        assert!(
            dismissed < 8,
            "{look} wrap: the two tiles never settled before the resize; screen:\n{}",
            session.screen()
        );
        session.send(b"\x1b:View notifications dismiss\r").unwrap();
        dismissed += 1;
    }
    // the replay starts at the session's first byte, so a cell no frame
    // after the resize repaints keeps what it showed before it
    let mut term = vt100::Parser::new(ROWS, COLS, 0);
    let mut replayed = 0;
    for (count, size) in (1..).zip([SHRUNK, (COLS, ROWS)]) {
        let screens = resize_and_replay(
            &mut session,
            &mut term,
            &mut replayed,
            size,
            |session, from| {
                wait_for_a_frame_at_the_new_size(session, from);
                wait_until_held(&held, count);
                // nvim moves the slots ahead of the handler on a shrink and
                // leaves a grow's for the redraw after it
                let (cols, rows) = size;
                assert!(
                    cols == COLS
                        || session.wait_for_screen(BUDGET, |screen| spans(screen, cols, rows)),
                    "{look} wrap: at {size:?} view never painted the slots \
                     moved; screen:\n{}",
                    session.screen()
                );
                // the key the handler's getchar() waits for
                session.send(b"x").unwrap();
            },
            resized_mark(count),
            (strays, spans),
        );
        let mark = mark_text(count);
        assert!(
            screens
                .iter()
                .position(|painted| painted.screen.contains(&mark))
                .is_some_and(|marked| marked > 0),
            "{look} wrap: at {size:?} view painted no frame before the \
             handler's mark; screen:\n{}",
            session.screen()
        );
        for (index, painted) in screens.iter().enumerate() {
            assert!(
                painted.why.is_none(),
                "{look} wrap: at {size:?}, frame {index} of {} (ending at \
                 byte {}): {}; the screen it left:\n{}",
                screens.len(),
                painted.at,
                painted.why.as_deref().unwrap_or_default(),
                painted.screen
            );
        }
    }
}

/// Every frame a child paints on a 10x3 screen from `rows`, three rows a
/// frame, each in the bracket view writes, read back and checked by
/// [`mismatch`].
fn replayed_frames(rows: &[[&str; 3]]) -> Vec<Painted> {
    let printed: String = rows
        .iter()
        .map(|[a, b, c]| {
            format!("\\033[?2026h\\033[H\\033[2J{a}\\033[2;1H{b}\\033[3;1H{c}\\033[?2026l")
        })
        .collect();
    let script = format!("stty -echo; printf '{printed}'; read _");
    let mut session = PtySession::spawn("/bin/sh", &["-c", &script], 10, 3).unwrap();
    session.record_raw_output();
    let last = rows.last().unwrap();
    assert!(
        session.wait_for_screen(BUDGET, |screen| screen.contents().contains(last[1])),
        "the last frame never showed; screen:\n{}",
        session.screen()
    );
    settle_on_a_frame_boundary(&mut session);
    let mut term = vt100::Parser::new(3, 10, 0);
    let raw = session.raw_output().to_vec();
    let screens = painted_screens(&mut term, &raw, mismatch, spans);
    assert_eq!(screens.len(), rows.len(), "{screens:?}");
    screens
}

const CUT: [&str; 3] = ["╭───╮╭───", "│###││###", "╰───╯╰───"];
const AGREEING: [&str; 3] = ["╭───╮╭──╮", "│###││##│", "╰───╯╰──╯"];

/// Frames painted for the old layout, cut at the shrunk screen's edge, pass
/// however many come before the frame that agrees.
#[test]
fn frames_painted_for_the_old_layout_pass() {
    let screens = replayed_frames(&[CUT, CUT, AGREEING]);
    assert!(screens[0].why.is_some(), "the cut frame read as agreeing");
    assert!(
        !screens[0].new_layout,
        "the cut frame read as the new layout"
    );
    assert_eq!(judge(&screens), None);
}

/// A frame that shows the new slots over grids nvim has not sized for them
/// fails, even as the only one that disagrees.
#[test]
fn a_frame_of_new_slots_over_old_grids_fails() {
    let held = ["╭───╮╭──╮", "│###││# │", "╰───╯╰──╯"];
    let screens = replayed_frames(&[held, AGREEING]);
    assert!(
        screens[0].new_layout,
        "the held frame read as the old layout"
    );
    assert!(judge(&screens).is_some(), "new slots over old grids passed");
}

/// Both looks in one session after another, then the panel and the tree
/// beside the tiles, one live session at a time.
#[test]
fn a_tile_frame_stays_on_its_text_while_nvim_redraws_a_resize() {
    wrapped_text_stays_inside_its_frame(true);
    wrapped_text_stays_inside_its_frame(false);
    resize_under(true);
    resize_under(false);
    panel_beside_the_tiles(true);
    panel_beside_the_tiles(false);
    tree_beside_the_tiles();
}
