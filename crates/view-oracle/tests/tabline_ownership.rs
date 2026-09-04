//! Who draws the tab row, proven against a live engine on both sides of the
//! differential and under both attach modes.
//!
//! `[native] tabline` ships off, so a config-less session never asks for
//! `ext_tabline` and nvim renders the user's own `'tabline'` -- a
//! bufferline plugin's, in a real session -- into row 0 of the grid it
//! composites. The claim is worth a live test because nothing about it is
//! visible in view's own code: the row arrives as ordinary `grid_line`
//! cells and there is no event to assert on. The negative control is the
//! same session with `ext_tabline` attached, where nvim sends
//! `tabline_update` instead and row 0 is the buffer's.
//!
//! No plugin: `'tabline'` set to a literal is the same option a bufferline
//! writes, and the fixtures carry no bufferline to install.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::time::Duration;

use view_oracle::{
    compare_grids, snapshot, EngineSession, GridScreens, ReferenceGrids, ReferenceSession,
    ViewGrids, UI_EXT_OPTIONS, UI_EXT_OPTIONS_MULTIGRID,
};

const COLS: u16 = 60;
const ROWS: u16 = 12;
const QUIESCE_SILENCE: Duration = Duration::from_millis(200);
const QUIESCE_DEADLINE: Duration = Duration::from_secs(5);

/// The tab row's content, spelled so it can only have come from `'tabline'`:
/// no buffer of this session holds it and no chrome nvim draws says it.
const TABLINE_MARK: &str = "TABROW-FROM-NVIM";

/// `'tabline'` set to a literal with the row forced visible, then text typed
/// into the buffer so a session that painted nothing at all is not mistaken
/// for one that painted the tab row.
const SCRIPT: &str = ":set showtabline=2 tabline=TABROW-FROM-NVIM<CR>ibuffer text<Esc>";

/// The global grid nvim composites chrome into, which is grid 1 under
/// either addressing.
const GLOBAL_GRID: u64 = 1;

/// Row 0 of the global grid, as both sides hold it.
fn top_row(grids: &GridScreens) -> String {
    grids
        .iter()
        .find(|(id, _)| *id == GLOBAL_GRID)
        .map(|(_, screen)| screen.rows.first().cloned().unwrap_or_default())
        .unwrap_or_else(|| panic!("no global grid in {grids:#?}"))
}

/// Both sides driven through [`SCRIPT`] under `ext`, as (view's session,
/// view's grids, the reference's grids, the divergences between the two).
///
/// The session comes back because the absence of the tab row proves nothing
/// on its own: a run where `'tabline'` never took would look the same, so
/// every leg asks the engine what the option actually holds.
fn run(
    ext: &[&str],
) -> (
    EngineSession,
    GridScreens,
    GridScreens,
    Vec<view_oracle::Divergence>,
) {
    let mut engine = EngineSession::spawn_with_ext(COLS, ROWS, ext)
        .expect("EngineSession::spawn_with_ext against real nvim");
    let mut reference = ReferenceSession::spawn_with_ext(COLS, ROWS, ext)
        .expect("ReferenceSession::spawn_with_ext against real nvim");
    assert!(engine.quiesce(QUIESCE_SILENCE, QUIESCE_DEADLINE).unwrap());
    assert!(reference
        .quiesce(QUIESCE_SILENCE, QUIESCE_DEADLINE)
        .unwrap());

    engine.arm_and_input(SCRIPT).unwrap();
    reference.arm_and_input(SCRIPT).unwrap();
    assert!(engine.quiesce(QUIESCE_SILENCE, QUIESCE_DEADLINE).unwrap());
    assert!(reference
        .quiesce(QUIESCE_SILENCE, QUIESCE_DEADLINE)
        .unwrap());

    let view_grids = engine.grid_screens();
    let ref_grids = reference.grid_screens();
    let view_state = snapshot(&mut engine).unwrap();
    let ref_state = snapshot(&mut reference).unwrap();
    let divergences = compare_grids(
        ViewGrids {
            state: &view_state,
            grids: &view_grids,
        },
        ReferenceGrids {
            state: &ref_state,
            grids: &ref_grids,
        },
    );
    assert_eq!(
        engine.eval_str("&tabline").unwrap(),
        TABLINE_MARK,
        "the option this test is about never took"
    );
    (engine, view_grids, ref_grids, divergences)
}

#[test]
fn the_shipped_attach_leaves_the_tab_row_to_nvim_under_both_addressings() {
    for (mode, ext) in [
        ("multigrid", view_oracle::ui_ext_options_shipped_multigrid()),
        ("single grid", view_oracle::ui_ext_options_shipped()),
    ] {
        assert!(
            !ext.contains(&"ext_tabline"),
            "the shipped attach under {mode} asked for the tab line itself: {ext:?}"
        );
        let (_engine, view_grids, ref_grids, divergences) = run(&ext);
        assert!(
            divergences.is_empty(),
            "the two sides disagree under {mode}: {divergences:#?}"
        );
        for (side, grids) in [("view", &view_grids), ("the reference", &ref_grids)] {
            let row = top_row(grids);
            assert!(
                row.contains(TABLINE_MARK),
                "{side} paints no tab row under {mode}, its top row is {row:?}"
            );
        }
    }
}

#[test]
fn attaching_the_tab_line_takes_the_row_off_the_grid() {
    for (mode, ext) in [
        ("multigrid", UI_EXT_OPTIONS_MULTIGRID),
        ("single grid", UI_EXT_OPTIONS),
    ] {
        assert!(
            ext.contains(&"ext_tabline"),
            "this control must attach the tab line under {mode}: {ext:?}"
        );
        let (engine, view_grids, ref_grids, divergences) = run(ext);
        assert!(
            divergences.is_empty(),
            "the two sides disagree under {mode}: {divergences:#?}"
        );
        for (side, grids) in [("view", &view_grids), ("the reference", &ref_grids)] {
            let row = top_row(grids);
            assert!(
                !row.contains(TABLINE_MARK),
                "{side} still has nvim's tab row in the grid under {mode}: {row:?}"
            );
        }
        assert_eq!(
            engine.tabline_tabs(),
            Some(1),
            "the row left the grid under {mode} and no tabline_update took its place"
        );
    }
}
