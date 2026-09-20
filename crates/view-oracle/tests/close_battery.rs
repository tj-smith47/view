//! The scenario a user photographed -- duplicate status bars after a
//! vsplit, stray letters scattered as each window closes -- replayed
//! against the pinned nvim at that user's own geometry, and held to two
//! things at once: view's screen is nvim's on every content row, and a
//! terminal that draws box drawing, nerd-font icons and regional
//! indicators two columns wide ends every step showing exactly what a
//! narrow one shows.
//!
//! The second half is what the first cannot see. A `vt100` screen models a
//! terminal that draws every glyph at its unicode width, which is the
//! assumption the painter makes; the residue the user saw only exists on a
//! terminal that disagrees, so proving it gone means replaying the same
//! bytes through a model that widens ([`view_test_support::WideTerm`]) and
//! comparing the two screens.
//!
//! view runs with the `[native]` defaults a user gets rather than the
//! all-off baseline the other parity legs use: the chrome is what draws
//! the box glyphs and icons this is about.
#![cfg(unix)]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use view_oracle::PtySession;
use view_test_support::{widening_residue, WideTerm, Widening};

/// The user's own terminal size, which is where the residue was seen: a
/// smaller grid fits fewer of the widening glyphs per row and repaints more
/// of them per frame.
const COLS: u16 = 263;
const ROWS: u16 = 88;

/// The rows below the content, which view paints itself (its statusline and
/// command line) and nvim paints its own way. Everything above them is
/// buffer text both editors must agree on cell for cell.
const CHROME_ROWS: u16 = 2;

/// What one settle may take on an idle host, before the load this run
/// started under widens it. A cold `view` spawn plus an nvim spawn is the
/// slow part; a healthy session satisfies every wait here in milliseconds.
///
/// Scaled by [`view_test_support::host_deadline`] rather than by the
/// crate's own `startup_budget`, whose host share is what a three-second
/// flat startup bound has left over: nothing at all, at this size.
const BUDGET: Duration = Duration::from_secs(20);

/// How often the two screens are re-read while waiting for them to agree.
const POLL: Duration = Duration::from_millis(25);

/// Room for the whole battery's raw output. Six full-screen frames at this
/// geometry run past the recorder's default bound, and a stream truncated
/// mid-frame replays as rows the last complete frame happened to leave.
const RAW_LIMIT: usize = 4 * 1024 * 1024;

/// The README's first line, which is how a session says it has opened the
/// buffer rather than the directory.
const FIRST_LINE: &str = "# close battery fixture";

/// The committed nvim configuration both sessions source: a highlight set
/// whose groups all carry backgrounds of their own, so the colour
/// comparison discriminates.
const FIXTURE_CONFIG: &str = "close-battery";

/// One line of each class of glyph a terminal may draw wider than the
/// painter assumes, carried in the buffer text so every step has them on
/// screen.
const WIDENING_LINES: [&str; 6] = [
    "box drawing: \u{256d}\u{2500}\u{252c}\u{2500}\u{256e} \u{2502} \u{2570}\u{2500}\u{2534}\u{2500}\u{256f}",
    "geometric: \u{2605} \u{25b6} \u{25a0} \u{25c6} \u{2588}\u{2592}\u{2591}",
    "east asian: \u{6f22}\u{5b57}\u{30c6}\u{30ad}\u{30b9}\u{30c8}",
    "regional and variation: \u{1f1ef}\u{1f1f5} \u{2615}\u{fe0f}",
    "private use: \u{e0b0} \u{f0219} separators",
    "text presentation: \u{1f5a5} \u{2328} \u{23f8} \u{1f321}",
];

/// One step of the battery: its name, the command line typed at both
/// sessions, and the layout each screen must show before they are compared.
type Step = (&'static str, &'static str, fn(&vt100::Screen) -> bool);

/// The content row the window layout is read off. Far enough down that
/// every window of the battery covers it, so a vertical separator there is
/// a column boundary rather than a border of something else.
const SPLIT_ROW: u16 = 10;

/// The battery, in order: the name of each step, the command line typed at
/// both sessions to reach it, and what each session's screen must show
/// before the two are compared. The first types nothing -- both editors are
/// spawned on the fixture directory.
///
/// The reached-condition is what separates "the screens agree" from "the
/// keys have landed": two editors that have not yet reacted to `:q` agree
/// with each other perfectly.
const STEPS: [Step; 9] = [
    ("dir", "", lists_the_fixture),
    ("file", ":e README.md\r", shows_the_buffer),
    ("vsplit", ":vsplit\r", is_split_in_columns),
    ("split", ":split\r", is_split_in_rows),
    ("close1", ":q\r", is_split_in_columns_only),
    ("close2", ":q\r", is_one_window),
    ("edit", "2G0f\u{256d}rx\x1b", edited_the_box_run),
    ("panel", PANEL_KEYS, is_panel_beside_a_window),
    ("panel-close", ":q\r", is_one_window),
];

