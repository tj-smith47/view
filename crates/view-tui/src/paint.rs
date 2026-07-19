//! The `Surface` -> ratatui compositor: style conversion plus z-order
//! layer painting. `view-surface`'s `render()` decides *what* to paint and
//! *where*; this module is the only place that turns those decisions into
//! `ratatui::Buffer` writes.

use ratatui::style::{Color, Modifier, Style};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};
use view_core::grid::Grid;
pub use view_core::hl::{HlAttr, HlTable};
use view_core::model::{CmdlineState, MessageEntry, Model, PopupmenuState, TablineState};
use view_core::theme::{ResolvedStyle, Theme};
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
    // derived once per frame from the engine's live highlight state: a
    // lookup over already-decoded fields, not an RPC round trip, so
    // re-deriving on every paint costs nothing beyond this struct copy
    let theme = Theme::from_hl(&model.engine.hl);
    for layer in &surface.layers {
        let area = clip_to_frame(layer.rect, frame_area);
        if area.width == 0 || area.height == 0 {
            continue;
        }
        match &layer.kind {
            LayerKind::EngineGrid => {
                paint_grid(&model.engine.grid, &theme, &model.engine.hl, area, frame);
            }
            LayerKind::Cmdline(state) => paint_cmdline(state, &theme, area, frame),
            LayerKind::Messages(entries) => paint_messages(entries, &theme, area, frame),
            LayerKind::Tabline(state) => paint_tabline(state, &theme, area, frame),
            LayerKind::Popupmenu(state) => paint_popupmenu(state, &theme, area, frame),
            LayerKind::Shell => paint_shell(&theme, area, frame),
            // LayerKind is #[non_exhaustive]: a future variant degrades to
            // painting nothing rather than failing to compile here
            _ => {}
        }
    }
}

