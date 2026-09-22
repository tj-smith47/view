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
        std::thread::sleep(view_test_support::host_deadline(POLL));
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
        std::thread::sleep(view_test_support::host_deadline(POLL));
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
        std::thread::sleep(view_test_support::host_deadline(POLL));
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
/// The silence window the two latency measurements below settle on --
/// every other leg in this file uses [`QUIESCE_SILENCE`] because it is
/// quiescing between steps whose correctness matters, not timing them. A
/// stopwatch that never returns before 200ms of quiet has passed cannot
/// tell a fast ring step from a slow one; it reads the floor.
const LATENCY_SILENCE: Duration = Duration::from_millis(5);

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

/// The width of every window holding the tree's own buffer, as nvim
/// reports it, joined by commas and empty where the tree has no window.
fn tree_window_widths(engine: &mut view_oracle::EngineSession) -> String {
    engine
        .eval_str(
            "join(map(filter(getwininfo(), \
             'getbufvar(v:val.bufnr, \"&filetype\") ==# \"view-tree\"'), \
             'v:val.width'), \",\")",
        )
        .expect("nvim answers for the tree's own window")
        .trim()
        .to_string()
}

/// How many of nvim's buffers were opened for a surface of view's own.
fn view_buffers(engine: &mut view_oracle::EngineSession) -> String {
    engine
        .eval_str(
            "join(map(filter(getbufinfo(), \
             'getbufvar(v:val.bufnr, \"&filetype\") =~# \"^view-\"'), \
             'v:val.bufnr'), \",\")",
        )
        .expect("nvim answers for its own buffers")
        .trim()
        .to_string()
}

/// A tiled session on `dir` with the tree placed in a window of its own.
fn windowed_tree_session(dir: &Path) -> view_oracle::EngineSession {
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
    engine
}

/// Opens the tree and leaves the cursor in its window, which is where
/// `<leader>e` puts it.
fn open_the_tree(engine: &mut view_oracle::EngineSession) {
    engine
        .feed(view_core::msg::Msg::FeatureInvoke {
            feature: "tree".to_string(),
            verb: "toggle".to_string(),
        })
        .expect("the tree's window opens");
    assert!(engine.quiesce(QUIESCE_SILENCE, QUIESCE_DEADLINE).unwrap());
    assert_eq!(
        engine.eval_str("&filetype").unwrap().trim(),
        "view-tree",
        "the cursor did not land in the tree's own window"
    );
}

/// Opens the tree, leaves its window the only one on the tabpage, and
/// closes it again -- the two keys a person reaches this by.
fn close_the_tree_alone_on_its_tabpage(engine: &mut view_oracle::EngineSession) {
    open_the_tree(engine);
    engine.arm_and_input(":only<CR>").unwrap();
    assert!(engine.quiesce(QUIESCE_SILENCE, QUIESCE_DEADLINE).unwrap());
    assert_eq!(
        nvim_window_sizes(engine).len(),
        1,
        "`:only` left a second window open, so nvim would take the close"
    );
    engine
        .feed(view_core::msg::Msg::FeatureInvoke {
            feature: "tree".to_string(),
            verb: "toggle".to_string(),
        })
        .expect("the toggle asks nvim to close the window");
    assert!(engine.quiesce(QUIESCE_SILENCE, QUIESCE_DEADLINE).unwrap());
}

/// What both arms of the close leave behind: one window, holding a buffer
/// the person can type in, with nothing of the tree's left anywhere.
fn assert_ordinary_window(engine: &mut view_oracle::EngineSession) {
    assert_eq!(
        nvim_window_sizes(engine).len(),
        1,
        "the window the tree sat in went away with the tree"
    );
    assert_eq!(
        tree_window_widths(engine),
        "",
        "the tree's scratch window is still on screen"
    );
    assert_eq!(
        view_buffers(engine),
        "",
        "the tree's scratch buffer outlived the window it was drawn in"
    );
    assert!(
        !engine.tree_is_open(),
        "the tree's state outlived the window it was drawn in"
    );
    assert_eq!(
        engine.eval_str("&buftype").unwrap().trim(),
        "",
        "the window came back holding a buffer nvim writes nothing to"
    );
    assert_eq!(
        engine.eval_str("&modifiable").unwrap().trim(),
        "1",
        "the window came back holding a buffer the person cannot type in"
    );
    assert_eq!(
        engine.focus(),
        view_core::model::Focus::Engine,
        "the keyboard stayed in a surface of view's own"
    );
    let wanted = nvim_window_sizes(engine);
    assert_eq!(
        window_grid_sizes(engine),
        wanted,
        "view holds a grid the handed-back window does not answer for"
    );
    let screen = engine.screen_text();
    assert!(
        !screen.contains("E444"),
        "the close still asked nvim to close the last window: {screen}"
    );
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
    let mut engine = windowed_tree_session(&dir);

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

    let opened_at = tree_window_widths(&mut engine);

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
            tree_window_widths(&mut engine),
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
        tree_window_widths(&mut engine),
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

    // the tree's window left as the only one on the tabpage, which is
    // what `:only` from inside it sets up: nvim closes no window there,
    // so the close hands this one back as an ordinary window
    engine.arm_and_input(":only<CR>").unwrap();
    assert!(engine.quiesce(QUIESCE_SILENCE, QUIESCE_DEADLINE).unwrap());
    assert_eq!(
        nvim_window_sizes(&mut engine).len(),
        1,
        "`:only` left a second window open, so this leg tests nothing"
    );
    engine
        .feed(view_core::msg::Msg::FeatureInvoke {
            feature: "tree".to_string(),
            verb: "toggle".to_string(),
        })
        .expect("the toggle asks nvim to close the window");
    assert!(engine.quiesce(QUIESCE_SILENCE, QUIESCE_DEADLINE).unwrap());
    assert_eq!(
        nvim_window_sizes(&mut engine).len(),
        1,
        "the window the tree sat in went away with the tree"
    );
    assert_eq!(
        tree_window_widths(&mut engine),
        "",
        "the tree's scratch window is still on screen"
    );
    assert!(
        !engine.tree_is_open(),
        "the tree's state outlived the window it was drawn in"
    );
    let wanted = nvim_window_sizes(&mut engine);
    assert_eq!(
        window_grid_sizes(&engine),
        wanted,
        "view holds a grid the handed-back window does not answer for"
    );
    let screen = engine.screen_text();
    assert!(
        !screen.contains("E444"),
        "the close still asked nvim to close the last window: {screen}"
    );
}

/// The tree closed while its window is the only one on the tabpage. nvim
/// refuses `nvim_win_close` there, and view used to drop the tree anyway,
/// leaving the person looking at the unnamed scratch window it had been
/// drawn on with E444 under it. Two keys away: `:only` inside the tree,
/// then the toggle.
///
/// Both arms of the fallback: a session with a file open takes the buffer
/// it was last in, and one that has opened nothing takes its own empty
/// buffer.
///
/// Disconfirm: dropping the `nvim_tabpage_list_wins` branch from
/// `CLOSE_NATIVE_WINDOW_CHUNK` puts the scratch window back, which fails
/// on the tree's own window still being on screen.
#[test]
fn closing_the_tree_in_its_last_window_leaves_an_ordinary_window() {
    let work = common::ScratchPaths::new("close-battery-tree-alone");
    let dir = build_fixture(&work.isolated_home);

    {
        let mut engine = windowed_tree_session(&dir);
        engine.arm_and_input(":e README.md<CR>").unwrap();
        assert!(engine.quiesce(QUIESCE_SILENCE, QUIESCE_DEADLINE).unwrap());
        close_the_tree_alone_on_its_tabpage(&mut engine);
        assert_ordinary_window(&mut engine);
        assert_eq!(
            engine.eval_str("expand('%:t')").unwrap().trim(),
            "README.md",
            "the window came back on a buffer the person had not been in"
        );
        // the window's own grid, which is where its text lives under
        // multigrid: the screen this harness rasters is the outer grid
        // and view's own chrome over it
        let text = engine.grid_screens();
        assert!(
            text.iter().any(|(id, screen)| *id != GLOBAL_GRID
                && screen
                    .rows
                    .first()
                    .is_some_and(|row| row.starts_with(FIRST_LINE))),
            "the window came back holding none of the file's text: {text:#?}"
        );
    }

    {
        let mut engine = windowed_tree_session(&dir);
        close_the_tree_alone_on_its_tabpage(&mut engine);
        assert_ordinary_window(&mut engine);
        assert_eq!(
            engine.eval_str("bufname('%')").unwrap().trim(),
            "",
            "a session that opened no file came back on a named buffer"
        );
        assert_eq!(
            engine
                .eval_str("line('$') . ':' . getline(1)")
                .unwrap()
                .trim(),
            "1:",
            "the empty buffer the window came back on holds text"
        );
    }
}

/// The window options a person sets for themselves. The tree's own window
/// wants the opposite of each, which is what makes them read.
const PERSONAL_OPTIONS: &str = ":set number signcolumn=yes wrap<CR>";