/// The user's own shape: a narrow left window in a background of its own,
/// the cursor in the wide right one.
///
/// Typed rather than scripted so the pinned nvim reaches it the same way
/// view does. Only `NormalNC` is remapped, which is what a file-tree panel
/// does and what makes the step after this one discriminating: the panel is
/// dark while the cursor is across from it, and takes the ordinary
/// background -- which names no colour of its own -- the moment `:q` leaves
/// it as the only window. The column it grows over is the one nvim painted
/// its separator into, in the dark colour the panel just stopped wearing.
const PANEL_KEYS: &str = concat!(
    ":vsplit\r",
    ":wincmd h\r",
    ":vertical resize 30\r",
    ":setlocal winhighlight=NormalNC:PanelNormal\r",
    ":wincmd l\r",
);

/// The width the panel window is sized to, and so the screen column the
/// separator between it and its neighbour falls in.
const PANEL_WIDTH: u16 = 30;

/// The directory listing has reached the screen.
fn lists_the_fixture(screen: &vt100::Screen) -> bool {
    screen.contents().contains("README.md")
}

/// The buffer's own first line has reached the screen.
fn shows_the_buffer(screen: &vt100::Screen) -> bool {
    screen.contents().contains(FIRST_LINE)
}

/// How many vertical window separators cross [`SPLIT_ROW`].
fn column_separators(screen: &vt100::Screen) -> usize {
    (0..COLS)
        .filter(|col| {
            screen
                .cell(SPLIT_ROW, *col)
                .is_some_and(|cell| cell.contents() == "\u{2502}")
        })
        .count()
}

/// How many content rows carry the buffer's name, which for both editors is
/// one per horizontally split window: the single-window and side-by-side
/// layouts put every status row in the rows below the content.
fn status_rows(screen: &vt100::Screen) -> usize {
    (0..ROWS - CHROME_ROWS)
        .filter(|row| {
            (0..COLS)
                .filter_map(|col| screen.cell(*row, col).map(|cell| cell.contents()))
                .collect::<String>()
                .contains("README.md")
        })
        .count()
}

fn is_split_in_columns(screen: &vt100::Screen) -> bool {
    column_separators(screen) >= 1
}

fn is_split_in_rows(screen: &vt100::Screen) -> bool {
    column_separators(screen) >= 1 && status_rows(screen) >= 1
}

fn is_split_in_columns_only(screen: &vt100::Screen) -> bool {
    column_separators(screen) >= 1 && status_rows(screen) == 0
}

fn is_one_window(screen: &vt100::Screen) -> bool {
    column_separators(screen) == 0
}

/// The panel is sized and the window beside it is the wide one: exactly one
/// column boundary, and it is the panel's own right edge.
fn is_panel_beside_a_window(screen: &vt100::Screen) -> bool {
    separator_columns(screen) == vec![PANEL_WIDTH]
}

/// Which screen columns carry a vertical window separator on
/// [`SPLIT_ROW`].
fn separator_columns(screen: &vt100::Screen) -> Vec<u16> {
    (0..COLS)
        .filter(|col| {
            screen
                .cell(SPLIT_ROW, *col)
                .is_some_and(|cell| cell.contents() == "\u{2502}")
        })
        .collect()
}

/// The head of the box-drawing run has been overwritten.
///
/// The one shape the other steps hide: a single changed cell at the head of
/// a run of glyphs a terminal draws two columns wide, where every other step
/// repaints whole regions and so satisfies a painter that follows the run
/// no further than its first column. This is the step that reads how far
/// the painter follows it: stopping short leaves the run's next glyph under
/// the head's own second half, which [`widening_residue`] names
/// (`(15, 1, "\u{252c}", "\0")` at this fixture's geometry) rather than
/// excusing.
fn edited_the_box_run(screen: &vt100::Screen) -> bool {
    screen.contents().contains("box drawing: x")
}

/// The fixture workspace: a README long enough that a split scrolls, a
/// subdirectory and a second file so the directory listing has rows.
fn build_fixture(root: &Path) -> PathBuf {
    let dir = root.join("workspace");
    std::fs::create_dir_all(dir.join("docs")).unwrap();
    std::fs::write(dir.join("docs").join("guide.md"), "# guide\n").unwrap();
    std::fs::write(dir.join("notes.txt"), "notes\n").unwrap();
    let mut readme = format!("{FIRST_LINE}\n");
    for line in WIDENING_LINES {
        readme.push_str(line);
        readme.push('\n');
    }
    for n in 0..120 {
        readme.push_str(&format!("line {n} of the fixture body\n"));
    }
    std::fs::write(dir.join("README.md"), readme).unwrap();
    dir
}

/// The `view` under test, on the fixture directory, with the `[native]`
/// defaults a user gets.
fn view_session(dir: &Path, home: &Path) -> PtySession {
    let mut cmd = portable_pty::CommandBuilder::new(common::view_bin_path());
    cmd.arg(".");
    cmd.cwd(dir);
    common::isolate_xdg_first_launch(&mut cmd, home);
    let mut session = PtySession::spawn_configured(cmd, COLS, ROWS)
        .expect("PtySession::spawn_configured against target/debug/view");
    session.record_raw_output_up_to(RAW_LIMIT);
    session
}

