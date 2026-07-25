//! Terminal grid model: the cell buffer nvim's `ext_linegrid` events paint into.
//!
//! [`Grid`] holds no I/O and no RPC awareness; it is a pure sink for
//! [`GridOp`] values that a higher layer decodes from nvim redraw events.

/// A single grid cell: display text and the highlight group it was painted with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cell {
    /// The text to display, generally a single grapheme.
    pub text: String,
    /// The highlight group id this cell was painted with.
    pub hl_id: u64,
}

impl Default for Cell {
    fn default() -> Self {
        Self {
            text: " ".to_string(),
            hl_id: 0,
        }
    }
}

/// A grid mutation decoded from an nvim `ext_linegrid` redraw event.
///
/// New variants may be added as more `ext_linegrid` event kinds are wired up.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GridOp {
    /// Resize the grid, preserving the overlapping region of existing content.
    Resize {
        /// New width in columns.
        width: u16,
        /// New height in rows.
        height: u16,
    },
    /// Reset every cell to its default value.
    Clear,
    /// Move the cursor to the given position, clamped to grid bounds.
    CursorGoto {
        /// Target row.
        row: u16,
        /// Target column.
        col: u16,
    },
    /// Paint a run of cells starting at `(row, col_start)`.
    PutLine {
        /// Row to paint into.
        row: u16,
        /// Column the run starts at.
        col_start: u16,
        /// `(text, hl_id, repeat)` triples; each is written `repeat` times in sequence.
        cells: Vec<(String, u64, u64)>,
    },
    /// Scroll the region `top..bot`, `left..right` by `rows` (positive scrolls content up).
    Scroll {
        /// Region top row, inclusive.
        top: u16,
        /// Region bottom row, exclusive.
        bot: u16,
        /// Region left column, inclusive.
        left: u16,
        /// Region right column, exclusive.
        right: u16,
        /// Rows to scroll; positive moves content up (toward row 0), negative moves it down.
        rows: i32,
    },
}

/// The rows a batch of [`GridOp`]s changed, so a repaint can composite only
/// the damaged region instead of the whole grid.
///
/// `full` supersedes `rows`: a resize or clear invalidates every cell, so the
/// paint layer must repaint the whole grid and can ignore `rows` entirely.
/// `rows` are grid-space row indices (0-based within the grid), which the
/// paint layer offsets by any reserved chrome rows to reach terminal-space.
/// Rows may repeat and are not sorted; a consumer only ever asks whether a
/// given row is present, for which membership, not order, is what matters.
#[non_exhaustive]
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GridDamage {
    /// Every cell changed (a resize or clear happened this batch): repaint
    /// the whole grid regardless of `rows`.
    pub full: bool,
    /// Grid-space rows that changed, when `full` is `false`.
    pub rows: Vec<u16>,
}

impl GridDamage {
    /// Damage that covers the whole grid, for callers that always repaint
    /// every cell (a first paint, a placeholder-shell frame, an error screen).
    #[must_use]
    pub fn full() -> Self {
        Self {
            full: true,
            rows: Vec::new(),
        }
    }

    /// Whether this damage covers grid-space `row` (always true when `full`).
    #[must_use]
    pub fn covers(&self, row: u16) -> bool {
        self.full || self.rows.contains(&row)
    }
}

/// A rectangular buffer of [`Cell`]s addressed by nvim's `ext_linegrid` protocol.
///
/// All mutation happens through [`Grid::apply`]; every [`GridOp`] is bounds-checked
/// and ignored rather than panicking when it falls outside the current grid size.
///
/// Each mutation also records which rows it touched (see [`Grid::take_dirty`])
/// so the compositor can clip a repaint to the changed region. Damage is
/// biased toward over-reporting, never under: a mutation that writes nothing
/// (a fully out-of-bounds run) may still mark its row, since repainting an
/// unchanged row is merely wasted work while missing a changed one paints a
/// stale cell.
#[derive(Debug, Clone)]
pub struct Grid {
    width: u16,
    height: u16,
    cells: Vec<Cell>,
    cursor_row: u16,
    cursor_col: u16,
    /// Set when a resize or clear invalidated every cell since the last
    /// [`Grid::take_dirty`]; supersedes `dirty_rows`.
    dirty_full: bool,
    /// Per-row changed flags accumulated since the last [`Grid::take_dirty`],
    /// one entry per grid row (kept `height`-long by [`Grid::resize`]).
    dirty_rows: Vec<bool>,
}

impl Grid {
    /// Create an empty (zero-sized) grid. Call [`GridOp::Resize`] via [`Grid::apply`]
    /// to give it dimensions before use.
    #[must_use]
    pub fn new() -> Self {
        Self {
            width: 0,
            height: 0,
            cells: Vec::new(),
            cursor_row: 0,
            cursor_col: 0,
            dirty_full: false,
            dirty_rows: Vec::new(),
        }
    }

