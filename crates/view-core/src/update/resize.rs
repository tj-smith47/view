//! Resizing by mouse and by keyboard: the resizes a dragged border asks
//! for, the resize mode's keys, and the one path a sidebar's new share
//! takes to the screen.

use crate::grid::registry::BorderAxis;
use crate::model::{BorderGrip, Focus, Model, MouseCapture, OverlayKind, ResizeMode, Resized};
use crate::msg::{Effect, MouseInput, RpcCall};
use crate::native::geometry::{clamp_panel_width, NativeSurface, PANEL_WIDTH_STEP_PCT};

/// The share of the grid one resize-mode step moves a window by, in
/// percent.
const WINDOW_STEP_PCT: u32 = 5;

/// The largest count resize mode keeps, so a held digit cannot overflow it.
const MAX_COUNT: u32 = 9999;

/// Answers one event of a border drag: a `drag` whose whole-cell distance
/// from the press changed resizes, and every other event does nothing.
pub(super) fn drag(model: &mut Model, input: &MouseInput, grip: BorderGrip) -> Vec<Effect> {
    if input.action != "drag" {
        return Vec::new();
    }
    let at = match grip.axis {
        BorderAxis::Columns => input.col,
        BorderAxis::Rows => input.row,
    };
    let delta = i32::from(at) - i32::from(grip.pressed_at);
    let Some(size) = grip.size_at(delta) else {
        return Vec::new();
    };
    model.capture_mouse(MouseCapture::Border(BorderGrip {
        moved: delta,
        ..grip
    }));
    match grip.resized {
        Resized::Window(win) => vec![Effect::Rpc(RpcCall::SetWindowSize {
            win: win.0,
            width: (grip.axis == BorderAxis::Columns).then_some(size),
            height: (grip.axis == BorderAxis::Rows).then_some(size),
        })],
        Resized::Sidebar(surface) => set_sidebar_share(model, surface, size),
    }
}

/// Sets `surface`'s share to `pct`, clamped to the sidebar range, and
/// resizes its window when it has one.
pub(super) fn set_sidebar_share(
    model: &mut Model,
    surface: NativeSurface,
    pct: u16,
) -> Vec<Effect> {
    match surface {
        NativeSurface::Tree => {
            if !model.set_tree_width(pct) {
                return Vec::new();
            }
            model.dirty = true;
            super::surfaces::resize_windowed_tree(model)
        }
        NativeSurface::Agent => {
            if !model.set_ai_panel_width(pct) {
                return Vec::new();
            }
            model.dirty = true;
            super::surfaces::resize_windowed_agent(model)
        }
        NativeSurface::Notifications => super::surfaces::resize_windowed_stream_to(model, pct),
        NativeSurface::Palette => Vec::new(),
    }
}

/// Sizes each windowed sidebar again from its share of the grid nvim has
/// just laid its windows in, for the sidebars whose axis changed from
/// `before`, the grid's size ahead of the resize. nvim keeps a sidebar's
/// window at its cells across a terminal resize, so a sidebar sized once
/// kept them on every later size.
pub(super) fn reshare_windowed_sidebars(model: &mut Model, before: (u16, u16)) -> Vec<Effect> {
    let after = model.engine.grids().global().size();
    let mut sized: Vec<crate::native::geometry::Anchor> = Vec::new();
    let mut effects = Vec::new();
    for surface in [
        NativeSurface::Tree,
        NativeSurface::Agent,
        NativeSurface::Notifications,
    ] {
        let layout = model.surfaces.layout(surface);
        let vertical = crate::msg::WinSplit::for_anchor(layout.anchor).is_vertical();
        let (from, to) = if vertical {
            (before.0, after.0)
        } else {
            (before.1, after.1)
        };
        // sidebars stacked on one edge share one column or row, which one
        // request sizes
        if !model.surfaces.windowed(surface) || from == to || sized.contains(&layout.anchor) {
            continue;
        }
        if let Some(call) = super::surfaces::window_size_call(model, surface, layout.size) {
            sized.push(layout.anchor);
            effects.push(call);
        }
    }
    effects
}