/// Runs a throwaway session in `home` so the measured one starts the way a
/// user's second launch does.
///
/// A state directory with no theme cache in it makes view announce that it
/// is falling back to built-in defaults, and with the `[native]` defaults on
/// that announcement is a float over the content rows. It is transient, so a
/// comparison that runs before it appears and one that runs after disagree
/// about a screen neither editor's window layout explains.
fn warm_the_home(dir: &Path, home: &Path) {
    let mut session = view_session(dir, home);
    assert!(
        session.wait_for_screen(view_test_support::host_deadline(BUDGET), lists_the_fixture),
        "the warming session never listed the fixture directory"
    );
    // the cache is written when the highlight probe confirms, which is
    // ordered after the attach and not before the listing above: quitting
    // on the listing alone leaves the measured session to meet a cold
    // state directory and paint the fallback notice over its content rows
    let deadline = Instant::now() + view_test_support::host_deadline(BUDGET);
    while !theme_cache_written(home) && Instant::now() < deadline {
        std::thread::sleep(POLL);
    }
    assert!(
        theme_cache_written(home),
        "the warming session never wrote a theme cache under {}",
        theme_cache_dir(home).display()
    );
    session.send(b"\x1b:qa!\r").unwrap();
    let _ = session.wait_for_exit(BUDGET);
}

/// Where a session started with [`view_session`] writes its theme cache.
///
/// Spelled here rather than read from `view_native::paths::cache_dir`
/// because this crate carries no edge to `view-native` and the audit keeps
/// it that way; the state root itself comes from the same helper that sets
/// the child's environment, so only the subdirectory name is restated.
fn theme_cache_dir(home: &Path) -> PathBuf {
    common::xdg_home(home, "XDG_STATE_HOME").join("view")
}

/// Whether a theme cache has appeared in `home`'s state directory.
fn theme_cache_written(home: &Path) -> bool {
    std::fs::read_dir(theme_cache_dir(home)).is_ok_and(|entries| {
        entries
            .flatten()
            .any(|entry| entry.file_name().to_string_lossy().starts_with("theme-"))
    })
}

/// The pinned `nvim` on the same directory, with none of the host's
/// configuration and all of the fixture's.
fn nvim_session(dir: &Path, home: &Path) -> PtySession {
    let mut cmd = common::reference_nvim(home);
    cmd.arg(".");
    cmd.cwd(dir);
    PtySession::spawn_configured(cmd, COLS, ROWS)
        .expect("PtySession::spawn_configured against the pinned nvim")
}

/// What the parsed screen shows at one cell.
///
/// A cell the child never painted reads back empty, and a terminal shows a
/// space there; nvim leaves the tail of a short row unpainted where view
/// paints it, so without this the two screens differ on rows a user sees as
/// identical.
fn cell_text(screen: &vt100::Screen, row: u16, col: u16) -> String {
    match screen.cell(row, col).map(|cell| cell.contents()) {
        Some(shown) if !shown.is_empty() => shown.to_string(),
        _ => " ".to_string(),
    }
}

/// One cell's painted colours, as the terminal received them.
///
/// The glyph alone cannot see a compositor that leaves a cell wearing the
/// layer underneath it: a window grown over the column its neighbour's
/// separator stood in shows a space either way, and only the background
/// says whose space it is.
fn cell_colors(screen: &vt100::Screen, row: u16, col: u16) -> (vt100::Color, vt100::Color) {
    screen
        .cell(row, col)
        .map_or((vt100::Color::Default, vt100::Color::Default), |cell| {
            (cell.fgcolor(), cell.bgcolor())
        })
}

/// One cell's colours rendered for a failure message and a dump file.
fn color_pair(colors: (vt100::Color, vt100::Color)) -> String {
    format!("fg={:?} bg={:?}", colors.0, colors.1)
}

/// A session's content rows as colour pairs, one row per line.
fn content_colors(session: &mut PtySession) -> Vec<Vec<(vt100::Color, vt100::Color)>> {
    screen_colors(session, ROWS - CHROME_ROWS)
}

/// The first cells where two rows' colours differ, as
/// `(column, view, nvim)`.
fn first_color_differences(
    mine: &[(vt100::Color, vt100::Color)],
    theirs: &[(vt100::Color, vt100::Color)],
) -> Vec<(usize, String, String)> {
    mine.iter()
        .zip(theirs)
        .enumerate()
        .filter(|(_, (a, b))| a != b)
        .map(|(col, (a, b))| (col, color_pair(*a), color_pair(*b)))
        .take(4)
        .collect()
}

/// A session's screen, cell by cell, down to `rows`.
fn screen_cells(session: &mut PtySession, rows: u16) -> Vec<Vec<String>> {
    session.with_screen(|screen| {
        (0..rows)
            .map(|row| (0..COLS).map(|col| cell_text(screen, row, col)).collect())
            .collect()
    })
}

/// A session's screen as colour pairs, down to `rows`.
fn screen_colors(session: &mut PtySession, rows: u16) -> Vec<Vec<(vt100::Color, vt100::Color)>> {
    session.with_screen(|screen| {
        (0..rows)
            .map(|row| (0..COLS).map(|col| cell_colors(screen, row, col)).collect())
            .collect()
    })
}

/// A session's content rows, cell by cell.
fn content_cells(session: &mut PtySession) -> Vec<Vec<String>> {
    screen_cells(session, ROWS - CHROME_ROWS)
}

/// [`content_cells`] as one string per row.
fn content_rows(session: &mut PtySession) -> Vec<String> {
    content_cells(session)
        .into_iter()
        .map(|row| row.concat())
        .collect()
}