    /// Drains the rows changed since the last call, resetting the tracker to
    /// clean. The repaint loop calls this once per frame to clip compositing
    /// to the damaged region; see [`GridDamage`].
    #[must_use]
    pub fn take_dirty(&mut self) -> GridDamage {
        let damage = if self.dirty_full {
            GridDamage {
                full: true,
                rows: Vec::new(),
            }
        } else {
            let mut rows = Vec::new();
            for (row, &dirty) in self.dirty_rows.iter().enumerate() {
                if dirty {
                    rows.push(u16::try_from(row).unwrap_or(u16::MAX));
                }
            }
            GridDamage { full: false, rows }
        };
        self.dirty_full = false;
        for row in &mut self.dirty_rows {
            *row = false;
        }
        damage
    }

    /// Marks grid-space `row` changed, ignoring an out-of-range index the
    /// same way the mutators themselves clamp rather than panic.
    fn mark_row(&mut self, row: u16) {
        if let Some(slot) = self.dirty_rows.get_mut(usize::from(row)) {
            *slot = true;
        }
    }

    /// Apply a single grid mutation. Out-of-bounds ops are ignored, never panic.
    pub fn apply(&mut self, op: GridOp) {
        match op {
            GridOp::Resize { width, height } => self.resize(width, height),
            GridOp::Clear => self.clear(),
            GridOp::CursorGoto { row, col } => self.cursor_goto(row, col),
            GridOp::PutLine {
                row,
                col_start,
                cells,
            } => self.put_line(row, col_start, &cells),
            GridOp::Scroll {
                top,
                bot,
                left,
                right,
                rows,
            } => self.scroll(top, bot, left, right, rows),
        }
    }

    /// Current `(width, height)` in cells.
    #[must_use]
    pub fn size(&self) -> (u16, u16) {
        (self.width, self.height)
    }

    /// Current cursor `(row, col)`, always within bounds of the current size.
    #[must_use]
    pub fn cursor(&self) -> (u16, u16) {
        (self.cursor_row, self.cursor_col)
    }

    /// The cell at `(row, col)`, or `None` if out of bounds.
    #[must_use]
    pub fn cell(&self, row: u16, col: u16) -> Option<&Cell> {
        let idx = self.index(row, col)?;
        self.cells.get(idx)
    }

    /// Concatenated text of every cell in `row`, left to right. Returns an empty
    /// string if `row` is out of bounds. Intended for debugging and tests.
    #[must_use]
    pub fn row_text(&self, row: u16) -> String {
        if row >= self.height {
            return String::new();
        }
        let mut out = String::with_capacity(self.width as usize);
        for col in 0..self.width {
            if let Some(cell) = self.cell(row, col) {
                out.push_str(&cell.text);
            }
        }
        out
    }

    fn index(&self, row: u16, col: u16) -> Option<usize> {
        if row >= self.height || col >= self.width {
            return None;
        }
        let width = usize::from(self.width);
        let row_off = usize::from(row).checked_mul(width)?;
        row_off.checked_add(usize::from(col))
    }

    fn resize(&mut self, width: u16, height: u16) {
        let mut new_cells = vec![Cell::default(); usize::from(width) * usize::from(height)];
        let copy_rows = self.height.min(height);
        let copy_cols = self.width.min(width);
        for row in 0..copy_rows {
            for col in 0..copy_cols {
                let Some(src) = self.index(row, col) else {
                    continue;
                };
                let dst_row_off = usize::from(row) * usize::from(width);
                let dst = dst_row_off + usize::from(col);
                if let (Some(cell), Some(slot)) = (self.cells.get(src), new_cells.get_mut(dst)) {
                    *slot = cell.clone();
                }
            }
        }
        self.width = width;
        self.height = height;
        self.cells = new_cells;
        self.cursor_row = self.cursor_row.min(self.height.saturating_sub(1));
        self.cursor_col = self.cursor_col.min(self.width.saturating_sub(1));
        // a resize moves the whole grid: every cell must repaint, and the
        // per-row mask is rebuilt to the new height so later marks land in
        // range
        self.dirty_full = true;
        self.dirty_rows = vec![false; usize::from(height)];
    }

    fn clear(&mut self) {
        for cell in &mut self.cells {
            *cell = Cell::default();
        }
        self.dirty_full = true;
    }

    fn cursor_goto(&mut self, row: u16, col: u16) {
        self.cursor_row = row.min(self.height.saturating_sub(1));
        self.cursor_col = col.min(self.width.saturating_sub(1));
    }

    fn put_line(&mut self, row: u16, col_start: u16, cells: &[(String, u64, u64)]) {
        if row >= self.height {
            return;
        }
        self.mark_row(row);
        let mut col = col_start;
        for (text, hl_id, repeat) in cells {
            for _ in 0..*repeat {
                if col >= self.width {
                    return;
                }
                if let Some(idx) = self.index(row, col) {
                    if let Some(slot) = self.cells.get_mut(idx) {
                        *slot = Cell {
                            text: text.clone(),
                            hl_id: *hl_id,
                        };
                    }
                }
                col = col.saturating_add(1);
            }
        }
    }

