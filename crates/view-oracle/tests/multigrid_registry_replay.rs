//! The pane registry replayed against the engine's own answer.
//!
//! `docs/multigrid-wire-capture.md` records two independent things: the
//! redraw events nvim sent, and the window layout nvim reported through
//! `nvim_list_wins` at the same settle points. This replays the first
//! through [`GridRegistry`] and compares the panes that come out against the
//! second. A registry checked against the events it was built from proves
//! only that it is self-consistent; the layout rows are the engine's own
//! answer to the same question.
//!
//! Every wire line below is quoted from the doc, section by section, and the
//! layout rows are read out of the committed doc rather than restated here,
//! so a capture that moves takes this test with it.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use view_core::grid::registry::{GridEvent, GridId, GridRegistry, PaneKind, GLOBAL_GRID};
use view_core::grid::{Grid, GridOp};

const DOC: &str = "docs/multigrid-wire-capture.md";
const GROUND_TRUTH: &str = "### Window-layout ground truth, multigrid arm";

/// One window as `nvim_list_wins` reported it: screen position, text size,
/// and whether it is a float (`relative` names its anchor) or a split
/// (`relative` empty).
#[derive(Debug, PartialEq, Eq, PartialOrd, Ord)]
struct Window {
    pos: (u16, u16),
    size: Option<(u16, u16)>,
    float: bool,
}

/// `grid_resize [grid, width, height]`, the doc's `grid_resize` section.
fn resize(grid: u64, width: u16, height: u16) -> GridEvent {
    GridEvent::Cells {
        grid: GridId(grid),
        op: GridOp::Resize { width, height },
    }
}

/// `win_pos [grid, win, startrow, startcol, width, height]`, the doc's
/// `win_pos` section. The width and height the wire repeats here are the
/// grid's own, which the matching `grid_resize` above already carries.
fn win_pos(grid: u64, startrow: u16, startcol: u16) -> GridEvent {
    GridEvent::Window {
        grid: GridId(grid),
        startrow,
        startcol,
    }
}

/// The events for one settle point, from a fresh attach.
///
/// Each list is one contiguous run of the capture rather than the whole
/// script: the doc quotes complete cycles for these points and nothing at
/// all for the intermediate `:split`/`:close`/`:only` steps, and a replay
/// that filled those in from anywhere but the wire would be exactly the
/// recall this capture protocol exists to prevent.
fn transcript(label: &str) -> Vec<GridEvent> {
    // "Engine identity"/`grid_resize`: the global grid takes the whole
    // terminal, and the first window grid is 2
    let attach = vec![
        resize(1, 80, 24),
        resize(2, 80, 23),
        win_pos(2, 0, 0), // win_pos [2, ext(1:1000), 0, 0, 80, 23]
    ];
    // "Ordering inside one cycle", verbatim minus the events carrying no
    // grid geometry (`win_viewport_margins`, `tabline_update`,
    // `win_viewport`, `grid_cursor_goto`, `flush`)
    let vsplit = vec![
        win_pos(2, 0, 41),
        resize(4, 40, 23),
        resize(2, 39, 23),
        win_pos(4, 0, 0),
    ];
    // "`nvim_ui_try_resize` and `nvim_ui_try_resize_grid`": one resize call
    // made the engine re-place and resize every grid itself
    let try_resize = vec![
        resize(1, 70, 20),
        win_pos(6, 0, 0),
        win_pos(5, 0, 41),
        resize(6, 40, 19),
        resize(5, 29, 19),
    ];
    match label {
        "attach" => attach,
        "vsplit" => [attach, vsplit].concat(),
        "nvim_ui_try_resize" => try_resize,
        // `win_float_pos` [7, ext(1:1004), "NW", 1, 2.0, 4.0, true, 50, 1,
        // 2, 4], with the border-carrying grid size from the same section
        "nvim_open_win float" => [
            try_resize,
            vec![
                resize(7, 22, 5),
                GridEvent::Float {
                    grid: GridId(7),
                    anchor_grid: GLOBAL_GRID,
                    screen_row: 2,
                    screen_col: 4,
                    zindex: 50,
                    compindex: 1,
                },
            ],
        ]
        .concat(),
        // `win_hide` (the grids of a tab page that stops being current) then
        // the `:tabclose` column of "Ordering inside one cycle": the tab's
        // own grid is destroyed with no `win_close`, and the hidden grids
        // come back through bare `win_pos` events
        "tabclose" => [
            try_resize,
            vec![
                GridEvent::Hide { grid: GridId(6) },
                GridEvent::Hide { grid: GridId(5) },
                GridEvent::Destroy { grid: GridId(8) },
                win_pos(6, 0, 0),
                win_pos(5, 0, 41),
            ],
        ]
        .concat(),
        // no other settle point has a complete cycle quoted, and one
        // filled in from anywhere but the wire would prove nothing
        _ => Vec::new(),
    }
}

