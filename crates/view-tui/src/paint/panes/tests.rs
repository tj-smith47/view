#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use ratatui::backend::TestBackend;
use ratatui::buffer::Buffer;
use ratatui::Terminal;
use view_core::events::{GridCell, UiEvent, WinHandle};
use view_core::grid::registry::GridId;
use view_core::hl::HlAttr;
use view_core::model::Model;
use view_core::msg::Msg;
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
    let buf = frame(&model);
    let mut out: Vec<String> = (0..buf.area.height)
        .map(|row| row_text(&buf, row))
        .collect();
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