/// A whole screen -- glyphs and painted colours -- beside the recorded
/// bytes that produced it. Every row, chrome included: the recording is
/// replayed through the widening models over the whole grid, so a row the
/// model is never checked against is a row its residue verdict rests on
/// nothing.
type Frame = (
    Vec<Vec<String>>,
    Vec<Vec<(vt100::Color, vt100::Color)>>,
    Vec<u8>,
);

/// A session's screen and the bytes that produced it, read so that the two
/// hold the same frame.
///
/// Draining feeds the parser and the recording in one step, so a snapshot
/// taken on either side of a drain can be a frame behind the other; taking
/// the screen twice around the recording and repeating until the two agree
/// leaves whatever the recording gained between them with nothing to show
/// for it.
fn screen_and_recording(session: &mut PtySession) -> Frame {
    let deadline = Instant::now() + view_test_support::host_deadline(BUDGET);
    loop {
        let before = (screen_cells(session, ROWS), screen_colors(session, ROWS));
        let raw = session.raw_output().to_vec();
        let after = (screen_cells(session, ROWS), screen_colors(session, ROWS));
        if before == after || Instant::now() >= deadline {
            return (after.0, after.1, raw);
        }
        // a child still writing would otherwise hold a core for the whole
        // deadline re-reading a grid this size
        std::thread::sleep(POLL);
    }
}

/// Blocks until both screens have held the same content rows across two
/// consecutive reads, so the comparison below runs against a settled frame
/// rather than a half-applied one.
fn settle_together(under_test: &mut PtySession, reference: &mut PtySession) -> bool {
    let deadline = Instant::now() + view_test_support::host_deadline(BUDGET);
    let mut agreed = 0_u8;
    while Instant::now() < deadline {
        if content_rows(under_test) == content_rows(reference)
            && content_colors(under_test) == content_colors(reference)
        {
            agreed += 1;
            if agreed == 2 {
                return true;
            }
        } else {
            agreed = 0;
        }
        std::thread::sleep(POLL);
    }
    false
}

/// Writes every differing row beside the scratch root and returns the path,
/// so a failure at this geometry names a file to read rather than printing
/// a 263-column grid into the test log.
fn dump(
    step: &str,
    under_test: &[String],
    reference: &[String],
    my_colors: &[Vec<(vt100::Color, vt100::Color)>],
    their_colors: &[Vec<(vt100::Color, vt100::Color)>],
) -> PathBuf {
    let path = common::scratch_root().join(format!("close-battery-{step}.txt"));
    let mut text = String::new();
    for (row, (mine, theirs)) in under_test.iter().zip(reference).enumerate() {
        if mine != theirs {
            text.push_str(&format!("row {row}\n  view: {mine}\n  nvim: {theirs}\n"));
        }
    }
    for (row, (mine, theirs)) in my_colors.iter().zip(their_colors).enumerate() {
        for (col, first, second) in first_color_differences(mine, theirs) {
            text.push_str(&format!(
                "row {row} col {col}\n  view: {first}\n  nvim: {second}\n"
            ));
        }
    }
    std::fs::write(&path, text).unwrap();
    path
}

/// The first cells where two rows differ, as `(column, view, nvim)`.
fn first_differences(mine: &str, theirs: &str) -> Vec<(usize, char, char)> {
    mine.chars()
        .zip(theirs.chars())
        .enumerate()
        .filter(|(_, (a, b))| a != b)
        .map(|(col, (a, b))| (col, a, b))
        .take(4)
        .collect()
}

/// Which rows carry the buffer's name.
///
/// The user's report was a status bar drawn twice, which shows the name on
/// a row nvim puts nothing on; holding the whole set to nvim's says both
/// that the bars are where they belong and that there is no extra one.
fn rows_naming_the_buffer(rows: &[String]) -> Vec<usize> {
    rows.iter()
        .enumerate()
        .filter(|(_, row)| row.contains("README.md"))
        .map(|(row, _)| row)
        .collect()
}