/// Writes `text` into row `row_offset` of `area` (styled `style`),
/// truncating at the area's width or height rather than writing past it.
/// The shared primitive every chrome renderer below uses, so column/row
/// bounds-checking lives in exactly one place.
///
/// Each character advances the column by its own display width (1 for
/// ordinary text, 2 for wide characters like CJK ideographs) rather than
/// unconditionally by one cell: a fixed one-column advance would place a
/// wide character's glyph in a single cell it does not fit, misaligning
/// every character painted after it on the row. A wide character's second
/// (shadow) cell is reset so no later character in this same call can draw
/// into it, matching the convention `ratatui::buffer::Buffer::set_stringn`
/// itself uses for multi-width graphemes.
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
    let mut col = 0_u16;
    for ch in text.chars() {
        if col >= area.width {
            break;
        }
        let ch = sanitized_char(ch);
        // sanitized_char already replaced every control character with a
        // plain space, so `width` is `None` here only for the handful of
        // zero-width combining marks that survive sanitization; `.max(1)`
        // still advances the column for those rather than looping forever
        // painting into the same cell
        let width = ch.width().unwrap_or(1).max(1) as u16;
        let mut encode_buf = [0_u8; 4];
        let cell = &mut buf[(area.x + col, area.y + row_offset)];
        cell.set_symbol(ch.encode_utf8(&mut encode_buf));
        cell.set_style(style);
        if width == 2 && col + 1 < area.width {
            buf[(area.x + col + 1, area.y + row_offset)].reset();
        }
        col = col.saturating_add(width);
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

/// Renders the command line: first a full-row clear to the cmdline's own
/// style (so no glyph the grid layer painted underneath -- a statusline,
/// most commonly -- survives past the end of the typed text), then
/// `firstc`, then `prompt`, then its content chunks. In prompt mode (e.g.
/// `:call input("name: ")`) nvim sends an empty `firstc` and puts the label
/// in `prompt` instead, so concatenating both in this order reproduces
/// nvim's own `:` prefix in the ordinary case and the prompt label in the
/// input() case without a branch (live-verified against a real nvim's
/// `cmdline_show` traffic for `:call input("name: ")`: `firstc=""`,
/// `prompt="name: "`). The cursor itself is a separate concern
/// (`view_surface::render`'s `CursorSpec`, applied by
/// [`crate::terminal::Term::draw_surface`]), not painted here.
fn paint_cmdline(
    state: &CmdlineState,
    theme: &Theme,
    area: ratatui::layout::Rect,
    frame: &mut ratatui::Frame<'_>,
) {
    // a live `--clean` capture of nvim's `hl_group_set` batch (see
    // view-engine's ui_events tests) carries no builtin group naming the
    // cmdline row: nvim styles it from "Normal" itself, so `theme.normal()`
    // is not a fallback standing in for a missing mapping here, it is the
    // correct source
    let style = ratatui_style(theme.normal());
    let blank = " ".repeat(usize::from(area.width));
    paint_text_row(&blank, style, area, 0, frame);

    let mut text = state.firstc.clone();
    text.push_str(&state.prompt);
    for (_, chunk) in &state.content {
        text.push_str(chunk);
    }
    paint_text_row(&text, style, area, 0, frame);
}

/// Renders the message log as stacked toasts: `render()` already sized and
/// right-anchored `area` to the widest visible physical line (one row per
/// `MessageEntry::lines` entry, not one row per `MessageEntry` -- a
/// multi-line `emsg` occupies as many rows as it has physical lines), so
/// painting picks which lines fit in `area`'s height (the most recently
/// shown ones, oldest of that visible set on top) and writes each on its
/// own row.
///
/// The whole rect is cleared to the toast's own style first, before any
/// text: without this, a row -- or the columns past a line's own text on a
/// row -- keeps showing whatever the `EngineGrid` layer painted underneath
/// (real nvim content, e.g. a floating window's cells composited into the
/// base grid when the frontend has no `ext_multigrid` support), which is
/// what a live repro showed as foreign glyphs bleeding through at a toast
/// row's right edge.
fn paint_messages(
    entries: &[MessageEntry],
    theme: &Theme,
    area: ratatui::layout::Rect,
    frame: &mut ratatui::Frame<'_>,
) {
    let style = ratatui_style(theme.msg_area);
    let blank = " ".repeat(usize::from(area.width));
    for row in 0..area.height {
        paint_text_row(&blank, style, area, row, frame);
    }

    let lines: Vec<String> = entries.iter().flat_map(MessageEntry::lines).collect();
    let visible = usize::from(area.height);
    let start = lines.len().saturating_sub(visible);
    for (i, line) in lines[start..].iter().enumerate() {
        let Ok(row) = u16::try_from(i) else {
            break;
        };
        paint_text_row(line, style, area, row, frame);
    }
}

/// Renders the tabline into its reserved row: each tab as ` name `, the
/// current tab reverse-styled so it reads as selected without needing
/// bracket characters that would shift every other tab's column.
fn paint_tabline(
    state: &TablineState,
    theme: &Theme,
    area: ratatui::layout::Rect,
    frame: &mut ratatui::Frame<'_>,
) {
    // painted before the tab labels themselves so `TabLineFill` shows
    // through any column the labels below do not reach (a short tab list
    // in a wide terminal), matching what that builtin group names: the
    // row's background beyond the tabs
    let fill = " ".repeat(usize::from(area.width));
    paint_text_row(&fill, ratatui_style(theme.tab_line_fill), area, 0, frame);

    let mut text = String::new();
    let mut current_range: Option<(u16, u16)> = None;
    for tab in &state.tabs {
        // display-cell width, not char count: a tab name containing a wide
        // (CJK) character occupies more columns than it has chars, and the
        // selection-highlight range below must land on the same columns
        // paint_text_row actually painted the label into
        let start = u16::try_from(text.width()).unwrap_or(u16::MAX);
        text.push_str(&format!(" {} ", tab.name));
        let end = u16::try_from(text.width()).unwrap_or(u16::MAX);
        if tab.tab == state.current {
            current_range = Some((start, end));
        }
    }
    paint_text_row(&text, ratatui_style(theme.tab_line), area, 0, frame);
    if let Some((start, end)) = current_range {
        let buf = frame.buffer_mut();
        for col in start..end.min(area.width) {
            buf[(area.x + col, area.y)].set_style(ratatui_style(theme.tab_line_sel));
        }
    }
}

/// Renders the popup menu: one item per row via [`PmItem::display_text`],
/// the `selected` index reverse-styled. `render()` already anchored and
/// sized `area` to the event's `(row, col)` and the widest item.
fn paint_popupmenu(
    state: &PopupmenuState,
    theme: &Theme,
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
            ratatui_style(theme.pmenu_sel)
        } else {
            ratatui_style(theme.pmenu)
        };
        paint_text_row(&item.display_text(), style, area, row, frame);
    }
}

