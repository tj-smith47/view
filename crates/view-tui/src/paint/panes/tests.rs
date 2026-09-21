#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use ratatui::backend::TestBackend;
use ratatui::buffer::Buffer;
use ratatui::Terminal;
use view_core::events::{GridCell, UiEvent, WinHandle};
use view_core::grid::registry::GridId;
use view_core::hl::HlAttr;
use view_core::model::{Model, WindowStatus};
use view_core::msg::Msg;
use view_core::native::statusline::SegmentUpdate;
use view_core::update::update;
use view_surface::Surface;

use crate::paint::{composite_into, Damage};

/// The `:vsplit` the wire capture records, scaled to a canvas a dump can be
/// read at: grid 1 is the whole screen, the two window grids are 9 and 10
/// columns wide, and column 9 is the one neither of them occupies.
///
/// Event order follows `docs/multigrid-wire-capture.md`'s own vsplit cycle
/// -- `win_pos [2]`, then both `grid_resize`, then `win_pos [4]` -- so the
/// fixture also exercises the tolerance that cycle demands: a window placed
/// before the grid behind it has a size.
const LEFT: u64 = 4;
const RIGHT: u64 = 2;
const SEPARATOR_COL: u16 = 9;
/// The highlight nvim paints its own separator column with, and the one a
/// colorscheme gives view's `WinSeparator`. Distinct on purpose: the frame
/// showing view's is what proves the overpaint, since both sides draw a
/// glyph in the same cell.
const ENGINE_SEPARATOR_HL: u64 = 12;
const VIEW_SEPARATOR_HL: u64 = 13;
const ENGINE_SEPARATOR_FG: u32 = 0x0011_2233;
const VIEW_SEPARATOR_FG: u32 = 0x00CC_DDEE;
/// A fixture terminal whose box-glyph probe came back saying it accounts
/// for a box-drawing glyph as one cell, and one whose did not: the bit the
/// separator charset is keyed on, named rather than spelled at a call site.
const DRAWS_BOX_GLYPHS: bool = true;
const NO_BOX_GLYPHS: bool = false;
const WIDTH: u16 = 20;
const HEIGHT: u16 = 6;

fn drive(model: &mut Model, events: Vec<UiEvent>) {
    let _ = update(model, Msg::Redraw(events));
}

fn line(grid: u64, row: u64, text: &str, hl_id: u64) -> UiEvent {
    UiEvent::GridLine {
        grid,
        row,
        col_start: 0,
        cells: text
            .chars()
            .map(|ch| GridCell {
                text: ch.to_string(),
                hl_id,
                repeat: 1,
            })
            .collect(),
    }
}

fn vsplit() -> Model {
    let mut model = Model::new();
    model.term_width = WIDTH;
    model.term_height = HEIGHT;
    model.caps = model.caps.with_unicode_boxes(DRAWS_BOX_GLYPHS);
    let mut events = vec![
        UiEvent::GridResize {
            grid: 1,
            width: u64::from(WIDTH),
            height: u64::from(HEIGHT),
        },
        attr(ENGINE_SEPARATOR_HL, ENGINE_SEPARATOR_FG),
        attr(VIEW_SEPARATOR_HL, VIEW_SEPARATOR_FG),
        UiEvent::HlGroupSet {
            name: "WinSeparator".to_string(),
            hl_id: VIEW_SEPARATOR_HL,
        },
        UiEvent::WinPos {
            grid: RIGHT,
            win: WinHandle(1000),
            startrow: 0,
            startcol: u64::from(SEPARATOR_COL) + 1,
            width: 10,
            height: 5,
        },
        UiEvent::GridResize {
            grid: LEFT,
            width: u64::from(SEPARATOR_COL),
            height: 5,
        },
        UiEvent::GridResize {
            grid: RIGHT,
            width: 10,
            height: 5,
        },
        UiEvent::WinPos {
            grid: LEFT,
            win: WinHandle(1001),
            startrow: 0,
            startcol: 0,
            width: u64::from(SEPARATOR_COL),
            height: 5,
        },
    ];
    // grid 1 keeps carrying the separator column under multigrid (the
    // capture's `grid_line [1, 0, 40, [["|", 12]]]`, one cell per window
    // row), so every assertion below is about which of the two glyphs
    // survives rather than about an empty cell
    events.extend((0..5).map(|row| UiEvent::GridLine {
        grid: 1,
        row,
        col_start: u64::from(SEPARATOR_COL),
        cells: vec![GridCell {
            text: "\u{2502}".to_string(),
            hl_id: ENGINE_SEPARATOR_HL,
            repeat: 1,
        }],
    }));
    events.extend([
        line(LEFT, 0, "left", 0),
        line(RIGHT, 0, "right", 0),
        UiEvent::GridCursorGoto {
            grid: LEFT,
            row: 0,
            col: 0,
        },
        UiEvent::Flush,
    ]);
    drive(&mut model, events);
    model
}

fn attr(hl_id: u64, fg: u32) -> UiEvent {
    UiEvent::HlAttrDefine {
        id: hl_id,
        fg: Some(fg),
        bg: None,
        bold: false,
        italic: false,
        underline: false,
        reverse: false,
    }
}

fn rgb(color: u32) -> Option<ratatui::style::Color> {
    Some(ratatui::style::Color::Rgb(
        (color >> 16) as u8,
        (color >> 8) as u8,
        color as u8,
    ))
}

/// Composites `model` into a fresh buffer, the whole frame at once.
fn frame(model: &Model) -> Buffer {
    let surface = view_surface::render(model);
    let backend = TestBackend::new(WIDTH, HEIGHT);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal
        .draw(|f| composite_into(f.buffer_mut(), model, &surface, &Damage::full()))
        .unwrap();
    terminal.backend().buffer().clone()
}

fn row_text(buf: &Buffer, row: u16) -> String {
    (0..buf.area.width)
        .map(|col| buf[(col, row)].symbol())
        .collect()
}

/// A row read one cell at a time, for an assertion about which cell a
/// glyph stands in -- which [`row_text`] cannot answer, since a cell whose
/// symbol is a grapheme cluster contributes more than one character to it.
fn row_cells(buf: &Buffer, row: u16) -> Vec<String> {
    (0..buf.area.width)
        .map(|col| buf[(col, row)].symbol().to_string())
        .collect()
}

/// Every cell of the column between the two windows carries view's own
/// separator -- its glyph and its style, over the one nvim painted into
/// grid 1 underneath -- and neither window's text is displaced by it.
#[test]
fn two_windows_paint_with_one_separator_column() {
    let buf = frame(&vsplit());
    assert!(
        row_text(&buf, 0).starts_with("left"),
        "the left pane's text did not reach its own box: {:?}",
        row_text(&buf, 0)
    );
    let after_separator: String = row_text(&buf, 0)
        .chars()
        .skip(usize::from(SEPARATOR_COL) + 1)
        .collect();
    assert_eq!(
        after_separator, "right     ",
        "the right pane paints at its own origin, not the layer's"
    );
    for row in 0..5 {
        assert_eq!(
            buf[(SEPARATOR_COL, row)].symbol(),
            "\u{2502}",
            "row {row} of the column between the two windows carries no separator"
        );
        assert_eq!(
            buf[(SEPARATOR_COL, row)].style().fg,
            rgb(VIEW_SEPARATOR_FG),
            "row {row} kept the engine's own separator style, so nothing was overpainted"
        );
    }
}

/// A terminal whose box-glyph probe never came back draws the separator in
/// ASCII, the same one-cell degrade every other view frame takes -- and the
/// cell underneath holds nvim's `|`, so the ASCII glyph landing there is
/// the overpaint proving itself.
#[test]
fn a_terminal_without_box_glyphs_separates_in_ascii() {
    let mut model = vsplit();
    model.caps = model.caps.with_unicode_boxes(NO_BOX_GLYPHS);
    let buf = frame(&model);
    assert_eq!(buf[(SEPARATOR_COL, 0)].symbol(), "|");
    assert_eq!(buf[(SEPARATOR_COL, 0)].style().fg, rgb(VIEW_SEPARATOR_FG));
}

/// The window the cursor is not in takes `NormalNC` where its cells carry
/// no highlight of their own, and the window the cursor is in does not.
#[test]
fn the_active_pane_is_distinguishable_from_the_inactive_one() {
    let mut model = vsplit();
    drive(
        &mut model,
        vec![
            UiEvent::HlAttrDefine {
                id: 7,
                fg: Some(0x0033_3333),
                bg: Some(0x0011_1111),
                bold: false,
                italic: false,
                underline: false,
                reverse: false,
            },
            UiEvent::HlGroupSet {
                name: "NormalNC".to_string(),
                hl_id: 7,
            },
            UiEvent::Flush,
        ],
    );
    let buf = frame(&model);
    let active = buf[(0, 0)].style();
    let inactive = buf[(SEPARATOR_COL + 1, 0)].style();
    assert_ne!(
        active, inactive,
        "a colorscheme that dims non-current windows reached neither pane"
    );
    assert_eq!(
        inactive.fg,
        Some(ratatui::style::Color::Rgb(0x33, 0x33, 0x33)),
        "the pane the cursor is not in did not resolve through NormalNC"
    );
}

/// nvim's own message area (`msg_set_pos`) is never the cursor's grid, but
/// it is not an inactive window either: its unstyled cells must resolve
/// through `Normal`, not `NormalNC`, the same exemption the global grid
/// gets.
#[test]
fn the_message_area_is_never_dimmed_like_an_inactive_window() {
    let mut model = vsplit();
    const MESSAGE_GRID: u64 = 6;
    drive(
        &mut model,
        vec![
            UiEvent::HlAttrDefine {
                id: 7,
                fg: Some(0x0033_3333),
                bg: None,
                bold: false,
                italic: false,
                underline: false,
                reverse: false,
            },
            UiEvent::HlGroupSet {
                name: "NormalNC".to_string(),
                hl_id: 7,
            },
            UiEvent::GridResize {
                grid: MESSAGE_GRID,
                width: u64::from(WIDTH),
                height: 1,
            },
            UiEvent::MsgSetPos {
                grid: MESSAGE_GRID,
                row: 5,
                scrolled: false,
                sep_char: " ".to_string(),
                zindex: 200,
                compindex: 0,
            },
            line(MESSAGE_GRID, 0, "recovered", 0),
            UiEvent::Flush,
        ],
    );
    let buf = frame(&model);
    assert!(
        row_text(&buf, 5).starts_with("recovered"),
        "the message grid's text never reached the row `msg_set_pos` named: {:?}",
        row_text(&buf, 5)
    );
    assert_ne!(
        buf[(0, 5)].style().fg,
        Some(ratatui::style::Color::Rgb(0x33, 0x33, 0x33)),
        "the message area resolved through NormalNC, as if it were an \
         inactive window"
    );
}

