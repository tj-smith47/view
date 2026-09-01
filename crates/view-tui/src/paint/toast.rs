//! The toast stack's painter: one framed box per visible notice, drawn
//! wherever `view-surface` placed it this frame.
//!
//! Split out of `paint.rs` because the stack is now a family of layers
//! rather than one, and because its geometry -- the frame, the interior
//! inset, the border charset -- is the one overlay `view-surface` hands
//! over unframed.

use ratatui::buffer::Buffer;
use ratatui::style::Style;

use view_core::native::views::Span;
use view_surface::overlay::BorderSet;

use super::{
    border_color, paint_text_row, ratatui_style, set_border_cell, ChromeGroup, Damage,
    ResolvedStyle, Theme,
};

/// Renders one notice's toast box: `render()` already picked exactly the
/// physical lines this box holds (`Messages::visible_toasts`) and
/// grew/anchored `area` to them plus a one-cell frame on every edge, so
/// painting only has to draw the border around `area` and write one line
/// per interior row, in the order given.
///
/// A truly empty `lines` paints nothing at all -- no clear, no border --
/// matching `render()`'s own contract of never emitting a layer for a notice
/// with no lines; a caller that hands this an empty slice with a stale
/// nonzero `area` (only possible by bypassing `render()`, e.g. directly in
/// tests) must still see no bleed from a frame that has no content to frame.
///
/// The whole rect -- border cells included -- is cleared to the toast's own
/// `msg_area` style first, before any text or border glyph: without this, a
/// row, a border cell, or the columns past a line's own text on a row keeps
/// showing whatever the `EngineGrid` layer painted underneath (real nvim
/// content, e.g. a floating window's cells composited into the base grid
/// when the frontend has no `ext_multigrid` support), which is what a live
/// repro showed as foreign glyphs bleeding through at a toast row's right
/// edge. It is also what makes a box sliding out to the right leave clean
/// cells behind it rather than a trail of its own last frame.
///
/// Every write here is clipped to the rows `damage` names, border cells
/// included: see `composite_layers` for why a row of this rect the frame is
/// not repainting is not this painter's to touch.
pub(super) fn paint_toast(
    lines: &[Vec<Span>],
    theme: &Theme,
    borders: BorderSet,
    area: ratatui::layout::Rect,
    damage: &Damage,
    buf: &mut Buffer,
) {
    if lines.is_empty() {
        return;
    }

    let msg = theme.float_chrome(ChromeGroup::MsgArea, theme.float_bg());
    let style = ratatui_style(msg);
    let blank = " ".repeat(usize::from(area.width));
    for row in (0..area.height).filter(|&row| damage.covers_row_of(area, row)) {
        paint_text_row(&blank, style, area, row, buf);
    }

    let border_style = ratatui_style(ResolvedStyle {
        fg: Some(toast_border_color(theme)),
        bg: msg.bg,
        ..ResolvedStyle::default()
    });
    paint_toast_border(area, borders, border_style, damage, buf);

    let inner = inset_by_one(area);
    // every toast line is a single `StyleRole::Plain` span (see
    // `LayerKind::Toast`'s doc comment), so this row's own `style` is the
    // whole story -- `paint_text_row` over the flattened text is the honest
    // rendering, not a placeholder for per-span resolution nobody asked for
    for (i, spans) in lines.iter().enumerate() {
        let Ok(row) = u16::try_from(i) else {
            break;
        };
        if !damage.covers_row_of(inner, row) {
            continue;
        }
        paint_text_row(
            &view_surface::overlay::line_text(spans),
            style,
            inner,
            row,
            buf,
        );
    }
}

/// `area` shrunk by one cell on every edge: the interior the border frame
/// leaves for the notice's own lines, matching exactly the unframed rect
/// `view-surface` grew by two cols/two rows to make room for the border this
/// module draws around it.
fn inset_by_one(area: ratatui::layout::Rect) -> ratatui::layout::Rect {
    ratatui::layout::Rect {
        x: area.x.saturating_add(1),
        y: area.y.saturating_add(1),
        width: area.width.saturating_sub(2),
        height: area.height.saturating_sub(2),
    }
}

/// Draws `borders` on all four edges of `area`, styled `style`.
/// A degenerate area narrower or shorter than 2 cells has no distinct edge
/// cells to draw (a box clipped to a sliver by the grid's right edge on its
/// way out is exactly this case, as is a direct unit-test caller's rect) and
/// paints nothing rather than writing corner glyphs on top of each other.
///
/// The charset arrives from the caller rather than being spelled here: a
/// toast is the one float `view-surface` hands over unframed, and a second
/// literal set would have kept drawing box-drawing glyphs at a terminal that
/// cannot render them long after every other float stopped.
fn paint_toast_border(
    area: ratatui::layout::Rect,
    borders: BorderSet,
    style: Style,
    damage: &Damage,
    buf: &mut Buffer,
) {
    if area.width < 2 || area.height < 2 {
        return;
    }
    let last_col = area.width - 1;
    let last_row = area.height - 1;
    let top_row = damage.covers_row_of(area, 0);
    let bottom_row = damage.covers_row_of(area, last_row);
    for col in 0..area.width {
        let (top, bottom) = match col {
            0 => (borders.top_left, borders.bottom_left),
            c if c == last_col => (borders.top_right, borders.bottom_right),
            _ => (borders.horizontal, borders.horizontal),
        };
        if top_row {
            set_border_cell(buf, area.x + col, area.y, top, style);
        }
        if bottom_row {
            set_border_cell(buf, area.x + col, area.y + last_row, bottom, style);
        }
    }
    let vert = borders.vertical;
    for row in 1..last_row {
        if !damage.covers_row_of(area, row) {
            continue;
        }
        set_border_cell(buf, area.x, area.y + row, vert, style);
        set_border_cell(buf, area.x + last_col, area.y + row, vert, style);
    }
}

/// The toast border's foreground. `Theme` resolves no builtin group carrying
/// a genuinely muted/comment tone -- `from_hl` maps only `StatusLine`, the
/// tabline and popup-menu families, and `MsgArea`, none of which plays the
/// role real nvim's own `Comment`/`NonText`/`FloatBorder` groups would (an
/// unobtrusive chrome color distinct from both emphasis and interior-text
/// colors), and this module never probes nvim for a group it does not
/// already resolve -- so the border derives a dimmed variant of the
/// interior's own `msg_area` foreground when one is set, visibly distinct
/// from the full-brightness interior text with no further highlight lookup
/// or RPC round trip. Never falls back to a dimmed `MsgArea` background: the
/// border sits ON that background, so dimming it paints a frame that is
/// merely a darker shade of the surface it is supposed to stand out from --
/// on a black-bg/no-fg theme this dims pure black to itself, an invisible
/// border around a box the user cannot tell apart from empty screen. The
/// floor is the plain (undimmed) neutral grey constant instead, which stays
/// visible against any background.
pub(super) fn toast_border_color(theme: &Theme) -> u32 {
    border_color(theme.chrome(ChromeGroup::MsgArea))
}