/// Those three options as nvim answers for them, globally or for one
/// window, joined the way both readings below take them.
const OPTION_READING: &str = "&number . ',' . &signcolumn . ',' . &wrap";

/// Opening the tree writes its own look into the window it opens, and the
/// window it hands back at the close takes the person's look. Both were
/// one bug: `vim.wo[win].number = false` writes nvim's global value as
/// well as the window's, so opening the tree took the person's numbers,
/// sign column and wrapping away from every window they opened for the
/// rest of the session, and the hand-back then restored the tree's own
/// values from that global.
///
/// Disconfirm: writing the open chunk's options with `vim.wo[win]` again
/// fails on the globals, and dropping the close chunk's `winfixwidth`
/// write fails on the width the handed-back window is pinned to.
#[test]
fn the_tree_leaves_the_persons_window_options_where_they_were() {
    let work = common::ScratchPaths::new("close-battery-tree-options");
    let dir = build_fixture(&work.isolated_home);
    let mut engine = windowed_tree_session(&dir);
    engine.arm_and_input(":e README.md<CR>").unwrap();
    assert!(engine.quiesce(QUIESCE_SILENCE, QUIESCE_DEADLINE).unwrap());
    engine.arm_and_input(PERSONAL_OPTIONS).unwrap();
    assert!(engine.quiesce(QUIESCE_SILENCE, QUIESCE_DEADLINE).unwrap());
    let globals = "&g:number . ',' . &g:signcolumn . ',' . &g:wrap";
    assert_eq!(
        engine.eval_str(globals).unwrap().trim(),
        "1,yes,1",
        "the session did not take the options this reads"
    );

    open_the_tree(&mut engine);
    assert_eq!(
        engine.eval_str(globals).unwrap().trim(),
        "1,yes,1",
        "opening the tree took the person's own options with it"
    );
    assert_eq!(
        engine.eval_str(OPTION_READING).unwrap().trim(),
        "0,no,0",
        "the tree's own window is not wearing the tree's look"
    );

    engine.arm_and_input(":only<CR>").unwrap();
    assert!(engine.quiesce(QUIESCE_SILENCE, QUIESCE_DEADLINE).unwrap());
    engine
        .feed(view_core::msg::Msg::FeatureInvoke {
            feature: "tree".to_string(),
            verb: "toggle".to_string(),
        })
        .expect("the toggle asks nvim to close the window");
    assert!(engine.quiesce(QUIESCE_SILENCE, QUIESCE_DEADLINE).unwrap());
    assert_eq!(
        engine.eval_str(OPTION_READING).unwrap().trim(),
        "1,yes,1",
        "the window came back wearing the tree's look"
    );
    assert_eq!(
        engine.eval_str("&winfixwidth").unwrap().trim(),
        "0",
        "the window came back holding the width the tree pinned it to"
    );
}

/// The tree alone on a tabpage that is not the last one. nvim closes that
/// window without complaint, taking the tabpage with it the way `:q`
/// would, so the hand-back is for the only window of the only tabpage and
/// nothing wider.
///
/// Disconfirm: taking `nvim_list_tabpages` out of the close chunk's
/// condition leaves the tabpage standing with a file in it, which fails
/// on the tabpage count.
#[test]
fn the_tree_alone_on_an_inner_tabpage_closes_with_its_tabpage() {
    let work = common::ScratchPaths::new("close-battery-tree-tabpage");
    let dir = build_fixture(&work.isolated_home);
    let mut engine = windowed_tree_session(&dir);
    engine.arm_and_input(":e README.md<CR>").unwrap();
    assert!(engine.quiesce(QUIESCE_SILENCE, QUIESCE_DEADLINE).unwrap());
    engine.arm_and_input(":tabnew<CR>").unwrap();
    assert!(engine.quiesce(QUIESCE_SILENCE, QUIESCE_DEADLINE).unwrap());
    open_the_tree(&mut engine);
    engine.arm_and_input(":only<CR>").unwrap();
    assert!(engine.quiesce(QUIESCE_SILENCE, QUIESCE_DEADLINE).unwrap());
    assert_eq!(
        engine
            .eval_str("tabpagenr('$') . ':' . winnr('$')")
            .unwrap()
            .trim(),
        "2:1",
        "the tree is not alone on the second of two tabpages"
    );

    engine
        .feed(view_core::msg::Msg::FeatureInvoke {
            feature: "tree".to_string(),
            verb: "toggle".to_string(),
        })
        .expect("the toggle asks nvim to close the window");
    assert!(engine.quiesce(QUIESCE_SILENCE, QUIESCE_DEADLINE).unwrap());
    assert_eq!(
        engine.eval_str("tabpagenr('$')").unwrap().trim(),
        "1",
        "the tree's tabpage outlived the window that was all it held"
    );
    assert_eq!(
        tree_window_widths(&mut engine),
        "",
        "the tree's scratch window is still on screen"
    );
    assert_eq!(
        view_buffers(&mut engine),
        "",
        "the tree's scratch buffer outlived its window"
    );
    assert!(
        !engine.tree_is_open(),
        "the tree's state outlived the window it was drawn in"
    );
    assert_eq!(
        engine.eval_str("expand('%:t')").unwrap().trim(),
        "README.md",
        "the tabpage that closed did not hand the person back their file"
    );
    let screen = engine.screen_text();
    assert!(
        !screen.contains("E444"),
        "the close said nvim could not take it: {screen}"
    );
}

/// A file put into the tree's window by something other than view. A
/// plugin autocommand, a quickfix jump or a `:buffer` can land there, and
/// the window stops being the tree's the moment it does: the person is
/// reading their own file, and view kept painting the tree's rows over it
/// until the next toggle. The window is theirs from that moment, the
/// buffer they put there is untouched, and the next `<leader>e` opens a
/// fresh tree beside it.
///
/// Disconfirm: a `BufWinEnter` callback in `OPEN_NATIVE_WINDOW_CHUNK`
/// that returns before it reports leaves the pane painting the tree,
/// which fails on the keys still belonging to it.
#[test]
fn a_file_put_in_the_trees_window_takes_it_back_from_view() {
    let work = common::ScratchPaths::new("close-battery-tree-foreign");
    let dir = build_fixture(&work.isolated_home);
    let mut engine = windowed_tree_session(&dir);
    engine.arm_and_input(":e notes.txt<CR>").unwrap();
    assert!(engine.quiesce(QUIESCE_SILENCE, QUIESCE_DEADLINE).unwrap());
    engine.arm_and_input(":set number<CR>").unwrap();
    assert!(engine.quiesce(QUIESCE_SILENCE, QUIESCE_DEADLINE).unwrap());
    open_the_tree(&mut engine);

    // the cursor is in the tree's window, so this is a file arriving
    // where view drew its surface
    engine.arm_and_input(":edit README.md<CR>").unwrap();
    assert!(engine.quiesce(QUIESCE_SILENCE, QUIESCE_DEADLINE).unwrap());
    engine
        .arm_and_input(":call setline(1, 'edited and not written')<CR>")
        .unwrap();
    assert!(engine.quiesce(QUIESCE_SILENCE, QUIESCE_DEADLINE).unwrap());

    assert_eq!(
        engine
            .eval_str("expand('%:t') . ':' . &modified . ':' . winnr('$')")
            .unwrap()
            .trim(),
        "README.md:1:2",
        "the file did not land in the tree's window with edits in it"
    );
    assert_eq!(
        engine.eval_str("getline(1)").unwrap().trim(),
        "edited and not written",
        "the edits in the person's own buffer are gone"
    );
    assert_eq!(
        engine.focus(),
        view_core::model::Focus::Engine,
        "keys still reach the tree while the cursor sits in a file"
    );
    // the window's own grid, which is where its text lives under
    // multigrid: the screen this harness rasters is the outer grid and
    // view's own chrome over it
    let text = engine.grid_screens();
    assert!(
        text.iter().any(|(id, screen)| *id != GLOBAL_GRID
            && screen
                .rows
                .first()
                .is_some_and(|row| row.contains("edited and not written"))),
        "view paints the tree over the file the person is reading: {text:#?}"
    );
    assert!(
        !engine.tree_is_open(),
        "the tree's state outlived the window it was drawn in"
    );
    assert_eq!(
        tree_window_widths(&mut engine),
        "",
        "the tree still holds a window of its own"
    );
    assert_eq!(
        view_buffers(&mut engine),
        "",
        "the tree's scratch buffer outlived the window it was drawn in"
    );
    assert_eq!(
        engine
            .eval_str("&number . ',' . &signcolumn . ',' . &winfixwidth")
            .unwrap()
            .trim(),
        "1,auto,0",
        "the person reads their file in a window still wearing the \
         tree's look"
    );

    engine
        .feed(view_core::msg::Msg::FeatureInvoke {
            feature: "tree".to_string(),
            verb: "toggle".to_string(),
        })
        .expect("the tree opens again");
    assert!(engine.quiesce(QUIESCE_SILENCE, QUIESCE_DEADLINE).unwrap());
    assert_eq!(
        engine.eval_str("winnr('$')").unwrap().trim(),
        "3",
        "the tree reopened into a window it no longer owns"
    );
    assert_ne!(
        tree_window_widths(&mut engine),
        "",
        "the next `<leader>e` opened no tree at all"
    );
}

