//! The falsifiable check under the multigrid attach mode: two live sides
//! driven across a vertical split agree grid by grid, and corrupting one
//! window's grid on view's side breaks that agreement with the grid named.
//!
//! Both directions are the point. A comparison that only ever runs green is
//! indistinguishable from one that compares nothing, and the way this one
//! could come to compare nothing is specific: `compare_grids` walks the ids
//! the two sides were addressed by, so a change that stopped collecting a
//! window's grid -- on either side -- would leave the state probes and the
//! global grid still agreeing and every window's content unexamined. The
//! corruption below is dropping one window grid's last row, which is what a
//! lost `grid_line` looks like after the fact.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::time::Duration;

use view_oracle::{
    compare_grids, snapshot, Divergence, EngineSession, GridScreens, ReferenceGrids,
    ReferenceSession, ViewGrids, UI_EXT_OPTIONS_MULTIGRID,
};

const COLS: u16 = 60;
const ROWS: u16 = 12;
const QUIESCE_SILENCE: Duration = Duration::from_millis(200);
const QUIESCE_DEADLINE: Duration = Duration::from_secs(5);

/// The layout both sides are driven into: a vertical split with a buffer of
/// its own in each window, so the two window grids hold different text and a
/// grid compared against the wrong one cannot pass by coincidence. The same
/// script `corpus/multigrid-vsplit-split.toml` drives, minus its horizontal
/// split, which this test has no use for.
const SPLIT_SCRIPT: &str = ":vsplit<CR>:enew<CR>ileft side<Esc><C-w>wiright side<Esc>";

/// The text typed into the right-hand window, which is how its grid is found
/// below: nvim allocates grid ids in the order it creates windows and never
/// reissues one, so an id hardcoded here would be a fact about this script's
/// history rather than about the window under test.
const RIGHT_HAND_TEXT: &str = "right side";

/// The grid holding `needle`, if any: the caller asserts on the answer
/// rather than defaulting, because a test that silently fell back to the
/// global grid would corrupt the chrome and still see a divergence, proving
/// nothing about window grids.
fn grid_holding(grids: &GridScreens, needle: &str) -> Option<u64> {
    grids
        .iter()
        .find(|(_, screen)| screen.rows.iter().any(|row| row.contains(needle)))
        .map(|(id, _)| *id)
}

/// Drops `grid`'s last row from both of its dumps, the after-the-fact shape
/// of a `grid_line` that never arrived.
fn drop_last_row(grids: &mut GridScreens, grid: u64) {
    for (id, screen) in grids.iter_mut() {
        if *id == grid {
            screen
                .rows
                .pop()
                .expect("the corrupted grid must have rows");
            screen
                .attr_rows
                .pop()
                .expect("the corrupted grid must have attr rows");
        }
    }
}

#[test]
fn a_split_layout_agrees_grid_by_grid_and_a_dropped_grid_line_names_its_grid() {
    let mut engine = EngineSession::spawn_with_ext(COLS, ROWS, UI_EXT_OPTIONS_MULTIGRID)
        .expect("EngineSession::spawn_with_ext against real nvim");
    let mut reference = ReferenceSession::spawn_with_ext(COLS, ROWS, UI_EXT_OPTIONS_MULTIGRID)
        .expect("ReferenceSession::spawn_with_ext against real nvim");
    assert!(engine.quiesce(QUIESCE_SILENCE, QUIESCE_DEADLINE).unwrap());
    assert!(reference
        .quiesce(QUIESCE_SILENCE, QUIESCE_DEADLINE)
        .unwrap());

    engine.arm_and_input(SPLIT_SCRIPT).unwrap();
    reference.arm_and_input(SPLIT_SCRIPT).unwrap();
    assert!(engine.quiesce(QUIESCE_SILENCE, QUIESCE_DEADLINE).unwrap());
    assert!(reference
        .quiesce(QUIESCE_SILENCE, QUIESCE_DEADLINE)
        .unwrap());

    let view_grids = engine.grid_screens();
    let ref_grids = reference.grid_screens();
    let view_state = snapshot(&mut engine).unwrap();
    let ref_state = snapshot(&mut reference).unwrap();

    assert!(
        view_grids.len() >= 3,
        "a split under ext_multigrid holds the global grid and one per \
         window; view reported {view_grids:#?}"
    );

    let clean = compare_grids(
        ViewGrids {
            state: &view_state,
            grids: &view_grids,
        },
        ReferenceGrids {
            state: &ref_state,
            grids: &ref_grids,
        },
    );
    assert!(
        clean.is_empty(),
        "an unmodified split layout must agree on every grid, got {clean:#?}"
    );

    let right_hand = grid_holding(&view_grids, RIGHT_HAND_TEXT);
    assert!(
        right_hand.is_some(),
        "no grid holds {RIGHT_HAND_TEXT:?}; grids were {view_grids:#?}"
    );
    let right_hand = right_hand.expect("the grid asserted present just above");
    assert_ne!(
        right_hand, 1,
        "the right-hand window's text must live in a window grid, not in the \
         global grid nvim paints chrome into"
    );

    let mut corrupted = view_grids.clone();
    drop_last_row(&mut corrupted, right_hand);
    let found = compare_grids(
        ViewGrids {
            state: &view_state,
            grids: &corrupted,
        },
        ReferenceGrids {
            state: &ref_state,
            grids: &ref_grids,
        },
    );

    assert!(
        !found.is_empty(),
        "dropping grid {right_hand}'s last row went unnoticed: the \
         comparison is not reading that grid"
    );
    for divergence in &found {
        // exhaustive, no wildcard: a divergence kind added later has to be
        // classified here rather than silently reported as the wrong grid
        let named = match divergence {
            Divergence::PaneGrid { grid, .. } | Divergence::PaneAttr { grid, .. } => Some(*grid),
            Divergence::State { .. } | Divergence::Grid { .. } | Divergence::Attr { .. } => None,
        };
        assert_eq!(
            named,
            Some(right_hand),
            "only the corrupted grid may diverge, and only per grid; got {divergence:?}"
        );
    }
}
