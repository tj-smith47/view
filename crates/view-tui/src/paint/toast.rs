//! The toast stack's painter: one framed box per visible notice, drawn
//! wherever `view-surface` placed it this frame.
//!
//! Split out of `paint.rs` because the stack is now a family of layers
//! rather than one, and because its geometry -- the frame, the interior
//! inset, the border charset -- is the one overlay `view-surface` hands
//! over unframed.

use ratatui::buffer::Buffer;
use ratatui::style::Style;

use view_core::native::text::{cluster_width, clusters};
use view_core::native::views::Span;
use view_surface::overlay::BorderSet;

use super::{
    float_border_color, paint_text_row, ratatui_style, set_border_cell, ChromeGroup, Damage,
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
/// interior style first, before any text or border glyph: without this, a
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
/// The two per-frame flags [`paint_toast`] takes beyond its lines and its
/// geometry -- bundled into one argument to stay under clippy's
/// too-many-arguments threshold once a corner's own exit slide needed a
/// second one alongside `paused`.
pub(super) struct ToastFrame {
    pub(super) skip: u16,
    pub(super) paused: bool,
}

pub(super) fn paint_toast(
    lines: &[Vec<Span>],
    frame: ToastFrame,
    theme: &Theme,
    borders: BorderSet,
    area: ratatui::layout::Rect,
    damage: &Damage,
    buf: &mut Buffer,
) {
    if lines.is_empty() {
        return;
    }

    let body = toast_body(theme);
    let style = ratatui_style(body);
    let blank = " ".repeat(usize::from(area.width));
    for row in (0..area.height).filter(|&row| damage.covers_row_of(area, row)) {
        paint_text_row(&blank, style, area, row, buf);
    }

    let border_style = ratatui_style(ResolvedStyle {
        fg: Some(toast_border_color(theme)),
        bg: body.bg,
        ..ResolvedStyle::default()
    });
    paint_toast_border(area, borders, frame.paused, border_style, damage, buf);

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
            skip_cells(&view_surface::overlay::line_text(spans), frame.skip),
            style,
            inner,
            row,
            buf,
        );
    }
}

/// `text` with its first `cells` display columns dropped -- a left corner's
/// exit slide (see [`view_surface::LayerKind::Toast`]'s `x_offset` doc):
/// the box's rect can travel no further left than column 0, so instead the
/// window into its own text narrows from the start, the same read as the
/// text sliding out underneath it that a right corner gets from `rect`
/// moving right on its own.
fn skip_cells(text: &str, cells: u16) -> &str {
    if cells == 0 {
        return text;
    }
    let mut consumed = 0_u16;
    let mut at = 0_usize;
    for cluster in clusters(text) {
        if consumed >= cells {
            break;
        }
        consumed = consumed.saturating_add(cluster_width(cluster));
        at = at.saturating_add(cluster.len());
    }
    text.get(at..).unwrap_or("")
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
///
/// `paused` sets that charset's own pause mark into the top run, one cell in
/// from the right corner -- the corner the eye already tracks, since every
/// box in the stack is right-anchored there. Under three cells wide the top
/// run is corners alone and the mark is dropped rather than drawn over one
/// of them: a corner replaced by a mark reads as a broken frame, and a box
/// that narrow is a sliver on its way off the right edge.
fn paint_toast_border(
    area: ratatui::layout::Rect,
    borders: BorderSet,
    paused: bool,
    style: Style,
    damage: &Damage,
    buf: &mut Buffer,
) {
    if area.width < 2 || area.height < 2 {
        return;
    }
    let last_col = area.width - 1;
    let mark = (paused && area.width >= 3).then_some(last_col - 1);
    let last_row = area.height - 1;
    let top_row = damage.covers_row_of(area, 0);
    let bottom_row = damage.covers_row_of(area, last_row);
    for col in 0..area.width {
        let (top, bottom) = match col {
            0 => (borders.top_left, borders.bottom_left),
            c if c == last_col => (borders.top_right, borders.bottom_right),
            c if mark == Some(c) => (borders.pause, borders.horizontal),
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

/// A toast's interior: a float body first (`NormalFloat`), with nvim's
/// message-area group filling whichever half of it the colorscheme left
/// unstated, and `Normal` underneath both as their declared fallback.
///
/// That order is what a toast is. It carries a message, so a scheme that
/// themes `MsgArea` and nothing else still reaches it; it is a box drawn
/// over the buffer, so a scheme that themes floats and says nothing about
/// the message area -- habamax, and it is far from alone -- gets its float
/// colors rather than a box the user can only find by its border.
fn toast_body(theme: &Theme) -> ResolvedStyle {
    let float = theme.float_chrome(ChromeGroup::NormalFloat, theme.float_bg());
    let msg = theme.float_chrome(ChromeGroup::MsgArea, theme.float_bg());
    ResolvedStyle {
        fg: float.fg.or(msg.fg),
        bg: float.bg.or(msg.bg),
        ..float
    }
}

/// The toast border's foreground: the same `FloatBorder`-first answer every
/// other native float's frame takes, over this box's own interior.
///
/// The derived floor behind it never dims a background: the border sits ON
/// that background, so dimming it paints a frame that is merely a darker
/// shade of the surface it is supposed to stand out from -- on a
/// black-bg/no-fg theme this dims pure black to itself, an invisible border
/// around a box the user cannot tell apart from empty screen. The floor is
/// the plain (undimmed) neutral grey constant instead, which stays visible
/// against any background.
pub(super) fn toast_border_color(theme: &Theme) -> u32 {
    float_border_color(theme, toast_body(theme))
}
