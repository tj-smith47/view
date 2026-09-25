//! Against real nvim: `window fit` sizes the focused tile to the longest
//! line it shows, and `[ui] fit_active` does the same on every window the
//! cursor enters.
//!
//! Every width is the window's layout slot, read back from nvim with
//! `nvim_win_get_width()` after the verb went through `update()` and the
//! effect loop, so the arithmetic under test is the chunk nvim ran. The slot
//! is what the fit sets; `winwidth()` reads the grid view asks for inside
//! it, which follows only once view has seen the slot move. The expected
//! width is derived here from the case's own inputs: the number column is
//! `numberwidth`'s default of 4 and the frame's inset is read off the
//! session's look.

#![cfg(unix)]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::time::Duration;

use view_core::msg::Msg;
use view_oracle::{EngineSession, UI_EXT_OPTIONS_MULTIGRID};

/// Wide enough that the widest case, a 200-column line with the number
/// column and a frame, fits beside a squeezed neighbour.
const COLS: u16 = 250;
const ROWS: u16 = 30;
const QUIESCE_SILENCE: Duration = Duration::from_millis(200);
const QUIESCE_DEADLINE: Duration = Duration::from_secs(10);

/// nvim's own `winwidth` default, the floor a fitted text area never goes
/// under.
const WINWIDTH: usize = 20;

/// The columns `number` adds with `numberwidth` at its default.
const NUMBER_COLUMN: usize = 4;

fn settle(engine: &mut EngineSession, step: &str) {
    assert!(
        engine.quiesce(QUIESCE_SILENCE, QUIESCE_DEADLINE).unwrap(),
        "{step}: the session never settled"
    );
}

fn keys(engine: &mut EngineSession, notation: &str) {
    engine.arm_and_input(notation).unwrap();
    settle(engine, notation);
}

fn invoke(engine: &mut EngineSession, feature: &str, verb: &str) {
    engine
        .feed(Msg::FeatureInvoke {
            feature: feature.to_string(),
            verb: verb.to_string(),
        })
        .unwrap();
}

/// Runs an ex command and waits for nvim to have run it. The command must
/// hold no single quote.
fn ex(engine: &mut EngineSession, cmd: &str) {
    engine.eval_str(&format!("execute('{cmd}')")).unwrap();
}

/// The current window's slot width.
fn slot(engine: &mut EngineSession) -> usize {
    let text = engine.eval_str("nvim_win_get_width(0)").unwrap();
    text.parse()
        .unwrap_or_else(|_| panic!("the slot width answered {text:?}"))
}

/// Every window's slot width, in window-number order, which is left to
/// right for side-by-side windows.
fn widths(engine: &mut EngineSession) -> Vec<usize> {
    let text = engine
        .eval_str("join(map(range(1, winnr('$')), 'nvim_win_get_width(win_getid(v:val))'), ',')")
        .unwrap();
    text.split(',').map(|w| w.parse().unwrap()).collect()
}

/// Replaces the current buffer's text with `lines`, each given as a run of
/// `x` of that many columns.
fn set_lines(engine: &mut EngineSession, lines: &[usize]) {
    let list: Vec<String> = lines.iter().map(|n| format!("repeat('x', {n})")).collect();
    engine.eval_str("deletebufline('%', 1, '$')").unwrap();
    engine
        .eval_str(&format!("setline(1, [{}])", list.join(", ")))
        .unwrap();
}

/// Two side-by-side windows under tiles, the cursor in the left one.
fn session(gaps: bool) -> EngineSession {
    let mut engine = EngineSession::spawn_with_ext(COLS, ROWS, UI_EXT_OPTIONS_MULTIGRID)
        .expect("EngineSession against real nvim");
    settle(&mut engine, "attach");
    keys(&mut engine, ":vsplit<CR>");
    engine.set_panes("tiles").unwrap();
    settle(&mut engine, "tiles");
    if !gaps {
        invoke(&mut engine, "ui", "gaps");
        settle(&mut engine, "gapless");
    }
    assert_eq!(engine.model().look.gaps, gaps);
    engine
}

