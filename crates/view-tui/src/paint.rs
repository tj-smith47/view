//! The `Surface` -> ratatui compositor: style conversion plus z-order
//! layer painting. `view-surface`'s `render()` decides *what* to paint and
//! *where*; this module is the only place that turns those decisions into
//! `ratatui::Buffer` writes.

use ratatui::style::{Color, Modifier, Style};
use view_core::grid::Grid;
pub use view_core::hl::{HlAttr, HlTable};
use view_core::model::{CmdlineState, MessageEntry, Model, PopupmenuState, TablineState};
use view_surface::{LayerKind, Rect, Surface};

/// Paints every layer in `surface`, in order (z ascending), into `frame`.
/// Later layers would overwrite the cells of earlier ones within their own
/// rect, which is the z-order compositing contract: nothing here tracks
/// damage or diffs between frames, since `ratatui`'s own buffer diff already
/// limits what actually reaches the terminal.
///
/// `model` supplies the engine grid and highlight table backing the
/// [`LayerKind::EngineGrid`] layer: `Surface` describes where to paint and
/// what kind of content goes there, not the grid's (potentially large)
/// per-cell content itself, so painting reads that straight from `model`
/// rather than cloning it into every frame's `Surface`.
///
/// `view-surface`'s `render()` chose each layer's placement per the two
/// layout mechanisms nvim's full ext attach demands: the tabline reserves a
/// real row (`view_core::model::Model::chrome_rows`) so it is never in the
/// same rect as the `EngineGrid` layer, while cmdline/messages/popupmenu
/// are transient overlays that intentionally paint over grid content only
/// while their state is active, then vanish the frame it clears (their
/// `LayerKind` variant is simply absent from `surface.layers` that frame,
/// so the unconditional `EngineGrid` paint below is what restores the
/// resting text underneath).
pub fn composite(model: &Model, surface: &Surface, frame: &mut ratatui::Frame<'_>) {
    let frame_area = frame.area();
    for layer in &surface.layers {
        let area = clip_to_frame(layer.rect, frame_area);
        if area.width == 0 || area.height == 0 {
            continue;
        }
        match &layer.kind {
            LayerKind::EngineGrid => {
                paint_grid(&model.engine.grid, &model.engine.hl, area, frame);
            }
            LayerKind::Cmdline(state) => paint_cmdline(state, area, frame),
            LayerKind::Messages(entries) => paint_messages(entries, area, frame),
            LayerKind::Tabline(state) => paint_tabline(state, area, frame),
            LayerKind::Popupmenu(state) => paint_popupmenu(state, area, frame),
            // LayerKind is #[non_exhaustive]: a future variant degrades to
            // painting nothing rather than failing to compile here
            _ => {}
        }
    }
}

/// Writes `text`, one character per cell, into row `row_offset` of `area`
/// (styled `style`), truncating at the area's width or height rather than
/// writing past it. The shared primitive every chrome renderer below uses,
/// so column/row bounds-checking lives in exactly one place.
fn paint_text_row(
    text: &str,
    style: Style,
    area: ratatui::layout::Rect,
    row_offset: u16,
    frame: &mut ratatui::Frame<'_>,
) {
    if row_offset >= area.height {
        return;
    }
    let buf = frame.buffer_mut();
    for (col, ch) in (0_u16..).zip(text.chars()) {
        if col >= area.width {
            break;
        }
        let mut encode_buf = [0_u8; 4];
        let cell = &mut buf[(area.x + col, area.y + row_offset)];
        cell.set_symbol(sanitized_char(ch).encode_utf8(&mut encode_buf));
        cell.set_style(style);
    }
}

/// Replaces a control character with a plain space, otherwise passes `ch`
/// through unchanged.
///
/// `ratatui::buffer::Cell::set_symbol` is a low-level API that, unlike
/// `Buffer::set_stringn`/`Span::styled_graphemes`, does not filter control
/// characters before computing cell width, and panics (in a debug build) on
/// one that slips through. Every renderer in this module calls
/// `set_symbol` directly (needed for per-cell placement, not just
/// left-to-right string layout), so every one of them routes through this
/// first. Reachable from live content, not just hostile input: an `emsg`
/// from a real nvim plugin's autocommand error carried a raw control byte
/// during manual verification of this module's overlay renderers.
fn sanitized_char(ch: char) -> char {
    if ch.is_control() {
        ' '
    } else {
        ch
    }
}