/// The window the close hands back is the person's, and the callback the
/// open armed has no business firing in it again. It outlived the close
/// once: the next file opened there walked the whole hand-back, wrote
/// nvim's global values into a window view no longer owned, and reported
/// a surface that had no window.
///
/// The options themselves are not the reading here. nvim drops a
/// `setlocal` window value when the window takes another buffer, so what
/// a stale hand-back writes and what nvim writes cannot be told apart
/// from the option alone. The group's absence can.
///
/// Disconfirm: dropping the `nvim_del_augroup_by_name` call from
/// `CLOSE_NATIVE_WINDOW_CHUNK` leaves the group armed, which fails on the
/// group still existing after the close.
#[test]
fn the_close_takes_the_trees_autocommand_with_it() {
    let work = common::ScratchPaths::new("close-battery-handed-back-look");
    let dir = build_fixture(&work.isolated_home);
    let mut engine = windowed_tree_session(&dir);
    engine.arm_and_input(":e README.md<CR>").unwrap();
    assert!(engine.quiesce(QUIESCE_SILENCE, QUIESCE_DEADLINE).unwrap());
    close_the_tree_alone_on_its_tabpage(&mut engine);
    assert_ordinary_window(&mut engine);
    assert_eq!(
        engine
            .eval_str("exists('#view_native_tree') . ':' . len(get(g:, 'view_native_windows', {}))")
            .unwrap()
            .trim(),
        "0:0",
        "the close left the tree's own autocommand armed in a window that \
         is the person's again"
    );

    engine.arm_and_input(":edit notes.txt<CR>").unwrap();
    assert!(engine.quiesce(QUIESCE_SILENCE, QUIESCE_DEADLINE).unwrap());
    assert_eq!(
        engine
            .eval_str("expand('%:t') . ':' . &winfixwidth . ':' . winnr('$')")
            .unwrap()
            .trim(),
        "notes.txt:0:1",
        "view touched a window it had handed back"
    );
    assert!(
        !engine.tree_is_open(),
        "a window that is nobody's surface reported itself taken"
    );
    assert_eq!(
        view_buffers(&mut engine),
        "",
        "the tree came back in a window the person is reading a file in"
    );
}

/// The other way the entry can be gone: nothing ran the close chunk, so
/// the augroup is still armed, and the person's own edit landed in the
/// tree's window while `vim.g.view_native_windows` held no entry for it
/// at all. The callback's first question is whether the entry still
/// names this window, and a `nil` entry answers that exactly as a
/// changed one does: nothing here is view's window to hand back.
///
/// Disconfirm: a callback that skips its entry check runs the hand-back
/// on a `nil` entry regardless, which fails on the width pin the tree
/// set staying put.
#[test]
fn a_callback_whose_entry_is_gone_touches_nothing() {
    let work = common::ScratchPaths::new("close-battery-tree-entry-gone");
    let dir = build_fixture(&work.isolated_home);
    let mut engine = windowed_tree_session(&dir);
    open_the_tree(&mut engine);
    assert_eq!(
        engine.eval_str("&winfixwidth").unwrap().trim(),
        "1",
        "the tree's own window did not open pinned"
    );

    // the close chunk never ran: the augroup is still armed and the
    // entry is gone by a route other than the hand-back itself
    engine
        .arm_and_input(":let g:view_native_windows = {}<CR>")
        .unwrap();
    assert!(engine.quiesce(QUIESCE_SILENCE, QUIESCE_DEADLINE).unwrap());
    engine
        .arm_and_input(":set number wrap signcolumn=yes<CR>")
        .unwrap();
    assert!(engine.quiesce(QUIESCE_SILENCE, QUIESCE_DEADLINE).unwrap());

    engine.arm_and_input(":edit README.md<CR>").unwrap();
    assert!(engine.quiesce(QUIESCE_SILENCE, QUIESCE_DEADLINE).unwrap());
    assert_eq!(
        engine.eval_str("&winfixwidth").unwrap().trim(),
        "1",
        "the callback ran the hand-back on an entry that was gone"
    );
    assert_eq!(
        engine
            .eval_str("exists('#view_native_tree#BufWinEnter')")
            .unwrap()
            .trim(),
        "0",
        "the callback did not delete itself"
    );
    assert!(
        engine.tree_is_open(),
        "the model was told the tree was taken though nothing checked \
         it out"
    );
}

/// A file whose filetype carries a look of its own, opened in the tree's
/// window. nvim fires `FileType` while the buffer loads, before the
/// window is handed back, so the hand-back's restore from the globals had
/// the last word and the person read the file without the look their own
/// config gives that filetype.
///
/// Disconfirm: dropping the `nvim_exec_autocmds('FileType', ...)` call
/// from the chunks' shared helper leaves the window reading the global
/// value, which fails against the control window beside it.
#[test]
fn a_file_taken_into_the_trees_window_keeps_its_filetypes_look() {
    let work = common::ScratchPaths::new("close-battery-ftplugin-look");
    let dir = build_fixture(&work.isolated_home);
    let mut engine = windowed_tree_session(&dir);
    engine.arm_and_input(":e README.md<CR>").unwrap();
    assert!(engine.quiesce(QUIESCE_SILENCE, QUIESCE_DEADLINE).unwrap());
    // what an ftplugin does, in the one line a test can type
    engine
        .arm_and_input(":autocmd FileType text setlocal nonumber<CR>")
        .unwrap();
    assert!(engine.quiesce(QUIESCE_SILENCE, QUIESCE_DEADLINE).unwrap());
    engine.arm_and_input(":set number<CR>").unwrap();
    assert!(engine.quiesce(QUIESCE_SILENCE, QUIESCE_DEADLINE).unwrap());
    open_the_tree(&mut engine);

    engine.arm_and_input(":edit notes.txt<CR>").unwrap();
    assert!(engine.quiesce(QUIESCE_SILENCE, QUIESCE_DEADLINE).unwrap());
    assert_eq!(
        engine
            .eval_str("&filetype . ':' . &number . ':' . &g:number")
            .unwrap()
            .trim(),
        "text:0:1",
        "the file is read with nvim's global value over the one the \
         person's config gives its filetype"
    );

    // the same filetype in a window that was never the tree's: a second
    // file, because `FileType` fires as a buffer loads and a window on an
    // already-loaded one takes its neighbour's values instead
    engine
        .arm_and_input(":call writefile(['plain'], 'other.txt')<CR>")
        .unwrap();
    assert!(engine.quiesce(QUIESCE_SILENCE, QUIESCE_DEADLINE).unwrap());
    engine.arm_and_input(":vsplit other.txt<CR>").unwrap();
    assert!(engine.quiesce(QUIESCE_SILENCE, QUIESCE_DEADLINE).unwrap());
    assert_eq!(
        engine.eval_str("&filetype . ':' . &number").unwrap().trim(),
        "text:0",
        "the control window disagrees with the handed-back one, so this \
         leg is measuring something other than the hand-back"
    );
}

/// `cycle_surfaces` stepped three times against the pinned nvim, under
/// `panes = "tiles"` at the battery's own geometry: the tree's window
/// opens on the step that carries it to `windowed`, closes on the step
/// that carries it to `overlay`, and the tree's own state -- what
/// `engine.tree_is_open()` answers for -- never closes with it.
///
/// The invoke, not the raw `<leader>uw` chord: this crate has no real
/// terminal to encode a keystroke from, so the chord's own arrival is
/// `view-core`'s `Msg::FeatureInvoke { feature: "ui", verb: "cycle_surfaces" }`
/// -- the message the registered nvim mapping sends back, and the one
/// `EngineSession::feed`'s own doc names as the front door for exactly
/// this case.
#[test]
fn cycle_surfaces_moves_the_trees_window_against_real_nvim() {
    let work = common::ScratchPaths::new("close-battery-cycle-tree");
    let dir = build_fixture(&work.isolated_home);
    let mut engine = windowed_tree_session(&dir);
    open_the_tree(&mut engine);
    let opened_at = tree_window_widths(&mut engine);
    assert_ne!(
        opened_at, "",
        "the tree never opened a window to begin with"
    );

    // ring position 1: windowed, where the tree already sat -- no window
    // opens or closes
    engine
        .feed(view_core::msg::Msg::FeatureInvoke {
            feature: "ui".to_string(),
            verb: "cycle_surfaces".to_string(),
        })
        .expect("the ring answers its own invoke");
    assert!(engine.quiesce(QUIESCE_SILENCE, QUIESCE_DEADLINE).unwrap());
    assert_eq!(
        tree_window_widths(&mut engine),
        opened_at,
        "ring position 1 (windowed) must leave the tree in its own window"
    );
    assert!(engine.tree_is_open());

    // ring position 2: overlay -- the tree's window closes and nvim is
    // left with the one window it started the battery with
    engine
        .feed(view_core::msg::Msg::FeatureInvoke {
            feature: "ui".to_string(),
            verb: "cycle_surfaces".to_string(),
        })
        .expect("the ring answers its own invoke");
    assert!(engine.quiesce(QUIESCE_SILENCE, QUIESCE_DEADLINE).unwrap());
    assert_eq!(
        nvim_window_sizes(&mut engine).len(),
        1,
        "ring position 2 (overlay) must close the tree's own window"
    );
    assert_eq!(
        tree_window_widths(&mut engine),
        "",
        "the tree's scratch window is still on screen under overlay"
    );
    assert!(
        engine.tree_is_open(),
        "the tree's own state must not close with its window"
    );

    // ring position 0 (config): this fixture's tree was never given a
    // `[ui.surfaces.tree]` of its own, so config is overlay too and this
    // step is a no-op -- proven by nvim's window count holding at one
    engine
        .feed(view_core::msg::Msg::FeatureInvoke {
            feature: "ui".to_string(),
            verb: "cycle_surfaces".to_string(),
        })
        .expect("the ring answers its own invoke");
    assert!(engine.quiesce(QUIESCE_SILENCE, QUIESCE_DEADLINE).unwrap());
    assert_eq!(
        nvim_window_sizes(&mut engine).len(),
        1,
        "ring position 0 (config, also overlay here) must open no window"
    );
    assert!(engine.tree_is_open());
}

