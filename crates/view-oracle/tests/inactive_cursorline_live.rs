//! Against real nvim: the cursor row of a window the cursor has left
//! carries exactly the attributes nvim sends for it, under every grid mode
//! and gap setting.
//!
//! `cursorline` set for every window lights the cursor row of each one,
//! current or not, and nvim draws it that way. view writes no user option
//! and adds nothing to a window grid, so its row has to equal the reference
//! applier's, attached to a second nvim driven by the same keys. A config
//! that clears the line on `WinLeave` has nvim redraw the row plain, and a
//! lit row left in view after that is an attribute the grid apply kept on a
//! row nvim rewrote.
#![cfg(unix)]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::time::Duration;

use view_core::events::WinHandle;
use view_core::grid::registry::GridId;
use view_oracle::{
    EngineSession, GridScreens, Probe, ReferenceSession, MULTIGRID_NAME, UI_EXT_OPTIONS,
    UI_EXT_OPTIONS_MULTIGRID,
};

const COLS: u16 = 80;
const ROWS: u16 = 20;
const QUIESCE_SILENCE: Duration = Duration::from_millis(200);
const QUIESCE_DEADLINE: Duration = Duration::from_secs(10);

/// How many times each leg moves the focus after the layout is built.
const FOCUS_MOVES: usize = 3;

/// A buffer of its own in each window, three lines each, the cursor on the
/// middle one: the lit row sits between two plain ones in both windows.
const SPLIT_SCRIPT: &str = ":set cursorline<CR>:vsplit<CR>:enew<CR>\
    ileft one<CR>left two<CR>left three<Esc>k<C-w>w\
    :enew<CR>iright one<CR>right two<CR>right three<Esc>k";

/// The config shape that hides the line in a window the cursor leaves.
const WINLEAVE_SCRIPT: &str = ":autocmd WinLeave * setlocal nocursorline<CR>\
    :autocmd WinEnter * setlocal cursorline<CR>";

struct Leg {
    name: &'static str,
    ext: &'static [&'static str],
    /// `Some(gaps)` for `panes = "tiles"`, `None` for `panes = "nvim"`.
    tiles: Option<bool>,
}

const LEGS: &[Leg] = &[
    Leg {
        name: "multigrid, tiles, gaps",
        ext: UI_EXT_OPTIONS_MULTIGRID,
        tiles: Some(true),
    },
    Leg {
        name: "multigrid, tiles, gapless",
        ext: UI_EXT_OPTIONS_MULTIGRID,
        tiles: Some(false),
    },
    Leg {
        name: "multigrid, panes nvim",
        ext: UI_EXT_OPTIONS_MULTIGRID,
        tiles: None,
    },
    Leg {
        name: "single grid",
        ext: UI_EXT_OPTIONS,
        tiles: None,
    },
];

impl Leg {
    fn multigrid(&self) -> bool {
        self.ext.contains(&MULTIGRID_NAME)
    }
}

fn settle(side: &str, settled: Result<bool, view_oracle::OracleError>, step: &str) {
    assert!(settled.unwrap(), "{side}: {step} never settled");
}

/// Splits a row fingerprint into one token per cell.
fn cells(fingerprint: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut open: Option<String> = None;
    for ch in fingerprint.chars() {
        match (&mut open, ch) {
            (None, '.') => out.push(".".to_string()),
            (None, '[') => open = Some("[".to_string()),
            (Some(token), ']') => {
                token.push(']');
                out.push(std::mem::take(token));
                open = None;
            }
            (Some(token), c) => token.push(c),
            (None, c) => panic!("unexpected {c:?} in fingerprint {fingerprint:?}"),
        }
    }
    out
}

/// The tab page's non-floating windows.
fn windows(side: &mut impl Probe) -> Vec<u64> {
    side.eval_str(
        "join(filter(nvim_tabpage_list_wins(0), \
         'nvim_win_get_config(v:val).relative ==# \"\"'), ',')",
    )
    .unwrap()
    .split(',')
    .map(|w| w.parse().unwrap())
    .collect()
}

fn number(side: &mut impl Probe, expr: &str) -> usize {
    let text = side.eval_str(expr).unwrap();
    text.parse()
        .unwrap_or_else(|_| panic!("{expr} answered {text:?}"))
}