/// Renders the command line: `firstc` plus its content chunks, on the
/// overlay's single row. The cursor itself is a separate concern
/// (`view_surface::render`'s `CursorSpec`, applied by
/// [`crate::terminal::Term::draw_surface`]), not painted here.
fn paint_cmdline(
    state: &CmdlineState,
    area: ratatui::layout::Rect,
    frame: &mut ratatui::Frame<'_>,
) {
    let mut text = state.firstc.clone();
    for (_, chunk) in &state.content {
        text.push_str(chunk);
    }
    paint_text_row(&text, Style::default(), area, 0, frame);
}

/// Renders the message log as stacked toasts: `render()` already sized and
/// right-anchored `area` to the widest visible entry, so painting only
/// picks which entries fit in `area`'s height (the most recently shown
/// ones, oldest of that visible set on top) and writes each on its own row.
fn paint_messages(
    entries: &[MessageEntry],
    area: ratatui::layout::Rect,
    frame: &mut ratatui::Frame<'_>,
) {
    let visible = usize::from(area.height);
    let start = entries.len().saturating_sub(visible);
    for (i, entry) in entries[start..].iter().enumerate() {
        let Ok(row) = u16::try_from(i) else {
            break;
        };
        let text: String = entry.content.iter().map(|(_, t)| t.as_str()).collect();
        paint_text_row(&text, Style::default(), area, row, frame);
    }
}

/// Renders the tabline into its reserved row: each tab as ` name `, the
/// current tab reverse-styled so it reads as selected without needing
/// bracket characters that would shift every other tab's column.
fn paint_tabline(
    state: &TablineState,
    area: ratatui::layout::Rect,
    frame: &mut ratatui::Frame<'_>,
) {
    let mut text = String::new();
    let mut current_range: Option<(u16, u16)> = None;
    for tab in &state.tabs {
        let start = u16::try_from(text.chars().count()).unwrap_or(u16::MAX);
        text.push_str(&format!(" {} ", tab.name));
        let end = u16::try_from(text.chars().count()).unwrap_or(u16::MAX);
        if tab.tab == state.current {
            current_range = Some((start, end));
        }
    }
    paint_text_row(&text, Style::default(), area, 0, frame);
    if let Some((start, end)) = current_range {
        let buf = frame.buffer_mut();
        for col in start..end.min(area.width) {
            buf[(area.x + col, area.y)]
                .set_style(Style::default().add_modifier(Modifier::REVERSED));
        }
    }
}

/// Renders the popup menu: one item per row via [`PmItem::display_text`],
/// the `selected` index reverse-styled. `render()` already anchored and
/// sized `area` to the event's `(row, col)` and the widest item.
fn paint_popupmenu(
    state: &PopupmenuState,
    area: ratatui::layout::Rect,
    frame: &mut ratatui::Frame<'_>,
) {
    for (i, item) in state.items.iter().enumerate() {
        let Ok(row) = u16::try_from(i) else {
            break;
        };
        if row >= area.height {
            break;
        }
        let is_selected = i64::try_from(i).is_ok_and(|idx| idx == state.selected);
        let style = if is_selected {
            Style::default().add_modifier(Modifier::REVERSED)
        } else {
            Style::default()
        };
        paint_text_row(&item.display_text(), style, area, row, frame);
    }
}

/// Intersects a [`view_surface::Rect`] with the frame's own area: a layer
/// rect is clamped to the grid at render time, but the terminal can still
/// be smaller than that grid between a resize and the next `Surface`
/// (`view-surface` has no live handle on the terminal size), so painting
/// clips a second time against the actual paintable area rather than
/// indexing past the buffer.
fn clip_to_frame(rect: Rect, frame_area: ratatui::layout::Rect) -> ratatui::layout::Rect {
    let x = frame_area.x.saturating_add(rect.col);
    let y = frame_area.y.saturating_add(rect.row);
    let max_x = frame_area.x.saturating_add(frame_area.width);
    let max_y = frame_area.y.saturating_add(frame_area.height);
    ratatui::layout::Rect {
        x: x.min(max_x),
        y: y.min(max_y),
        width: rect.width.min(max_x.saturating_sub(x)),
        height: rect.height.min(max_y.saturating_sub(y)),
    }
}

/// Paints every visible `grid` cell within `area`, styled per `hl`.
fn paint_grid(
    grid: &Grid,
    hl: &HlTable,
    area: ratatui::layout::Rect,
    frame: &mut ratatui::Frame<'_>,
) {
    let buf = frame.buffer_mut();
    let (w, h) = grid.size();
    for row in 0..h.min(area.height) {
        for col in 0..w.min(area.width) {
            if let Some(cell) = grid.cell(row, col) {
                let out = &mut buf[(area.x + col, area.y + row)];
                out.set_symbol(&sanitized_symbol(&cell.text));
                out.set_style(style_for(cell.hl_id, hl));
            }
        }
    }
}