/// A tiled session on `dir` with every surface left at its shipped
/// default (`Overlay`) -- the ring's `config` stop, and where the tree,
/// the agent panel and the notification stream all start.
fn overlay_session(dir: &Path) -> view_oracle::EngineSession {
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
    // this driver builds its model directly rather than through a
    // `view.toml`, whose absent `[native] palette` key still means "on"
    // (`enabled()`'s own default) -- the same default `view_session`'s
    // comment above names for the PTY-driven tests
    engine.enable_palette();
    engine
}

/// The row `getwininfo()` reports for the window holding a surface's own
/// scratch buffer (`view-<id>`'s filetype), or `None` while it has none.
fn native_window_row(engine: &mut view_oracle::EngineSession, id: &str) -> Option<i64> {
    let out = engine
        .eval_str(&format!(
            "join(map(filter(getwininfo(), \
             'getbufvar(v:val.bufnr, \"&filetype\") ==# \"view-{id}\"'), \
             'v:val.winrow'), \",\")"
        ))
        .expect("nvim answers for its own windows");
    out.trim().split(',').next()?.parse().ok()
}

/// A surface's own window, as `getwininfo()` reports it: 1-based `row`/
/// `col` and cell `width`/`height`, or `None` while it holds no window.
#[derive(Debug, Clone, Copy)]
struct NativeWindowRect {
    row: i64,
    col: i64,
    width: i64,
    height: i64,
}

/// [`native_window_row`], reading `wincol`/`width`/`height` alongside
/// `winrow`, for a live pin that reads back an edge rather than a row
/// alone: a default windowed position (a left or right column, a top or
/// bottom band) is a column and an extent, not a single coordinate.
fn native_window_rect(
    engine: &mut view_oracle::EngineSession,
    id: &str,
) -> Option<NativeWindowRect> {
    let out = engine
        .eval_str(&format!(
            "join(map(filter(getwininfo(), \
             'getbufvar(v:val.bufnr, \"&filetype\") ==# \"view-{id}\"'), \
             'v:val.winrow . \",\" . v:val.wincol . \",\" . v:val.width . \",\" . v:val.height'), \
             \";\")"
        ))
        .expect("nvim answers for its own windows");
    let first = out.trim().split(';').next()?;
    let mut parts = first.split(',');
    Some(NativeWindowRect {
        row: parts.next()?.parse().ok()?,
        col: parts.next()?.parse().ok()?,
        width: parts.next()?.parse().ok()?,
        height: parts.next()?.parse().ok()?,
    })
}

/// The palette's own tile, as the view-computed layer paints it -- no
/// nvim window backs it (see [`native_window_rect`]'s doc for the other
/// three surfaces, which still open real ones), so its rect is read off
/// the same [`view_surface::Surface`] the terminal paints from.
///
/// `LayerKind::Palette` is also what the notification stream's history
/// overlay paints through (`layer_kind`'s own doc in `view-surface`), so a
/// `Palette` layer titled anything but the cmdline's own is that overlay's,
/// not the command palette's.
fn palette_layer_rect(engine: &mut view_oracle::EngineSession) -> Option<view_surface::Rect> {
    engine
        .surface()
        .layers
        .into_iter()
        .find_map(|layer| match layer.kind {
            view_surface::LayerKind::Palette(view)
                if view.title != view_core::native::palette::MESSAGE_HISTORY_TITLE =>
            {
                Some(layer.rect)
            }
            _ => None,
        })
}

/// Against real nvim: the tree, the agent panel and the
/// notification stream are open as overlays (the ring's `config` stop),
/// with the keyboard in the buffer throughout -- nothing here ever enters
/// any of the three. One `cycle_surfaces` invoke to `windowed` opens all
/// three in the same fold, each carrying its own surface's own generation;
/// a second invoke to `overlay` must close all three real windows it
/// opened, which a shared generation counter cannot do (the second and
/// third replies answer a generation the counter has already moved past,
/// so their handles are never claimed, the ring's own close finds nothing
/// to close, and their scratch windows outlive the step that was meant to
/// take them away).
///
/// Disconfirm: reverting `SurfaceState::generation` to one shared `u64`
/// leaves two of the three windows standing after the second invoke.
#[test]
fn a_ring_step_opens_and_closes_three_surfaces_windows_against_real_nvim() {
    let work = common::ScratchPaths::new("close-battery-ring-three");
    let dir = build_fixture(&work.isolated_home);
    let mut engine = overlay_session(&dir);

    engine
        .feed(view_core::msg::Msg::FeatureInvoke {
            feature: "tree".to_string(),
            verb: "toggle".to_string(),
        })
        .expect("the tree opens as an overlay");
    engine.trust_ai();
    engine
        .feed(view_core::msg::Msg::FeatureInvoke {
            feature: "ai".to_string(),
            verb: "open".to_string(),
        })
        .expect("the agent panel opens as an overlay");
    engine
        .feed(view_core::msg::Msg::FeatureInvoke {
            feature: "notifications".to_string(),
            verb: "history".to_string(),
        })
        .expect("the notification stream opens as an overlay");
    assert!(engine.quiesce(QUIESCE_SILENCE, QUIESCE_DEADLINE).unwrap());
    assert_eq!(
        nvim_window_sizes(&mut engine).len(),
        1,
        "an overlay must draw over the buffer, never split a window for it"
    );

    // ring position 1: windowed -- all three open in the same fold
    engine
        .feed(view_core::msg::Msg::FeatureInvoke {
            feature: "ui".to_string(),
            verb: "cycle_surfaces".to_string(),
        })
        .expect("the ring answers its own invoke");
    assert!(engine.quiesce(QUIESCE_SILENCE, QUIESCE_DEADLINE).unwrap());
    assert_eq!(
        nvim_window_sizes(&mut engine).len(),
        4,
        "the tree, the agent panel and the stream must each claim a window \
         of their own: {:?}",
        nvim_window_sizes(&mut engine)
    );
    assert!(engine.tree_is_open());
    assert!(engine.agent_is_open());
    let focus = engine.focus();
    assert!(
        !matches!(
            focus,
            view_core::model::Focus::Pane(
                view_core::native::geometry::NativeSurface::Tree
                    | view_core::native::geometry::NativeSurface::Agent
                    | view_core::native::geometry::NativeSurface::Notifications
            )
        ),
        "a ring step opened three windows nobody asked to visit and left \
         the keyboard in one of them: {focus:?}"
    );

    // ring position 2: overlay -- every real window the step above opened
    // must close, or a shared generation's dropped reply leaves it orphaned
    engine
        .feed(view_core::msg::Msg::FeatureInvoke {
            feature: "ui".to_string(),
            verb: "cycle_surfaces".to_string(),
        })
        .expect("the ring answers its own invoke");
    assert!(engine.quiesce(QUIESCE_SILENCE, QUIESCE_DEADLINE).unwrap());
    assert_eq!(
        nvim_window_sizes(&mut engine).len(),
        1,
        "every window the ring opened for a surface must close with it, \
         not just the last one whose reply a shared generation counter \
         still answered to: {:?}",
        nvim_window_sizes(&mut engine)
    );
    assert!(engine.tree_is_open(), "the tree's own state must survive");
    assert!(
        engine.agent_is_open(),
        "the agent panel's own state must survive"
    );
}