/// The attribute tokens of `win`'s rows: its own grid under multigrid, its
/// rectangle of the one grid without it.
fn window_rows(
    side: &mut impl Probe,
    grids: &GridScreens,
    grid: u64,
    multigrid: bool,
    win: u64,
) -> Vec<Vec<String>> {
    let lookup = |id: u64| {
        grids
            .iter()
            .find(|(other, _)| *other == id)
            .map(|(_, screen)| screen.attr_rows.clone())
            .unwrap_or_else(|| panic!("grid {id} is not in {grids:#?}"))
    };
    if multigrid {
        return lookup(grid).iter().map(|row| cells(row)).collect();
    }
    let top = number(side, &format!("win_screenpos({win})[0]")) - 1;
    let left = number(side, &format!("win_screenpos({win})[1]")) - 1;
    let height = number(side, &format!("winheight({win})"));
    let width = number(side, &format!("winwidth({win})"));
    lookup(1)
        .iter()
        .skip(top)
        .take(height)
        .map(|row| cells(row).into_iter().skip(left).take(width).collect())
        .collect()
}

fn cursor_row(side: &mut impl Probe, win: u64) -> usize {
    number(side, &format!("line('.', {win}) - line('w0', {win})"))
}

/// The grid view has `win` on. Without multigrid every window is on the
/// global grid.
fn view_grid(engine: &EngineSession, win: u64, multigrid: bool) -> u64 {
    if !multigrid {
        return 1;
    }
    let grids = engine.model().engine.grids();
    grids
        .window_grids()
        .into_iter()
        .find(|&grid| grids.window_handle(grid) == Some(WinHandle(win)))
        .map(|GridId(id)| id)
        .unwrap_or_else(|| panic!("no grid carries window {win}"))
}

/// The token a cell lit by `CursorLine` carries: its background.
fn cursorline_bg(engine: &mut EngineSession) -> String {
    let bg = engine
        .eval_str("synIDattr(synIDtrans(hlID('CursorLine')), 'bg#')")
        .unwrap();
    let hex = bg.trim_start_matches('#').to_ascii_lowercase();
    assert_eq!(hex.len(), 6, "CursorLine has no background: {bg:?}");
    format!("bg={hex};")
}

fn lit(row: &[String], bg: &str) -> bool {
    row.iter().any(|cell| cell.contains(bg))
}

/// Both sides with the layout built and settled. The reference attaches at
/// the size view's global grid holds under the leg's look, so every window
/// takes the same slot on both sides.
fn sides(leg: &Leg, prelude: &str) -> (EngineSession, ReferenceSession) {
    let mut engine = EngineSession::spawn_with_ext(COLS, ROWS, leg.ext).unwrap();
    settle(
        leg.name,
        engine.quiesce(QUIESCE_SILENCE, QUIESCE_DEADLINE),
        "attach",
    );
    if let Some(gaps) = leg.tiles {
        engine.set_panes("tiles").unwrap();
        settle(
            leg.name,
            engine.quiesce(QUIESCE_SILENCE, QUIESCE_DEADLINE),
            "tiles",
        );
        if !gaps {
            engine
                .feed(view_core::msg::Msg::FeatureInvoke {
                    feature: "ui".to_string(),
                    verb: "gaps".to_string(),
                })
                .unwrap();
            settle(
                leg.name,
                engine.quiesce(QUIESCE_SILENCE, QUIESCE_DEADLINE),
                "gaps",
            );
        }
        assert_eq!(engine.model().look.gaps, gaps, "{}", leg.name);
    }
    let (cols, rows) = engine
        .grid_screens()
        .iter()
        .find(|(id, _)| *id == 1)
        .map(|(_, screen)| {
            (
                screen.rows.first().map_or(0, |row| row.chars().count()),
                screen.rows.len(),
            )
        })
        .unwrap();
    let mut reference = ReferenceSession::spawn_with_ext(
        u16::try_from(cols).unwrap(),
        u16::try_from(rows).unwrap(),
        leg.ext,
    )
    .unwrap();
    settle(
        leg.name,
        reference.quiesce(QUIESCE_SILENCE, QUIESCE_DEADLINE),
        "reference attach",
    );

    let script = format!("{prelude}{SPLIT_SCRIPT}");
    engine.arm_and_input(&script).unwrap();
    reference.arm_and_input(&script).unwrap();
    settle(
        leg.name,
        engine.quiesce(QUIESCE_SILENCE, QUIESCE_DEADLINE),
        "split",
    );
    settle(
        leg.name,
        reference.quiesce(QUIESCE_SILENCE, QUIESCE_DEADLINE),
        "reference split",
    );
    (engine, reference)
}

fn move_focus(leg: &Leg, engine: &mut EngineSession, reference: &mut ReferenceSession) {
    engine.arm_and_input("<C-w>w").unwrap();
    reference.arm_and_input("<C-w>w").unwrap();
    settle(
        leg.name,
        engine.quiesce(QUIESCE_SILENCE, QUIESCE_DEADLINE),
        "focus move",
    );
    settle(
        leg.name,
        reference.quiesce(QUIESCE_SILENCE, QUIESCE_DEADLINE),
        "reference focus move",
    );
}

