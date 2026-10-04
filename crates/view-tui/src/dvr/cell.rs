//! A recorded cell packed into twenty bytes, the group's table of the
//! symbols its cells show, and the view of a cell that leaves the crate.

use std::collections::HashMap;
use std::hash::{BuildHasherDefault, Hasher};

use ratatui::buffer::Cell;
use ratatui::style::{Color, Modifier};
use view_core::hash::{fnv1a, fnv1a_extend, fnv1a_step, FNV_OFFSET};
use view_core::native::text::clusters;

/// The sixteen named colors in the order their packed payload numbers them.
const NAMED: [Color; 16] = [
    Color::Black,
    Color::Red,
    Color::Green,
    Color::Yellow,
    Color::Blue,
    Color::Magenta,
    Color::Cyan,
    Color::Gray,
    Color::DarkGray,
    Color::LightRed,
    Color::LightGreen,
    Color::LightYellow,
    Color::LightBlue,
    Color::LightMagenta,
    Color::LightCyan,
    Color::White,
];

/// Symbol ids below this are the ASCII character of that code; the rest
/// index the group's table.
const ASCII_IDS: u32 = 128;

/// Every ASCII byte at its own index, so a one-byte symbol is a slice of it.
static ASCII: [u8; 128] = {
    let mut bytes = [0u8; 128];
    let mut i = 0u8;
    while i < 128 {
        bytes[i as usize] = i;
        i += 1;
    }
    bytes
};

/// One recorded cell: its symbol's id in the group's table and its colors
/// and attributes packed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) struct Packed {
    sym: u32,
    fg: u32,
    bg: u32,
    ul: u32,
    modifier: u16,
}

/// The symbols a group's cells show, each stored once. A one-byte ASCII
/// symbol needs no entry.
#[derive(Debug, Default, Clone)]
pub(crate) struct Symbols {
    text: String,
    /// Where each entry's text starts in `text` and how long it is.
    spans: Vec<(u32, u32)>,
    /// An entry's id by the FNV-1a hash of its text. A colliding symbol
    /// gets an entry of its own and no index row.
    index: HashMap<u64, u32, BuildHasherDefault<Prehashed>>,
}

/// A hasher for keys that already are a hash.
#[derive(Debug, Default)]
struct Prehashed(u64);

impl Hasher for Prehashed {
    fn finish(&self) -> u64 {
        self.0
    }

    fn write(&mut self, bytes: &[u8]) {
        for &byte in bytes {
            self.0 = self.0.rotate_left(8) ^ u64::from(byte);
        }
    }

    fn write_u64(&mut self, hash: u64) {
        self.0 = hash;
    }
}

impl Symbols {
    /// Empties the table. Storage past what `keep` entries take is given
    /// back, so a table grown on a screen of distinct glyphs does not stay
    /// that size for every later group.
    pub(crate) fn clear(&mut self, keep: usize) {
        self.text.clear();
        self.spans.clear();
        self.index.clear();
        if self.spans.capacity() > 2 * keep {
            self.spans.shrink_to(keep);
            self.index.shrink_to(keep);
        }
        if self.text.capacity() > 8 * keep {
            self.text.shrink_to(4 * keep);
        }
    }

    /// The bytes the table holds. The index is counted by its buckets,
    /// each an entry and a control byte, of which its capacity is 7/8.
    pub(crate) fn bytes(&self) -> usize {
        let capacity = self.index.capacity();
        let buckets = if capacity == 0 {
            0
        } else {
            (capacity * 8 / 7).next_power_of_two()
        };
        self.text.capacity()
            + self.spans.capacity() * std::mem::size_of::<(u32, u32)>()
            + buckets * (std::mem::size_of::<(u64, u32)>() + 1)
    }

    fn id(&mut self, symbol: &str) -> u32 {
        if let [byte] = symbol.as_bytes() {
            if byte.is_ascii() {
                return u32::from(*byte);
            }
        }
        let hash = fnv1a(symbol.as_bytes());
        if let Some(&id) = self.index.get(&hash) {
            if self.get(id) == symbol {
                return id;
            }
        }
        let (Ok(start), Ok(len), Ok(entry)) = (
            u32::try_from(self.text.len()),
            u32::try_from(symbol.len()),
            u32::try_from(self.spans.len()),
        ) else {
            return u32::from(b' ');
        };
        let id = ASCII_IDS.saturating_add(entry);
        self.text.push_str(symbol);
        self.spans.push((start, len));
        self.index.entry(hash).or_insert(id);
        id
    }

    /// The symbol of id `id`, a blank for an id the table does not hold.
    pub(crate) fn get(&self, id: u32) -> &str {
        let found = if id < ASCII_IDS {
            usize::try_from(id)
                .ok()
                .and_then(|i| ASCII.get(i..=i))
                .and_then(|b| std::str::from_utf8(b).ok())
        } else {
            usize::try_from(id - ASCII_IDS)
                .ok()
                .and_then(|i| self.spans.get(i))
                .and_then(|&(start, len)| {
                    let start = usize::try_from(start).ok()?;
                    self.text.get(start..start + usize::try_from(len).ok()?)
                })
        };
        found.unwrap_or(" ")
    }

    /// Packs `cell`, entering its symbol in the table when it is new.
    pub(crate) fn pack(&mut self, cell: &Cell) -> Packed {
        Packed {
            sym: self.id(cell.symbol()),
            fg: pack(cell.fg),
            bg: pack(cell.bg),
            ul: pack(cell.underline_color),
            modifier: cell.modifier.bits(),
        }
    }