#[test]
fn replayed_panes_match_the_window_layout_the_engine_reported() {
    let read = std::fs::read_to_string(view_oracle::workspace_root().join(DOC));
    assert!(read.is_ok(), "{DOC} must be readable: {:?}", read.err());
    let doc = read.unwrap_or_default();

    for label in [
        "attach",
        "vsplit",
        "nvim_ui_try_resize",
        "nvim_open_win float",
        "tabclose",
    ] {
        let mut registry = GridRegistry::new();
        let events = transcript(label);
        assert!(!events.is_empty(), "no transcript is quoted for {label:?}");
        for event in events {
            registry.apply(event);
        }
        assert_eq!(
            panes(&registry),
            recorded_layout(&doc, label),
            "the panes replayed for {label:?} are not the windows nvim \
             reported at the same point"
        );
    }
}

/// The registry's answer, in the shape `nvim_list_wins` reports.
///
/// The global grid is excluded: it is not a window, and under
/// `ext_multigrid` its cells are the chrome *between* windows, which the
/// capture's grid-1 section settles.
fn panes(registry: &GridRegistry) -> Vec<Window> {
    let mut windows: Vec<Window> = registry
        .panes_in_z_order()
        .into_iter()
        .filter(|pane| pane.id != GLOBAL_GRID)
        .map(|pane| {
            let float = matches!(pane.kind, PaneKind::Float { .. });
            Window {
                pos: pane.origin,
                // a float's grid holds its border, so its size is not the
                // text size `nvim_win_get_width`/`_height` report: the
                // capture's own float measured 22x5 for a 20x3 window with
                // `border='single'`. Position and kind are compared for one;
                // size is compared for every window that has no border of
                // its own.
                size: (!float)
                    .then(|| registry.grid(pane.id).map(Grid::size))
                    .flatten(),
                float,
            }
        })
        .collect();
    windows.sort();
    windows
}

/// The layout rows the capture recorded for `label`, parsed out of the
/// committed doc.
fn recorded_layout(doc: &str, label: &str) -> Vec<Window> {
    let prefix = format!("{label}: ");
    let section = doc.split_once(GROUND_TRUTH);
    assert!(
        section.is_some(),
        "{DOC} must carry a {GROUND_TRUTH:?} section"
    );
    let row = section
        .unwrap_or_default()
        .1
        .lines()
        .find(|line| line.starts_with(&prefix));
    assert!(
        row.is_some(),
        "{DOC}'s ground truth records no {label:?} step"
    );

    let mut windows: Vec<Window> = row
        .unwrap_or_default()
        .trim_start_matches(&prefix)
        .split(" | ")
        .map(parse_window)
        .collect();
    windows.sort();
    windows
}

/// Both halves of a `[row,col]` or `WxH` pair as numbers. A field the
/// engine printed with `%d` that will not parse is a doc nobody generated,
/// which the zero it degrades to fails the comparison over.
fn pin((a, b): (&str, &str)) -> (u16, u16) {
    (a.parse().unwrap_or_default(), b.parse().unwrap_or_default())
}

/// One `win=1000 tab=1 pos=[0,0] size=80x23 relative=` row.
fn parse_window(row: &str) -> Window {
    let field = |name: &str| -> String {
        let found = row.split_whitespace().find_map(|f| f.strip_prefix(name));
        assert!(found.is_some(), "{row:?} carries no {name:?} field");
        found.unwrap_or_default().to_string()
    };
    let pos = field("pos=[");
    let pair = pos.trim_end_matches(']').split_once(',').map(pin);
    assert!(pair.is_some(), "{pos:?} is not a [row,col] pair");
    let (top, left) = pair.unwrap_or_default();
    let size = field("size=");
    let measured = size.split_once('x').map(pin);
    assert!(measured.is_some(), "{size:?} is not a WxH pair");
    let (width, height) = measured.unwrap_or_default();
    let float = !row.ends_with("relative=");
    Window {
        pos: (top, left),
        size: (!float).then_some((width, height)),
        float,
    }
}
