//! Renders a [`Grid`] into a ratatui [`Frame`], styled per a `view-core`
//! [`HlTable`].

use ratatui::style::{Color, Style};
use view_core::grid::Grid;
pub use view_core::hl::{HlAttr, HlTable};

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