/// Renders the pre-content startup shell: a themed statusline placeholder
/// bar on the terminal's bottom row, plus a static "waiting for nvim"
/// indicator centered in the remaining rows. Present only while
/// `view_core::model::Model::content_painted` is `false` (see
/// `view_surface::render`); `render()` stops including the
/// [`LayerKind::Shell`] layer at all once real grid content has arrived, so
/// this function has nothing left to overwrite it with.
///
/// No animation: the runtime loop is timer-free (no clock anywhere in its
/// steady-state body), so this glyph is fixed rather than advancing frames
/// on its own -- a real spinner would need a tick this architecture
/// deliberately does not have.
fn paint_shell(theme: &Theme, area: ratatui::layout::Rect, frame: &mut ratatui::Frame<'_>) {
    if area.height == 0 || area.width == 0 {
        return;
    }
    let fill = " ".repeat(usize::from(area.width));
    let bottom_row = area.height - 1;
    paint_text_row(
        &fill,
        ratatui_style(theme.status_line),
        area,
        bottom_row,
        frame,
    );

    let label = "view: waiting for nvim...";
    let text: String = label.chars().take(usize::from(area.width)).collect();
    let mid_row = area.height / 2;
    paint_text_row(&text, ratatui_style(theme.normal()), area, mid_row, frame);
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

/// Paints every visible `grid` cell within `area`, styled per `hl` through
/// `theme`.
fn paint_grid(
    grid: &Grid,
    theme: &Theme,
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
                out.set_style(style_for(theme, cell.hl_id, hl));
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

/// One grid cell's resolved style, converted to `ratatui`: derivation
/// itself lives in [`Theme::style_for`] (backend-free, in `view-core`), so
/// this is purely the `ResolvedStyle` -> `ratatui::style::Style` mapping.
fn style_for(theme: &Theme, hl_id: u64, table: &HlTable) -> Style {
    ratatui_style(theme.style_for(hl_id, table))
}

/// Converts a backend-free [`ResolvedStyle`] into a `ratatui::style::Style`.
fn ratatui_style(resolved: ResolvedStyle) -> Style {
    let mut style = Style::default();
    if let Some(c) = resolved.fg {
        style = style.fg(rgb(c));
    }
    if let Some(c) = resolved.bg {
        style = style.bg(rgb(c));
    }
    if resolved.bold {
        style = style.add_modifier(Modifier::BOLD);
    }
    if resolved.italic {
        style = style.add_modifier(Modifier::ITALIC);
    }
    if resolved.underline {
        style = style.add_modifier(Modifier::UNDERLINED);
    }
    if resolved.reverse {
        style = style.add_modifier(Modifier::REVERSED);
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
            groups: std::collections::HashMap::new(),
            probe_generation: 0,
            confirmed: None,
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
        let theme = Theme::from_hl(&table);
        let style = style_for(&theme, 1, &table);
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
        let theme = Theme::from_hl(&table);
        let style = style_for(&theme, 1, &table);
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

    /// End-to-end regression at the rendering layer: once a probe reply
    /// confirms `Normal` has no background at all (the transparent-config
    /// fixture -- see `view-engine`'s `decode_hl_probe_reply` doc comment
    /// for the wire-verified shape this mirrors), a default grid cell must
    /// carry `Color::Reset` (ratatui's "no color set" default), never an
    /// explicit RGB, so the real terminal's own background shows through.
    /// Disconfirm: reverting `Theme::from_hl`'s generation-matched branch
    /// (see `theme.rs`) makes this assert `Color::Rgb(0,0,0)` instead -- an
    /// all-black paint where transparency was expected.
    #[test]
    fn transparent_confirmed_default_paints_grid_cells_with_no_bg_color() {
        let mut model = Model::new();
        model.engine.grid.apply(GridOp::Resize {
            width: 4,
            height: 1,
        });
        model.engine.grid.apply(GridOp::PutLine {
            row: 0,
            col_start: 0,
            cells: vec![("x".into(), 0, 1)],
        });
        apply(
            &mut model,
            view_core::events::UiEvent::DefaultColorsSet {
                fg: Some(0xF8F8F2),
                bg: Some(0), // wire-ambiguous: nvim sends 0 for "unset"
                sp: None,
            },
        );
        let generation = model.engine.hl.probe_generation;
        let _ = view_core::update::update(
            &mut model,
            view_core::msg::Msg::HlProbeReply {
                generation,
                fg: Some(0xF8F8F2),
                bg: None, // the probe reply's map had no "bg" key
            },
        );

        let surface = view_surface::render(&model);
        let backend = TestBackend::new(4, 1);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| composite(&model, &surface, f)).unwrap();
        let buf = terminal.backend().buffer().clone();

        assert_eq!(
            buf[(0, 0)].bg,
            ratatui::style::Color::Reset,
            "confirmed-unset bg must paint with no color, letting the terminal default show through"
        );
    }

    /// The counterpart: a probe reply that confirms `bg = 0` (a genuinely
    /// black theme) keeps painting an explicit black cell rather than being
    /// conflated with the unset case above.
    #[test]
    fn genuinely_black_confirmed_default_still_paints_black() {
        let mut model = Model::new();
        model.engine.grid.apply(GridOp::Resize {
            width: 4,
            height: 1,
        });
        model.engine.grid.apply(GridOp::PutLine {
            row: 0,
            col_start: 0,
            cells: vec![("x".into(), 0, 1)],
        });
        apply(
            &mut model,
            view_core::events::UiEvent::DefaultColorsSet {
                fg: Some(0xFFFFFF),
                bg: Some(0),
                sp: None,
            },
        );
        let generation = model.engine.hl.probe_generation;
        let _ = view_core::update::update(
            &mut model,
            view_core::msg::Msg::HlProbeReply {
                generation,
                fg: Some(0xFFFFFF),
                bg: Some(0), // the probe reply's map DID carry "bg": 0
            },
        );

        let surface = view_surface::render(&model);
        let backend = TestBackend::new(4, 1);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| composite(&model, &surface, f)).unwrap();
        let buf = terminal.backend().buffer().clone();

        assert_eq!(buf[(0, 0)].bg, ratatui::style::Color::Rgb(0, 0, 0));
    }

    /// A helper for driving `view-core` events through `update()` the same
    /// way production code does, since the state structs behind each
    /// `LayerKind` are `#[non_exhaustive]` and cannot be built with
    /// struct-literal syntax from outside `view-core`.
    fn apply(model: &mut Model, ev: view_core::events::UiEvent) {
        let _ = view_core::update::update(model, view_core::msg::Msg::Redraw(vec![ev]));
    }

    /// Supersedes an earlier EngineGrid-only regression test: the cmdline is
    /// a transient overlay, so it is correct UX for it to paint over the
    /// grid's bottom row while it is open (matching the cmdheight=0
    /// floating UX external UIs give). The invariant pinned here instead is
    /// that the overlay vanishes with its state, restoring the resting
    /// buffer text on the very next frame that has no cmdline layer.
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

    /// Regression: while the cmdline is active, it must claim the *whole*
    /// bottom row, not just the cells its own prompt+content text occupies.
    /// Reproduces the reported bug shape (a statusline-bearing grid row
    /// bleeding through past the typed command). Disconfirm: without the
    /// row-claim fill in `paint_cmdline`, cell `(5, 2)` still reads a
    /// statusline glyph instead of a blank space.
    #[test]
    fn cmdline_overlay_claims_the_full_bottom_row_no_grid_glyph_bleeds_through() {
        let mut model = Model::new();
        model.engine.grid.apply(GridOp::Resize {
            width: 10,
            height: 3,
        });
        model.engine.grid.apply(GridOp::PutLine {
            row: 2,
            col_start: 0,
            cells: "STATUSLIN".chars().map(|c| (c.to_string(), 0, 1)).collect(),
        });

        apply(
            &mut model,
            view_core::events::UiEvent::CmdlineShow {
                content: vec![(0, "wq".to_string())],
                pos: 2,
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

        let row: String = (0..10).map(|c| buf[(c, 2)].symbol().to_string()).collect();
        assert_eq!(
            row, ":wq       ",
            "cmdline overlay must claim the whole bottom row"
        );
    }

    /// Regression for a real bug: nvim's prompt mode (`:call input("name:
    /// ")`) sends an empty `firstc` and puts the label in `prompt` instead,
    /// which `paint_cmdline` previously dropped entirely, rendering a blank
    /// bottom row while keystrokes were silently accepted with no visible
    /// label. Values match a live nvim capture of that exact command (see
    /// `paint_cmdline`'s doc comment).
    ///
    /// Also pins that `paint_cmdline`'s full-row claim (see
    /// `cmdline_overlay_claims_the_full_bottom_row_no_grid_glyph_bleeds_through`)
    /// covers prompt mode too, not just the `firstc` mode that regression
    /// test drives: row 2 is seeded with statusline-shaped glyphs first, so
    /// the full-row assertion below disconfirms unless the blank-row fill
    /// actually runs ahead of the prompt label and content.
    #[test]
    fn cmdline_prompt_mode_renders_prompt_label_and_places_cursor_after_it() {
        let mut model = Model::new();
        model.engine.grid.apply(GridOp::Resize {
            width: 20,
            height: 3,
        });
        model.engine.grid.apply(GridOp::PutLine {
            row: 2,
            col_start: 0,
            cells: "STATUSLINESTATUSLINE"
                .chars()
                .map(|c| (c.to_string(), 0, 1))
                .collect(),
        });

        apply(
            &mut model,
            view_core::events::UiEvent::CmdlineShow {
                content: vec![(0, "X".to_string())],
                pos: 1,
                firstc: String::new(),
                prompt: "name: ".to_string(),
                indent: 0,
                level: 1,
            },
        );
        let surface = view_surface::render(&model);
        let backend = TestBackend::new(20, 3);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| composite(&model, &surface, f)).unwrap();
        let buf = terminal.backend().buffer().clone();

        for (col, ch) in "name: X".chars().enumerate() {
            assert_eq!(
                &buf[(col as u16, 2)].symbol(),
                &ch.to_string(),
                "prompt row column {col} must hold {ch:?}"
            );
        }

        let cursor = surface.cursor.expect("prompt mode must place a cursor");
        assert_eq!(
            cursor.col, 7,
            "cursor must land after \"name: \" (6 cols) plus pos=1 into the typed content"
        );
        assert_eq!(cursor.row, 2);

        let full_row: String = (0..20).map(|c| buf[(c, 2)].symbol().to_string()).collect();
        assert_eq!(
            full_row,
            format!("{:<20}", "name: X"),
            "prompt-mode cmdline must claim the whole bottom row; the seeded \
             statusline glyphs past column 6 must not bleed through"
        );
    }

    /// Invariant pinned here: persistent chrome (the tabline) may never sit
    /// over resting buffer text. With more than one tab open the grid is
    /// offset below the reserved top row, so the tabline and the
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
        // (post-round-trip) frame.
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

    /// Pins the one-frame transient: `TablineUpdate` crossing the 1-tab
    /// boundary reserves a chrome row immediately (the same frame), but
    /// the grid itself keeps its pre-shrink height until
    /// nvim's corresponding `GridOp::Resize` round-trips on a later frame.
    /// For exactly that one frame, `render()`'s `EngineGrid` layer (now
    /// offset by the new chrome row, but still the old height) extends one
    /// row past the terminal's actual frame -- `clip_to_frame` must clip
    /// it rather than let the draw call index past the buffer.
    #[test]
    fn tabline_update_without_matching_grid_resize_clips_the_transient_overflow_row() {
        let mut model = Model::new();
        model.engine.grid.apply(GridOp::Resize {
            width: 10,
            height: 3,
        });
        model.engine.grid.apply(GridOp::PutLine {
            row: 0,
            col_start: 0,
            cells: vec![("a".into(), 0, 1)],
        });
        model.engine.grid.apply(GridOp::PutLine {
            row: 2,
            col_start: 0,
            cells: vec![("z".into(), 0, 1)],
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
        // deliberately no GridOp::Resize here: the grid is still 3 rows
        // tall while the tabline already reserves row 0, the exact
        // transient window under test

        let surface = view_surface::render(&model);
        let backend = TestBackend::new(10, 3);
        let mut terminal = Terminal::new(backend).unwrap();
        // must not panic despite the EngineGrid layer (row=1, height=3)
        // extending one row past the frame's own 3 rows
        terminal.draw(|f| composite(&model, &surface, f)).unwrap();
        let buf = terminal.backend().buffer().clone();

        assert_eq!(
            &buf[(0, 0)].symbol(),
            &" ",
            "tabline label starts with a space on the reserved row"
        );
        assert_eq!(
            &buf[(0, 1)].symbol(),
            &"a",
            "grid row 0 shifted down by the reserved chrome row"
        );
        for row in 0..3 {
            for col in 0..10 {
                assert_ne!(
                    buf[(col, row)].symbol(),
                    "z",
                    "grid row 2 (\"z\") is outside the clipped area and must not be painted"
                );
            }
        }
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

    /// A CJK (wide, 2-cell) character in a message toast must not misalign
    /// the character painted after it: `messages_width` sizes the layer by
    /// display cells (3 for "中b": 2 for the wide glyph, 1 for "b"), not
    /// char count (which would undercount to 2 and anchor the layer one
    /// column too far right), and `paint_text_row` must advance past the
    /// wide glyph's own shadow cell before placing "b" rather than writing
    /// "b" directly over it.
    #[test]
    fn messages_overlay_wide_char_advances_two_columns_not_one() {
        let mut model = Model::new();
        model.engine.grid.apply(GridOp::Resize {
            width: 10,
            height: 3,
        });
        apply(
            &mut model,
            view_core::events::UiEvent::MsgShow {
                kind: "echomsg".into(),
                content: vec![(0, "中b".into())],
                replace_last: false,
            },
        );

        let surface = view_surface::render(&model);
        let backend = TestBackend::new(10, 3);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| composite(&model, &surface, f)).unwrap();
        let buf = terminal.backend().buffer().clone();
        // right-anchored at 3 display cells wide (10 - 3 = 7), not 2: a
        // char-count-based width would anchor this one column further
        // right and clip the wide glyph's leading edge
        assert_eq!(&buf[(7, 0)].symbol(), &"中");
        assert_eq!(
            &buf[(8, 0)].symbol(),
            &" ",
            "the wide glyph's shadow cell must be empty, not overwritten by the next char"
        );
        assert_eq!(&buf[(9, 0)].symbol(), &"b");
    }

    /// Reproduces a real repro: a startup `emsg` autocommand error carries
    /// an embedded `\n` (nvim's own multi-line message convention), and the
    /// toast must lay it out as two rows sized to the longer physical
    /// line, not one row wide enough to hold both lines concatenated with
    /// the columns past each line's own text left showing the grid content
    /// underneath.
    #[test]
    fn messages_overlay_multiline_message_gets_one_row_per_physical_line_and_clears_its_box() {
        let mut model = Model::new();
        model.engine.grid.apply(GridOp::Resize {
            width: 26,
            height: 5,
        });
        // stands in for real grid content underneath the toast's rect (a
        // composited floating window in the live repro): every cell the
        // toast's own clear must overwrite, or it bleeds through exactly
        // like the reported foreign glyphs at a message row's right edge
        for row in 0..2u16 {
            model.engine.grid.apply(GridOp::PutLine {
                row,
                col_start: 0,
                cells: vec![("X".into(), 0, 26)],
            });
        }
        apply(
            &mut model,
            view_core::events::UiEvent::MsgShow {
                kind: "echoerr".into(),
                content: vec![(0, "short\nmuch longer second line".into())],
                replace_last: false,
            },
        );

        let surface = view_surface::render(&model);
        let backend = TestBackend::new(26, 5);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| composite(&model, &surface, f)).unwrap();
        let buf = terminal.backend().buffer().clone();
        let row_text =
            |r: u16| -> String { (0..26).map(|c| buf[(c, r)].symbol().to_string()).collect() };

        // box width = the longer physical line (23 cells), not the sum of
        // both lines (28): right-anchored at column 3, so columns 0-2 stay
        // real grid content (outside the toast's own rect) on both rows,
        // and the second line exactly fills its row with no leftover to
        // clear, while the first line's row has 18 cells of clear past
        // "short" that must not still show the grid's "X" stand-in
        let expected_first_line = format!("XXX{:<23}", "short");
        let expected_second_line = format!("XXX{}", "much longer second line");
        assert_eq!(
            row_text(0),
            expected_first_line,
            "first physical line's row must be cleared past its own text, not left showing the grid underneath"
        );
        assert_eq!(
            row_text(1),
            expected_second_line,
            "second physical line must land on its own row, right-anchored to the wider line's width"
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

    #[test]
    fn shell_paints_a_themed_statusline_row_and_a_waiting_indicator() {
        let mut model = Model::with_term_size(20, 4);
        model.content_painted = false;

        let surface = view_surface::render(&model);
        let backend = TestBackend::new(20, 4);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| composite(&model, &surface, f)).unwrap();
        let buf = terminal.backend().buffer().clone();

        // bottom row (3): the statusline placeholder fill, present even
        // though nothing has painted real content there yet
        assert_eq!(&buf[(0, 3)].symbol(), &" ");
        // middle row (height/2 == 2): the waiting indicator's text
        assert_eq!(&buf[(0, 2)].symbol(), &"v");
        assert_eq!(&buf[(1, 2)].symbol(), &"i");
    }

    #[test]
    fn shell_never_paints_once_content_painted_is_true() {
        let model = Model::with_term_size(20, 4);
        assert!(model.content_painted, "default must be the steady state");

        let surface = view_surface::render(&model);
        let backend = TestBackend::new(20, 4);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| composite(&model, &surface, f)).unwrap();
        let buf = terminal.backend().buffer().clone();

        assert_eq!(
            &buf[(0, 2)].symbol(),
            &" ",
            "no waiting indicator once content_painted is true"
        );
    }
}