/// One window's rows on both sides, and where its cursor row is.
struct Pair {
    view: Vec<Vec<String>>,
    reference: Vec<Vec<String>>,
    row: usize,
}

impl Pair {
    fn read(
        leg: &Leg,
        engine: &mut EngineSession,
        reference: &mut ReferenceSession,
        win: u64,
    ) -> Self {
        let multigrid = leg.multigrid();
        let grid = view_grid(engine, win, multigrid);
        let view_grids = engine.grid_screens();
        let ref_grids = reference.grid_screens();
        let row = cursor_row(engine, win);
        assert_eq!(
            row,
            cursor_row(reference, win),
            "{}: cursor rows differ",
            leg.name
        );
        if multigrid {
            // the reference applier keeps no window placement, so the grid
            // view names for `win` has to show `win`'s own cursor line there
            let want = reference
                .eval_str(&format!("getbufline(winbufnr({win}), line('.', {win}))[0]"))
                .unwrap();
            let shown = ref_grids
                .iter()
                .find(|(id, _)| *id == grid)
                .and_then(|(_, screen)| screen.rows.get(row))
                .cloned()
                .unwrap_or_default();
            assert!(
                !want.is_empty() && shown.starts_with(&want),
                "{}: reference grid {grid} row {row} is {shown:?}, not window {win}'s line {want:?}",
                leg.name
            );
        }
        Self {
            view: window_rows(engine, &view_grids, grid, multigrid, win),
            reference: window_rows(reference, &ref_grids, grid, multigrid, win),
            row,
        }
    }

    /// The cursor row on each side, over the columns both sides hold: a
    /// tile's grid is the reference slot with the frame taken off.
    fn cursor_rows(&self) -> (Vec<String>, Vec<String>) {
        let view = self.view.get(self.row).cloned().unwrap_or_default();
        let reference = self.reference.get(self.row).cloned().unwrap_or_default();
        let width = view.len().min(reference.len());
        (
            view.into_iter().take(width).collect(),
            reference.into_iter().take(width).collect(),
        )
    }
}

#[test]
fn the_inactive_tile_row_carries_exactly_the_attrs_nvim_sends() {
    for leg in LEGS {
        let (mut engine, mut reference) = sides(leg, "");
        let bg = cursorline_bg(&mut engine);
        for step in 0..=FOCUS_MOVES {
            if step > 0 {
                move_focus(leg, &mut engine, &mut reference);
            }
            let current = u64::try_from(number(&mut engine, "win_getid()")).unwrap();
            let inactive: Vec<u64> = windows(&mut engine)
                .into_iter()
                .filter(|&win| win != current)
                .collect();
            assert!(
                !inactive.is_empty(),
                "{}: the split left one window",
                leg.name
            );
            for win in inactive {
                let pair = Pair::read(leg, &mut engine, &mut reference, win);
                let (view, nvim) = pair.cursor_rows();
                assert!(
                    lit(&nvim, &bg),
                    "{} step {step}: nvim sent no lit cursor row for window {win}: {nvim:?}",
                    leg.name
                );
                assert!(
                    !view.is_empty(),
                    "{} step {step}: window {win} has no row",
                    leg.name
                );
                assert_eq!(
                    view, nvim,
                    "{} step {step}: window {win}'s cursor row in view differs from nvim's",
                    leg.name
                );
            }
        }
    }
}

#[test]
fn a_winleave_nocursorline_config_leaves_no_lit_row_in_view() {
    for leg in LEGS {
        let (mut engine, mut reference) = sides(leg, WINLEAVE_SCRIPT);
        let bg = cursorline_bg(&mut engine);
        for step in 0..=FOCUS_MOVES {
            if step > 0 {
                move_focus(leg, &mut engine, &mut reference);
            }
            let current = u64::try_from(number(&mut engine, "win_getid()")).unwrap();
            for win in windows(&mut engine) {
                let pair = Pair::read(leg, &mut engine, &mut reference, win);
                let (view, nvim) = pair.cursor_rows();
                assert_eq!(
                    view, nvim,
                    "{} step {step}: window {win}'s cursor row in view differs from nvim's",
                    leg.name
                );
                if win == current {
                    assert!(
                        lit(&view, &bg),
                        "{} step {step}: the current window {win} shows no cursor line",
                        leg.name
                    );
                    continue;
                }
                let stale: Vec<usize> = pair
                    .view
                    .iter()
                    .enumerate()
                    .filter(|(_, row)| lit(row, &bg))
                    .map(|(index, _)| index)
                    .collect();
                assert!(
                    stale.is_empty(),
                    "{} step {step}: inactive window {win} keeps a lit row at {stale:?}",
                    leg.name
                );
            }
        }
    }
}