/// Closing one of two side-by-side windows leaves no trace of the column
/// that separated them: the survivor grows over it, and the cell nvim
/// painted its own separator into is a cell of the survivor's own text
/// from that frame on.
///
/// nvim leaves that cell standing in grid 1 under `ext_multigrid` -- it
/// sends no `grid_line` clearing it, because the window grid growing over
/// it is what covers it -- so a compositor that stops short of it shows a
/// one-column band in the old separator's colours down the whole height of
/// the screen, which is what a `:q` on a side-by-side layout looked like.
#[test]
fn closing_one_of_two_windows_leaves_no_separator_column_behind() {
    let mut model = vsplit();
    drive(
        &mut model,
        vec![
            UiEvent::WinClose { grid: RIGHT },
            UiEvent::GridDestroy { grid: RIGHT },
            UiEvent::WinPos {
                grid: LEFT,
                win: WinHandle(1001),
                startrow: 0,
                startcol: 0,
                width: u64::from(WIDTH),
                height: 5,
            },
            UiEvent::GridResize {
                grid: LEFT,
                width: u64::from(WIDTH),
                height: 5,
            },
            UiEvent::GridLine {
                grid: LEFT,
                row: 0,
                col_start: 0,
                cells: vec![GridCell {
                    text: " ".to_string(),
                    hl_id: 0,
                    repeat: u64::from(WIDTH),
                }],
            },
            UiEvent::Flush,
        ],
    );
    let buf = frame(&model);
    let plain = buf[(0, 0)].style();
    for row in 0..5 {
        assert_eq!(
            buf[(SEPARATOR_COL, row)].symbol(),
            " ",
            "row {row} still carries a separator glyph between windows that \
             no longer face each other"
        );
        assert_eq!(
            buf[(SEPARATOR_COL, row)].style(),
            plain,
            "row {row} of the old separator column kept a style of its own \
             while the rest of the row is the survivor's"
        );
    }
}

/// A float paints over the windows it overlaps, whatever order nvim named
/// the grids in.
#[test]
fn a_float_paints_above_the_windows_it_overlaps() {
    let mut model = vsplit();
    drive(
        &mut model,
        vec![
            UiEvent::GridResize {
                grid: 7,
                width: 4,
                height: 1,
            },
            UiEvent::WinFloatPos {
                grid: 7,
                win: WinHandle(1004),
                anchor_grid: 1,
                zindex: 50,
                compindex: 1,
                screen_row: 0,
                screen_col: 0,
            },
            line(7, 0, "flt!", 0),
            UiEvent::Flush,
        ],
    );
    let buf = frame(&model);
    assert!(
        row_text(&buf, 0).starts_with("flt!"),
        "the float did not paint over the window under it: {:?}",
        row_text(&buf, 0)
    );
}

/// A float view is holding off the screen while it classifies what opened
/// it paints no cell: the window under it is what the user sees, exactly as
/// if the plugin had never opened one.
///
/// Disconfirm: dropping the withheld filter from the pane walk puts the
/// float's own text back on row 0 here.
#[test]
fn a_withheld_float_paints_nothing() {
    let mut model = vsplit();
    drive(
        &mut model,
        vec![
            UiEvent::GridResize {
                grid: 7,
                width: 4,
                height: 1,
            },
            UiEvent::WinFloatPos {
                grid: 7,
                win: WinHandle(1004),
                anchor_grid: 1,
                zindex: 50,
                compindex: 1,
                screen_row: 0,
                screen_col: 0,
            },
            line(7, 0, "flt!", 0),
            UiEvent::Flush,
        ],
    );
    assert!(
        row_text(&frame(&model), 0).starts_with("flt!"),
        "the fixture's float has to reach the frame for the withholding to \
         be what takes it off"
    );
    assert!(model.engine.withhold_float(GridId(7), true));
    let row = row_text(&frame(&model), 0);
    assert!(
        !row.contains("flt!"),
        "a withheld float painted its cells: {row:?}"
    );
    assert!(model.engine.withhold_float(GridId(7), false));
    assert!(
        row_text(&frame(&model), 0).starts_with("flt!"),
        "and the release puts it back"
    );
}

/// The separator column is a cell of the global grid, so a float over it
/// wins the same way a float over a window's text does. The overpaint that
/// draws view's own glyph there used to run after every pane, which put
/// nvim's chrome column back on top of the float's text.
///
/// Disconfirm: painting the separators after the float layer again leaves
/// the separator glyph mid-word here.
#[test]
fn a_float_paints_above_the_separator_column() {
    let mut model = vsplit();
    drive(
        &mut model,
        vec![
            UiEvent::GridResize {
                grid: 7,
                width: 10,
                height: 1,
            },
            UiEvent::WinFloatPos {
                grid: 7,
                win: WinHandle(1004),
                anchor_grid: 1,
                zindex: 50,
                compindex: 1,
                screen_row: 1,
                screen_col: u64::from(SEPARATOR_COL) - 4,
            },
            line(7, 0, "overlapped", 0),
            UiEvent::Flush,
        ],
    );
    let buf = frame(&model);
    let row = row_text(&buf, 1);
    let start = usize::from(SEPARATOR_COL) - 4;
    assert_eq!(
        &row[start..start + 10],
        "overlapped",
        "the separator column overpainted the float that covers it: {row:?}"
    );
}

/// `win_hide` takes a pane off screen without destroying its grid, so the
/// cells it still holds must not reach the frame -- and with nothing across
/// the column any more, view stops overpainting it and grid 1's own cell
/// shows through untouched.
#[test]
fn a_hidden_pane_paints_nothing() {
    let mut model = vsplit();
    drive(
        &mut model,
        vec![UiEvent::WinHide { grid: RIGHT }, UiEvent::Flush],
    );
    let buf = frame(&model);
    assert!(
        !row_text(&buf, 0).contains("right"),
        "a hidden pane's cells still painted: {:?}",
        row_text(&buf, 0)
    );
    assert_eq!(
        buf[(SEPARATOR_COL, 0)].style().fg,
        rgb(ENGINE_SEPARATOR_FG),
        "view drew its own separator beside a window that is no longer on screen"
    );
}

/// The remedy the float-claimant detector applies is `nvim_win_hide`, which
/// reaches this frontend as `win_hide` for the float's own grid. The pane
/// compositor must honour it: a float that claimed a view-owned surface is
/// hidden precisely so view's own surface shows, and painting it anyway
/// from its grid would hand the surface back.
#[test]
fn a_claimed_float_still_resolves_under_multigrid() {
    let mut model = vsplit();
    drive(
        &mut model,
        vec![
            UiEvent::GridResize {
                grid: 9,
                width: 6,
                height: 1,
            },
            UiEvent::WinFloatPos {
                grid: 9,
                win: WinHandle(1009),
                anchor_grid: 1,
                zindex: 200,
                compindex: 1,
                screen_row: 0,
                screen_col: 0,
            },
            line(9, 0, "claim!", 0),
            UiEvent::Flush,
        ],
    );
    assert!(
        row_text(&frame(&model), 0).starts_with("claim!"),
        "the fixture float never painted, so hiding it proves nothing"
    );
    drive(
        &mut model,
        vec![UiEvent::WinHide { grid: 9 }, UiEvent::Flush],
    );
    assert!(
        row_text(&frame(&model), 0).starts_with("left"),
        "a hidden claimant float kept painting from its own grid"
    );
}

/// The picker, tree and agent panel are view's own surfaces and outrank
/// every engine grid, a float included: a plugin float painting over the
/// agent panel is the regression this pins.
#[test]
fn view_overlays_stay_above_every_pane() {
    let mut model = vsplit();
    drive(
        &mut model,
        vec![
            UiEvent::GridResize {
                grid: 7,
                width: 6,
                height: 1,
            },
            UiEvent::WinFloatPos {
                grid: 7,
                win: WinHandle(1004),
                anchor_grid: 1,
                zindex: 200,
                compindex: 1,
                screen_row: 0,
                screen_col: 0,
            },
            line(7, 0, "plugin", 0),
            UiEvent::Flush,
        ],
    );
    let surface = view_surface::render(&model);
    let mut layers = surface.layers.clone();
    layers.push(view_surface::Layer::new(
        view_surface::Rect::new(0, 0, WIDTH, 3),
        view_surface::LayerKind::Ai(view_core::native::views::AiPanelView::new("AI Agent")),
        model.caps,
    ));
    let panel = Surface::from_layers(layers);

    let backend = TestBackend::new(WIDTH, HEIGHT);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal
        .draw(|f| composite_into(f.buffer_mut(), &model, &panel, &Damage::full()))
        .unwrap();
    let buf = terminal.backend().buffer().clone();
    assert!(
        !row_text(&buf, 0).contains("plugin"),
        "a plugin float painted over view's own panel: {:?}",
        row_text(&buf, 0)
    );
}

/// Damage is per pane, not per screen: a window redrawing one of its own
/// rows must not repaint the rows of the window beside it.
#[test]
fn a_panes_redraw_damages_its_own_rows_alone() {
    let mut model = vsplit();
    let _ = model.take_paint_damage();
    drive(&mut model, vec![line(RIGHT, 2, "x", 0), UiEvent::Flush]);
    let damage = model.take_paint_damage();
    assert!(!damage.full, "one pane's line redrew the whole frame");
    assert_eq!(
        damage.rows,
        vec![2],
        "a pane's row must reach the frame in screen coordinates"
    );

    // both panes redrawing the same screen row is the ordinary case for a
    // vsplit, and every consumer of this list scans it linearly
    drive(
        &mut model,
        vec![
            line(LEFT, 2, "y", 0),
            line(RIGHT, 2, "z", 0),
            UiEvent::Flush,
        ],
    );
    assert_eq!(
        model.take_paint_damage().rows,
        vec![2],
        "two panes redrawing one screen row reported it twice"
    );
}

/// A window that moves names no cell at all, and the box it vacated belongs
/// to whatever was underneath it.
#[test]
fn a_window_that_moves_repaints_the_frame_it_left() {
    let mut model = vsplit();
    let _ = model.take_paint_damage();
    drive(
        &mut model,
        vec![
            UiEvent::WinPos {
                grid: RIGHT,
                win: WinHandle(1000),
                startrow: 1,
                startcol: u64::from(SEPARATOR_COL) + 1,
                width: 10,
                height: 5,
            },
            UiEvent::Flush,
        ],
    );
    assert!(
        model.take_paint_damage().full,
        "a window moved and the rows it vacated were never repainted"
    );
}

