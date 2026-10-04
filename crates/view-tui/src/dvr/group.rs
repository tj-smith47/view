//! A keyframe and the delta frames painted on top of it, the unit the ring
//! keeps and drops.

use ratatui::buffer::Cell;

use super::cell::{Packed, Symbols};

/// The frames one group holds before the next frame opens a new keyframe.
pub(crate) const GROUP_FRAMES: usize = 256;

/// A region of a frame that shows the frame before it shifted: rows
/// `top..bottom` of columns `left..right` hold what the frame before held
/// `by` rows further down, or further up when `by` is negative.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct Scroll {
    /// The first row the shift fills.
    pub top: u16,
    /// The row after the last one the shift fills.
    pub bottom: u16,
    /// The first column of the region.
    pub left: u16,
    /// The column after the region's last.
    pub right: u16,
    /// How many rows below each filled row its content comes from.
    pub by: i16,
}

impl Scroll {
    /// A shift of rows `rows` and columns `cols` by `by` rows.
    #[must_use]
    pub fn new(rows: std::ops::Range<u16>, cols: std::ops::Range<u16>, by: i16) -> Self {
        Self {
            top: rows.start,
            bottom: rows.end,
            left: cols.start,
            right: cols.end,
            by,
        }
    }

    /// Whether every row the shift reads and fills lies inside `area`.
    pub(crate) fn fits(self, area: (u16, u16)) -> bool {
        let read = |row: u16| i32::from(row) + i32::from(self.by);
        self.by != 0
            && self.top < self.bottom
            && self.bottom <= area.1
            && self.left < self.right
            && self.right <= area.0
            && read(self.top) >= 0
            && read(self.bottom) <= i32::from(area.1)
    }

    /// Whether column `x` of row `y` is filled by the shift.
    pub(crate) fn covers(self, x: u16, y: u16) -> bool {
        (self.top..self.bottom).contains(&y) && (self.left..self.right).contains(&x)
    }

    /// The row the content of row `y` comes from.
    pub(crate) fn source(self, y: u16) -> Option<u16> {
        y.checked_add_signed(self.by)
    }

    /// Shifts the region of the row-major `screen`, `width` cells wide.
    pub(crate) fn apply(self, screen: &mut [Cell], width: u16) {
        let width = usize::from(width);
        // a row is read before the shift overwrites it: top down when the
        // content comes from below, bottom up when it comes from above
        for i in 0..self.bottom.saturating_sub(self.top) {
            let y = if self.by > 0 {
                self.top + i
            } else {
                self.bottom - 1 - i
            };
            let Some(from) = self.source(y) else {
                continue;
            };
            for x in self.left..self.right {
                let at = |row: u16| usize::from(row) * width + usize::from(x);
                if let Some(cell) = screen.get(at(from)).cloned() {
                    if let Some(slot) = screen.get_mut(at(y)) {
                        *slot = cell;
                    }
                }
            }
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct Frame {
    pub(crate) seq: u64,
    pub(crate) at_us: u64,
    pub(crate) cursor: Option<(u16, u16)>,
    /// Where this frame's changed cells end in the group's delta list. A
    /// frame's own cells start where the frame before it ends.
    pub(crate) deltas_end: usize,
    /// The shift applied to the frame before, ahead of the changed cells.
    pub(crate) scroll: Option<Scroll>,
}

/// A keyframe and the delta frames painted on top of it, evicted whole.
#[derive(Debug, Default, Clone)]
pub(crate) struct Group {
    pub(crate) area: (u16, u16),
    pub(crate) key: Vec<Packed>,
    pub(crate) deltas: Vec<(u16, u16, Packed)>,
    pub(crate) frames: Vec<Frame>,
    pub(crate) symbols: Symbols,
    /// The bytes the ring has counted for this group.
    pub(crate) counted: usize,
}

impl Group {
    /// The bytes the group holds.
    pub(crate) fn bytes(&self) -> usize {
        self.key.capacity() * std::mem::size_of::<Packed>()
            + self.deltas.capacity() * std::mem::size_of::<(u16, u16, Packed)>()
            + self.frames.capacity() * std::mem::size_of::<Frame>()
            + self.symbols.bytes()
    }

    /// Empties the group and reserves it for `area`.
    pub(crate) fn reset(&mut self, area: (u16, u16)) {
        let cells = usize::from(area.0) * usize::from(area.1);
        self.key.clear();
        self.deltas.clear();
        self.frames.clear();
        // a screen where one cell in sixteen shows a glyph of its own
        self.symbols.clear(cells / 16);
        // a group grown for a larger screen would otherwise hold that
        // screen's memory for the rest of the session
        if self.key.capacity() > 2 * cells {
            self.key.shrink_to(cells);
            self.deltas.shrink_to(cells);
        }
        self.key.reserve_exact(cells);
        self.deltas.reserve_exact(cells);
        self.frames.reserve_exact(GROUP_FRAMES);
        self.area = area;
    }

    /// The bytes a group reserved for `area` holds before any symbol.
    pub(crate) fn reserved_bytes(area: (u16, u16)) -> usize {
        let cells = usize::from(area.0) * usize::from(area.1);
        cells * std::mem::size_of::<Packed>()
            + cells * std::mem::size_of::<(u16, u16, Packed)>()
            + GROUP_FRAMES * std::mem::size_of::<Frame>()
    }

    /// Appends one changed cell to the frame being built. Returns false,
    /// holding nothing new, when the reserved delta list is full.
    pub(crate) fn push_cell(&mut self, x: u16, y: u16, cell: &Cell) -> bool {
        if self.deltas.len() == self.deltas.capacity() {
            return false;
        }
        let packed = self.symbols.pack(cell);
        self.deltas.push((x, y, packed));
        true
    }

    /// Drops the cells of the frame being built.
    pub(crate) fn abort(&mut self) {
        let end = self.frames.last().map_or(0, |f| f.deltas_end);
        self.deltas.truncate(end);
    }

    /// Writes frame `index` into `screen`, row-major at the group's size:
    /// the keyframe, then each later frame's shift and changed cells.
    pub(crate) fn replay(&self, index: usize, screen: &mut Vec<Cell>) {
        screen.clear();
        screen.extend(self.key.iter().map(|p| self.symbols.unpack(*p)));
        let width = usize::from(self.area.0);
        let mut start = 0;
        for frame in self.frames.iter().take(index + 1).skip(1) {
            if let Some(scroll) = frame.scroll {
                scroll.apply(screen, self.area.0);
            }
            for &(x, y, packed) in self.deltas.get(start..frame.deltas_end).unwrap_or_default() {
                if x < self.area.0 {
                    if let Some(slot) = screen.get_mut(usize::from(y) * width + usize::from(x)) {
                        *slot = self.symbols.unpack(packed);
                    }
                }
            }
            start = frame.deltas_end;
        }
    }

    pub(crate) fn first_seq(&self) -> u64 {
        self.frames.first().map_or(0, |f| f.seq)
    }
}