fn inset(engine: &EngineSession) -> usize {
    usize::from(engine.model().look.inset().1)
}

/// The width a fit gives, from the case's own inputs.
fn expected_width(longest: usize, tw: usize, cc: &str, numbered: bool, inset: usize) -> usize {
    let mut target = (longest + 1).max(WINWIDTH);
    // a relative colorcolumn is counted from textwidth, which is 0 here
    let absolute = !cc.starts_with(['+', '-']);
    let cap = if tw > 0 {
        Some(tw)
    } else {
        cc.parse::<usize>().ok().filter(|_| absolute)
    };
    if let Some(cap) = cap {
        target = target.min(cap);
    }
    target + if numbered { NUMBER_COLUMN } else { 0 } + 2 * inset
}

/// The walk the arithmetic table names: every line length on both sides
/// of each cap, both textwidths, a colorcolumn that is absolute, relative
/// or absent, with and without the number column, gapped and gapless.
#[test]
fn fit_widens_to_the_longest_visible_line_within_its_cap() {
    for gaps in [true, false] {
        let mut engine = session(gaps);
        let inset = inset(&engine);
        assert_eq!(inset, usize::from(gaps), "the frame's columns a side");
        let mut checked = 0;
        for longest in [5, 40, 79, 81, 200] {
            for tw in [0, 72] {
                for cc in ["", "81", "+1"] {
                    for numbered in [false, true] {
                        ex(&mut engine, "wincmd =");
                        set_lines(&mut engine, &[3, longest]);
                        let nu = if numbered { "number" } else { "nonumber" };
                        ex(&mut engine, &format!("setlocal tw={tw} cc={cc} {nu}"));
                        invoke(&mut engine, "window", "fit");
                        let got = slot(&mut engine);
                        let want = expected_width(longest, tw, cc, numbered, inset);
                        assert_eq!(
                            got, want,
                            "longest {longest}, tw {tw}, cc {cc:?}, {nu}, gaps {gaps}"
                        );
                        checked += 1;
                    }
                }
            }
        }
        assert_eq!(checked, 60);

        // a line scrolled out of view is not measured, and one scrolled
        // into view is
        ex(&mut engine, "wincmd =");
        ex(&mut engine, "setlocal tw=0 cc= nonumber");
        let mut lines = vec![10; 99];
        lines.push(150);
        set_lines(&mut engine, &lines);
        ex(&mut engine, "normal! gg");
        invoke(&mut engine, "window", "fit");
        assert_eq!(
            slot(&mut engine),
            expected_width(10, 0, "", false, inset),
            "the 150-column line is below the window's last row"
        );
        ex(&mut engine, "normal! G");
        invoke(&mut engine, "window", "fit");
        assert_eq!(
            slot(&mut engine),
            expected_width(150, 0, "", false, inset),
            "the 150-column line is on screen now"
        );
    }
}

/// A fixed-width sidebar keeps its width when the tile beside it is
/// fitted, and a fit from inside the sidebar or a terminal moves nothing.
#[test]
fn fit_leaves_winfixwidth_sidebars_alone() {
    let mut engine = session(true);
    let inset = inset(&engine);
    keys(&mut engine, ":vsplit<CR>");
    keys(
        &mut engine,
        "<C-w>t:setlocal winfixwidth<CR>:vertical resize 30<CR><C-w>l",
    );
    set_lines(&mut engine, &[40]);
    ex(&mut engine, "setlocal nonumber tw=0 cc=");
    let before = widths(&mut engine);
    assert_eq!(before[0], 30, "the sidebar's own width: {before:?}");

    invoke(&mut engine, "window", "fit");
    let after = widths(&mut engine);
    assert_eq!(after[0], 30, "the sidebar moved: {before:?} -> {after:?}");
    assert_eq!(after[1], expected_width(40, 0, "", false, inset));
    assert_ne!(
        after[2], before[2],
        "the tile on the far side takes the rest"
    );

    // from inside the sidebar the fit has nothing it may size
    keys(&mut engine, "<C-w>t");
    invoke(&mut engine, "window", "fit");
    assert_eq!(
        widths(&mut engine),
        after,
        "a fit from the sidebar moved a tile"
    );

    // nor from a terminal, whose lines are the program's own
    keys(&mut engine, "<C-w>b:terminal true<CR>");
    assert_eq!(engine.eval_str("&buftype").unwrap(), "terminal");
    let terminal = widths(&mut engine);
    invoke(&mut engine, "window", "fit");
    assert_eq!(
        widths(&mut engine),
        terminal,
        "a fit from a terminal moved a tile"
    );
}