#[test]
fn closing_each_window_of_a_split_leaves_no_residue_on_a_widening_terminal() {
    let work = common::ScratchPaths::new("close-battery-work");
    let view_paths = common::ScratchPaths::new("close-battery-view");
    let nvim_paths = common::ScratchPaths::new("close-battery-nvim");
    let dir = build_fixture(&work.isolated_home);

    // both sessions source the same committed highlights, so a colour they
    // disagree about is the compositor's answer rather than two default
    // colourschemes'
    for home in [&view_paths.isolated_home, &nvim_paths.isolated_home] {
        common::plant_nvim_config(home, FIXTURE_CONFIG);
    }

    warm_the_home(&dir, &view_paths.isolated_home);
    let mut under_test = view_session(&dir, &view_paths.isolated_home);
    let mut reference = nvim_session(&dir, &nvim_paths.isolated_home);

    for (step, keys, reached) in STEPS {
        if !keys.is_empty() {
            under_test.send(keys.as_bytes()).unwrap();
            reference.send(keys.as_bytes()).unwrap();
        }
        let budget = view_test_support::host_deadline(BUDGET);
        for (name, session) in [
            ("view", &mut under_test),
            ("the pinned nvim", &mut reference),
        ] {
            assert!(
                session.wait_for_screen(budget, reached),
                "{step}: {name} never reached the layout this step asks for"
            );
        }
        let settled = settle_together(&mut under_test, &mut reference);
        let (cells, colors, raw) = screen_and_recording(&mut under_test);
        let mine: Vec<String> = cells
            .iter()
            .take(usize::from(ROWS - CHROME_ROWS))
            .map(|row| row.concat())
            .collect();
        let my_colors: Vec<Vec<(vt100::Color, vt100::Color)>> = colors
            .iter()
            .take(usize::from(ROWS - CHROME_ROWS))
            .cloned()
            .collect();
        let theirs = content_rows(&mut reference);
        let their_colors = content_colors(&mut reference);
        let path = dump(step, &mine, &theirs, &my_colors, &their_colors);
        assert!(
            settled,
            "{step}: view's content rows never matched the pinned nvim's; \
             every differing row is in {}",
            path.display()
        );
        for (row, (mine, theirs)) in mine.iter().zip(&theirs).enumerate() {
            assert_eq!(
                mine,
                theirs,
                "{step}: row {row} differs at {:?}; every differing row is in {}",
                first_differences(mine, theirs),
                path.display()
            );
        }
        // the half a glyph comparison cannot see: a cell wearing the layer
        // underneath it shows the same character either way, and only the
        // colours say which pane painted it
        for (row, (mine, theirs)) in my_colors.iter().zip(&their_colors).enumerate() {
            assert_eq!(
                mine,
                theirs,
                "{step}: row {row}'s colours differ at {:?}; every differing \
                 cell is in {}",
                first_color_differences(mine, theirs),
                path.display()
            );
        }
        assert_eq!(
            rows_naming_the_buffer(&mine),
            rows_naming_the_buffer(&theirs),
            "{step}: view names the buffer on a different set of rows than \
             nvim does, which is a status bar drawn where nvim draws none"
        );

        assert!(
            raw.len() < RAW_LIMIT,
            "{step}: the recording filled its {RAW_LIMIT}-byte bound, so the \
             replay below reads a stream cut mid-frame"
        );
        let mut narrow = WideTerm::new(COLS, ROWS, Widening::Narrow);
        let mut wide = WideTerm::new(COLS, ROWS, Widening::Ambiguous);
        narrow.feed(&raw);
        wide.feed(&raw);

        for row in 0..ROWS {
            for col in 0..COLS {
                let modelled = narrow.cell(col, row);
                if modelled == view_test_support::WIDE_HALF {
                    continue;
                }
                let shown = &cells[row as usize][col as usize];
                assert_eq!(
                    modelled,
                    shown.as_str(),
                    "{step}: the narrow model shows {modelled:?} at \
                     ({col},{row}) where the parsed screen shows {shown:?}, \
                     so the widening comparison below runs against a model \
                     that does not follow view's own output"
                );
            }
        }

        let residue = widening_residue(&narrow, &wide);
        assert!(
            residue.is_empty(),
            "{step}: a widening terminal shows {} cells the narrow one does \
             not; the first are {:?}",
            residue.len(),
            residue.iter().take(4).collect::<Vec<_>>()
        );
    }

    for session in [&mut under_test, &mut reference] {
        session.send(b"\x1b:qa!\r").unwrap();
        let _ = session.wait_for_exit(BUDGET);
    }
}

/// What a gapped tile spends on each axis of the outer grid: a frame cell
/// on each of the two sides.
const OUTER_RING: u16 = 2;

/// The row the pill takes off the top of the outer grid under tiles.
const PILL_ROW: u16 = 1;

/// The grid nvim paints chrome into, which no window owns.
const GLOBAL_GRID: u64 = 1;

/// The close sequence this leg types, in the notation the engine-attached
/// driver takes.
const TILED_STEPS: [(&str, &str); 9] = [
    ("file", ":e README.md<CR>"),
    ("vsplit", ":vsplit<CR>"),
    ("split", ":split<CR>"),
    ("vsplit2", ":vsplit<CR>"),
    // a cursor that moves and a focus that moves with it: both are what
    // the `window` trigger fires on, and neither may change a window's
    // size
    ("cursor", "5j"),
    ("focus", "<C-w>w"),
    ("close1", ":q<CR>"),
    ("close2", ":q<CR>"),
    ("close3", ":q<CR>"),
];

const QUIESCE_SILENCE: Duration = Duration::from_millis(200);
const QUIESCE_DEADLINE: Duration = Duration::from_secs(10);

/// The size of every window open in the engine, read off nvim itself.
fn nvim_window_sizes(engine: &mut view_oracle::EngineSession) -> Vec<(usize, usize)> {
    let listed = engine
        .eval_str(
            "join(map(range(1, winnr('$')), \
             'winwidth(v:val) . \"x\" . winheight(v:val)'), \",\")",
        )
        .expect("nvim answers for its own windows");
    let mut sizes: Vec<(usize, usize)> = listed
        .split(',')
        .filter(|entry| !entry.is_empty())
        .map(|entry| {
            let (cols, rows) = entry.split_once('x').expect("winwidth x winheight");
            (
                cols.parse().expect("a column count"),
                rows.parse().expect("a row count"),
            )
        })
        .collect();
    sizes.sort_unstable();
    sizes
}