/// Against real nvim: the tree and the agent panel share an edge
/// (`anchor = "left"` on both), windowed in an order that puts the agent
/// panel's window in nvim before the tree's -- the case a stacking rule
/// keyed on open order alone gets backwards, since design puts the tree
/// above the agent panel whichever one opened first.
///
/// Disconfirm: reverting the open chunk's stacking rule to "the new window
/// always goes below the standing one" passes this leg with the open
/// order reversed and fails it here.
#[test]
fn the_tree_stacks_above_the_agent_panel_however_they_open_against_real_nvim() {
    let work = common::ScratchPaths::new("close-battery-stack-order");
    let dir = build_fixture(&work.isolated_home);
    let mut engine = overlay_session(&dir);
    engine.set_surface(
        view_core::native::geometry::NativeSurface::Tree,
        view_core::native::geometry::SurfaceLayout::new(
            view_core::native::geometry::SurfacePlacement::Windowed,
            view_core::native::geometry::Anchor::Left,
            30,
        ),
    );
    engine.set_surface(
        view_core::native::geometry::NativeSurface::Agent,
        view_core::native::geometry::SurfaceLayout::new(
            view_core::native::geometry::SurfacePlacement::Windowed,
            view_core::native::geometry::Anchor::Left,
            30,
        ),
    );
    engine.trust_ai();

    // the agent panel opens first, the tree second -- the ordering design
    // says must still end with the tree on top
    engine
        .feed(view_core::msg::Msg::FeatureInvoke {
            feature: "ai".to_string(),
            verb: "open".to_string(),
        })
        .expect("the agent panel's window opens");
    assert!(engine.quiesce(QUIESCE_SILENCE, QUIESCE_DEADLINE).unwrap());
    engine
        .feed(view_core::msg::Msg::FeatureInvoke {
            feature: "tree".to_string(),
            verb: "toggle".to_string(),
        })
        .expect("the tree's window opens");
    assert!(engine.quiesce(QUIESCE_SILENCE, QUIESCE_DEADLINE).unwrap());

    let tree_row = native_window_row(&mut engine, "tree").expect("the tree has a window");
    let agent_row = native_window_row(&mut engine, "agent").expect("the agent panel has a window");
    assert!(
        tree_row < agent_row,
        "the tree must sit above the agent panel on a shared edge \
         whichever one opened first: tree={tree_row} agent={agent_row}"
    );
}

/// Against real nvim, from a default session with no `[ui.surfaces]` table
/// touched: one ring step to `windowed` (a `cycle_surfaces` invoke from a
/// fresh `Model::new()`) opens the tree, the agent panel and the notification
/// stream at the position `default_windowed_anchor` names for each --
/// nothing here sets a surface's layout by hand, so a stale fallback that
/// silently mapped every anchor onto a left sidebar (the defect a config
/// word could not reach either) would land all three on the same edge
/// instead of the tree alone on the left. A following `:` opens the
/// palette's own tile, at the bottom edge its own default names.
#[test]
fn a_ring_step_opens_every_default_surface_at_its_designed_windowed_position_against_real_nvim() {
    let work = common::ScratchPaths::new("close-battery-ring-default-positions");
    let dir = build_fixture(&work.isolated_home);
    let mut engine = overlay_session(&dir);

    engine
        .feed(view_core::msg::Msg::FeatureInvoke {
            feature: "tree".to_string(),
            verb: "toggle".to_string(),
        })
        .expect("the tree opens as an overlay");
    engine.trust_ai();
    engine
        .feed(view_core::msg::Msg::FeatureInvoke {
            feature: "ai".to_string(),
            verb: "open".to_string(),
        })
        .expect("the agent panel opens as an overlay");
    engine
        .feed(view_core::msg::Msg::FeatureInvoke {
            feature: "notifications".to_string(),
            verb: "history".to_string(),
        })
        .expect("the notification stream opens as an overlay");
    assert!(engine.quiesce(QUIESCE_SILENCE, QUIESCE_DEADLINE).unwrap());

    engine
        .feed(view_core::msg::Msg::FeatureInvoke {
            feature: "ui".to_string(),
            verb: "cycle_surfaces".to_string(),
        })
        .expect("the ring answers its own invoke");
    assert!(engine.quiesce(QUIESCE_SILENCE, QUIESCE_DEADLINE).unwrap());

    let tree = native_window_rect(&mut engine, "tree").expect("the tree claims a window");
    let agent = native_window_rect(&mut engine, "agent").expect("the agent panel claims a window");
    let stream =
        native_window_rect(&mut engine, "notifications").expect("the stream claims a window");
    assert_eq!(
        tree.col, 1,
        "the tree's default windowed anchor is the left edge: {tree:?}"
    );
    // the outer ring and the frame's own gap already sit inside `COLS`
    // (`docs/tiled-ui.md` names up to six columns of that gap), so a right
    // edge is graded against a margin of ten rather than the raw terminal
    // width
    let right_margin = 10;
    assert!(
        agent.col + agent.width >= i64::from(COLS) - right_margin,
        "the agent panel's default windowed anchor is the right edge: \
         {agent:?} against {COLS} columns"
    );
    assert!(
        stream.col + stream.width >= i64::from(COLS) - right_margin,
        "the notification stream's default windowed anchor is the right \
         edge, stacked with the agent panel: {stream:?} against {COLS} \
         columns"
    );

    engine.arm_and_input(":").unwrap();
    assert!(engine.quiesce(QUIESCE_SILENCE, QUIESCE_DEADLINE).unwrap());
    let palette = palette_layer_rect(&mut engine).expect("the palette claims a tile");
    // the statusline and the cmdline row nvim always reserves sit below the
    // last row the tile's own rect reaches, the same margin the right
    // edge check above grants the outer ring and the frame's own gap
    assert!(
        i64::from(palette.row) + i64::from(palette.height) >= i64::from(ROWS) - right_margin,
        "the palette's default windowed anchor is the bottom edge: \
         {palette:?} against {ROWS} rows"
    );
}

/// A replay of all four surfaces through every ring stop -- windowed,
/// overlay, back to each one's own configured placement -- against real
/// nvim, with the keyboard never once landing in any of the four. At every
/// stop this asserts both halves of a surface's claim: the model's own
/// `*_is_open()`, and either nvim's own window (present at the windowed
/// stop, gone at the other two, since `panes = "tiles"` configures every
/// surface as an overlay by default) or, for the palette, the placement its
/// own view-computed tile paints at. The tree, the agent panel and the
/// stream are held open across every stop; the palette is opened fresh at
/// each one and left with `<Esc>` before the ring moves again -- its own
/// tile paints for exactly as long as nvim's cmdline stays open, so nothing
/// here races a `CloseNativeWindow` the palette no longer issues. The close
/// at the end must leave nothing behind: no surface open, no window but the
/// user's own buffer.
#[test]
fn a_ring_cycles_all_four_surfaces_through_every_position_against_real_nvim() {
    let work = common::ScratchPaths::new("close-battery-ring-all-four");
    let dir = build_fixture(&work.isolated_home);
    let mut engine = overlay_session(&dir);

    engine
        .feed(view_core::msg::Msg::FeatureInvoke {
            feature: "tree".to_string(),
            verb: "toggle".to_string(),
        })
        .expect("the tree opens as an overlay");
    engine.trust_ai();
    engine
        .feed(view_core::msg::Msg::FeatureInvoke {
            feature: "ai".to_string(),
            verb: "open".to_string(),
        })
        .expect("the agent panel opens as an overlay");
    engine
        .feed(view_core::msg::Msg::FeatureInvoke {
            feature: "notifications".to_string(),
            verb: "history".to_string(),
        })
        .expect("the notification stream opens as an overlay");
    assert!(engine.quiesce(QUIESCE_SILENCE, QUIESCE_DEADLINE).unwrap());
    assert!(engine.tree_is_open());
    assert!(engine.agent_is_open());
    assert!(engine.notifications_is_open());

    let stops = ["windowed", "overlay", "config"];
    for stop in stops {
        engine
            .feed(view_core::msg::Msg::FeatureInvoke {
                feature: "ui".to_string(),
                verb: "cycle_surfaces".to_string(),
            })
            .expect("the ring answers its own invoke");
        assert!(engine.quiesce(QUIESCE_SILENCE, QUIESCE_DEADLINE).unwrap());
        engine.arm_and_input(":").unwrap();
        assert!(engine.quiesce(QUIESCE_SILENCE, QUIESCE_DEADLINE).unwrap());

        assert!(engine.tree_is_open(), "{stop}: the tree's claim must hold");
        assert!(
            engine.agent_is_open(),
            "{stop}: the agent panel's claim must hold"
        );
        assert!(
            engine.notifications_is_open(),
            "{stop}: the stream's claim must hold"
        );
        assert!(
            engine.palette_is_open(),
            "{stop}: the palette's claim must hold"
        );

        let windowed = stop == "windowed";
        assert_eq!(
            native_window_rect(&mut engine, "tree").is_some(),
            windowed,
            "{stop}: the tree's own nvim window must exist only at the \
             windowed stop"
        );
        assert_eq!(
            native_window_rect(&mut engine, "agent").is_some(),
            windowed,
            "{stop}: the agent panel's own nvim window must exist only at \
             the windowed stop"
        );
        assert_eq!(
            native_window_rect(&mut engine, "notifications").is_some(),
            windowed,
            "{stop}: the stream's own nvim window must exist only at the \
             windowed stop"
        );
        assert_eq!(
            engine.palette_windowed_active(),
            windowed,
            "{stop}: the palette's own tile must paint windowed only at the \
             windowed stop"
        );

        let focus = engine.focus();
        assert!(
            !matches!(
                focus,
                view_core::model::Focus::Pane(
                    view_core::native::geometry::NativeSurface::Tree
                        | view_core::native::geometry::NativeSurface::Agent
                        | view_core::native::geometry::NativeSurface::Notifications
                )
            ),
            "{stop}: a ring step must never land the keyboard in the tree, \
             the agent panel or the stream -- the cmdline the user is \
             already typing into is the only one of the four allowed to \
             hold it: {focus:?}"
        );

        engine.arm_and_input("<Esc>").unwrap();
        assert!(engine.quiesce(QUIESCE_SILENCE, QUIESCE_DEADLINE).unwrap());
        assert!(
            !engine.palette_is_open(),
            "{stop}: escape must close the palette before the next ring \
             step, or its held-open window races the async close every \
             other surface uses"
        );
        assert!(
            palette_layer_rect(&mut engine).is_none(),
            "{stop}: escape must take the palette's tile off the next \
             painted frame"
        );
    }

    engine
        .feed(view_core::msg::Msg::FeatureInvoke {
            feature: "tree".to_string(),
            verb: "toggle".to_string(),
        })
        .expect("the tree closes");
    engine
        .feed(view_core::msg::Msg::FeatureInvoke {
            feature: "ai".to_string(),
            verb: "close".to_string(),
        })
        .expect("the agent panel closes");
    engine
        .feed(view_core::msg::Msg::FeatureInvoke {
            feature: "notifications".to_string(),
            verb: "history".to_string(),
        })
        .expect("the stream closes");
    assert!(engine.quiesce(QUIESCE_SILENCE, QUIESCE_DEADLINE).unwrap());

    assert!(!engine.tree_is_open(), "the tree must be fully closed");
    assert!(
        !engine.agent_is_open(),
        "the agent panel must be fully closed"
    );
    assert!(
        !engine.notifications_is_open(),
        "the stream must be fully closed"
    );
    assert!(
        !engine.palette_is_open(),
        "the palette must be fully closed"
    );
    assert_eq!(
        nvim_window_sizes(&mut engine).len(),
        1,
        "every surface's window must be gone, leaving only the user's own \
         buffer: {:?}",
        nvim_window_sizes(&mut engine)
    );
}

