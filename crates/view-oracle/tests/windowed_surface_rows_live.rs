//! Against real nvim: a side surface view draws beside the tiles, in a
//! window of its own or floating over them, frames on the rows the tiles
//! beside it frame on, and a band across the tiles frames on their columns.
//!
//! The tiles' boxes come from the layout nvim reported for each window and
//! the surface's box from the rect the paint path resolves, both read off
//! the live session's model. The composited cells are the pure twin's
//! subject (`every_windowed_surface_frames_on_the_tile_ring_rows` in
//! view-tui's pane tests), since this crate's raster draws no tile frames.

#![cfg(unix)]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::path::Path;
use std::time::Duration;

use view_core::grid::registry::GLOBAL_GRID;
use view_core::model::{Model, OverlayKind};
use view_core::msg::Msg;
use view_core::native::geometry::{Anchor, NativeSurface, SurfaceLayout, SurfacePlacement};
use view_oracle::{EngineSession, UI_EXT_OPTIONS_MULTIGRID};

const COLS: u16 = 120;
const ROWS: u16 = 30;
const QUIESCE_SILENCE: Duration = Duration::from_millis(200);
const QUIESCE_DEADLINE: Duration = Duration::from_secs(10);

/// A frame's box on the terminal, as inclusive `(top, bottom, left, right)`.
type FrameBox = (i32, i32, i32, i32);

/// Every surface a person places beside or across the tiles. The
/// notification stream's overlay is a centred history box and its toasts a
/// corner stack; neither stands beside a tile, so neither is walked.
const CASES: &[(NativeSurface, SurfacePlacement, Anchor)] = &[
    (NativeSurface::Tree, SurfacePlacement::Overlay, Anchor::Left),
    (
        NativeSurface::Tree,
        SurfacePlacement::Overlay,
        Anchor::Right,
    ),
    (
        NativeSurface::Tree,
        SurfacePlacement::Windowed,
        Anchor::Left,
    ),
    (
        NativeSurface::Tree,
        SurfacePlacement::Windowed,
        Anchor::Right,
    ),
    (
        NativeSurface::Agent,
        SurfacePlacement::Overlay,
        Anchor::Left,
    ),
    (
        NativeSurface::Agent,
        SurfacePlacement::Overlay,
        Anchor::Right,
    ),
    (
        NativeSurface::Agent,
        SurfacePlacement::Windowed,
        Anchor::Left,
    ),
    (
        NativeSurface::Agent,
        SurfacePlacement::Windowed,
        Anchor::Right,
    ),
    (
        NativeSurface::Notifications,
        SurfacePlacement::Windowed,
        Anchor::Left,
    ),
    (
        NativeSurface::Notifications,
        SurfacePlacement::Windowed,
        Anchor::Right,
    ),
    (
        NativeSurface::Notifications,
        SurfacePlacement::Windowed,
        Anchor::Top,
    ),
    (
        NativeSurface::Notifications,
        SurfacePlacement::Windowed,
        Anchor::Bottom,
    ),
];

fn build_fixture(root: &Path) {
    for (name, text) in [
        ("a.txt", "alpha\n"),
        ("b.txt", "beta\n"),
        ("c.txt", "gamma\n"),
    ] {
        std::fs::write(root.join(name), text).unwrap();
    }
}

fn settle(engine: &mut EngineSession, step: &str) {
    assert!(
        engine.quiesce(QUIESCE_SILENCE, QUIESCE_DEADLINE).unwrap(),
        "{step}: the session never settled"
    );
}

/// One session the walk opens every case in.
#[derive(Clone, Copy, Debug)]
struct Look {
    gaps: bool,
    pill: bool,
    bottom_split: bool,
}