/// The size of every window nvim has open, tabpages other than the
/// current one included: a new tabpage leaves the old one's windows open,
/// and `winnr('$')` counts only the tabpage the session is on.
fn all_window_sizes(engine: &mut view_oracle::EngineSession) -> Vec<(usize, usize)> {
    let listed = engine
        .eval_str("join(map(getwininfo(), 'v:val.width . \"x\" . v:val.height'), \",\")")
        .expect("nvim answers for its own windows");
    let mut sizes: Vec<(usize, usize)> = listed
        .split(',')
        .filter(|entry| !entry.is_empty())
        .map(|entry| {
            let (cols, rows) = entry.split_once('x').expect("width x height");
            (
                cols.parse().expect("a column count"),
                rows.parse().expect("a row count"),
            )
        })
        .collect();
    sizes.sort_unstable();
    sizes
}

/// The size of every window grid view holds. The global grid is chrome
/// rather than a window and is left out.
fn window_grid_sizes(engine: &view_oracle::EngineSession) -> Vec<(usize, usize)> {
    let mut sizes: Vec<(usize, usize)> = engine
        .grid_screens()
        .iter()
        .filter(|(id, _)| *id != GLOBAL_GRID)
        .map(|(_, screen)| {
            (
                screen.rows.first().map_or(0, |row| row.chars().count()),
                screen.rows.len(),
            )
        })
        .collect();
    sizes.sort_unstable();
    sizes
}

/// The same splits and closes as the battery above, under
/// `panes = "tiles"` at the same geometry, as an assertion rather than a
/// capture: at every step view holds one grid per open window, each the
/// size nvim itself reports for that window, so neither a grid outliving
/// the window that owned it nor one left at a stale size survives a close.
///
/// Driven through [`view_oracle::EngineSession::set_panes`]. That driver
/// builds its own `Model` and never runs `view-native`'s resolver, so
/// `VIEW_UI_PANES` reaches nothing here.
///
/// Disconfirm: dropping the `Destroy` arm from
/// `view_core::grid::registry` leaves the closed windows' grids in the
/// first `close` step's list, which fails naming both sets.
#[test]
fn every_tile_keeps_its_own_windows_size_as_the_windows_of_a_split_close() {
    let work = common::ScratchPaths::new("close-battery-tiles");
    let dir = build_fixture(&work.isolated_home);
    let mut engine = view_oracle::EngineSession::spawn_with_ext(
        COLS,
        ROWS,
        view_oracle::UI_EXT_OPTIONS_MULTIGRID,
    )
    .expect("EngineSession::spawn_with_ext against real nvim");
    assert!(engine.quiesce(QUIESCE_SILENCE, QUIESCE_DEADLINE).unwrap());
    engine
        .arm_and_input(&format!(":cd {}<CR>", dir.display()))
        .unwrap();
    assert!(engine.quiesce(QUIESCE_SILENCE, QUIESCE_DEADLINE).unwrap());
    engine
        .set_panes("tiles")
        .expect("the tiled look is reachable");
    assert!(engine.quiesce(QUIESCE_SILENCE, QUIESCE_DEADLINE).unwrap());

    let outer = engine
        .grid_screens()
        .iter()
        .find(|(id, _)| *id == GLOBAL_GRID)
        .map(|(_, screen)| {
            (
                screen.rows.first().map_or(0, |row| row.chars().count()),
                screen.rows.len(),
            )
        })
        .expect("the global grid is always named");
    assert_eq!(
        outer,
        (
            usize::from(COLS - OUTER_RING),
            usize::from(ROWS - OUTER_RING - PILL_ROW)
        ),
        "the tiled look takes its ring and the pill's row out of the outer grid"
    );

    for (step, keys) in TILED_STEPS {
        engine.arm_and_input(keys).unwrap();
        assert!(
            engine.quiesce(QUIESCE_SILENCE, QUIESCE_DEADLINE).unwrap(),
            "{step}: the session never settled"
        );
        let wanted = nvim_window_sizes(&mut engine);
        assert_eq!(
            window_grid_sizes(&engine),
            wanted,
            "{step}: view's window grids are not the windows nvim has open"
        );
    }
}