/// A pane whose rows start below the top of the screen offsets its own
/// damage: reporting grid-local rows would repaint the wrong ones.
#[test]
fn a_lower_panes_damage_is_offset_by_where_its_box_sits() {
    let mut model = vsplit();
    drive(
        &mut model,
        vec![
            UiEvent::WinPos {
                grid: RIGHT,
                win: WinHandle(1000),
                startrow: 3,
                startcol: u64::from(SEPARATOR_COL) + 1,
                width: 10,
                height: 2,
            },
            UiEvent::Flush,
        ],
    );
    let _ = model.take_paint_damage();
    drive(&mut model, vec![line(RIGHT, 1, "x", 0), UiEvent::Flush]);
    let damage = model.take_paint_damage();
    assert_eq!(damage.rows, vec![4], "the pane's origin was not applied");
}

/// Nothing about a single-grid session changes: one pane, at the origin, no
/// separator anywhere on the frame.
#[test]
fn a_single_grid_session_paints_one_pane_and_no_chrome() {
    let mut model = Model::new();
    model.term_width = WIDTH;
    model.term_height = HEIGHT;
    drive(
        &mut model,
        vec![
            UiEvent::GridResize {
                grid: 1,
                width: u64::from(WIDTH),
                height: u64::from(HEIGHT),
            },
            line(1, 0, "one grid, no panes", 0),
            UiEvent::Flush,
        ],
    );
    let buf = frame(&model);
    assert_eq!(row_text(&buf, 0), "one grid, no panes  ");
    for row in 0..HEIGHT {
        assert_eq!(
            buf[(SEPARATOR_COL, row)].symbol(),
            " ",
            "a single-grid session grew chrome between windows it has none of"
        );
    }
}

/// A window's right edge is a separator only where a second window sits
/// across it. `:split` inside the left half is the shape that parts the
/// two: the rows of each half face the right-hand window, and the row
/// between them is the upper window's statusline, which nvim paints into
/// the global grid and view must leave alone.
#[test]
fn a_split_separates_only_the_rows_a_window_faces() {
    let mut model = vsplit();
    drive(
        &mut model,
        vec![
            UiEvent::GridResize {
                grid: LEFT,
                width: u64::from(SEPARATOR_COL),
                height: 2,
            },
            UiEvent::GridResize {
                grid: 5,
                width: u64::from(SEPARATOR_COL),
                height: 2,
            },
            UiEvent::WinPos {
                grid: LEFT,
                win: WinHandle(1001),
                startrow: 0,
                startcol: 0,
                width: u64::from(SEPARATOR_COL),
                height: 2,
            },
            UiEvent::WinPos {
                grid: 5,
                win: WinHandle(1002),
                startrow: 3,
                startcol: 0,
                width: u64::from(SEPARATOR_COL),
                height: 2,
            },
            UiEvent::Flush,
        ],
    );
    let buf = frame(&model);
    for row in [0, 1, 3, 4] {
        assert_eq!(
            buf[(SEPARATOR_COL, row)].style().fg,
            rgb(VIEW_SEPARATOR_FG),
            "row {row} faces the right-hand window and kept the engine's separator"
        );
    }
    assert_eq!(
        buf[(SEPARATOR_COL, 2)].style().fg,
        rgb(ENGINE_SEPARATOR_FG),
        "view drew over the statusline row the engine owns"
    );
}

/// The env var that rewrites a golden instead of asserting against it,
/// spelled as `view-oracle`'s overlay goldens spell it so one command
/// regenerates every committed dump in the tree.
const UPDATE: &str = "VIEW_UPDATE_GOLDENS";

/// The `:vsplit` frame as one terminal of the named capability answers
/// draws it, rendered through the real painter rather than a second
/// rasterizer.
///
/// Glyphs alone would not pin this feature: nvim paints its own `\u{2502}`
/// into the same column, so a dump of text only reads identically whether
/// view overpainted it or left it alone -- the separator paint could be
/// deleted and two of the three goldens would still pass. The `fg` block is
/// what closes that: one letter per cell for its resolved foreground, with
/// the colors themselves in a legend, so which side owns the column is on
/// the face of the committed file.
fn dump(sync: bool, truecolor: bool, kitty: bool, unicode_boxes: bool) -> String {
    let mut model = vsplit();
    model.caps = view_core::model::TermCaps::from_probe(sync, truecolor, kitty)
        .with_unicode_boxes(unicode_boxes);
    screen_dump(&frame(&model))
}

/// The same picture for a tiled scene, composited at the fixture's own
/// terminal size.
fn tiles_dump(tier: (&str, bool, bool, bool, bool), mut tiles: Tiles) -> String {
    let (_, sync, truecolor, kitty, unicode_boxes) = tier;
    tiles.model.caps = view_core::model::TermCaps::from_probe(sync, truecolor, kitty)
        .with_unicode_boxes(unicode_boxes);
    screen_dump(&tiled_frame(&tiles.model))
}

fn screen_dump(buf: &Buffer) -> String {
    let mut out: Vec<String> = (0..buf.area.height).map(|row| row_text(buf, row)).collect();
    out.push("--- fg ---".to_string());
    // Saturating on Z: b'A' + index overflows u8 at index 191, well before the
    // try_from fallback could fire, so the cap has to come before the add.
    fn legend_letter(index: usize) -> char {
        char::from(b'A' + u8::try_from(index.min(25)).unwrap_or(25))
    }
    let mut legend: Vec<ratatui::style::Color> = Vec::new();
    for row in 0..buf.area.height {
        out.push(
            (0..buf.area.width)
                .map(|col| match buf[(col, row)].style().fg {
                    None => '.',
                    Some(color) => {
                        let seen = legend.iter().position(|c| *c == color).unwrap_or_else(|| {
                            legend.push(color);
                            legend.len() - 1
                        });
                        legend_letter(seen)
                    }
                })
                .collect(),
        );
    }
    for (index, color) in legend.iter().enumerate() {
        out.push(format!("{} = {color:?}", legend_letter(index)));
    }
    out.join("\n")
}

fn assert_golden(name: &str, actual: &str) {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("goldens")
        .join(format!("{name}.txt"));
    if std::env::var_os(UPDATE).is_some() {
        std::fs::create_dir_all(path.parent().expect("a golden has a directory")).unwrap();
        std::fs::write(&path, format!("{actual}\n")).unwrap();
        return;
    }
    let expected = std::fs::read_to_string(&path).unwrap_or_else(|err| {
        panic!(
            "golden {} is missing ({err}); regenerate with {UPDATE}=1 and read it before committing",
            path.display()
        )
    });
    assert_eq!(
        actual,
        expected.trim_end_matches('\n'),
        "golden {} diverged",
        path.display()
    );
}

/// The committed picture of two windows and the column view draws between
/// them, one per tier. Deleting the separator paint fails all three, which
/// is what makes them a check on the feature rather than on the fixture.
/// A tile's own gap ring is cleared, and not the cell beyond the slot
/// alone.
///
/// nvim paints the separator column into grid 1 for as long as two windows
/// face each other there, and when one closes it leaves the glyph standing
/// in the cells the surviving window's smaller grid does not cover. Nothing
/// later repaints them, `:redraw!` included, so a `\u{2502}` sat under the
/// bottom frame of a 263x88 session for the rest of its life.
#[test]
fn a_glyph_left_in_a_tiles_gap_row_is_cleared() {
    let tiles = tiled(true);
    let (row, col, width, height) = tiles.slots[0];
    let mut model = tiles.model;
    let gap_row = u64::from(row + height - 1);
    drive(
        &mut model,
        vec![
            UiEvent::GridLine {
                grid: 1,
                row: gap_row,
                col_start: u64::from(col + 1),
                cells: vec![GridCell {
                    text: "\u{2502}".to_string(),
                    hl_id: VIEW_SEPARATOR_HL,
                    repeat: u64::from(width - 2),
                }],
            },
            UiEvent::Flush,
        ],
    );
    let buf = tiled_frame(&model);
    let offset = model.look.grid_offset();
    let painted = row_text(&buf, row + height - 1 + offset);
    assert!(
        !painted.contains('\u{2502}'),
        "the gap row under a tile kept a separator: {painted:?}"
    );
}

/// nvim writes the mode message and the answer to every prompt into the
/// row it keeps at the grid's foot, which is the row under the slot when
/// the window has no status row of its own.
#[test]
fn the_command_line_under_the_lowest_tile_keeps_its_text() {
    let (grid_width, grid_height) = outer_grid(true, TILED_HEIGHT);
    let slots = vec![(0, 0, grid_width, grid_height - 1)];
    let mut model = tiled_model(true, TILED_HEIGHT, &slots);
    with_nvims_command_line(&mut model);
    drive(
        &mut model,
        vec![
            line(1, u64::from(grid_height - 1), "-- INSERT --", 0),
            UiEvent::Flush,
        ],
    );
    let buf = tiled_frame(&model);
    let offset = model.look.grid_offset();
    let message = row_text(&buf, grid_height - 1 + offset);
    assert!(
        message.contains("-- INSERT --"),
        "the command line lost nvim's message: {message:?}"
    );
}

/// The status row nvim paints under a window is what the frame replaces,
/// so it is cleared wherever another row stands between it and the
/// command line.
#[test]
fn a_status_row_under_a_tile_is_cleared() {
    // `laststatus = 2` gives every window one, and the upper right tile of
    // this layout has another window under its own
    let tiles = tiled_nested(true);
    let (row, col, width, height) = tiles.slots[1];
    let mut model = tiles.model;
    drive(
        &mut model,
        vec![
            UiEvent::GridLine {
                grid: 1,
                row: u64::from(row + height),
                col_start: u64::from(col),
                cells: vec![GridCell {
                    text: "\u{2500}".to_string(),
                    hl_id: VIEW_SEPARATOR_HL,
                    repeat: u64::from(width),
                }],
            },
            UiEvent::Flush,
        ],
    );
    let buf = tiled_frame(&model);
    let offset = model.look.grid_offset();
    let status = row_text(&buf, row + height + offset);
    assert!(
        !status.contains('\u{2500}'),
        "the status row under a tile kept nvim's own line: {status:?}"
    );
}

/// A session that owns the command line keeps no row for it, so the
/// bottom window's status row is the grid's last row and is cleared like
/// any other. It shipped uncleared: the clear stopped one row short
/// whatever the attach was, and a real session left nvim's status line
/// standing under every tile.
///
/// Disconfirm: bounding the clear at the grid's height less one paints the
/// line straight back.
#[test]
fn the_status_row_on_the_grids_last_row_is_cleared() {
    let tiles = tiled(true);
    let (row, _, width, height) = tiles.slots[0];
    let (_, grid_height) = outer_grid(true, TILED_HEIGHT);
    assert_eq!(
        row + height,
        grid_height - 1,
        "the fixture no longer puts a status row on the grid's last row"
    );
    let mut model = tiles.model;
    drive(
        &mut model,
        vec![
            UiEvent::GridLine {
                grid: 1,
                row: u64::from(row + height),
                col_start: 0,
                cells: vec![GridCell {
                    text: "\u{2500}".to_string(),
                    hl_id: 0,
                    repeat: u64::from(width),
                }],
            },
            UiEvent::Flush,
        ],
    );
    let buf = tiled_frame(&model);
    let offset = model.look.grid_offset();
    let status = row_text(&buf, row + height + offset);
    assert!(
        !status.contains('\u{2500}'),
        "the grid's last row kept nvim's own status line: {status:?}"
    );
}