/// The measured cost of one ring step -- the median over 30 steps, all
/// four surfaces open at the ring's `config` stop (the heaviest of the
/// three: every surface an overlay, so a step to `windowed` opens four
/// real windows in one fold) -- since a step's own close-then-reopen work
/// scales with how many surfaces it carries, not just whether one moved.
#[test]
fn the_cost_of_one_ring_step_with_all_four_surfaces_open() {
    let work = common::ScratchPaths::new("close-battery-ring-step-latency");
    let dir = build_fixture(&work.isolated_home);
    let mut engine = overlay_session(&dir);

    engine
        .feed(view_core::msg::Msg::FeatureInvoke {
            feature: "tree".to_string(),
            verb: "toggle".to_string(),
        })
        .expect("the tree opens as an overlay");
    engine.trust_ai();
    engine
        .feed(view_core::msg::Msg::FeatureInvoke {
            feature: "ai".to_string(),
            verb: "open".to_string(),
        })
        .expect("the agent panel opens as an overlay");
    engine
        .feed(view_core::msg::Msg::FeatureInvoke {
            feature: "notifications".to_string(),
            verb: "history".to_string(),
        })
        .expect("the notification stream opens as an overlay");
    assert!(engine.quiesce(QUIESCE_SILENCE, QUIESCE_DEADLINE).unwrap());

    let mut samples = Vec::with_capacity(30);
    for _ in 0..30 {
        let start = Instant::now();
        engine
            .feed(view_core::msg::Msg::FeatureInvoke {
                feature: "ui".to_string(),
                verb: "cycle_surfaces".to_string(),
            })
            .expect("the ring answers its own invoke");
        assert!(engine.quiesce(LATENCY_SILENCE, QUIESCE_DEADLINE).unwrap());
        samples.push(start.elapsed());
    }
    samples.sort_unstable();

    let baseline = median_noop_latency(&mut overlay_session(&dir));

    eprintln!(
        "median ring-step latency, four surfaces open: {:?}, harness \
         baseline (same surfaces, no step): {baseline:?}",
        samples[samples.len() / 2]
    );
}

/// The median wall time of the same quiesce this file's latency tests
/// settle on, with a key that moves nothing -- the harness's own round-trip
/// cost, isolated from any surface actually opening or closing, so the ring
/// step's own cost is the difference between the two medians.
fn median_noop_latency(engine: &mut view_oracle::EngineSession) -> Duration {
    let mut samples = Vec::with_capacity(30);
    for _ in 0..30 {
        let start = Instant::now();
        engine.arm_and_input("<Esc>").unwrap();
        assert!(engine.quiesce(LATENCY_SILENCE, QUIESCE_DEADLINE).unwrap());
        samples.push(start.elapsed());
    }
    samples.sort_unstable();
    samples[samples.len() / 2]
}

/// The ticker's own tile, against real nvim under `panes = "tiles"` at the
/// battery's own geometry: `<leader>fm` opening it (`enter = true`, the
/// toggle a person pressed) both claims the model's overlay and lands the
/// cursor in a bottom-anchored window nvim pins with `winfixheight`;
/// closing it drops both the window and the model's claim, and hands the
/// keyboard back to whatever was nvim's current window before the ticker
/// ever opened, never left dangling on the tile's own closed handle.
#[test]
fn the_ticker_opens_and_closes_its_own_window_against_real_nvim() {
    let work = common::ScratchPaths::new("close-battery-ticker-live");
    let dir = build_fixture(&work.isolated_home);
    let mut engine = overlay_session(&dir);
    engine.set_surface(
        view_core::native::geometry::NativeSurface::Notifications,
        view_core::native::geometry::SurfaceLayout::new(
            view_core::native::geometry::SurfacePlacement::Windowed,
            view_core::native::geometry::Anchor::Bottom,
            30,
        ),
    );
    let before_current = engine.eval_str("win_getid()").unwrap().trim().to_string();

    engine
        .feed(view_core::msg::Msg::FeatureInvoke {
            feature: "notifications".to_string(),
            verb: "history".to_string(),
        })
        .expect("the ticker's window opens");
    assert!(engine.quiesce(QUIESCE_SILENCE, QUIESCE_DEADLINE).unwrap());

    assert!(
        engine.notifications_is_open(),
        "the model must claim the ticker's overlay once it is open"
    );
    let rect =
        native_window_rect(&mut engine, "notifications").expect("the ticker claims a window");
    assert!(
        rect.row + rect.height <= i64::from(ROWS),
        "a bottom-anchored ticker must sit inside the terminal's own rows: \
         {rect:?} against {ROWS} rows"
    );
    let ticker_id = engine.eval_str("win_getid()").unwrap().trim().to_string();
    assert_eq!(
        engine.eval_str("&winfixheight").unwrap().trim(),
        "1",
        "the ticker's own window must pin its height so a sibling split \
         cannot steal its rows"
    );
    assert_eq!(
        engine.eval_str("&filetype").unwrap().trim(),
        "view-notifications",
        "the toggle that opened the ticker must have entered it, the same \
         way the tree and the agent panel's own toggle keys do"
    );

    engine
        .feed(view_core::msg::Msg::FeatureInvoke {
            feature: "notifications".to_string(),
            verb: "history".to_string(),
        })
        .expect("the ticker's window closes");
    assert!(engine.quiesce(QUIESCE_SILENCE, QUIESCE_DEADLINE).unwrap());

    assert!(
        !engine.notifications_is_open(),
        "the model's claim must not outlive the window it named"
    );
    assert!(
        native_window_rect(&mut engine, "notifications").is_none(),
        "the ticker's window must be gone from nvim too"
    );
    let after_current = engine.eval_str("win_getid()").unwrap().trim().to_string();
    assert_eq!(
        after_current, before_current,
        "the close must hand the keyboard back to the window the ticker \
         was opened from, not leave it on the tile's own closed handle: \
         {ticker_id} closed"
    );
}

