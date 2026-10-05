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
/// just laid its windows in, for the sidebars whose axis `columns` or
/// `rows` says changed. nvim keeps a sidebar's window at its cells across a
/// terminal resize, so a sidebar sized once kept them on every later size.
pub(super) fn reshare_windowed_sidebars(model: &Model, columns: bool, rows: bool) -> Vec<Effect> {
    let mut sized: Vec<crate::native::geometry::Anchor> = Vec::new();
    let mut effects = Vec::new();
    for surface in [
        NativeSurface::Tree,
        NativeSurface::Agent,
        NativeSurface::Notifications,
    ] {
        let layout = model.surfaces.layout(surface);
        let vertical = crate::msg::WinSplit::for_anchor(layout.anchor).is_vertical();
        // sidebars stacked on one edge share one column or row, which one
        // request sizes
        if !model.surfaces.windowed(surface)
            || !(if vertical { columns } else { rows })
            || sized.contains(&layout.anchor)
        {
            continue;
        }
        if let Some(call) = super::surfaces::window_size_call(model, surface, layout.size) {
            sized.push(layout.anchor);
            effects.push(call);
        }
    }
    effects
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
