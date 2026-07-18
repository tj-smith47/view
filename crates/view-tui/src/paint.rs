//! Renders a [`Grid`] into a ratatui [`Frame`], and the wire-value clamping
//! helpers used to get untrusted `redraw` coordinates into `Grid`'s `u16`
//! address space without a truncating cast.

use ratatui::style::{Color, Style};
use view_core::grid::Grid;

/// One highlight group's rendering attributes, decoded from `hl_attr_define`.
pub struct HlAttr {
    /// Foreground color, or `None` to fall back to the default.
    pub fg: Option<u32>,
    /// Background color, or `None` to fall back to the default.
    pub bg: Option<u32>,
    /// Whether the group renders bold.
    pub bold: bool,
    /// Whether the group renders italic.
    pub italic: bool,
    /// Whether the group renders underlined.
    pub underline: bool,
    /// Whether foreground and background swap for this group.
    pub reverse: bool,
}

/// The highlight table: default colors plus every highlight group defined
/// so far, keyed by the `hl_id` `grid_line` cells reference.
pub struct HlTable {
    /// Default foreground, or `None` if nvim has not set one yet.
    pub default_fg: Option<u32>,
    /// Default background, or `None` if nvim has not set one yet.
    pub default_bg: Option<u32>,
    /// Highlight groups by id.
    pub attrs: std::collections::HashMap<u64, HlAttr>,
}

/// Largest grid dimension accepted from the wire. Far beyond any physical
/// terminal, small enough that a malformed or desynced `grid_resize`
/// cannot make the grid allocate unboundedly.
const MAX_GRID_DIM: u16 = 2048;

/// Saturates a wire dimension into `u16` and clamps it to [`MAX_GRID_DIM`].
#[must_use]
pub fn clamp_dim(dim: u64) -> u16 {
    saturate_u16(dim).min(MAX_GRID_DIM)
}

// a plain `as u16` cast would wrap out-of-range wire values back into
// range (65536 becomes 0), turning a malformed coordinate into a write at
// a real cell; saturating keeps it out of range so Grid ignores it
/// Saturates a wire `u64` coordinate into `u16`, clamping to `u16::MAX`
/// instead of truncating.
#[must_use]
pub fn saturate_u16(v: u64) -> u16 {
    u16::try_from(v).unwrap_or(u16::MAX)
}

/// Paints every visible `grid` cell into `frame`'s buffer, styled per `hl`.
pub fn paint(grid: &Grid, hl: &HlTable, frame: &mut ratatui::Frame<'_>) {
    let area = frame.area();
    let buf = frame.buffer_mut();
    let (w, h) = grid.size();
    for row in 0..h.min(area.height) {
        for col in 0..w.min(area.width) {
            if let Some(cell) = grid.cell(row, col) {
                let out = &mut buf[(area.x + col, area.y + row)];
                out.set_symbol(if cell.text.is_empty() {
                    " "
                } else {
                    &cell.text
                });
                out.set_style(style_for(cell.hl_id, hl));
            }
        }
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
    use super::*;

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
}