    /// The cell `packed` was packed from.
    pub(crate) fn unpack(&self, packed: Packed) -> Cell {
        restore(self.view(0, 0, packed))
    }

    /// `packed` at column `x` of row `y`, as it leaves the crate.
    pub(crate) fn view(&self, x: u16, y: u16, packed: Packed) -> CellView<'_> {
        CellView {
            x,
            y,
            symbol: self.get(packed.sym),
            fg: packed.fg,
            bg: packed.bg,
            ul: packed.ul,
            modifier: packed.modifier,
        }
    }
}

/// One recorded cell. A color packs a tag in its top byte (0 the
/// terminal's default, 1 a named color, 2 an indexed one, 3 an rgb one)
/// and its payload in the low bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct CellView<'a> {
    /// The cell's column.
    pub x: u16,
    /// The cell's row.
    pub y: u16,
    /// The grapheme the cell shows.
    pub symbol: &'a str,
    /// The foreground color, packed.
    pub fg: u32,
    /// The background color, packed.
    pub bg: u32,
    /// The underline color, packed.
    pub ul: u32,
    /// The text attributes, as `ratatui`'s modifier bits.
    pub modifier: u16,
}

impl<'a> CellView<'a> {
    /// A cell at column `x` of row `y` showing `symbol` in the packed
    /// colors and attributes given, as a clip decoder builds it.
    #[must_use]
    pub fn new(x: u16, y: u16, symbol: &'a str, colors: [u32; 3], modifier: u16) -> Self {
        let [fg, bg, ul] = colors;
        Self {
            x,
            y,
            symbol,
            fg,
            bg,
            ul,
            modifier,
        }
    }
}

/// The cell `view` was recorded from, holding the symbol's first grapheme
/// cluster. A symbol carrying a control character, which only a damaged
/// clip holds, comes back blank.
pub(crate) fn restore(view: CellView<'_>) -> Cell {
    let mut cell = Cell::EMPTY;
    if !view.symbol.chars().any(char::is_control) {
        if let Some(cluster) = clusters(view.symbol).next() {
            cell.set_symbol(cluster);
        }
    }
    cell.fg = unpack(view.fg);
    cell.bg = unpack(view.bg);
    cell.underline_color = unpack(view.ul);
    cell.modifier = Modifier::from_bits_truncate(view.modifier);
    cell
}

/// A hash of `cells`, the same for two rows that show the same cells.
pub(crate) fn row_hash(cells: &[Cell]) -> u64 {
    cells.iter().fold(FNV_OFFSET, |hash, cell| {
        let hash = fnv1a_extend(hash, cell.symbol().as_bytes());
        let colors = (u64::from(pack(cell.fg)) << 32) | u64::from(pack(cell.bg));
        let hash = fnv1a_step(hash, colors);
        let rest = (u64::from(pack(cell.underline_color)) << 16) | u64::from(cell.modifier.bits());
        fnv1a_step(hash, rest)
    })
}

pub(crate) fn pack(color: Color) -> u32 {
    match color {
        Color::Indexed(i) => u32::from_be_bytes([2, 0, 0, i]),
        Color::Rgb(r, g, b) => u32::from_be_bytes([3, r, g, b]),
        named => NAMED
            .iter()
            .position(|c| *c == named)
            .and_then(|i| u8::try_from(i).ok())
            .map_or(0, |i| u32::from_be_bytes([1, 0, 0, i])),
    }
}

pub(crate) fn unpack(packed: u32) -> Color {
    let [tag, r, g, b] = packed.to_be_bytes();
    match tag {
        1 => NAMED.get(usize::from(b)).copied().unwrap_or(Color::Reset),
        2 => Color::Indexed(b),
        3 => Color::Rgb(r, g, b),
        _ => Color::Reset,
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    #[test]
    fn a_packed_cell_is_twenty_bytes_and_comes_back_whole() {
        assert_eq!(std::mem::size_of::<Packed>(), 20);
        let mut symbols = Symbols::default();
        let mut styled = Cell::new("世");
        styled.fg = Color::Rgb(1, 2, 3);
        styled.bg = Color::Indexed(17);
        styled.underline_color = Color::LightRed;
        styled.modifier = Modifier::BOLD | Modifier::UNDERLINED;
        for cell in [
            Cell::EMPTY,
            Cell::new("a"),
            Cell::new("e\u{301}"),
            Cell::new("\u{1F468}\u{200D}\u{1F469}"),
            styled,
        ] {
            let packed = symbols.pack(&cell);
            assert_eq!(symbols.unpack(packed), cell);
        }
    }

    #[test]
    fn a_symbol_is_stored_once() {
        let mut symbols = Symbols::default();
        let first = symbols.pack(&Cell::new("│"));
        let bytes = symbols.text.len();
        assert_eq!(symbols.pack(&Cell::new("│")), first);
        assert_eq!(symbols.text.len(), bytes);
        assert_eq!(symbols.pack(&Cell::new("a")).sym, u32::from(b'a'));
        assert_eq!(symbols.text.len(), bytes, "ASCII takes no entry");
    }

    #[test]
    fn a_restored_cell_holds_one_grapheme_cluster() {
        let view = CellView::new(0, 0, "e\u{301}x", [0; 3], 0);
        assert_eq!(restore(view).symbol(), "e\u{301}");
    }
}