/// With `noequalalways` nvim takes the columns from the neighbour and the
/// tile past it stays put; with `equalalways` both share what is left.
#[test]
fn fit_under_noequalalways_moves_only_the_neighbour() {
    let mut engine = session(true);
    let inset = inset(&engine);
    keys(&mut engine, ":vsplit<CR>");
    set_lines(&mut engine, &[30]);
    ex(&mut engine, "setlocal nonumber tw=0 cc=");
    let fitted = expected_width(30, 0, "", false, inset);

    for equalalways in [false, true] {
        let option = if equalalways {
            "equalalways"
        } else {
            "noequalalways"
        };
        ex(&mut engine, &format!("set {option}"));
        ex(&mut engine, "wincmd =");
        keys(&mut engine, "<C-w>t");
        let before = widths(&mut engine);
        invoke(&mut engine, "window", "fit");
        let after = widths(&mut engine);
        assert_eq!(after[0], fitted, "{option}: {before:?} -> {after:?}");
        assert_ne!(
            after[1], before[1],
            "{option}: the neighbour gives the columns"
        );
        if equalalways {
            assert_ne!(after[2], before[2], "{option}: the far tile shares them");
            assert!(
                after[1].abs_diff(after[2]) <= 1,
                "{option}: the two others are not equal: {after:?}"
            );
        } else {
            assert_eq!(after[2], before[2], "{option}: the far tile moved");
        }
    }
}

/// `fit_active` fits the window the cursor enters, leaves a zoomed layout
/// zoomed when the cursor comes back from a float, and stops once it is
/// turned off.
#[test]
fn fit_active_fits_on_enter_and_skips_a_zoomed_layout() {
    let mut engine = session(true);
    let inset = inset(&engine);
    ex(&mut engine, "setglobal nonumber tw=0 cc=");
    set_lines(&mut engine, &[30]);
    keys(&mut engine, "<C-w>l:enew<CR>");
    set_lines(&mut engine, &[60]);
    ex(&mut engine, "wincmd =");
    keys(&mut engine, "<C-w>h");
    let even = widths(&mut engine);

    engine.set_fit_active(true).unwrap();
    keys(&mut engine, "<C-w>l");
    assert_eq!(
        slot(&mut engine),
        expected_width(60, 0, "", false, inset),
        "entering the right tile fits it: {even:?} -> {:?}",
        widths(&mut engine)
    );
    keys(&mut engine, "<C-w>h");
    let left = expected_width(30, 0, "", false, inset);
    assert_eq!(slot(&mut engine), left);

    // a float entered and closed over an unzoomed tile fits it again
    ex(&mut engine, "vertical resize 90");
    let float = ":lua vim.api.nvim_open_win(0, true, \
                 { relative = 'editor', row = 2, col = 2, width = 20, height = 3 })<CR>\
                 :close<CR>";
    keys(&mut engine, float);
    assert_eq!(
        slot(&mut engine),
        left,
        "coming back from the float refits an unzoomed tile"
    );

    // the same round trip over a zoomed tile leaves the zoom standing
    keys(&mut engine, "<C-w>_<C-w>|");
    let zoomed = widths(&mut engine);
    assert!(zoomed[1] <= 1, "the zoom squeezed the sibling: {zoomed:?}");
    keys(&mut engine, float);
    assert_eq!(widths(&mut engine), zoomed, "the float undid the zoom");

    engine.set_fit_active(false).unwrap();
    ex(&mut engine, "wincmd =");
    let off = widths(&mut engine);
    keys(&mut engine, "<C-w>l<C-w>h");
    assert_eq!(widths(&mut engine), off, "a fit ran with fit_active off");
}