/// The grid a float's own text arrives in, and the box it draws around
/// itself: an LSP hover or a completion menu wider than the window it
/// opened from paints its border into its own grid.
const TILED_FLOAT: u64 = 9;
const FLOAT_LINES: [&str; 3] = [
    "┌──── hover ─────┐",
    "│ across the gap │",
    "└────────────────┘",
];
/// The float's top-left cell, in the outer grid's own coordinates: far
/// enough left that the box crosses the gap band between the two tiles and
/// the frame column on each side of it.
const FLOAT_AT: (u16, u16) = (5, 30);

/// The gapped split with a float opened over the gap between the tiles.
fn tiled_float(gaps: bool) -> Tiles {
    let mut tiles = tiled(gaps);
    let width = u64::try_from(FLOAT_LINES[0].chars().count()).unwrap_or(0);
    let mut events = vec![
        UiEvent::GridResize {
            grid: TILED_FLOAT,
            width,
            height: 3,
        },
        UiEvent::WinFloatPos {
            grid: TILED_FLOAT,
            win: WinHandle(1009),
            anchor_grid: 1,
            zindex: 50,
            compindex: 1,
            screen_row: u64::from(FLOAT_AT.0),
            screen_col: u64::from(FLOAT_AT.1),
        },
    ];
    for (row, text) in FLOAT_LINES.iter().enumerate() {
        events.push(line(TILED_FLOAT, row as u64, text, 0));
    }
    events.push(UiEvent::Flush);
    drive(&mut tiles.model, events);
    tiles
}

/// A float wide enough to cross the gap band keeps every cell of its own
/// box: the frames are painted where the separators are, under everything
/// that floats, so nothing draws through it.
///
/// Disconfirm: painting the frames after the whole pane list again puts the
/// gap's blank band and two frame columns through the float's rows.
#[test]
fn a_float_over_a_gap_keeps_its_border() {
    let tiles = tiled_float(true);
    let buf = tiled_frame(&tiles.model);
    let offset = tiles.model.look.grid_offset();
    for (index, text) in FLOAT_LINES.iter().enumerate() {
        let row = row_text(
            &buf,
            FLOAT_AT.0 + offset + u16::try_from(index).unwrap_or(0),
        );
        let start = usize::from(FLOAT_AT.1 + offset);
        let painted: String = row.chars().skip(start).take(text.chars().count()).collect();
        assert_eq!(
            &painted, text,
            "the frames were drawn through the float's own row {index}: {row:?}"
        );
    }
}

/// The row nvim keeps for its command line carries no run of the gapless
/// lattice: a line drawn there would sit on the mode message and the
/// answer to every prompt.
#[test]
fn the_gapless_lattice_stops_above_the_command_line() {
    let (_, grid_height) = outer_grid(false, TILED_HEIGHT);
    let tiles = tiled(false);
    let mut model = tiles.model;
    with_nvims_command_line(&mut model);
    drive(
        &mut model,
        vec![
            line(1, u64::from(grid_height - 1), "-- INSERT --", 0),
            UiEvent::Flush,
        ],
    );
    let buf = tiled_frame(&model);
    let offset = model.look.grid_offset();
    let command_line = row_text(&buf, grid_height - 1 + offset);
    assert!(
        command_line.contains("-- INSERT --"),
        "the lattice drew through nvim's command line: {command_line:?}"
    );
    assert!(
        !command_line.contains(['│', '─', '┴', '┼']),
        "the lattice left an edge on the command line: {command_line:?}"
    );
}

/// Records an attach carrying the tab line and gives the pill two names to
/// draw, which is the shape a tiles session runs in: the derived default
/// turns the row on and the tabpages are what it names.
fn with_the_pill(model: &mut Model) {
    let mut surfaces = view_core::native::ext::shipped_multigrid();
    surfaces.push(view_core::native::ext::Ext::Tabline);
    model.attach_surfaces(surfaces);
    drive(
        model,
        vec![UiEvent::TablineUpdate {
            current: view_core::events::TabHandle(1),
            tabs: vec![
                view_core::events::TabEntry {
                    tab: view_core::events::TabHandle(1),
                    name: "work".into(),
                },
                view_core::events::TabEntry {
                    tab: view_core::events::TabHandle(2),
                    name: "docs".into(),
                },
            ],
        }],
    );
}

/// The pill keeps row 0 to itself. Every horizontal run of the lattice
/// moves down a row once the row is reserved, so no frame edge is drawn
/// over a name a click has to land on.
#[test]
fn the_lattice_sits_under_the_pills_row() {
    let mut tiles = tiled(true);
    assert_eq!(
        tiles.model.chrome_rows(),
        0,
        "the fixture starts with the tab line left to nvim"
    );
    let (_, bare) = frame_lines(&tiled_frame(&tiles.model));
    with_the_pill(&mut tiles.model);
    assert_eq!(tiles.model.chrome_rows(), 1, "the pill reserves its row");
    let buf = tiled_frame(&tiles.model);
    let (_, under) = frame_lines(&buf);
    let moved: Vec<u16> = bare.iter().map(|row| row + 1).collect();
    assert_eq!(
        under.first().copied(),
        moved.first().copied(),
        "the lattice did not move under the pill: {under:?} against {moved:?}"
    );
    let pill = row_text(&buf, 0);
    assert!(
        pill.contains("work") && pill.contains("docs"),
        "the reserved row carries no names: {pill:?}"
    );
    assert!(
        !pill.contains(['\u{2502}', '\u{2500}', '\u{252c}', '\u{253c}']),
        "the lattice drew an edge through the pill's row: {pill:?}"
    );
}

/// The three committed tiers, as the name a golden carries and the four
/// capability probes that make it.
const TIERS: [(&str, bool, bool, bool, bool); 3] = [
    ("full", true, true, true, DRAWS_BOX_GLYPHS),
    ("standard", false, true, false, DRAWS_BOX_GLYPHS),
    ("basic", false, false, false, NO_BOX_GLYPHS),
];

/// The gapped split: a frame around each window with a clear cell between
/// the two frames and around the outside.
#[test]
fn vsplit_tiles() {
    for tier in TIERS {
        assert_golden(
            &format!("{}-vsplit-tiles", tier.0),
            &tiles_dump(tier, tiled(true)),
        );
    }
}

/// The same split with `gaps = false`: one shared line between the windows
/// and no clear cell anywhere.
#[test]
fn vsplit_gapless() {
    for tier in TIERS {
        assert_golden(
            &format!("{}-vsplit-gapless", tier.0),
            &tiles_dump(tier, tiled(false)),
        );
    }
}

/// The gapped split with a float across the gap: the float's own cells,
/// border included, stand where the frames and the gap band would be.
#[test]
fn vsplit_tiles_float() {
    for tier in TIERS {
        assert_golden(
            &format!("{}-vsplit-tiles-float", tier.0),
            &tiles_dump(tier, tiled_float(true)),
        );
    }
}

/// A layout nvim built in two steps: three tiles, each framed inside its
/// own slot, with the two on the right stacked.
#[test]
fn vsplit_tiles_nested() {
    for tier in TIERS {
        assert_golden(
            &format!("{}-vsplit-tiles-nested", tier.0),
            &tiles_dump(tier, tiled_nested(true)),
        );
    }
}

#[test]
fn full_vsplit() {
    assert_golden("full-vsplit", &dump(true, true, true, DRAWS_BOX_GLYPHS));
}

#[test]
fn standard_vsplit() {
    assert_golden(
        "standard-vsplit",
        &dump(false, true, false, DRAWS_BOX_GLYPHS),
    );
}

#[test]
fn basic_vsplit() {
    assert_golden("basic-vsplit", &dump(false, false, false, NO_BOX_GLYPHS));
}

/// The two crossings no file is committed for: the separator charset reads
/// the box-glyph probe and nothing else, so a 16-color terminal that draws
/// box glyphs is separated like any other and a truecolor one that cannot
/// falls to ASCII. Re-point the charset at the tier and both rows fail.
#[test]
fn a_terminals_tier_never_reaches_its_separator() {
    assert_eq!(
        dump(false, false, false, DRAWS_BOX_GLYPHS),
        dump(true, true, true, DRAWS_BOX_GLYPHS),
        "a 16-color terminal that draws box glyphs is separated like any other"
    );
    assert_eq!(
        dump(true, true, true, NO_BOX_GLYPHS),
        dump(false, false, false, NO_BOX_GLYPHS),
        "a terminal that cannot draw box glyphs is separated in ASCII, whatever its colors"
    );
}

/// The chrome groups the compositor resolves through must be groups nvim
/// broadcasts, or they can only ever hold their fallback. Asserted here
/// too, from the consumer's side, because the pin in `view-core` walks the
/// declared groups rather than the ones something paints with.
#[test]
fn the_pane_chrome_groups_resolve_from_the_live_table() {
    let mut table = view_core::hl::HlTable::new();
    table.define_attr(
        3,
        HlAttr {
            fg: Some(0x00AB_CDEF),
            bg: None,
            bold: false,
            italic: false,
            underline: false,
            reverse: false,
        },
    );
    table.set_group("WinSeparator".to_string(), 3);
    let theme = view_core::theme::Theme::from_hl(&table);
    assert_eq!(
        theme.chrome(view_core::theme::ChromeGroup::WinSeparator).fg,
        Some(0x00AB_CDEF),
        "a colorscheme's own separator color never reached the theme"
    );
}

/// A tiled `:vsplit` on a canvas wide enough to carry frames: grid 1 is the
/// terminal less the ring, two windows sit side by side with nvim's
/// separator column between them, and each window's grid is the inner size
/// the look asked nvim for.
const TILED_WIDTH: u16 = 80;
const TILED_HEIGHT: u16 = 24;
const ACCENT_FG: u32 = 0x0089_B4FA;
/// One word per window, so a golden names which tile each rect holds.
const TILE_TEXT: [&str; 3] = ["left", "right", "under"];

struct Tiles {
    /// The slot nvim gave each window, as `(row, col, width, height)`.
    slots: Vec<(u16, u16, u16, u16)>,
    model: Model,
}

fn tiled(gaps: bool) -> Tiles {
    let (grid_width, grid_height) = outer_grid(gaps, TILED_HEIGHT);
    // tiles hold nvim at `laststatus = 2`, so every window has a status row
    // of its own under it -- the row the frame's bottom edge is painted
    // over -- and a session owning the command line keeps no row for it
    let window_height = grid_height - 1;
    let left_width = (grid_width - 1) / 2;
    let right_col = left_width + 1;
    let slots = vec![
        (0, 0, left_width, window_height),
        (0, right_col, grid_width - right_col, window_height),
    ];
    Tiles {
        model: tiled_model(gaps, TILED_HEIGHT, &slots),
        slots,
    }
}