/// Takes `cells`, where `surface`'s window stands along its axis, as its
/// share of `extent`, the grid's size along that axis, where they are not
/// the cells its share gives. Only nvim's side sizes a window to other
/// cells (`:vertical resize`, `<C-w>|`, a plugin), and a size view asked
/// for and has not seen answered yet is skipped.
fn keep_size_set_in_nvim(model: &mut Model, surface: NativeSurface, cells: u16, extent: u16) {
    let layout = model.surfaces.layout(surface);
    if extent == 0
        || surface == NativeSurface::Palette
        || model.surfaces.awaits_size(surface)
        || cells == crate::native::geometry::share(extent, layout.size).max(1)
    {
        return;
    }
    let pct = pct_for_cells(cells, extent);
    match surface {
        NativeSurface::Tree => {
            model.set_tree_width(pct);
        }
        NativeSurface::Agent => {
            model.set_ai_panel_width(pct);
        }
        NativeSurface::Notifications | NativeSurface::Palette => {}
    }
    model.surfaces.set_layout(
        surface,
        crate::native::geometry::SurfaceLayout::new(layout.placement, layout.anchor, pct),
    );
    super::surfaces::sync_stacked_siblings(model, surface, layout.anchor, pct);
}

/// The percent of `extent` stored for a sidebar nvim sized to `cells`: the
/// smallest whose share is `cells`, or, where one percent of `extent` is
/// more than one cell and none is, the largest whose share stays under it.
/// A width outside the sidebar's percent range is stored at the nearer
/// bound, so one narrower than the narrowest share reopens wider than nvim
/// left it.
fn pct_for_cells(cells: u16, extent: u16) -> u16 {
    let smallest = (u32::from(cells) * 100).div_ceil(u32::from(extent));
    let smallest = u16::try_from(smallest).unwrap_or(u16::MAX);
    let pct = if crate::native::geometry::share(extent, smallest) == cells {
        smallest
    } else {
        smallest.saturating_sub(1)
    };
    clamp_panel_width(i64::from(pct))
}

/// Notes the cells a placement of `grid`'s window reports, which answer a
/// size view asked for, and keeps them as the sidebar's share when nvim's
/// side set them.
pub(super) fn note_sidebar_placed(model: &mut Model, grid: crate::grid::registry::GridId) {
    let Some(surface) = model.engine.grids().native_surface(grid) else {
        return;
    };
    let vertical =
        crate::msg::WinSplit::for_anchor(model.surfaces.layout(surface).anchor).is_vertical();
    let slot = model
        .engine
        .grids()
        .native_window(surface)
        .and_then(|win| model.engine.grids().window_slot(win));
    let Some((_, _, width, height)) = slot else {
        return;
    };
    let cells = if vertical { width } else { height };
    model.surfaces.placed_at(surface, cells);
    if model.surfaces.windowed(surface) {
        let (columns, rows) = model.engine.grids().global().size();
        keep_size_set_in_nvim(model, surface, cells, if vertical { columns } else { rows });
    }
}

/// What resize mode resizes from where the keyboard is: `Some(None)` for
/// nvim's current window, the sidebar for a focused one, and `None` where
/// the keyboard is in anything else.
fn target(model: &Model) -> Option<Option<NativeSurface>> {
    match model.focus() {
        Focus::Engine => Some(None),
        Focus::Pane(
            surface @ (NativeSurface::Tree | NativeSurface::Agent | NativeSurface::Notifications),
        ) => Some(Some(surface)),
        Focus::Native(_) => match model.focused_overlay().map(|overlay| &overlay.kind) {
            Some(OverlayKind::Tree(_)) => Some(Some(NativeSurface::Tree)),
            Some(OverlayKind::Ai) => Some(Some(NativeSurface::Agent)),
            _ => None,
        },
        _ => None,
    }
}

/// Enters resize mode on whatever holds the keyboard, when that is a
/// window or a sidebar.
pub(super) fn enter(model: &mut Model) -> Vec<Effect> {
    if let Some(sidebar) = target(model) {
        model.set_resize_mode(Some(ResizeMode { sidebar, count: 0 }));
    }
    Vec::new()
}