    fn scroll(&mut self, top: u16, bot: u16, left: u16, right: u16, rows: i32) {
        let top = top.min(self.height);
        let bot = bot.min(self.height);
        let left = left.min(self.width);
        let right = right.min(self.width);
        if top >= bot || left >= right || rows == 0 {
            return;
        }
        // the whole region repaints: scrolled-in rows carry moved content and
        // the vacated tail is filled, so every row in `top..bot` changed
        for row in top..bot {
            self.mark_row(row);
        }

        if rows > 0 {
            let shift = u16::try_from(rows).unwrap_or(u16::MAX);
            let mut dst = top;
            let mut src = top.saturating_add(shift);
            // when shift >= bot - top the loop body never runs and dst is
            // still top, so the fill below clears the whole region: the
            // degenerate case needs no separate branch here
            while src < bot {
                self.copy_row_range(src, dst, left, right);
                dst = dst.saturating_add(1);
                src = src.saturating_add(1);
            }
            self.fill_row_range(dst, bot, left, right);
        } else {
            let shift = u16::try_from(rows.unsigned_abs()).unwrap_or(u16::MAX);
            // unlike the upward path, the downward loop counts src down from
            // bot - shift and would underflow-saturate into copying wrong
            // rows for oversized shifts, so full-clear explicitly
            if shift >= bot.saturating_sub(top) {
                self.fill_row_range(top, bot, left, right);
                return;
            }
            let mut dst = bot;
            let mut src = bot.saturating_sub(shift);
            while src > top {
                dst = dst.saturating_sub(1);
                src = src.saturating_sub(1);
                self.copy_row_range(src, dst, left, right);
            }
            self.fill_row_range(top, top.saturating_add(shift), left, right);
        }
    }

    fn copy_row_range(&mut self, src_row: u16, dst_row: u16, left: u16, right: u16) {
        for col in left..right {
            let (Some(src_idx), Some(dst_idx)) =
                (self.index(src_row, col), self.index(dst_row, col))
            else {
                continue;
            };
            let value = self.cells.get(src_idx).cloned().unwrap_or_default();
            if let Some(slot) = self.cells.get_mut(dst_idx) {
                *slot = value;
            }
        }
    }

    fn fill_row_range(&mut self, row_start: u16, row_end: u16, left: u16, right: u16) {
        for row in row_start..row_end {
            for col in left..right {
                if let Some(idx) = self.index(row, col) {
                    if let Some(slot) = self.cells.get_mut(idx) {
                        *slot = Cell::default();
                    }
                }
            }
        }
    }
}

impl Default for Grid {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    fn grid_10x3() -> Grid {
        let mut g = Grid::new();
        g.apply(GridOp::Resize {
            width: 10,
            height: 3,
        });
        g
    }

    #[test]
    fn put_line_writes_cells_with_repeat() {
        let mut g = grid_10x3();
        g.apply(GridOp::PutLine {
            row: 0,
            col_start: 2,
            cells: vec![("h".into(), 1, 1), ("i".into(), 1, 1), (".".into(), 0, 3)],
        });
        assert_eq!(g.row_text(0), "  hi...   ");
    }

    #[test]
    fn scroll_up_moves_rows_and_clears_vacated() {
        let mut g = grid_10x3();
        for (i, s) in ["aaaa", "bbbb", "cccc"].iter().enumerate() {
            g.apply(GridOp::PutLine {
                row: i as u16,
                col_start: 0,
                cells: s.chars().map(|c| (c.to_string(), 0, 1)).collect(),
            });
        }
        // rows: 1 means content moves up by one row within the region
        g.apply(GridOp::Scroll {
            top: 0,
            bot: 3,
            left: 0,
            right: 10,
            rows: 1,
        });
        assert_eq!(g.row_text(0).trim_end(), "bbbb");
        assert_eq!(g.row_text(1).trim_end(), "cccc");
        assert_eq!(g.row_text(2).trim_end(), "");
    }

    #[test]
    fn resize_preserves_overlapping_content_and_clamps_cursor() {
        let mut g = grid_10x3();
        g.apply(GridOp::CursorGoto { row: 2, col: 9 });
        g.apply(GridOp::Resize {
            width: 5,
            height: 2,
        });
        assert_eq!(g.size(), (5, 2));
        assert_eq!(g.cursor(), (1, 4));
    }

    #[test]
    fn out_of_bounds_ops_are_ignored_not_panicking() {
        let mut g = grid_10x3();
        g.apply(GridOp::PutLine {
            row: 99,
            col_start: 0,
            cells: vec![("x".into(), 0, 1)],
        });
        g.apply(GridOp::CursorGoto { row: 99, col: 99 });
        assert_eq!(g.cursor(), (2, 9)); // clamped to bounds
    }
}