/// The same vsplit with the right-hand column split again, so a frame meets
/// three neighbours instead of one and the lattice has an interior crossing.
fn tiled_nested(gaps: bool) -> Tiles {
    let (grid_width, grid_height) = outer_grid(gaps, TILED_HEIGHT);
    let left_width = (grid_width - 1) / 2;
    let right_col = left_width + 1;
    let right_width = grid_width - right_col;
    // every window keeps a status row under `laststatus = 2`, so the right
    // column spends two of its rows on them where the left spends one
    let top_height = (grid_height - 2).div_ceil(2);
    let slots = vec![
        (0, 0, left_width, grid_height - 1),
        (0, right_col, right_width, top_height),
        (
            top_height + 1,
            right_col,
            right_width,
            grid_height - 2 - top_height,
        ),
    ];
    Tiles {
        model: tiled_model(gaps, TILED_HEIGHT, &slots),
        slots,
    }
}

/// The size of grid 1 under a look, which is the terminal less the ring the
/// look spends on its outer frame.
fn outer_grid(gaps: bool, height: u16) -> (u16, u16) {
    let ring = if gaps { 2 } else { 1 };
    (TILED_WIDTH - ring, height - ring)
}

/// Hands the command line back to nvim, which is the session nvim keeps a
/// row of its own at the grid's foot for: the takeover holds `cmdheight`
/// at 0 only where view owns that row's other tenant as well.
///
/// The tab line stays with nvim here for [`tiled_model`]'s reason: the row
/// the pill takes is a separate question from the row nvim's command line
/// keeps.
fn with_nvims_command_line(model: &mut Model) {
    use view_core::native::ext::Ext;
    model.attach_surfaces(vec![Ext::LineGrid, Ext::Messages]);
}

fn tiled_model(gaps: bool, height: u16, slots: &[(u16, u16, u16, u16)]) -> Model {
    let (grid_width, grid_height) = outer_grid(gaps, height);
    let look = view_core::model::Look::new(view_core::model::Panes::Tiles, gaps);
    let mut model = Model::new().with_look(look);
    model.term_width = TILED_WIDTH;
    model.term_height = height;
    model.statusline_enabled = true;
    // the shipped attach: view owns the command line and the message area,
    // so the takeover holds `cmdheight` at 0 and the grid's last row is a
    // window's status row rather than nvim's own. The tab line is left with
    // nvim, which is what keeps the lattice at the top of the screen: the
    // row the pill takes is `the_lattice_sits_under_the_pills_row`'s
    // question, and every slot here would otherwise be one row lower for a
    // reason that has nothing to do with frames
    model.attach_surfaces(view_core::native::ext::shipped_multigrid());
    model.caps = model.caps.with_unicode_boxes(DRAWS_BOX_GLYPHS);
    let mut events = vec![
        UiEvent::GridResize {
            grid: 1,
            width: u64::from(grid_width),
            height: u64::from(grid_height),
        },
        attr(VIEW_SEPARATOR_HL, VIEW_SEPARATOR_FG),
        UiEvent::HlGroupSet {
            name: "WinSeparator".to_string(),
            hl_id: VIEW_SEPARATOR_HL,
        },
    ];
    for (index, slot) in slots.iter().copied().enumerate() {
        let grid = LEFT + index as u64;
        let (row, col, width, height) = slot;
        // the inner size the look asks for, which is the size nvim answers
        // the request with
        let (inner_width, inner_height) = look.inner_request((width, height), 0);
        let (inner_width, inner_height) = if (inner_width, inner_height) == (0, 0) {
            (width, height)
        } else {
            (inner_width, inner_height)
        };
        events.push(UiEvent::WinPos {
            grid,
            win: WinHandle(1000 + index as u64),
            startrow: u64::from(row),
            startcol: u64::from(col),
            width: u64::from(width),
            height: u64::from(height),
        });
        events.push(UiEvent::GridResize {
            grid,
            width: u64::from(inner_width),
            height: u64::from(inner_height),
        });
        events.push(line(grid, 0, TILE_TEXT[index.min(TILE_TEXT.len() - 1)], 0));
    }
    events.extend([
        UiEvent::GridCursorGoto {
            grid: LEFT,
            row: 0,
            col: 0,
        },
        UiEvent::Flush,
    ]);
    drive(&mut model, events);
    model.engine.set_accent_token(Some(ACCENT_FG));
    model
        .engine
        .statusline
        .apply(SegmentUpdate::Mode("-- INSERT --".to_string()));
    for (index, _) in slots.iter().enumerate() {
        let _ = update(
            &mut model,
            Msg::WindowStatus {
                win: WinHandle(1000 + index as u64),
                status: window_status(TILE_TEXT[index.min(TILE_TEXT.len() - 1)], index),
            },
        );
    }
    model
}

/// One window's reported status, distinct per tile so a golden names which
/// frame carries which.
fn window_status(name: &str, index: usize) -> WindowStatus {
    let mut status = WindowStatus::default();
    status.buf = 1 + index as u64;
    status.name = format!("{name}.rs");
    status.row = 1 + index as u32;
    status.col = 1;
    status.errors = index as u32;
    status
}

fn tiled_frame(model: &Model) -> Buffer {
    let surface = view_surface::render(model);
    let backend = TestBackend::new(model.term_width, model.term_height);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal
        .draw(|f| composite_into(f.buffer_mut(), model, &surface, &Damage::full()))
        .unwrap();
    terminal.backend().buffer().clone()
}

/// Every column carrying a vertical frame run, and every row carrying a
/// horizontal one, read back off the composited screen.
fn frame_lines(buf: &Buffer) -> (Vec<u16>, Vec<u16>) {
    let is = |col: u16, row: u16, glyphs: &str| glyphs.contains(buf[(col, row)].symbol());
    let cols = (0..buf.area.width)
        .filter(|&col| {
            (0..buf.area.height)
                .filter(|&row| is(col, row, "│├┤┼┬┴"))
                .count()
                >= 2
        })
        .collect();
    let rows = (0..buf.area.height)
        .filter(|&row| {
            (0..buf.area.width)
                .filter(|&col| is(col, row, "─├┤┼┬┴"))
                .count()
                >= 2
        })
        .collect();
    (cols, rows)
}

#[test]
fn a_gapped_frame_leaves_two_cells_between_neighbouring_tiles() {
    let tiles = tiled(true);
    let buf = tiled_frame(&tiles.model);
    let (cols, _) = frame_lines(&buf);
    assert_eq!(
        cols.len(),
        4,
        "two gapped tiles carry two vertical frame runs each: {cols:?}"
    );
    let (left_edge, right_edge) = (cols[1], cols[2]);
    assert_eq!(
        right_edge - left_edge - 1,
        3,
        "two gaps and nvim's separator column sit between the frames: {cols:?}"
    );
    for col in (left_edge + 1)..right_edge {
        for row in 0..TILED_HEIGHT {
            assert_eq!(
                buf[(col, row)].symbol(),
                " ",
                "the cells between two frames are gap, not nvim's separator"
            );
        }
    }
}

#[test]
fn no_two_gapless_edge_lines_are_adjacent() {
    let buf = tiled_frame(&tiled(false).model);
    let (cols, rows) = frame_lines(&buf);
    assert!(!cols.is_empty() && !rows.is_empty(), "no lattice was drawn");
    for lines in [&cols, &rows] {
        for pair in lines.windows(2) {
            assert!(
                pair[1] - pair[0] > 1,
                "two edge lines sit side by side, which is a doubled edge: {lines:?}"
            );
        }
    }
}

#[test]
fn a_gapless_junction_takes_the_crossing_glyph() {
    let tiles = tiled(false);
    let buf = tiled_frame(&tiles.model);
    let (_, col, width, _) = tiles.slots[0];
    // the engine layer sits one cell in from the terminal on each axis
    // under gapless, so the lattice's own coordinates shift with it
    let x = col + width + 1;
    assert_eq!(
        buf[(x, 0)].symbol(),
        "┬",
        "the separator column starts where the ring's top run carries on"
    );

    // the nested layout is where a line carries on past the junction
    let nested = tiled_nested(false);
    let buf = tiled_frame(&nested.model);
    let (row, col, _, height) = nested.slots[1];
    assert_eq!(
        buf[(col, row + height + 1)].symbol(),
        "├",
        "the row under the upper right tile starts where the column beside \
         the left tile carries on"
    );
    assert_eq!(
        buf[(TILED_WIDTH - 1, row + height + 1)].symbol(),
        "─",
        "that row ends at the terminal edge, which draws no line of its own"
    );
}

#[test]
fn only_the_active_tiles_frame_carries_the_accent_fg() {
    let tiles = tiled(true);
    let buf = tiled_frame(&tiles.model);
    let accent = rgb(ACCENT_FG).expect("the accent token resolves to a colour");
    let frame_fg = |slot: (u16, u16, u16, u16)| {
        let (row, col, _, _) = slot;
        buf[(col + 1 + 1, row + 1 + 1)].fg
    };
    assert_eq!(
        frame_fg(tiles.slots[0]),
        accent,
        "the tile the cursor is in carries the accent"
    );
    assert_ne!(
        frame_fg(tiles.slots[1]),
        accent,
        "every other tile's frame is the quiet separator colour"
    );
}

#[test]
fn a_grid_line_on_a_window_leaves_the_frame_rows_undamaged() {
    let mut tiles = tiled(true);
    let _ = tiles.model.take_paint_damage();
    drive(&mut tiles.model, vec![line(LEFT, 1, "typed", 0)]);
    let damage = tiles.model.take_paint_damage();
    let (row, _, _, height) = tiles.slots[0];
    let offset = tiles.model.chrome_rows() + tiles.model.look.grid_offset();
    let damaged = Damage::from_frame(&damage, offset, &[], false);
    assert!(
        damaged.covers(offset + row + 2 + 1),
        "the row the line landed on is repainted"
    );
    for frame_row in [row + 1, row + height - 2] {
        assert!(
            !damaged.covers(offset + frame_row),
            "a grid line inside a window damaged the frame row at {frame_row}"
        );
    }
}

/// A session's first frame, before the engine has sent a picture to put
/// inside it.
fn shell_model(look: view_core::model::Look) -> Model {
    let mut model = Model::new().with_look(look);
    model.term_width = TILED_WIDTH;
    model.term_height = TILED_HEIGHT;
    model.caps = model.caps.with_unicode_boxes(DRAWS_BOX_GLYPHS);
    model.chrome_painted = false;
    model.statusline_enabled = true;
    model
}

fn shell_frame(look: view_core::model::Look) -> Buffer {
    tiled_frame(&shell_model(look))
}