/// Like [`sanitized_char`], but over a whole grid cell's (possibly
/// multi-char grapheme) text, and substituting a single space for an empty
/// cell the same way the pre-sanitization code always did. Only allocates
/// when a control character is actually present; the common case (an
/// ordinary printable grapheme) borrows straight through.
fn sanitized_symbol(text: &str) -> std::borrow::Cow<'_, str> {
    if text.is_empty() {
        return std::borrow::Cow::Borrowed(" ");
    }
    if text.chars().any(char::is_control) {
        std::borrow::Cow::Owned(text.chars().map(sanitized_char).collect())
    } else {
        std::borrow::Cow::Borrowed(text)
    }
}

fn style_for(hl_id: u64, table: &HlTable) -> Style {
    let mut fg = table.default_fg;
    let mut bg = table.default_bg;
    let mut style = Style::default();
    if let Some(a) = table.attrs.get(&hl_id) {
        if a.fg.is_some() {
            fg = a.fg;
        }
        if a.bg.is_some() {
            bg = a.bg;
        }
        if a.reverse {
            std::mem::swap(&mut fg, &mut bg);
        }
        if a.bold {
            style = style.add_modifier(ratatui::style::Modifier::BOLD);
        }
        if a.italic {
            style = style.add_modifier(ratatui::style::Modifier::ITALIC);
        }
        if a.underline {
            style = style.add_modifier(ratatui::style::Modifier::UNDERLINED);
        }
    }
    if let Some(c) = fg {
        style = style.fg(rgb(c));
    }
    if let Some(c) = bg {
        style = style.bg(rgb(c));
    }
    style
}