/// The outer grid under tiles with the pill on, as tabpages open and close
/// and as the row names buffers instead.
///
/// The pill's row comes out of the grid nvim lays its windows in, so a row
/// that appeared or went away without the grid following would leave every
/// window a row taller or shorter than the screen has for it. Each step
/// checks both: what the row names, and that view's window grids are still
/// the windows nvim has open.
#[test]
fn the_pill_holds_its_row_as_tabpages_and_buffers_come_and_go() {
    let work = common::ScratchPaths::new("close-battery-pill");
    let dir = build_fixture(&work.isolated_home);
    let mut engine = view_oracle::EngineSession::spawn_with_ext(
        COLS,
        ROWS,
        view_oracle::UI_EXT_OPTIONS_MULTIGRID,
    )
    .expect("EngineSession::spawn_with_ext against real nvim");
    assert!(engine.quiesce(QUIESCE_SILENCE, QUIESCE_DEADLINE).unwrap());
    engine
        .arm_and_input(&format!(":cd {}<CR>", dir.display()))
        .unwrap();
    assert!(engine.quiesce(QUIESCE_SILENCE, QUIESCE_DEADLINE).unwrap());
    engine
        .set_panes("tiles")
        .expect("the tiled look is reachable");
    assert!(engine.quiesce(QUIESCE_SILENCE, QUIESCE_DEADLINE).unwrap());

    let outer_rows = |engine: &view_oracle::EngineSession| {
        engine
            .grid_screens()
            .iter()
            .find(|(id, _)| *id == GLOBAL_GRID)
            .map(|(_, screen)| screen.rows.len())
            .expect("the global grid is always named")
    };
    let reserved = usize::from(ROWS - OUTER_RING - PILL_ROW);

    for (step, keys) in [
        ("file", ":e README.md<CR>"),
        ("tab2", ":tabnew<CR>"),
        ("tab3", ":tabnew<CR>"),
        ("split", ":vsplit<CR>"),
        ("close", ":q<CR>"),
        ("tabclose", ":tabclose<CR>"),
        ("tabonly", ":tabonly<CR>"),
    ] {
        engine.arm_and_input(keys).unwrap();
        assert!(
            engine.quiesce(QUIESCE_SILENCE, QUIESCE_DEADLINE).unwrap(),
            "{step}: the session never settled"
        );
        assert_eq!(
            outer_rows(&engine),
            reserved,
            "{step}: the pill's row left the outer grid"
        );
        let wanted = all_window_sizes(&mut engine);
        assert_eq!(
            window_grid_sizes(&engine),
            wanted,
            "{step}: view's window grids are not the windows nvim has open"
        );
    }

    // three tabpages named across the row, each by the file it holds
    engine
        .arm_and_input(":e README.md<CR>:tabnew notes.txt<CR>:tabnew docs/guide.md<CR>")
        .unwrap();
    assert!(engine.quiesce(QUIESCE_SILENCE, QUIESCE_DEADLINE).unwrap());
    let row = engine.screen().rows[0].clone();
    assert_eq!(
        row.chars().count(),
        usize::from(COLS),
        "the pill spans the whole terminal row"
    );
    for name in ["README.md", "notes.txt", "guide.md"] {
        assert!(
            row.contains(name),
            "the tabpage holding {name} is missing from the row: {row}"
        );
    }

    // the same row naming four buffers instead, one of them unsaved
    engine.arm_and_input(":tabonly<CR>").unwrap();
    assert!(engine.quiesce(QUIESCE_SILENCE, QUIESCE_DEADLINE).unwrap());
    let listed: Vec<view_core::model::BufferEntry> = ["a.rs", "b.rs", "c.rs", "d.rs"]
        .into_iter()
        .enumerate()
        .map(|(index, name)| {
            view_core::model::BufferEntry::new(
                index as u64 + 1,
                name.to_string(),
                index == 1,
                index == 0,
            )
        })
        .collect();
    engine.seed_pill_buffers(view_core::native::pill::TablineShows::Buffers, listed);
    let row = engine.screen().rows[0].clone();
    for name in ["a.rs", "b.rs", "c.rs", "d.rs"] {
        assert!(row.contains(name), "{name} is missing from the row: {row}");
    }
    assert!(
        row.contains("b.rs +"),
        "the unsaved buffer carries no marker: {row}"
    );
    assert_eq!(
        outer_rows(&engine),
        reserved,
        "naming buffers instead of tabpages moved the grid"
    );

    engine.arm_and_input(":qa!<CR>").unwrap();
}