/// Answers `notation` while resize mode holds the keyboard, or `None` when
/// the mode is off or the key left it, and the key goes on to its usual
/// handling.
pub(super) fn key(model: &mut Model, notation: &str) -> Option<Vec<Effect>> {
    let mode = model.resize_mode()?;
    // focus that moved with no key of the mode's own (a click, a surface
    // closing) takes the mode's target with it
    if target(model) != Some(mode.sidebar) {
        model.set_resize_mode(None);
        return None;
    }
    let digit = notation
        .parse::<u32>()
        .ok()
        .filter(|&d| notation.len() == 1 && (d != 0 || mode.count != 0));
    if let Some(digit) = digit {
        let count = mode.count.saturating_mul(10).saturating_add(digit);
        model.set_resize_mode(Some(ResizeMode {
            count: count.min(MAX_COUNT),
            ..mode
        }));
        return Some(Vec::new());
    }
    let (axis, grow) = match notation {
        "h" | "<Left>" => (BorderAxis::Columns, false),
        "l" | "<Right>" => (BorderAxis::Columns, true),
        "j" | "<Down>" => (BorderAxis::Rows, false),
        "k" | "<Up>" => (BorderAxis::Rows, true),
        "=" => {
            model.set_resize_mode(Some(ResizeMode { count: 0, ..mode }));
            return Some(vec![input("<C-w>=")]);
        }
        "<Esc>" | "<CR>" | "q" => {
            model.set_resize_mode(None);
            return Some(Vec::new());
        }
        _ => {
            model.set_resize_mode(None);
            return None;
        }
    };
    model.set_resize_mode(Some(ResizeMode { count: 0, ..mode }));
    let steps = mode.count.max(1);
    Some(match mode.sidebar {
        None => window_step(model, axis, grow, steps),
        Some(surface) if model.sidebar_axis(surface) == axis => {
            let step = i64::from(PANEL_WIDTH_STEP_PCT) * i64::from(steps);
            let share = i64::from(model.sidebar_share(surface));
            let pct = clamp_panel_width(if grow { share + step } else { share - step });
            set_sidebar_share(model, surface, pct)
        }
        Some(_) => Vec::new(),
    })
}

/// nvim's own resize command for its current window, `steps` steps of
/// [`WINDOW_STEP_PCT`] of the grid along `axis`, one cell at least.
fn window_step(model: &Model, axis: BorderAxis, grow: bool, steps: u32) -> Vec<Effect> {
    let (columns, rows) = model.engine.grids().global().size();
    let dimension = match axis {
        BorderAxis::Columns => columns,
        BorderAxis::Rows => rows,
    };
    let cells = (u32::from(dimension) * WINDOW_STEP_PCT / 100).max(1) * steps;
    let command = match (axis, grow) {
        (BorderAxis::Columns, true) => "<C-w>>",
        (BorderAxis::Columns, false) => "<C-w><lt>",
        (BorderAxis::Rows, true) => "<C-w>+",
        (BorderAxis::Rows, false) => "<C-w>-",
    };
    vec![input(&format!("{cells}{command}"))]
}

fn input(notation: &str) -> Effect {
    Effect::Rpc(RpcCall::Input {
        notation: notation.to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::pct_for_cells;
    use crate::native::geometry::{share, MAX_PANEL_WIDTH_PCT, MIN_PANEL_WIDTH_PCT};

    /// A sidebar nvim sized reopens at the cells nvim gave it, at every
    /// extent from 80 to 300 and every width from 15 to 70 percent. Past
    /// 100 cells one percent is more than one cell, so some widths no
    /// percent reproduces; those reopen at the widest share under them, and
    /// no such width exists at 100 cells or fewer. A width outside the
    /// range reopens at the nearer bound's share.
    #[test]
    fn a_width_set_in_nvim_reopens_at_the_same_cells() {
        let percents = MIN_PANEL_WIDTH_PCT..=MAX_PANEL_WIDTH_PCT;
        for extent in 80..=300 {
            let reachable: Vec<u16> = percents.clone().map(|p| share(extent, p)).collect();
            let skips = reachable
                .windows(2)
                .any(|pair| matches!(pair, [a, b] if b - a > 1));
            if extent <= 100 {
                assert!(!skips, "one percent of {extent} skips a cell");
            }
            let narrowest = share(extent, MIN_PANEL_WIDTH_PCT);
            let widest = share(extent, MAX_PANEL_WIDTH_PCT);
            for cells in 1..narrowest {
                let pct = pct_for_cells(cells, extent);
                assert_eq!(pct, MIN_PANEL_WIDTH_PCT, "{cells} cells of {extent}");
            }
            for cells in widest + 1..=extent {
                let pct = pct_for_cells(cells, extent);
                assert_eq!(pct, MAX_PANEL_WIDTH_PCT, "{cells} cells of {extent}");
            }
            for cells in narrowest..=widest {
                let pct = pct_for_cells(cells, extent);
                let reopened = share(extent, pct);
                if let Some(at) = reachable.iter().position(|&c| c == cells) {
                    assert_eq!(
                        (reopened, pct),
                        (cells, MIN_PANEL_WIDTH_PCT + u16::try_from(at).unwrap_or(0)),
                        "{cells} cells of {extent}: the smallest percent giving them"
                    );
                } else {
                    let nearest = reachable.iter().filter(|&&c| c < cells).max();
                    assert!(skips, "{cells} of {extent} has no percent");
                    assert_eq!(Some(&reopened), nearest, "{cells} cells of {extent}");
                }
            }
        }
    }
}
