//! Against real nvim: a border drag and the resize mode move the windows
//! either side of a `:vsplit`, read back with `nvim_win_get_width()` after
//! the input went through `update()` and the effect loop.

#![cfg(unix)]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::time::Duration;

use view_core::msg::{Key, MouseInput, Msg};
use view_oracle::{EngineSession, UI_EXT_OPTIONS_MULTIGRID};

const COLS: u16 = 80;
const ROWS: u16 = 24;
const QUIESCE_SILENCE: Duration = Duration::from_millis(200);
const QUIESCE_DEADLINE: Duration = Duration::from_secs(10);

fn settle(engine: &mut EngineSession, step: &str) {
    assert!(
        engine.quiesce(QUIESCE_SILENCE, QUIESCE_DEADLINE).unwrap(),
        "{step}: the session never settled"
    );
}

/// Both windows' widths, left first.
fn widths(engine: &mut EngineSession) -> Vec<u16> {
    let text = engine
        .eval_str("join(map(range(1, winnr('$')), 'nvim_win_get_width(win_getid(v:val))'), ',')")
        .unwrap();
    text.split(',').map(|w| w.parse().unwrap()).collect()
}

/// Both windows' heights as `winheight()` reads them, top first.
fn heights(engine: &mut EngineSession) -> Vec<u16> {
    let text = engine
        .eval_str("join(map(range(1, winnr('$')), 'winheight(v:val)'), ',')")
        .unwrap();
    text.split(',').map(|h| h.parse().unwrap()).collect()
}

/// Two side-by-side windows under nvim's own look, the cursor in the left.
fn vsplit() -> EngineSession {
    split(":vsplit<CR>")
}

/// Two windows `command` lays out under nvim's own look, after `setup`.
fn split_after(setup: &str, command: &str) -> EngineSession {
    let mut engine = EngineSession::spawn_with_ext(COLS, ROWS, UI_EXT_OPTIONS_MULTIGRID)
        .expect("EngineSession against real nvim");
    settle(&mut engine, "attach");
    if !setup.is_empty() {
        engine.arm_and_input(setup).unwrap();
        settle(&mut engine, setup);
    }
    engine.arm_and_input(command).unwrap();
    settle(&mut engine, command);
    engine
}

fn split(command: &str) -> EngineSession {
    split_after("", command)
}

fn mouse(engine: &mut EngineSession, action: &str, row: u16, col: u16) {
    engine
        .feed(Msg::Mouse(MouseInput {
            button: "left".into(),
            action: action.into(),
            modifier: String::new(),
            row,
            col,
        }))
        .unwrap();
    settle(engine, action);
}

fn key(engine: &mut EngineSession, notation: &str) {
    engine
        .feed(Msg::Key(Key {
            notation: notation.to_string(),
        }))
        .unwrap();
    settle(engine, notation);
}

#[test]
fn dragging_the_separator_resizes_both_windows() {
    let mut engine = vsplit();
    let before = widths(&mut engine);
    assert_eq!(before.len(), 2, "{before:?}");
    let separator = before[0];
    let row = engine.model().chrome_rows() + 3;
    mouse(&mut engine, "press", row, separator);
    mouse(&mut engine, "drag", row, separator + 3);
    mouse(&mut engine, "drag", row, separator + 5);
    mouse(&mut engine, "release", row, separator + 5);
    let after = widths(&mut engine);
    assert_eq!(
        after,
        [before[0] + 5, before[1] - 5],
        "the separator moved five columns right: {before:?} -> {after:?}"
    );
}

/// Drags the status line between two stacked windows two rows down.
fn drag_the_status_line_down(engine: &mut EngineSession) {
    let before = heights(engine);
    assert_eq!(before.len(), 2, "{before:?}");
    // the row above the lower window's first, the winbar's where it has one
    let lower_top: u16 = engine
        .eval_str("win_screenpos(2)[0]")
        .unwrap()
        .parse()
        .unwrap();
    let row = engine.model().chrome_rows() + lower_top - 2;
    let col = 10;
    mouse(engine, "press", row, col);
    mouse(engine, "drag", row + 1, col);
    mouse(engine, "drag", row + 2, col);
    mouse(engine, "release", row + 2, col);
    let after = heights(engine);
    assert_eq!(
        after,
        [before[0] + 2, before[1] - 2],
        "the status line moved two rows down: {before:?} -> {after:?}"
    );
}

#[test]
fn dragging_the_status_line_resizes_both_stacked_windows() {
    let mut engine = split(":split<CR>");
    drag_the_status_line_down(&mut engine);
}

#[test]
fn a_winbar_moves_the_status_line_by_the_rows_dragged() {
    let mut engine = split_after(":set winbar=%f<CR>", ":split<CR>");
    drag_the_status_line_down(&mut engine);
}

#[test]
fn a_counted_step_in_the_resize_mode_widens_the_current_window() {
    let mut engine = vsplit();
    let before = widths(&mut engine);
    engine
        .feed(Msg::FeatureInvoke {
            generation: None,
            feature: "window".to_string(),
            verb: "resize_mode".to_string(),
        })
        .unwrap();
    key(&mut engine, "3");
    key(&mut engine, "l");
    key(&mut engine, "<Esc>");
    assert!(
        engine.model().resize_mode().is_none(),
        "<Esc> left the mode"
    );
    // three steps of 5% of 80 columns
    let after = widths(&mut engine);
    assert_eq!(
        after,
        [before[0] + 12, before[1] - 12],
        "{before:?} -> {after:?}"
    );
}