fn rgb(c: u32) -> Color {
    Color::Rgb((c >> 16) as u8, (c >> 8) as u8, c as u8)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;
    use view_core::grid::GridOp;

    fn table_with(attr: HlAttr) -> HlTable {
        let mut attrs = std::collections::HashMap::new();
        attrs.insert(1, attr);
        HlTable {
            default_fg: None,
            default_bg: None,
            attrs,
        }
    }

    #[test]
    fn underline_attr_sets_underlined_modifier() {
        let table = table_with(HlAttr {
            fg: None,
            bg: None,
            bold: false,
            italic: false,
            underline: true,
            reverse: false,
        });
        let style = style_for(1, &table);
        assert!(style
            .add_modifier
            .contains(ratatui::style::Modifier::UNDERLINED));
    }

    #[test]
    fn no_underline_attr_leaves_modifier_unset() {
        let table = table_with(HlAttr {
            fg: None,
            bg: None,
            bold: false,
            italic: false,
            underline: false,
            reverse: false,
        });
        let style = style_for(1, &table);
        assert!(!style
            .add_modifier
            .contains(ratatui::style::Modifier::UNDERLINED));
    }

    /// Golden test: the grid layer's own content paints correctly.
    #[test]
    fn composite_paints_grid_layer_content() {
        let mut model = Model::new();
        model.engine.grid.apply(GridOp::Resize {
            width: 10,
            height: 3,
        });
        model.engine.grid.apply(GridOp::PutLine {
            row: 0,
            col_start: 0,
            cells: vec![("h".into(), 0, 1), ("i".into(), 0, 1)],
        });
        let surface = view_surface::render(&model);

        let backend = TestBackend::new(10, 3);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| composite(&model, &surface, f)).unwrap();

        let buf = terminal.backend().buffer().clone();
        assert_eq!(&buf[(0, 0)].symbol(), &"h");
        assert_eq!(&buf[(1, 0)].symbol(), &"i");
    }

    /// A helper for driving `view-core` events through `update()` the same
    /// way production code does, since the state structs behind each
    /// `LayerKind` are `#[non_exhaustive]` and cannot be built with
    /// struct-literal syntax from outside `view-core`.
    fn apply(model: &mut Model, ev: view_core::events::UiEvent) {
        let _ = view_core::update::update(model, view_core::msg::Msg::Redraw(vec![ev]));
    }

    /// Invariant test superseding the prior phase's EngineGrid-only
    /// regression test: the cmdline is a transient overlay, so it is
    /// correct UX for it to paint over the grid's bottom row while it is
    /// open (matching the cmdheight=0 floating UX external UIs give) -- the
    /// invariant this phase pins instead is that the overlay vanishes with
    /// its state, restoring the resting buffer text on the very next frame
    /// that has no cmdline layer.
    #[test]
    fn cmdline_overlay_paints_while_shown_and_vanishes_with_cmdlinehide() {
        let mut model = Model::new();
        model.engine.grid.apply(GridOp::Resize {
            width: 10,
            height: 3,
        });
        model.engine.grid.apply(GridOp::PutLine {
            row: 2,
            col_start: 0,
            cells: vec![("x".into(), 0, 1), ("y".into(), 0, 1)],
        });

        apply(
            &mut model,
            view_core::events::UiEvent::CmdlineShow {
                content: vec![(0, "q".to_string())],
                pos: 0,
                firstc: ":".to_string(),
                prompt: String::new(),
                indent: 0,
                level: 1,
            },
        );
        let surface = view_surface::render(&model);
        let backend = TestBackend::new(10, 3);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| composite(&model, &surface, f)).unwrap();
        let buf = terminal.backend().buffer().clone();
        assert_eq!(
            &buf[(0, 2)].symbol(),
            &":",
            "cmdline overlay must paint over the grid's bottom row while shown"
        );

        apply(&mut model, view_core::events::UiEvent::CmdlineHide);
        let surface = view_surface::render(&model);
        terminal.draw(|f| composite(&model, &surface, f)).unwrap();
        let buf = terminal.backend().buffer().clone();
        assert_eq!(
            &buf[(0, 2)].symbol(),
            &"x",
            "resting grid text must return once the overlay's state is gone"
        );
        assert_eq!(&buf[(1, 2)].symbol(), &"y");
    }

    /// New invariant this phase pins: persistent chrome (the tabline) may
    /// never sit over resting buffer text. With more than one tab open the
    /// grid is offset below the reserved top row, so the tabline and the
    /// grid's own content occupy disjoint rows in the same frame.
    #[test]
    fn tabline_reserves_the_top_row_and_never_covers_resting_grid_text() {
        let mut model = Model::new();
        model.engine.grid.apply(GridOp::Resize {
            width: 10,
            height: 3,
        });
        model.engine.grid.apply(GridOp::PutLine {
            row: 0,
            col_start: 0,
            cells: vec![("a".into(), 0, 1), ("b".into(), 0, 1)],
        });
        apply(
            &mut model,
            view_core::events::UiEvent::TablineUpdate {
                current: view_core::events::TabHandle(1),
                tabs: vec![
                    view_core::events::TabEntry {
                        tab: view_core::events::TabHandle(1),
                        name: "one".into(),
                    },
                    view_core::events::TabEntry {
                        tab: view_core::events::TabHandle(2),
                        name: "two".into(),
                    },
                ],
            },
        );
        // TablineUpdate crossing the 1-tab boundary shrinks the grid target;
        // the grid itself only reflects that once nvim's GridResize round
        // trips, which this test drives directly to exercise the settled
        // (post-round-trip) frame the reviewer's contract describes.
        model.engine.grid.apply(GridOp::Resize {
            width: 10,
            height: 2,
        });
        model.engine.grid.apply(GridOp::PutLine {
            row: 0,
            col_start: 0,
            cells: vec![("a".into(), 0, 1), ("b".into(), 0, 1)],
        });

        let surface = view_surface::render(&model);
        let backend = TestBackend::new(10, 3);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| composite(&model, &surface, f)).unwrap();
        let buf = terminal.backend().buffer().clone();

        assert_eq!(
            &buf[(0, 0)].symbol(),
            &" ",
            "tabline label starts with a space"
        );
        assert_eq!(&buf[(1, 0)].symbol(), &"o");
        assert_eq!(&buf[(2, 0)].symbol(), &"n");
        assert_eq!(
            &buf[(0, 1)].symbol(),
            &"a",
            "grid content must land one row below the reserved tabline row"
        );
        assert_eq!(&buf[(1, 1)].symbol(), &"b");
    }

    #[test]
    fn single_tab_reserves_no_row_and_grid_fills_the_full_frame() {
        let mut model = Model::new();
        model.engine.grid.apply(GridOp::Resize {
            width: 10,
            height: 3,
        });
        model.engine.grid.apply(GridOp::PutLine {
            row: 0,
            col_start: 0,
            cells: vec![("z".into(), 0, 1)],
        });

        let surface = view_surface::render(&model);
        let backend = TestBackend::new(10, 3);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| composite(&model, &surface, f)).unwrap();
        let buf = terminal.backend().buffer().clone();
        assert_eq!(&buf[(0, 0)].symbol(), &"z");
    }

    #[test]
    fn messages_overlay_renders_stacked_toasts_top_right() {
        let mut model = Model::new();
        model.engine.grid.apply(GridOp::Resize {
            width: 10,
            height: 3,
        });
        apply(
            &mut model,
            view_core::events::UiEvent::MsgShow {
                kind: "echomsg".into(),
                content: vec![(0, "hi".into())],
                replace_last: false,
            },
        );

        let surface = view_surface::render(&model);
        let backend = TestBackend::new(10, 3);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| composite(&model, &surface, f)).unwrap();
        let buf = terminal.backend().buffer().clone();
        assert_eq!(&buf[(8, 0)].symbol(), &"h");
        assert_eq!(&buf[(9, 0)].symbol(), &"i");

        apply(&mut model, view_core::events::UiEvent::MsgClear);
        let surface = view_surface::render(&model);
        terminal.draw(|f| composite(&model, &surface, f)).unwrap();
        let buf = terminal.backend().buffer().clone();
        assert_eq!(
            &buf[(8, 0)].symbol(),
            &" ",
            "message toast must vanish once MsgClear empties the log"
        );
    }

    /// Reproduces a real crash: a live nvim with user plugins fed an `emsg`
    /// containing a raw control byte (observed via a `BufNewFile`
    /// autocommand error), which `ratatui`'s low-level `Cell::set_symbol`
    /// does not filter (unlike its safe `Buffer::set_stringn`/`Span`
    /// counterparts) and asserts against in a debug build. Every renderer
    /// that calls `set_symbol` directly must sanitize first.
    #[test]
    fn control_characters_in_message_text_do_not_panic_and_are_sanitized() {
        let mut model = Model::new();
        model.engine.grid.apply(GridOp::Resize {
            width: 10,
            height: 3,
        });
        apply(
            &mut model,
            view_core::events::UiEvent::MsgShow {
                kind: "emsg".into(),
                content: vec![(0, "a\u{7}b".into())], // BEL between two printable chars
                replace_last: false,
            },
        );

        let surface = view_surface::render(&model);
        let backend = TestBackend::new(10, 3);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| composite(&model, &surface, f)).unwrap();
        let buf = terminal.backend().buffer().clone();
        assert_eq!(&buf[(7, 0)].symbol(), &"a");
        assert_eq!(
            &buf[(8, 0)].symbol(),
            &" ",
            "control byte sanitized to a space"
        );
        assert_eq!(&buf[(9, 0)].symbol(), &"b");
    }

    #[test]
    fn control_characters_in_grid_cell_text_do_not_panic_and_are_sanitized() {
        let mut model = Model::new();
        model.engine.grid.apply(GridOp::Resize {
            width: 10,
            height: 1,
        });
        model.engine.grid.apply(GridOp::PutLine {
            row: 0,
            col_start: 0,
            cells: vec![("\u{7}".into(), 0, 1)],
        });
        let surface = view_surface::render(&model);
        let backend = TestBackend::new(10, 1);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| composite(&model, &surface, f)).unwrap();
        let buf = terminal.backend().buffer().clone();
        assert_eq!(&buf[(0, 0)].symbol(), &" ");
    }

    #[test]
    fn popupmenu_overlay_anchors_at_its_grid_coords_and_highlights_selected() {
        let mut model = Model::new();
        model.engine.grid.apply(GridOp::Resize {
            width: 20,
            height: 6,
        });
        apply(
            &mut model,
            view_core::events::UiEvent::PopupmenuShow {
                items: vec![
                    view_core::events::PmItem {
                        word: "foo".into(),
                        ..Default::default()
                    },
                    view_core::events::PmItem {
                        word: "bar".into(),
                        ..Default::default()
                    },
                ],
                selected: 1,
                row: 2,
                col: 3,
                grid: 0,
            },
        );

        let surface = view_surface::render(&model);
        let backend = TestBackend::new(20, 6);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| composite(&model, &surface, f)).unwrap();
        let buf = terminal.backend().buffer().clone();
        assert_eq!(&buf[(3, 2)].symbol(), &"f");
        assert_eq!(&buf[(3, 3)].symbol(), &"b");
        assert!(
            buf[(3, 3)]
                .style()
                .add_modifier
                .contains(ratatui::style::Modifier::REVERSED),
            "selected item must be highlighted"
        );
        assert!(
            !buf[(3, 2)]
                .style()
                .add_modifier
                .contains(ratatui::style::Modifier::REVERSED),
            "unselected item must not be highlighted"
        );

        apply(&mut model, view_core::events::UiEvent::PopupmenuHide);
        let surface = view_surface::render(&model);
        terminal.draw(|f| composite(&model, &surface, f)).unwrap();
        let buf = terminal.backend().buffer().clone();
        assert_eq!(
            &buf[(3, 2)].symbol(),
            &" ",
            "popup menu must vanish once PopupmenuHide clears its state"
        );
    }
}