/// The windowed stream against real nvim: `<Esc>` leaves it for the window
/// that held focus before it opened, the same `wincmd p` the design names,
/// without closing the stream's own window; and a window command the
/// stream's own resize chord does not claim (`<C-w>w`, cycling back in)
/// still reaches nvim rather than being swallowed by
/// [`view_core::update::surfaces::notifications_pane_key`]'s message-key
/// fallback.
#[test]
fn a_windowed_stream_leaves_focus_on_escape_and_forwards_a_window_command_against_real_nvim() {
    let work = common::ScratchPaths::new("close-battery-stream-window-cmds");
    let dir = build_fixture(&work.isolated_home);
    let mut engine = overlay_session(&dir);
    engine.set_surface(
        view_core::native::geometry::NativeSurface::Notifications,
        view_core::native::geometry::SurfaceLayout::new(
            view_core::native::geometry::SurfacePlacement::Windowed,
            view_core::native::geometry::Anchor::Bottom,
            30,
        ),
    );
    let before_current = engine.eval_str("win_getid()").unwrap().trim().to_string();

    engine
        .feed(view_core::msg::Msg::FeatureInvoke {
            feature: "notifications".to_string(),
            verb: "history".to_string(),
        })
        .expect("the stream's window opens");
    assert!(engine.quiesce(QUIESCE_SILENCE, QUIESCE_DEADLINE).unwrap());
    assert_eq!(
        engine.eval_str("&filetype").unwrap().trim(),
        "view-notifications",
        "opening the stream must enter its own window"
    );
    let stream_id = engine.eval_str("win_getid()").unwrap().trim().to_string();

    engine
        .feed(view_core::msg::Msg::Key(view_core::msg::Key {
            notation: "<Esc>".to_string(),
        }))
        .expect("escape leaves the stream's window");
    assert!(engine.quiesce(QUIESCE_SILENCE, QUIESCE_DEADLINE).unwrap());

    assert_eq!(
        engine.eval_str("win_getid()").unwrap().trim(),
        before_current,
        "escape must hand focus back to the window the stream was opened \
         from, the way wincmd p does"
    );
    assert!(
        native_window_rect(&mut engine, "notifications").is_some(),
        "escape leaves the stream's window in place; it must not close it"
    );

    engine
        .eval_str(&format!("win_gotoid({stream_id})"))
        .expect("nvim answers the jump back into the stream");
    assert!(engine.quiesce(QUIESCE_SILENCE, QUIESCE_DEADLINE).unwrap());
    assert_eq!(
        engine.eval_str("&filetype").unwrap().trim(),
        "view-notifications",
        "the jump back must land in the stream's own window again"
    );

    engine
        .feed(view_core::msg::Msg::Key(view_core::msg::Key {
            notation: "<C-w>".to_string(),
        }))
        .expect("the window-command prefix reaches nvim armed, not swallowed");
    engine
        .feed(view_core::msg::Msg::Key(view_core::msg::Key {
            notation: "w".to_string(),
        }))
        .expect("the follower nvim answers as wincmd w reaches nvim too");
    assert!(engine.quiesce(QUIESCE_SILENCE, QUIESCE_DEADLINE).unwrap());

    assert_ne!(
        engine.eval_str("win_getid()").unwrap().trim(),
        stream_id,
        "<C-w>w is not the stream's own resize chord, so nvim must have \
         answered it and cycled the window itself"
    );
}

/// The windowed tree against real nvim: `<C-w>w`, which the tree's own
/// resize chord does not claim, must still leave the tree's window and
/// cycle nvim's own -- the tree used to swallow the whole chord with
/// nothing reaching nvim, so the cursor never left.
#[test]
fn a_windowed_tree_forwards_an_unclaimed_window_command_against_real_nvim() {
    let work = common::ScratchPaths::new("close-battery-tree-window-cmds");
    let dir = build_fixture(&work.isolated_home);
    let mut engine = windowed_tree_session(&dir);

    open_the_tree(&mut engine);
    let tree_id = engine.eval_str("win_getid()").unwrap().trim().to_string();

    engine
        .feed(view_core::msg::Msg::Key(view_core::msg::Key {
            notation: "<C-w>".to_string(),
        }))
        .expect("the window-command prefix is held, not swallowed");
    engine
        .feed(view_core::msg::Msg::Key(view_core::msg::Key {
            notation: "w".to_string(),
        }))
        .expect("the follower reaches nvim together with the prefix");
    assert!(engine.quiesce(QUIESCE_SILENCE, QUIESCE_DEADLINE).unwrap());

    assert_ne!(
        engine.eval_str("&filetype").unwrap().trim(),
        "view-tree",
        "<C-w>w must leave the tree's own window"
    );
    assert_ne!(
        engine.eval_str("win_getid()").unwrap().trim(),
        tree_id,
        "<C-w>w is not the tree's own resize chord, so nvim must have \
         cycled the window itself"
    );
}

/// [`a_windowed_tree_forwards_an_unclaimed_window_command_against_real_nvim`],
/// for the agent panel.
#[test]
fn a_windowed_agent_panel_forwards_an_unclaimed_window_command_against_real_nvim() {
    let work = common::ScratchPaths::new("close-battery-agent-window-cmds");
    let dir = build_fixture(&work.isolated_home);
    let mut engine = overlay_session(&dir);
    engine.set_surface(
        view_core::native::geometry::NativeSurface::Agent,
        view_core::native::geometry::SurfaceLayout::new(
            view_core::native::geometry::SurfacePlacement::Windowed,
            view_core::native::geometry::Anchor::Right,
            30,
        ),
    );
    engine.trust_ai();
    engine
        .feed(view_core::msg::Msg::FeatureInvoke {
            feature: "ai".to_string(),
            verb: "open".to_string(),
        })
        .expect("the agent panel's window opens");
    assert!(engine.quiesce(QUIESCE_SILENCE, QUIESCE_DEADLINE).unwrap());
    let agent_id = engine.eval_str("win_getid()").unwrap().trim().to_string();

    engine
        .feed(view_core::msg::Msg::Key(view_core::msg::Key {
            notation: "<C-w>".to_string(),
        }))
        .expect("the window-command prefix is held, not swallowed");
    engine
        .feed(view_core::msg::Msg::Key(view_core::msg::Key {
            notation: "w".to_string(),
        }))
        .expect("the follower reaches nvim together with the prefix");
    assert!(engine.quiesce(QUIESCE_SILENCE, QUIESCE_DEADLINE).unwrap());

    assert_ne!(
        engine.eval_str("&filetype").unwrap().trim(),
        "view-agent",
        "<C-w>w must leave the agent panel's own window"
    );
    assert_ne!(
        engine.eval_str("win_getid()").unwrap().trim(),
        agent_id,
        "<C-w>w is not the agent panel's own resize chord, so nvim must \
         have cycled the window itself"
    );
}

/// The agent panel and the notification stream stacked on the same right
/// edge: a resize key pressed in the focused one (the stream, entered
/// last) has to carry its new share to the sibling too -- in the model,
/// which [`sync_stacked_siblings`] owns, and in nvim, where the two
/// windows share one column because `retile_open_surface`'s own split
/// opens the second inside the first's own window (see
/// [`sync_stacked_siblings`]'s doc for why nvim's own width already moves
/// on either window's resize and only the model's copy can drift).
#[test]
fn resizing_one_windowed_sidebar_carries_its_width_to_a_sibling_stacked_beside_it_against_real_nvim(
) {
    let work = common::ScratchPaths::new("close-battery-stacked-resize");
    let dir = build_fixture(&work.isolated_home);
    let mut engine = overlay_session(&dir);
    engine.set_surface(
        view_core::native::geometry::NativeSurface::Agent,
        view_core::native::geometry::SurfaceLayout::new(
            view_core::native::geometry::SurfacePlacement::Windowed,
            view_core::native::geometry::Anchor::Right,
            30,
        ),
    );
    engine.set_surface(
        view_core::native::geometry::NativeSurface::Notifications,
        view_core::native::geometry::SurfaceLayout::new(
            view_core::native::geometry::SurfacePlacement::Windowed,
            view_core::native::geometry::Anchor::Right,
            30,
        ),
    );
    engine.trust_ai();
    engine
        .feed(view_core::msg::Msg::FeatureInvoke {
            feature: "ai".to_string(),
            verb: "open".to_string(),
        })
        .expect("the agent panel opens its own window");
    assert!(engine.quiesce(QUIESCE_SILENCE, QUIESCE_DEADLINE).unwrap());
    engine
        .feed(view_core::msg::Msg::FeatureInvoke {
            feature: "notifications".to_string(),
            verb: "history".to_string(),
        })
        .expect("the stream opens its own window, stacked with the agent panel's");
    assert!(engine.quiesce(QUIESCE_SILENCE, QUIESCE_DEADLINE).unwrap());
    assert_eq!(
        engine.eval_str("&filetype").unwrap().trim(),
        "view-notifications",
        "the stream's own toggle must have entered its window"
    );

    engine
        .feed(view_core::msg::Msg::Key(view_core::msg::Key {
            notation: "<S-Right>".to_string(),
        }))
        .expect("the resize key reaches the focused stream");
    assert!(engine.quiesce(QUIESCE_SILENCE, QUIESCE_DEADLINE).unwrap());

    let agent = native_window_rect(&mut engine, "agent").expect("the agent panel claims a window");
    let stream =
        native_window_rect(&mut engine, "notifications").expect("the stream claims a window");
    assert_eq!(
        agent.width, stream.width,
        "nvim's own column is shared, so both windows must report the same \
         width after either one resizes: agent {agent:?}, stream {stream:?}"
    );

    // nvim's own column stays shared mechanically whatever the model
    // does, so the width equality above holds even when the model's own
    // copy of the agent panel's share goes stale -- the layout read here
    // is what would drift and feed a reopen the wrong number, which is
    // the half only `sync_stacked_siblings` guards
    let agent_layout = engine.surface_layout(view_core::native::geometry::NativeSurface::Agent);
    let stream_layout =
        engine.surface_layout(view_core::native::geometry::NativeSurface::Notifications);
    assert_eq!(
        agent_layout.size, stream_layout.size,
        "the model's own layout for the sibling must follow the resized \
         share too, not just nvim's window: agent {agent_layout:?}, stream \
         {stream_layout:?}"
    );
}