/// The tape's layout: two files side by side under tiles, with a
/// full-width window under them when `bottom_split` holds.
fn session(dir: &Path, look: Look) -> EngineSession {
    let Look {
        gaps,
        pill,
        bottom_split,
    } = look;
    let ext: Vec<&str> = UI_EXT_OPTIONS_MULTIGRID
        .iter()
        .copied()
        .filter(|ext| pill || *ext != "ext_tabline")
        .collect();
    let mut engine =
        EngineSession::spawn_with_ext(COLS, ROWS, &ext).expect("EngineSession against real nvim");
    settle(&mut engine, "attach");
    engine.hold_tabline_as_attached();
    engine.trust_ai();
    engine
        .arm_and_input(&format!(
            ":cd {}<CR>:edit a.txt<CR>:vsplit b.txt<CR>",
            dir.display()
        ))
        .unwrap();
    settle(&mut engine, "split");
    if bottom_split {
        engine.arm_and_input(":botright split c.txt<CR>").unwrap();
        settle(&mut engine, "bottom split");
    }
    engine.set_panes("tiles").unwrap();
    settle(&mut engine, "tiles");
    if !gaps {
        invoke(&mut engine, "ui", "gaps");
        settle(&mut engine, "gapless");
    }
    let model = engine.model();
    assert_eq!(
        (model.chrome_rows(), model.look.gaps, model.cmdline_rows()),
        (u16::from(pill), gaps, 0),
        "the session is not the one the walk names"
    );
    engine
}

fn invoke(engine: &mut EngineSession, feature: &str, verb: &str) {
    engine
        .feed(Msg::FeatureInvoke {
            feature: feature.to_string(),
            verb: verb.to_string(),
        })
        .unwrap();
}

fn toggle(engine: &mut EngineSession, surface: NativeSurface, open: bool) {
    match (surface, open) {
        (NativeSurface::Tree, _) => invoke(engine, "tree", "toggle"),
        (NativeSurface::Agent, true) => invoke(engine, "ai", "open"),
        (NativeSurface::Agent, false) => invoke(engine, "ai", "close"),
        _ => invoke(engine, "notifications", "history"),
    }
}

/// A window slot's frame box on the terminal. A gapped frame is drawn on
/// the slot itself; a gapless one on the lattice around it, which is the
/// cell before the slot on each axis and nvim's separator column and status
/// row after it.
fn frame_of(model: &Model, slot: (u16, u16, u16, u16)) -> FrameBox {
    let (origin_row, origin_col) = view_surface::grid_origin(model);
    let (row, col, width, height) = slot;
    let top = i32::from(row) + i32::from(origin_row);
    let left = i32::from(col) + i32::from(origin_col);
    let (width, height) = (i32::from(width), i32::from(height));
    if model.look.gaps {
        (top, top + height - 1, left, left + width - 1)
    } else {
        (top - 1, top + height, left - 1, left + width)
    }
}

/// The surface's own frame box, the tiles' boxes, and the four values
/// each pane's box is built from: its slot, its grid's size and whether
/// nvim put a winbar row on top of it.
fn measure(
    engine: &mut EngineSession,
    surface: NativeSurface,
) -> (FrameBox, Vec<FrameBox>, String) {
    let winbars = engine
        .eval_str("join(map(getwininfo(), {_, w -> w.winid . '=' . get(w, 'winbar', 0)}), ' ')")
        .unwrap();
    let model = engine.model();
    let registry = model.engine.grids();
    let mut dump = format!(
        "chrome_rows {} grid_origin {:?} winbar {}\n",
        model.chrome_rows(),
        view_surface::grid_origin(model),
        winbars.trim()
    );
    let mut tiles = Vec::new();
    let mut own = None;
    for pane in registry.panes_in_z_order() {
        if pane.id == GLOBAL_GRID || !pane.kind.is_window() || pane.hidden {
            continue;
        }
        let size = registry.grid(pane.id).map(view_core::grid::Grid::size);
        let frame = frame_of(model, pane.slot);
        dump.push_str(&format!(
            "  grid {:?} {:?} slot {:?} size {size:?} frame {frame:?}\n",
            pane.id,
            pane.kind.native_surface(),
            pane.slot
        ));
        if pane.kind.native_surface() == Some(surface) {
            own = Some(frame);
        } else if pane.kind.native_surface().is_none() {
            tiles.push(frame);
        }
    }
    dump.push_str(&format!(
        "  native_pane_rect {:?}\n",
        registry.native_pane_rect(surface)
    ));
    let overlay = model.overlays().iter().find(|overlay| {
        matches!(
            (&overlay.kind, surface),
            (OverlayKind::Tree(_), NativeSurface::Tree) | (OverlayKind::Ai, NativeSurface::Agent)
        )
    });
    if let (None, Some(overlay)) = (own, overlay.filter(|o| model.draws_as_overlay(&o.kind))) {
        let rect = model.overlay_rect(overlay);
        dump.push_str(&format!("  overlay_rect {rect:?}\n"));
        let (row, col) = (i32::from(rect.row), i32::from(rect.col));
        own = Some((
            row,
            row + i32::from(rect.height) - 1,
            col,
            col + i32::from(rect.width) - 1,
        ));
    }
    let own = own.unwrap_or_else(|| panic!("{surface:?} drew no box:\n{dump}"));
    (own, tiles, dump)
}