/// The rect the bar claims on that frame, or `None` where the look gives
/// it no row. Read off the surface rather than off the picture, since an
/// empty bar paints no glyph either way.
fn shell_bar_rect(look: view_core::model::Look) -> Option<view_surface::Rect> {
    view_surface::render(&shell_model(look))
        .layers
        .iter()
        .find_map(|layer| {
            matches!(layer.kind, view_surface::LayerKind::Statusline(_)).then_some(layer.rect)
        })
}

#[test]
fn the_shell_frame_paints_the_ring_under_tiles_and_the_bar_under_nvim() {
    let tiles = shell_frame(view_core::model::Look::new(
        view_core::model::Panes::Tiles,
        true,
    ));
    assert_eq!(
        tiles[(0, 0)].symbol(),
        "╭",
        "the tiled shell opens with the ring's own corner"
    );
    // tiles keep no row for a bar, so the ring runs to the terminal's own
    // bottom row
    assert_eq!(
        tiles[(TILED_WIDTH - 1, TILED_HEIGHT - 1)].symbol(),
        "╯",
        "and closes on the terminal's own bottom row"
    );

    let nvim = shell_frame(view_core::model::Look::new(
        view_core::model::Panes::Nvim,
        true,
    ));
    assert_eq!(
        row_text(&nvim, TILED_HEIGHT - 1).trim(),
        "",
        "the nvim-mode shell is a bar, which carries no glyph of its own"
    );
    let ringed = (0..TILED_HEIGHT)
        .any(|row| row_text(&nvim, row).contains(['\u{256d}', '\u{2500}', '\u{2502}']));
    assert!(
        !ringed,
        "the nvim-mode shell reserves a row for its bar and frames nothing"
    );

    // the bar's own rect, which the blank row above cannot discriminate:
    // this frame has no grid to measure, and the arithmetic the attached
    // frame uses put a two-cell bar in the ring's top-left corner here
    for gaps in [true, false] {
        for panes in [
            view_core::model::Panes::Tiles,
            view_core::model::Panes::Nvim,
        ] {
            let look = view_core::model::Look::new(panes, gaps);
            let expected = (panes == view_core::model::Panes::Nvim)
                .then(|| view_surface::Rect::new(TILED_HEIGHT - 1, 0, TILED_WIDTH, 1));
            assert_eq!(
                shell_bar_rect(look),
                expected,
                "the shell frame's bar row disagreed with the look \
                 ({panes:?}, gaps {gaps})"
            );
        }
    }
}

/// Nothing reserves a bottom row under tiles: the segments sit in each
/// frame's own bottom edge, so the bar's row would be a blank one the ring
/// has to stop above.
///
/// Four readers answer the question and a disagreement between any two
/// leaves a row painted by nobody or a grid an cell taller than the space
/// for it: the model's own answer, the geometry the spawn's `--cmd` asks
/// nvim for, the outer grid's target size, and whether the composed frame
/// carries a bar layer at all.
#[test]
fn no_bar_row_stands_under_tiles() {
    for gaps in [true, false] {
        for (panes, bar) in [
            (view_core::model::Panes::Tiles, 0),
            (view_core::model::Panes::Nvim, 1),
        ] {
            let look = view_core::model::Look::new(panes, gaps);
            let mut model = shell_model(look);
            model.chrome_painted = true;
            assert_eq!(
                model.statusline_rows(),
                bar,
                "the model's own bar rows ({panes:?}, gaps {gaps})"
            );
            assert_eq!(
                look.bar_rows(true),
                bar,
                "the rule the spawn geometry reads ({panes:?}, gaps {gaps})"
            );
            assert_eq!(
                model.grid_target().1,
                TILED_HEIGHT - look.ring() - bar,
                "the outer grid's height ({panes:?}, gaps {gaps})"
            );
            assert_eq!(
                shell_bar_rect(look).is_some(),
                bar > 0,
                "the composed frame's bar layer ({panes:?}, gaps {gaps})"
            );
        }
    }
}

/// The rows a gapped tile's two frame edges land on, and the one row a
/// gapless tile's single edge does.
fn edge_rows(model: &Model, slot: (u16, u16, u16, u16)) -> (u16, u16) {
    let (row, _, _, height) = slot;
    let offset = model.look.grid_offset() + model.chrome_rows();
    if model.look.gaps {
        (row + 1 + offset, row + height - 2 + offset)
    } else {
        (row + height + offset, row + height + offset)
    }
}

/// The mode is the session's, not the window's, so it belongs only to the
/// tile the user is typing into. Two tiles both claiming `-- INSERT --`
/// would say the user is in both at once.
#[test]
fn an_inactive_tile_shows_no_mode_segment() {
    let tiles = tiled(true);
    let buf = tiled_frame(&tiles.model);
    let (_, active_row) = edge_rows(&tiles.model, tiles.slots[0]);
    let (_, inactive_row) = edge_rows(&tiles.model, tiles.slots[1]);
    assert!(
        row_text(&buf, active_row).contains("-- INSERT --"),
        "the tile the cursor is in lost its mode: {:?}",
        row_text(&buf, active_row)
    );
    // one row, two tiles: the assertion above already read the left half
    // of it, so the right half is where an inactive mode would show
    let (_, _, left_width, _) = tiles.slots[0];
    let right = row_text(&buf, inactive_row);
    let right = right
        .chars()
        .skip(usize::from(left_width))
        .collect::<String>();
    assert!(
        !right.contains("INSERT"),
        "the tile the cursor is not in claimed the mode: {right:?}"
    );
}

/// Under tiles the position comes from the bridge's `window` trigger,
/// because nvim stops emitting `msg_ruler` the moment `laststatus` is 2 and
/// the segment would otherwise read a number frozen at the last frame
/// before the hold went in.
#[test]
fn the_position_segment_reads_the_window_trigger_under_tiles() {
    let tiles = tiled(true);
    let mut model = tiles.model;
    model
        .engine
        .statusline
        .apply(view_core::native::statusline::SegmentUpdate::Ruler(
            "99,99".to_string(),
        ));
    let _ = update(
        &mut model,
        Msg::WindowStatus {
            win: WinHandle(1000),
            status: {
                let mut status = WindowStatus::default();
                status.name = "left.rs".to_string();
                status.row = 42;
                status.col = 13;
                status
            },
        },
    );
    let buf = tiled_frame(&model);
    let (_, bottom) = edge_rows(&model, tiles.slots[0]);
    let edge = row_text(&buf, bottom);
    assert!(
        edge.contains("42:13"),
        "the tile's own position never reached its frame: {edge:?}"
    );
    assert!(
        !edge.contains("99,99"),
        "the tile's frame took the session's stale ruler: {edge:?}"
    );
}

/// A cursor move inside a tile repaints the edge its position is painted
/// on. nvim redraws nothing for one, and the edge row lies outside the
/// window's own grid, so a frame clipped to the damage nvim reported would
/// keep the reading before the move.
///
/// Disconfirm: leaving the edge rows out of the update's own damage paints
/// the first reading again.
#[test]
fn a_cursor_move_repaints_the_edge_its_position_stands_on() {
    let tiles = tiled(true);
    let mut model = tiles.model;
    let moved = |row: u32, col: u32| {
        let mut status = WindowStatus::default();
        status.name = "left.rs".to_string();
        status.row = row;
        status.col = col;
        status
    };
    let _ = update(
        &mut model,
        Msg::WindowStatus {
            win: WinHandle(1000),
            status: moved(1, 1),
        },
    );
    let mut buf = tiled_frame(&model);
    let _ = model.take_paint_damage();
    let _ = update(
        &mut model,
        Msg::WindowStatus {
            win: WinHandle(1000),
            status: moved(42, 13),
        },
    );
    let grid_damage = model.take_paint_damage();
    let damage = Damage::from_frame(
        &grid_damage,
        view_surface::grid_origin(&model).0,
        &[],
        false,
    );
    let surface = view_surface::render(&model);
    composite_into(&mut buf, &model, &surface, &damage);
    let (_, bottom) = edge_rows(&model, tiles.slots[0]);
    let edge = row_text(&buf, bottom);
    assert!(
        edge.contains("42:13"),
        "the moved cursor never reached the frame: {edge:?}"
    );
}

/// A session-wide segment repaints every tile's edge. The mode, the
/// branch and the diagnostic counts reach every frame's own bottom edge,
/// and nvim raises no grid damage for any of them, so a frame clipped to
/// its damage kept the segments from before the update. It shipped that
/// way: `key_flood` died on the composite guard at the bottom edge row.
///
/// Disconfirm: leaving the segment rows out of the frame's damage paints
/// the mode the tile already showed.
#[test]
fn a_segment_change_repaints_every_tiles_edge() {
    let tiles = tiled(true);
    let mut model = tiles.model;
    let mut buf = tiled_frame(&model);
    let _ = model.take_paint_damage();
    let _ = update(
        &mut model,
        Msg::Redraw(vec![UiEvent::MsgShowmode {
            content: vec![(0, "-- VISUAL --".to_string())],
        }]),
    );
    let grid_damage = model.take_paint_damage();
    let damage = Damage::from_frame(
        &grid_damage,
        view_surface::grid_origin(&model).0,
        &[],
        false,
    );
    let surface = view_surface::render(&model);
    composite_into(&mut buf, &model, &surface, &damage);
    let (_, bottom) = edge_rows(&model, tiles.slots[0]);
    let edge = row_text(&buf, bottom);
    assert!(
        edge.contains("VISUAL"),
        "the new mode never reached the frame: {edge:?}"
    );
}

/// A gapped tile's name is in the top edge, where the buffer it holds is
/// read as a title rather than mixed in with the counts.
#[test]
fn a_gapped_tile_names_its_buffer_in_the_top_edge() {
    let tiles = tiled(true);
    let buf = tiled_frame(&tiles.model);
    let (top, bottom) = edge_rows(&tiles.model, tiles.slots[0]);
    assert!(
        row_text(&buf, top).contains("left.rs"),
        "the top edge carries no name: {:?}",
        row_text(&buf, top)
    );
    assert!(
        !row_text(&buf, bottom).contains("left.rs"),
        "the name was written twice: {:?}",
        row_text(&buf, bottom)
    );
}

/// A gapless tile has one edge row and no top run of its own, so the name
/// and the segments share it.
#[test]
fn a_gapless_tile_puts_the_name_and_the_segments_in_one_row() {
    let tiles = tiled(false);
    let buf = tiled_frame(&tiles.model);
    let (_, bottom) = edge_rows(&tiles.model, tiles.slots[0]);
    let edge = row_text(&buf, bottom);
    let (name, position) = (
        edge.find("left.rs").expect("the edge carries the name"),
        edge.find("1:1").expect("the edge carries the position"),
    );
    assert!(
        name < position,
        "the name follows the segments instead of leading them: {edge:?}"
    );
}