/// The same tiled session with the file tree in a window of its own: the
/// tree opens into a tile, the cursor lands in it, `<Esc>` hands the
/// keyboard back to the window it came from, and every split and close
/// after that still leaves view holding one grid per open window at the
/// size nvim reports for it.
///
/// Disconfirm: dropping `winfixwidth` from the open chunk lets the splits
/// re-flow the tree's window, which fails on the width it opened at.
#[test]
fn a_windowed_tree_keeps_its_tile_as_the_windows_of_a_split_close() {
    let work = common::ScratchPaths::new("close-battery-tree");
    let dir = build_fixture(&work.isolated_home);
    let mut engine = view_oracle::EngineSession::spawn_with_ext(
        COLS,
        ROWS,
        view_oracle::UI_EXT_OPTIONS_MULTIGRID,
    )
    .expect("EngineSession::spawn_with_ext against real nvim");
    assert!(engine.quiesce(QUIESCE_SILENCE, QUIESCE_DEADLINE).unwrap());
    engine
        .arm_and_input(&format!(":cd {}<CR>", dir.display()))
        .unwrap();
    assert!(engine.quiesce(QUIESCE_SILENCE, QUIESCE_DEADLINE).unwrap());
    engine
        .set_panes("tiles")
        .expect("the tiled look is reachable");
    assert!(engine.quiesce(QUIESCE_SILENCE, QUIESCE_DEADLINE).unwrap());
    engine.set_surface(
        view_core::native::geometry::NativeSurface::Tree,
        view_core::native::geometry::SurfaceLayout::new(
            view_core::native::geometry::SurfacePlacement::Windowed,
            view_core::native::geometry::Anchor::Left,
            30,
        ),
    );

    let before = nvim_window_sizes(&mut engine).len();
    engine
        .feed(view_core::msg::Msg::FeatureInvoke {
            feature: "tree".to_string(),
            verb: "toggle".to_string(),
        })
        .expect("the tree's window opens");
    assert!(engine.quiesce(QUIESCE_SILENCE, QUIESCE_DEADLINE).unwrap());
    assert_eq!(
        nvim_window_sizes(&mut engine).len(),
        before + 1,
        "the tree took no window of its own"
    );
    assert_eq!(
        engine.focus(),
        view_core::model::Focus::Pane(view_core::native::geometry::NativeSurface::Tree),
        "the cursor did not land in the tree's own window"
    );
    assert_eq!(
        engine.eval_str("&filetype").unwrap().trim(),
        "view-tree",
        "the window the cursor sits in is not the tree's"
    );

    engine
        .feed(view_core::msg::Msg::Key(view_core::msg::Key {
            notation: "<Esc>".to_string(),
        }))
        .expect("<Esc> leaves the tree's window");
    assert!(engine.quiesce(QUIESCE_SILENCE, QUIESCE_DEADLINE).unwrap());
    assert_eq!(
        engine.focus(),
        view_core::model::Focus::Engine,
        "<Esc> left the keyboard in the tree"
    );
    assert_ne!(
        engine.eval_str("&filetype").unwrap().trim(),
        "view-tree",
        "<Esc> left the cursor in the tree's own window"
    );
    assert_eq!(
        nvim_window_sizes(&mut engine).len(),
        before + 1,
        "<Esc> took the tree's tile down with the focus"
    );

    let tree_width = |engine: &mut view_oracle::EngineSession| -> String {
        engine
            .eval_str(
                "join(map(filter(getwininfo(), \
                 'getbufvar(v:val.bufnr, \"&filetype\") ==# \"view-tree\"'), \
                 'v:val.width'), \",\")",
            )
            .expect("nvim answers for the tree's own window")
            .trim()
            .to_string()
    };
    let opened_at = tree_width(&mut engine);

    for (step, keys) in TILED_STEPS {
        engine.arm_and_input(keys).unwrap();
        assert!(
            engine.quiesce(QUIESCE_SILENCE, QUIESCE_DEADLINE).unwrap(),
            "{step}: the session never settled"
        );
        let wanted = nvim_window_sizes(&mut engine);
        assert_eq!(
            window_grid_sizes(&engine),
            wanted,
            "{step}: view's window grids are not the windows nvim has open"
        );
        assert_eq!(
            tree_width(&mut engine),
            opened_at,
            "{step}: the splits re-flowed the tree's own window"
        );
    }

    // nvim's own close of the window, which reaches view as win_close and
    // grid_destroy and nothing else
    engine
        .feed(view_core::msg::Msg::FeatureInvoke {
            feature: "tree".to_string(),
            verb: "toggle".to_string(),
        })
        .expect("the toggle enters the standing window");
    assert!(engine.quiesce(QUIESCE_SILENCE, QUIESCE_DEADLINE).unwrap());
    assert_eq!(
        engine.focus(),
        view_core::model::Focus::Pane(view_core::native::geometry::NativeSurface::Tree),
        "the toggle did not step back into the tree"
    );
    engine.arm_and_input(":q<CR>").unwrap();
    assert!(engine.quiesce(QUIESCE_SILENCE, QUIESCE_DEADLINE).unwrap());
    assert_eq!(
        tree_width(&mut engine),
        "",
        "nvim still has a window holding the tree"
    );
    assert_eq!(
        engine.focus(),
        view_core::model::Focus::Engine,
        "the keyboard stayed in a window nvim closed"
    );
    assert!(
        !engine.tree_is_open(),
        "the tree's state outlived the window nvim closed, so its scan \
         worker is still walking a sidebar nobody can see"
    );
    let wanted = nvim_window_sizes(&mut engine);
    assert_eq!(
        window_grid_sizes(&engine),
        wanted,
        "the closed window left a grid behind"
    );

    engine
        .feed(view_core::msg::Msg::FeatureInvoke {
            feature: "tree".to_string(),
            verb: "toggle".to_string(),
        })
        .expect("the tree opens again");
    assert!(engine.quiesce(QUIESCE_SILENCE, QUIESCE_DEADLINE).unwrap());
    assert_eq!(
        engine.focus(),
        view_core::model::Focus::Pane(view_core::native::geometry::NativeSurface::Tree),
        "the reopened window is not one view draws the tree into"
    );
    let wanted = nvim_window_sizes(&mut engine);
    assert_eq!(
        window_grid_sizes(&engine),
        wanted,
        "the reopen left view holding a grid nvim has no window for"
    );

    // nvim refusing the close outright, which is what `:only` from inside
    // the tree sets up: its window is then the last one and
    // `nvim_win_close` answers E444, so no `win_close` follows
    engine.arm_and_input(":only<CR>").unwrap();
    assert!(engine.quiesce(QUIESCE_SILENCE, QUIESCE_DEADLINE).unwrap());
    engine
        .feed(view_core::msg::Msg::FeatureInvoke {
            feature: "tree".to_string(),
            verb: "toggle".to_string(),
        })
        .expect("the toggle asks nvim to close the window");
    assert!(engine.quiesce(QUIESCE_SILENCE, QUIESCE_DEADLINE).unwrap());
    assert_ne!(
        tree_width(&mut engine),
        "",
        "nvim did not refuse the close, so this leg tests nothing"
    );
    assert!(
        !engine.tree_is_open(),
        "the tree's state waits on a win_close nvim will never send"
    );
    let screen = engine.screen_text();
    assert!(
        screen.contains("E444"),
        "a refused close said nothing to the reader: {screen}"
    );
}