#[test]
fn a_windowed_surface_frames_on_its_neighbours_ring_rows() {
    let work = view_test_support::ScratchDir::new("windowed-surface-rows").unwrap();
    build_fixture(&work);
    let mut found = Vec::new();
    for gaps in [true, false] {
        for pill in [false, true] {
            for bottom_split in [false, true] {
                let look = Look {
                    gaps,
                    pill,
                    bottom_split,
                };
                let mut engine = session(&work, look);
                for &case in CASES {
                    found.extend(walk_case(&mut engine, look, case));
                }
            }
        }
    }
    assert!(
        found.is_empty(),
        "surfaces off their neighbours' frame lines:\n{}",
        found.join("\n")
    );
}

/// Opens one case in `engine`, measures it and closes it again, answering
/// each way its frame misses the tiles' lines.
fn walk_case(
    engine: &mut EngineSession,
    look: Look,
    (surface, placement, anchor): (NativeSurface, SurfacePlacement, Anchor),
) -> Vec<String> {
    let label = format!("{surface:?} {placement:?} {anchor:?} {look:?}");
    engine.set_surface(surface, SurfaceLayout::new(placement, anchor, 30));
    toggle(engine, surface, true);
    settle(engine, &label);
    let (own, tiles, dump) = measure(engine, surface);
    eprintln!("{label}\n  own {own:?}\n{dump}");
    assert!(
        !tiles.is_empty(),
        "{label}: no tile beside the surface\n{dump}"
    );
    let mut found = Vec::new();
    let across = matches!(anchor, Anchor::Top | Anchor::Bottom);
    let (own_ends, tile_ends) = if across {
        (
            (own.2, own.3),
            (
                tiles.iter().map(|t| t.2).min().unwrap(),
                tiles.iter().map(|t| t.3).max().unwrap(),
            ),
        )
    } else {
        (
            (own.0, own.1),
            (
                tiles.iter().map(|t| t.0).min().unwrap(),
                tiles.iter().map(|t| t.1).max().unwrap(),
            ),
        )
    };
    if own_ends != tile_ends {
        found.push(format!(
            "{label}: frame edges {own_ends:?}, tiles' {tile_ends:?}\n{dump}"
        ));
    }
    if !across {
        // the ring is as wide on both sides, so a side surface's outer
        // column mirrors the tiles' outermost frame column on the far side
        let last = i32::from(COLS) - 1;
        let (outer, expected) = if anchor == Anchor::Left {
            (own.2, last - tiles.iter().map(|t| t.3).max().unwrap())
        } else {
            (own.3, last - tiles.iter().map(|t| t.2).min().unwrap())
        };
        if outer != expected {
            found.push(format!(
                "{label}: outer column {outer}, the tiles' ring {expected}\n{dump}"
            ));
        }
    }
    toggle(engine, surface, false);
    settle(engine, &label);
    found
}