/// A name is measured in the cells it draws, not in characters. A script
/// that draws two cells to the character fits the edge by count and runs
/// past the closing blank and over the corner.
///
/// Disconfirm: counting `chars()` writes the 36-cell name into a 32-cell
/// edge.
#[test]
fn a_two_cell_name_is_measured_in_cells_and_not_in_chars() {
    // the gapped top edge is the slot less its gap ring, less a blank and a
    // corner either side: 32 cells for text
    for (chars, fits) in [(18_usize, false), (15, true)] {
        let tiles = tiled(true);
        let mut model = tiles.model;
        let _ = update(
            &mut model,
            Msg::WindowStatus {
                win: WinHandle(1000),
                status: {
                    let mut status = WindowStatus::default();
                    status.name = "編".repeat(chars);
                    status
                },
            },
        );
        let buf = tiled_frame(&model);
        let (top, _) = edge_rows(&model, tiles.slots[0]);
        let edge = row_text(&buf, top);
        assert_eq!(
            edge.contains('編'),
            fits,
            "a {chars}-character name drawing {} cells in a 32-cell edge: {edge:?}",
            chars * 2
        );
        if fits {
            // the cell a two-cell glyph covers is reset to a space, so a
            // run that advanced one column per character reads back with
            // the glyphs against each other
            assert!(
                edge.contains("編 編"),
                "a two-cell glyph advanced one column: {edge:?}"
            );
        }
    }
}

/// The gapless edge of a full-width tile ends on the screen's last column,
/// so a name that overran it would take the corner and then the cell
/// outside the buffer. The edge carries 76 cells of text between its two
/// blanks, and a name is measured in the cells its clusters draw, so one
/// of exactly 76 fits and one of 77 is dropped whole.
///
/// Disconfirm: measuring a decomposed name by its characters drops the one
/// that fits, and measuring it by the string's display width paints the
/// one that does not, over the corner and past the buffer.
#[test]
fn a_decomposed_name_stops_inside_the_edge() {
    let slots = vec![(0, 0, TILED_WIDTH - 1, TILED_HEIGHT - 2)];
    let plain = edge_with_name(&slots, "left.rs".to_string());
    // two combining marks, which take no cell of their own
    for (cells, fits) in [(76_usize, true), (77, false)] {
        let name = format!("e\u{301}e\u{301}{}", "a".repeat(cells - 2));
        let edge = edge_with_name(&slots, name);
        assert_eq!(
            edge.chars().last(),
            plain.chars().last(),
            "the name walked over the corner: {edge:?}"
        );
        assert_eq!(
            edge.contains("aaa"),
            fits,
            "a name of {cells} cells in a 76-cell edge: {edge:?}"
        );
    }
}

/// A name reaches view in whatever form the filesystem holds it, and the
/// two forms of `café.rs` are the same name: macOS hands back the
/// decomposed one, Linux usually the composed one, and a user who opens
/// the file on either sees the same frame.
///
/// Disconfirm: a run that places one character per cell gives the mark a
/// cell of its own, which moves `.rs` one column right and leaves the
/// accent standing after the letter.
#[test]
fn a_decomposed_name_paints_as_its_composed_form() {
    let slots = vec![(0, 0, TILED_WIDTH - 1, TILED_HEIGHT - 2)];
    let cells = |name: &str| {
        let mut model = tiled_model(false, TILED_HEIGHT, &slots);
        let _ = update(
            &mut model,
            Msg::WindowStatus {
                win: WinHandle(1000),
                status: {
                    let mut status = WindowStatus::default();
                    status.name = name.to_string();
                    status
                },
            },
        );
        let buf = tiled_frame(&model);
        row_cells(&buf, edge_rows(&model, slots[0]).1)
    };
    let composed = cells("caf\u{e9}.rs");
    let decomposed = cells("cafe\u{301}.rs");
    let differing: Vec<usize> = composed
        .iter()
        .zip(&decomposed)
        .enumerate()
        .filter(|(_, (left, right))| left != right)
        .map(|(col, _)| col)
        .collect();
    assert_eq!(
        differing.len(),
        1,
        "the two forms of one name took different cells: {:?} against {:?}",
        composed.join("|"),
        decomposed.join("|")
    );
    let col = differing[0];
    assert_eq!(
        (composed[col].as_str(), decomposed[col].as_str()),
        ("\u{e9}", "e\u{301}"),
        "the cell they differ in is not the accented letter"
    );
}

/// One tile's gapless edge row, with `name` reported for it.
fn edge_with_name(slots: &[(u16, u16, u16, u16)], name: String) -> String {
    let mut model = tiled_model(false, TILED_HEIGHT, slots);
    let _ = update(
        &mut model,
        Msg::WindowStatus {
            win: WinHandle(1000),
            status: {
                let mut status = WindowStatus::default();
                status.name = name;
                status
            },
        },
    );
    let buf = tiled_frame(&model);
    row_text(&buf, edge_rows(&model, slots[0]).1)
}

/// A group arrives whole or not at all. The separator is a span of its own
/// and the counts are two more, so a fit taken span by span paints a
/// separator with nothing behind it and cuts the diagnostics between the
/// errors and the warnings, which reads as no warnings at all.
///
/// Disconfirm: fitting span by span leaves `E 1` on the edge with the `W 2`
/// it was measured beside dropped.
#[test]
fn an_edge_too_narrow_for_a_whole_group_carries_none_of_it() {
    // 18 cells of text: the mode, and neither room for the counts beside it
    // nor for the separator they would follow
    let slots = vec![(0, 0, 24, TILED_HEIGHT - 3)];
    let mut model = tiled_model(true, TILED_HEIGHT, &slots);
    let _ = update(
        &mut model,
        Msg::WindowStatus {
            win: WinHandle(1000),
            status: {
                let mut status = WindowStatus::default();
                status.name = "left.rs".to_string();
                status.errors = 1;
                status.warnings = 2;
                status
            },
        },
    );
    let buf = tiled_frame(&model);
    let (_, bottom) = edge_rows(&model, slots[0]);
    let edge = row_text(&buf, bottom);
    assert!(
        edge.contains("INSERT"),
        "the edge carries no segments at all: {edge:?}"
    );
    assert!(
        !edge.contains("E 1"),
        "the errors were painted without the warnings beside them: {edge:?}"
    );
}

/// `[native] statusline = false` hands the segments back and leaves the
/// frame standing: the surface the switch answers for is the text, and the
/// tile's own edge is drawn whatever the answer.
#[test]
fn statusline_false_under_tiles_keeps_the_row_and_empties_the_segments() {
    for gaps in [true, false] {
        let tiles = tiled(gaps);
        let mut model = tiles.model;
        model.statusline_enabled = false;
        let buf = tiled_frame(&model);
        let (top, bottom) = edge_rows(&model, tiles.slots[0]);
        let edge = row_text(&buf, bottom);
        assert!(
            !edge.contains("left.rs") && !edge.contains("1:1") && !edge.contains("INSERT"),
            "the handed-back segments were painted anyway (gaps {gaps}): {edge:?}"
        );
        assert!(
            !row_text(&buf, top).contains("left.rs"),
            "the handed-back name was painted anyway (gaps {gaps})"
        );
        let (_, col, width, _) = tiles.slots[0];
        let offset = model.look.grid_offset();
        let run = (col + offset..col + width + offset)
            .filter(|&c| "─┴┼╰┤├".contains(buf[(c, bottom)].symbol()))
            .count();
        assert!(
            run > usize::from(width) / 2,
            "the tile lost its bottom edge with its segments (gaps {gaps}): {run}"
        );
    }
}

/// The left tile handed to the tree, as nvim reports it: the surface
/// claims the window, the redraw places the same slot again, and the scan
/// answers with two rows.
fn tree_in_the_left_tile(gaps: bool) -> Tiles {
    let tiles = tiled(gaps);
    let slots = tiles.slots;
    let mut model = tiles.model;
    model.surfaces.set_layout(
        view_core::native::geometry::NativeSurface::Tree,
        view_core::native::geometry::SurfaceLayout::new(
            view_core::native::geometry::SurfacePlacement::Windowed,
            view_core::native::geometry::Anchor::Left,
            30,
        ),
    );
    let effects = update(
        &mut model,
        Msg::FeatureInvoke {
            feature: "tree".to_string(),
            verb: "toggle".to_string(),
        },
    );
    let mut generation = 0;
    let mut scan = 0;
    for effect in &effects {
        match effect {
            view_core::msg::Effect::Rpc(view_core::msg::RpcCall::OpenNativeWindow {
                generation: g,
                ..
            }) => generation = *g,
            view_core::msg::Effect::TreeScan { generation: g, .. } => scan = *g,
            _ => {}
        }
    }
    let _ = update(
        &mut model,
        Msg::NativeWindowOpened {
            generation,
            surface: view_core::native::geometry::NativeSurface::Tree,
            win: WinHandle(1000),
        },
    );
    let _ = update(
        &mut model,
        Msg::TreeScanResult {
            generation: scan,
            entries: vec![
                view_core::native::tree::TreeEntry::new("alpha.rs".into(), false, 0),
                view_core::native::tree::TreeEntry::new("beta.rs".into(), false, 0),
            ],
        },
    );
    let (row, col, width, height) = slots[0];
    drive(
        &mut model,
        vec![
            UiEvent::WinPos {
                grid: LEFT,
                win: WinHandle(1000),
                startrow: u64::from(row),
                startcol: u64::from(col),
                width: u64::from(width),
                height: u64::from(height),
            },
            UiEvent::Flush,
        ],
    );
    Tiles { slots, model }
}

/// The windowed tree draws inside the rect nvim gave its window and
/// nowhere else: its entries stand in the left tile's columns, and the
/// right tile still carries the buffer text nvim painted there.
#[test]
fn the_tree_view_paints_into_its_native_panes_rect() {
    let tiles = tree_in_the_left_tile(false);
    let (row, col, width, _) = tiles.slots[0];
    let model = tiles.model;
    let buf = tiled_frame(&model);
    let offset = model.look.grid_offset();
    let inside = |line: &str| -> String {
        line.chars()
            .skip(usize::from(col + offset))
            .take(usize::from(width))
            .collect()
    };
    let mut found = false;
    for screen_row in row..row + 6 {
        if inside(&row_text(&buf, screen_row)).contains("alpha.rs") {
            found = true;
        }
    }
    assert!(found, "the tree painted nothing into its pane");
    assert!(
        (row..row + 6).any(|screen_row| row_text(&buf, screen_row).contains("right")),
        "the tree's pane took the neighbour's text with it"
    );
}

/// The committed picture of the tree in a tile of its own: its rows inside
/// the left frame, the buffer's text inside the right one.
#[test]
fn tree_windowed() {
    for tier in TIERS {
        assert_golden(
            &format!("{}-tree-windowed", tier.0),
            &tiles_dump(tier, tree_in_the_left_tile(true)),
        );
    }
}