/// A terminal with no room for the tree's own vertical split beside the
/// user's window, which raises E36 ("No room") from `topleft vsplit`
/// partway through the open chunk's body. Left at the tree's default
/// (overlay) placement: the toggle below opens it with no nvim window of
/// its own, so the only `OpenNativeWindow` this session ever sends is the
/// one a later ring step raises with `enter = false`.
///
/// The ring's own anchor for the tree is always `left` or `right`
/// (`accepted_anchors`), never `bottom`, so this terminal's 12x3 shape
/// alone does not starve a vertical split of room the way it does the
/// horizontal one the old toggle-driven leg forced through an anchor the
/// tree never actually accepts. `winminwidth` is what a real "no room"
/// vertical split comes from instead: raised past half the terminal's own
/// width, `topleft vsplit` cannot satisfy it for either side and refuses
/// with the same E36 the horizontal case raised.
fn tiny_overlay_tree_session(dir: &Path) -> view_oracle::EngineSession {
    let mut engine =
        view_oracle::EngineSession::spawn_with_ext(12, 3, view_oracle::UI_EXT_OPTIONS_MULTIGRID)
            .expect("EngineSession::spawn_with_ext against real nvim");
    assert!(engine.quiesce(QUIESCE_SILENCE, QUIESCE_DEADLINE).unwrap());
    engine
        .arm_and_input(&format!(":cd {}<CR>", dir.display()))
        .unwrap();
    assert!(engine.quiesce(QUIESCE_SILENCE, QUIESCE_DEADLINE).unwrap());
    engine
        .arm_and_input(":set winwidth=99 winminwidth=99<CR>")
        .unwrap();
    assert!(engine.quiesce(QUIESCE_SILENCE, QUIESCE_DEADLINE).unwrap());
    engine
}

/// A split nvim refuses for lack of room used to leave `eventignore`
/// set for the rest of the session, since the option was written before
/// the `vim.cmd` that could raise and restored only on the success path.
/// The open chunk now runs the whole split-and-configure step under one
/// `pcall`, so the refusal still restores it.
///
/// The toggle only opens the tree as an overlay (`enter = true` never
/// writes `eventignore` at all, see `native_window.rs`'s own `if not
/// enter` guard) -- it is here for the window-count baseline. The ring
/// step that follows is the one path that actually reaches the guarded
/// `enter = false` open this leg exists to prove: a retile from overlay
/// to windowed, into a terminal with no room for the split.
#[test]
fn opening_a_windowed_surface_with_no_room_for_the_split_leaves_eventignore_untouched_against_real_nvim(
) {
    let work = common::ScratchPaths::new("close-battery-no-room-open");
    let dir = build_fixture(&work.isolated_home);
    let mut engine = tiny_overlay_tree_session(&dir);

    engine
        .feed(view_core::msg::Msg::FeatureInvoke {
            feature: "tree".to_string(),
            verb: "toggle".to_string(),
        })
        .expect("the toggle opens the tree as an overlay, no window of its own");
    assert!(engine.quiesce(QUIESCE_SILENCE, QUIESCE_DEADLINE).unwrap());
    assert_eq!(
        nvim_window_sizes(&mut engine).len(),
        1,
        "an overlay tree must draw over the buffer, never split a window for it"
    );

    engine
        .feed(view_core::msg::Msg::FeatureInvoke {
            feature: "ui".to_string(),
            verb: "cycle_surfaces".to_string(),
        })
        .expect("the ring step sends its own open request into a terminal with no room for it");
    assert!(engine.quiesce(QUIESCE_SILENCE, QUIESCE_DEADLINE).unwrap());

    assert_eq!(
        nvim_window_sizes(&mut engine).len(),
        1,
        "nvim split its own window even though there was no room for the tree's"
    );
    assert_eq!(
        engine.eval_str("&eventignore").unwrap().trim(),
        "",
        "a split nvim refused left eventignore set for the rest of the session"
    );
}

/// The other reachable path to the same leak: a ring step or a mapped key can fire
/// `OpenNativeWindow` while the user is inside the command-line window
/// (`q:`), where `botright split` raises E11 rather than E36. The chunk
/// refuses outright (`getcmdwintype() ~= ''`) before it ever sets
/// `eventignore`, so this leg never even reaches the pcall the one above
/// does.
///
/// The tree opens as an overlay before the command-line window is
/// entered, the same as the E36 leg above: the toggle's `enter = true`
/// open never writes `eventignore` at all, so only the ring step's
/// `enter = false` retile, taken from inside `q:`, actually exercises the
/// guard this leg is named for.
#[test]
fn opening_a_windowed_surface_from_inside_the_command_line_window_leaves_eventignore_untouched_against_real_nvim(
) {
    let work = common::ScratchPaths::new("close-battery-cmdwin-open");
    let dir = build_fixture(&work.isolated_home);
    let mut engine = overlay_session(&dir);

    engine
        .feed(view_core::msg::Msg::FeatureInvoke {
            feature: "tree".to_string(),
            verb: "toggle".to_string(),
        })
        .expect("the toggle opens the tree as an overlay, no window of its own");
    assert!(engine.quiesce(QUIESCE_SILENCE, QUIESCE_DEADLINE).unwrap());

    engine.arm_and_input("q:").unwrap();
    assert!(engine.quiesce(QUIESCE_SILENCE, QUIESCE_DEADLINE).unwrap());
    assert_eq!(
        engine.eval_str("getcmdwintype()").unwrap().trim(),
        ":",
        "the session never entered the command-line window"
    );
    let before = nvim_window_sizes(&mut engine).len();

    engine
        .feed(view_core::msg::Msg::FeatureInvoke {
            feature: "ui".to_string(),
            verb: "cycle_surfaces".to_string(),
        })
        .expect("the ring step sends its own open request from inside the command-line window");
    assert!(engine.quiesce(QUIESCE_SILENCE, QUIESCE_DEADLINE).unwrap());

    assert_eq!(
        nvim_window_sizes(&mut engine).len(),
        before,
        "the tree's window opened inside the command-line window"
    );

    engine.arm_and_input(":q<CR>").unwrap();
    assert!(engine.quiesce(QUIESCE_SILENCE, QUIESCE_DEADLINE).unwrap());
    assert_eq!(
        engine.eval_str("&eventignore").unwrap().trim(),
        "",
        "a refused open left eventignore set for the rest of the session"
    );
}

/// E36 and E11 both refuse the split itself, before the chunk ever holds a
/// window handle. This leg forces the other shape: a split that succeeds,
/// then a failure on the window the split just made -- `winfixbuf`, primed
/// by a one-shot `WinNew` autocmd on the new window before the chunk's own
/// `nvim_win_set_buf` runs, raises `E1513` deterministically. A window a
/// failure like this leaves behind is what `native_window.rs`'s failure arm
/// now closes.
#[test]
fn a_post_split_failure_on_the_new_window_leaves_no_window_behind_against_real_nvim() {
    let work = common::ScratchPaths::new("close-battery-post-split-failure");
    let dir = build_fixture(&work.isolated_home);
    let mut engine = windowed_tree_session(&dir);
    let before = nvim_window_sizes(&mut engine).len();

    engine
        .arm_and_input(":autocmd WinNew * ++once setlocal winfixbuf<CR>")
        .unwrap();
    assert!(engine.quiesce(QUIESCE_SILENCE, QUIESCE_DEADLINE).unwrap());

    engine
        .feed(view_core::msg::Msg::FeatureInvoke {
            feature: "tree".to_string(),
            verb: "toggle".to_string(),
        })
        .expect("the toggle sends its own open request into the primed autocmd");
    assert!(engine.quiesce(QUIESCE_SILENCE, QUIESCE_DEADLINE).unwrap());

    assert_eq!(
        nvim_window_sizes(&mut engine).len(),
        before,
        "the split the chunk could not finish configuring must not survive \
         the failure that stopped it"
    );
    assert_eq!(
        engine.eval_str("winnr('$')").unwrap().trim(),
        before.to_string(),
        "winnr('$') must read exactly what it did before the failed open"
    );
}
