//! The falsifiable check that `panes = "tiles"` changes the size of a
//! window's grid and nothing about what nvim puts in it.
//!
//! Two live sides are driven across the same vertical split: view under
//! gapped tiles, and bare nvim attached at the size view's outer grid
//! really is once the look has taken its ring. The chrome nvim paints into
//! the global grid has to come out identical, because everything view draws
//! over it is drawn by the compositor and nvim is never told about it; each
//! window's grid has to come out four columns and four rows smaller, still
//! holding the same text in the cells it kept.
//!
//! Both directions are the point. Comparing a truncated dump against its
//! own source would pass for a grid view never read, so the inner-rect walk
//! is run a second time over a dump with one cell changed, and has to name
//! that grid.
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

/// The grid nvim paints chrome into, which no window owns.
const GLOBAL_GRID: u64 = 1;

/// What a gapped tile spends on each axis: a frame cell and a gap cell on
/// each of the two sides.
const RING: usize = 4;

/// The same layout `multigrid_parity.rs` drives, for the same reason: a
/// buffer of its own in each window, so a grid compared against the wrong
/// one cannot pass by coincidence.
const SPLIT_SCRIPT: &str = ":vsplit<CR>:enew<CR>ileft side<Esc><C-w>wiright side<Esc>";

fn dimensions(screen: &view_oracle::Screen) -> (usize, usize) {
    (
        screen.rows.first().map_or(0, |row| row.chars().count()),
        screen.rows.len(),
    )
}

/// Every way a window grid on the tiled side fails to be the bare side's
/// own grid with the ring taken off it, one line each.
fn inner_mismatches(view: &GridScreens, reference: &GridScreens) -> Vec<String> {
    let mut found = Vec::new();
    for (id, tiled) in view.iter().filter(|(id, _)| *id != GLOBAL_GRID) {
        let Some((_, slot)) = reference.iter().find(|(other, _)| other == id) else {
            found.push(format!("grid {id} has no counterpart on the bare side"));
            continue;
        };
        let (slot_cols, slot_rows) = dimensions(slot);
        let (inner_cols, inner_rows) = (slot_cols.saturating_sub(RING), slot_rows - RING);
        if dimensions(tiled) != (inner_cols, inner_rows) {
            found.push(format!(
                "grid {id} is {:?}, not the {:?} the ring leaves of {:?}",
                dimensions(tiled),
                (inner_cols, inner_rows),
                (slot_cols, slot_rows)
            ));
            continue;
        }
        for (row, (kept, whole)) in tiled.rows.iter().zip(&slot.rows).enumerate() {
            let want: String = whole.chars().take(inner_cols).collect();
            let got: String = kept.chars().take(inner_cols).collect();
            if got != want {
                found.push(format!("grid {id} row {row}: {got:?} against {want:?}"));
            }
        }
    }
    found
}

/// Rewrites one cell of the first window grid, the after-the-fact shape of
/// a `grid_line` that landed in the wrong column.
fn corrupt_one_cell(grids: &mut GridScreens) -> u64 {
    let (id, screen) = grids
        .iter_mut()
        .find(|(id, _)| *id != GLOBAL_GRID)
        .expect("the split leaves at least one window grid");
    let row = screen.rows.first_mut().expect("a window grid has rows");
    let mut cells: Vec<char> = row.chars().collect();
    let cell = cells.first_mut().expect("a window grid row has cells");
    *cell = if *cell == 'X' { 'Y' } else { 'X' };
    *row = cells.into_iter().collect();
    *id
}

#[test]
fn a_tiled_split_agrees_with_bare_nvim_inside_every_inner_rect() {
    let mut engine = EngineSession::spawn_with_ext(COLS, ROWS, UI_EXT_OPTIONS_MULTIGRID)
        .expect("EngineSession::spawn_with_ext against real nvim");
    assert!(engine.quiesce(QUIESCE_SILENCE, QUIESCE_DEADLINE).unwrap());
    engine
        .set_panes("tiles")
        .expect("the tiled look is reachable");
    assert!(engine.quiesce(QUIESCE_SILENCE, QUIESCE_DEADLINE).unwrap());

    // the bare side attaches at the size the ring left view's outer grid,
    // read off the live session rather than recomputed here, so the two
    // global grids are the same picture drawn by the same nvim
    let (outer_cols, outer_rows) = engine
        .grid_screens()
        .iter()
        .find(|(id, _)| *id == GLOBAL_GRID)
        .map(|(_, screen)| dimensions(screen))
        .expect("the global grid is always named");
    assert!(
        (outer_cols, outer_rows) < (usize::from(COLS), usize::from(ROWS)),
        "the tiled look must take a ring off the outer grid; it stayed {outer_cols}x{outer_rows}"
    );
    let mut reference = ReferenceSession::spawn_with_ext(
        u16::try_from(outer_cols).unwrap(),
        u16::try_from(outer_rows).unwrap(),
        UI_EXT_OPTIONS_MULTIGRID,
    )
    .expect("ReferenceSession::spawn_with_ext against real nvim");
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
    assert!(
        view_grids.len() >= 3,
        "a split under ext_multigrid holds the global grid and one per \
         window; view reported {view_grids:#?}"
    );

    // the chrome outside every window: view asks nvim for a smaller window
    // grid and tells it nothing else, so the separator column and the
    // status rows are bare nvim's own, cell for cell
    let divergences = compare_grids(
        ViewGrids {
            state: &snapshot(&mut engine).unwrap(),
            grids: &view_grids,
        },
        ReferenceGrids {
            state: &snapshot(&mut reference).unwrap(),
            grids: &ref_grids,
        },
    );
    for divergence in &divergences {
        // exhaustive, no wildcard: a divergence kind added later has to be
        // classified here rather than passing as a window grid's size
        let named = match divergence {
            Divergence::PaneGrid { grid, .. } | Divergence::PaneAttr { grid, .. } => Some(*grid),
            Divergence::State { .. } | Divergence::Grid { .. } | Divergence::Attr { .. } => None,
        };
        assert!(
            named.is_some_and(|grid| grid != GLOBAL_GRID),
            "only a window grid may differ under tiles; got {divergence:?}"
        );
    }

    let found = inner_mismatches(&view_grids, &ref_grids);
    assert!(
        found.is_empty(),
        "every window grid must hold bare nvim's own cells inside the ring, got {found:#?}"
    );

    let mut corrupted = view_grids.clone();
    let changed = corrupt_one_cell(&mut corrupted);
    let caught = inner_mismatches(&corrupted, &ref_grids);
    assert!(
        caught
            .iter()
            .any(|line| line.contains(&format!("grid {changed} row 0"))),
        "changing one cell of grid {changed} went unnoticed: {caught:#?}"
    );
}