/// The agent panel windowed into the right tile, mirroring
/// `tree_in_the_left_tile`: claims the existing tile's window (same grid,
/// same handle) rather than asking nvim to open a fresh one, and carries one
/// transcript line so the golden shows painted content, not an empty box.
fn agent_in_the_right_tile(gaps: bool) -> Tiles {
    let tiles = tiled(gaps);
    let slots = tiles.slots;
    let mut model = tiles.model;
    model.ai_trusted = true;
    model.surfaces.set_layout(
        view_core::native::geometry::NativeSurface::Agent,
        view_core::native::geometry::SurfaceLayout::new(
            view_core::native::geometry::SurfacePlacement::Windowed,
            view_core::native::geometry::Anchor::Right,
            30,
        ),
    );
    let effects = update(
        &mut model,
        Msg::FeatureInvoke {
            feature: "ai".to_string(),
            verb: "toggle".to_string(),
        },
    );
    let mut generation = 0;
    for effect in &effects {
        if let view_core::msg::Effect::Rpc(view_core::msg::RpcCall::OpenNativeWindow {
            generation: g,
            ..
        }) = effect
        {
            generation = *g;
        }
    }
    let _ = update(
        &mut model,
        Msg::NativeWindowOpened {
            generation,
            surface: view_core::native::geometry::NativeSurface::Agent,
            win: WinHandle(1001),
        },
    );
    model
        .ai_panel_mut()
        .transcript
        .echo_user_prompt("what does the gaps toggle do?");
    let (row, col, width, height) = slots[1];
    drive(
        &mut model,
        vec![
            UiEvent::WinPos {
                grid: LEFT + 1,
                win: WinHandle(1001),
                startrow: u64::from(row),
                startcol: u64::from(col),
                width: u64::from(width),
                height: u64::from(height),
            },
            UiEvent::Flush,
        ],
    );
    Tiles { slots, model }
}

/// The committed picture of the agent panel in a tile of its own: its
/// transcript line inside the right frame, the buffer's text inside the left
/// one.
#[test]
fn agent_windowed() {
    for tier in TIERS {
        assert_golden(
            &format!("{}-agent-windowed", tier.0),
            &tiles_dump(tier, agent_in_the_right_tile(true)),
        );
    }
}

/// The palette windowed into the left tile, the same claim-the-tile
/// recipe as the tree and the agent panel: nvim's own `CmdlineShow` is the
/// whole signal a windowed palette watches (see `ui_event.rs`'s own doc),
/// so there is no `FeatureInvoke` to send here.
fn palette_in_the_left_tile(gaps: bool) -> Tiles {
    let tiles = tiled(gaps);
    let slots = tiles.slots;
    let mut model = tiles.model;
    model.surfaces.set_layout(
        view_core::native::geometry::NativeSurface::Palette,
        view_core::native::geometry::SurfaceLayout::new(
            view_core::native::geometry::SurfacePlacement::Windowed,
            view_core::native::geometry::Anchor::Bottom,
            30,
        ),
    );
    let effects = update(
        &mut model,
        Msg::Redraw(vec![UiEvent::CmdlineShow {
            content: vec![(0, "e file.txt".to_string())],
            pos: 11,
            firstc: ":".to_string(),
            prompt: String::new(),
            indent: 0,
            level: 1,
        }]),
    );
    let mut generation = 0;
    for effect in &effects {
        if let view_core::msg::Effect::Rpc(view_core::msg::RpcCall::OpenNativeWindow {
            generation: g,
            ..
        }) = effect
        {
            generation = *g;
        }
    }
    let _ = update(
        &mut model,
        Msg::NativeWindowOpened {
            generation,
            surface: view_core::native::geometry::NativeSurface::Palette,
            win: WinHandle(1000),
        },
    );
    let (row, col, width, height) = slots[0];
    drive(
        &mut model,
        vec![
            UiEvent::WinPos {
                grid: LEFT,
                win: WinHandle(1000),
                startrow: u64::from(row),
                startcol: u64::from(col),
                width: u64::from(width),
                height: u64::from(height),
            },
            UiEvent::Flush,
        ],
    );
    Tiles { slots, model }
}

/// The committed picture of the palette in a tile of its own: nvim's
/// command line inside the left frame, the buffer's text inside the right
/// one.
#[test]
fn palette_windowed() {
    for tier in TIERS {
        assert_golden(
            &format!("{}-palette-windowed", tier.0),
            &tiles_dump(tier, palette_in_the_left_tile(true)),
        );
    }
}

/// The notification stream windowed into the left tile, one history entry
/// recorded before the tile is claimed so the golden shows painted content.
fn notifications_in_the_left_tile(gaps: bool) -> Tiles {
    let tiles = tiled(gaps);
    let slots = tiles.slots;
    let mut model = tiles.model;
    model.surfaces.set_layout(
        view_core::native::geometry::NativeSurface::Notifications,
        view_core::native::geometry::SurfaceLayout::new(
            view_core::native::geometry::SurfacePlacement::Windowed,
            view_core::native::geometry::Anchor::Left,
            30,
        ),
    );
    let _ = model.engine.record_message(
        "echomsg".to_string(),
        vec![(0, "3 files saved".to_string())],
        false,
    );
    let effects = update(
        &mut model,
        Msg::FeatureInvoke {
            feature: "notifications".to_string(),
            verb: "history".to_string(),
        },
    );
    let mut generation = 0;
    for effect in &effects {
        if let view_core::msg::Effect::Rpc(view_core::msg::RpcCall::OpenNativeWindow {
            generation: g,
            ..
        }) = effect
        {
            generation = *g;
        }
    }
    let _ = update(
        &mut model,
        Msg::NativeWindowOpened {
            generation,
            surface: view_core::native::geometry::NativeSurface::Notifications,
            win: WinHandle(1000),
        },
    );
    let (row, col, width, height) = slots[0];
    drive(
        &mut model,
        vec![
            UiEvent::WinPos {
                grid: LEFT,
                win: WinHandle(1000),
                startrow: u64::from(row),
                startcol: u64::from(col),
                width: u64::from(width),
                height: u64::from(height),
            },
            UiEvent::Flush,
        ],
    );
    Tiles { slots, model }
}

/// The committed picture of the notification stream in a tile of its own:
/// the recorded message inside the left frame, the buffer's text inside the
/// right one.
#[test]
fn notifications_windowed() {
    for tier in TIERS {
        assert_golden(
            &format!("{}-notifications-windowed", tier.0),
            &tiles_dump(tier, notifications_in_the_left_tile(true)),
        );
    }
}

/// A toast stack anchored to one of the four corners, over the same two-tile
/// scene every other golden here uses -- toasts float over the tiles rather
/// than claiming one, so this needs none of the native-window machinery the
/// windowed surfaces above do.
fn toast_stack_in_the_corner(gaps: bool, anchor: view_core::native::geometry::Anchor) -> Tiles {
    let tiles = tiled(gaps);
    let slots = tiles.slots;
    let mut model = tiles.model;
    let _ = model
        .engine
        .messages
        .resolve_startup_hold(view_core::native::toast::HoldOutcome::Release);
    let layout = model
        .surfaces
        .layout(view_core::native::geometry::NativeSurface::Notifications);
    model.surfaces.set_layout(
        view_core::native::geometry::NativeSurface::Notifications,
        view_core::native::geometry::SurfaceLayout::new(layout.placement, anchor, layout.size),
    );
    for text in ["saved", "2 matches", "linted"] {
        drive(
            &mut model,
            vec![UiEvent::MsgShow {
                kind: "echomsg".to_string(),
                content: vec![(0, text.to_string())],
                replace_last: false,
            }],
        );
    }
    Tiles { slots, model }
}

/// The four committed pictures of the toast stack growing away from its own
/// corner, at three tiers each.
#[test]
fn notifications_corner_scenes() {
    let corners = [
        ("top-left", view_core::native::geometry::Anchor::TopLeft),
        ("top-right", view_core::native::geometry::Anchor::TopRight),
        (
            "bottom-left",
            view_core::native::geometry::Anchor::BottomLeft,
        ),
        (
            "bottom-right",
            view_core::native::geometry::Anchor::BottomRight,
        ),
    ];
    for (name, anchor) in corners {
        for tier in TIERS {
            assert_golden(
                &format!("{}-notifications-corner-{name}", tier.0),
                &tiles_dump(tier, toast_stack_in_the_corner(true, anchor)),
            );
        }
    }
}

/// A surface's own window holds an unnamed scratch buffer, so its frame
/// carries the surface's name where an ordinary tile carries the file's,
/// and its bottom edge carries neither the mode nor a cursor position:
/// nvim's are the scratch window's own and describe nothing a person is
/// editing.
#[test]
fn a_native_panes_frame_carries_its_surfaces_name() {
    let tiles = tree_in_the_left_tile(true);
    let (_, col, width, _) = tiles.slots[0];
    let buf = tiled_frame(&tiles.model);
    let offset = tiles.model.look.grid_offset();
    let (top_row, bottom_row) = edge_rows(&tiles.model, tiles.slots[0]);
    let run = |row: u16| -> String {
        row_text(&buf, row)
            .chars()
            .skip(usize::from(col + offset))
            .take(usize::from(width))
            .collect()
    };
    let top = run(top_row);
    assert!(
        top.contains("tree"),
        "the tree's frame did not name the surface: {top:?}"
    );
    assert!(
        !top.contains("left.rs"),
        "the tree's frame kept the buffer name of the window it took: {top:?}"
    );
    let bottom = run(bottom_row);
    assert!(
        !bottom.contains("-- INSERT --"),
        "the tree's frame claims nvim's mode: {bottom:?}"
    );
    assert!(
        !bottom.chars().any(|ch| ch == ':'),
        "the tree's frame claims a cursor position: {bottom:?}"
    );
}

/// The gapless look has one edge row and no top run, so the name leads the
/// segments there rather than sitting above them. The surface's name still
/// has to be on it, and the segments still have to be gone.
#[test]
fn a_gapless_native_panes_edge_keeps_the_name_and_drops_the_segments() {
    let tiles = tree_in_the_left_tile(false);
    let (_, col, width, _) = tiles.slots[0];
    let buf = tiled_frame(&tiles.model);
    let offset = tiles.model.look.grid_offset();
    let (_, edge_row) = edge_rows(&tiles.model, tiles.slots[0]);
    let edge: String = row_text(&buf, edge_row)
        .chars()
        .skip(usize::from(col + offset))
        .take(usize::from(width))
        .collect();
    assert!(
        edge.contains("tree"),
        "the gapless edge lost the surface's name: {edge:?}"
    );
    assert!(
        !edge.contains("-- INSERT --"),
        "the gapless edge claims nvim's mode: {edge:?}"
    );
    assert!(
        !edge.chars().any(|ch| ch == ':'),
        "the gapless edge claims a cursor position: {edge:?}"
    );
}
