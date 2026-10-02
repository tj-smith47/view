//! Terminal grid model: the cell buffer nvim's `ext_linegrid` events paint into.
//!
//! [`Grid`] holds no I/O and no RPC awareness; it is a pure sink for
//! [`GridOp`] values that a higher layer decodes from nvim redraw events.
//! [`registry`] addresses many of them by the id nvim gives each one.

pub mod registry;

/// A single grid cell: display text and the highlight group it was painted with.
#[derive(Debug, PartialEq, Eq)]
pub struct Cell {
    /// The text to display, generally a single grapheme.
    pub text: String,
    /// The highlight group id this cell was painted with.
    pub hl_id: u64,
}

impl Clone for Cell {
    fn clone(&self) -> Self {
        Self {
            text: self.text.clone(),
            hl_id: self.hl_id,
        }
    }

    // a derived Clone replaces the String, so copying a grid over another
    // of its size would allocate once per cell
    fn clone_from(&mut self, source: &Self) {
        self.text.clone_from(&source.text);
        self.hl_id = source.hl_id;
    }
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
    /// Bumped by every method that writes cells: a reader that keeps a
    /// copy of something drawn from these cells compares it to tell
    /// whether the copy is stale.
    revision: u64,
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
            revision: 0,
        }
    }

    /// How many mutations this grid has taken, wrapping.
    #[must_use]
    pub fn revision(&self) -> u64 {
        self.revision
    }

    /// Drains the rows changed since the last call, resetting the tracker to
    /// clean; see [`GridDamage`].
    ///
    /// Crate-private on purpose: the grid is one of two paint inputs, and a
    /// repaint clipped to this alone strands the styles the other one owns.
    /// [`crate::model::Model::take_paint_damage`] is the sanctioned drain,
    /// and folds both.
    #[must_use]
    pub(crate) fn take_dirty(&mut self) -> GridDamage {
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
        self.dirty_rows.fill(false);
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
        let wrote = match op {
            GridOp::Resize { width, height } => self.resize_cells(width, height),
            GridOp::Clear => {
                self.clear_cells();
                true
            }
            GridOp::CursorGoto { row, col } => {
                self.cursor_goto(row, col);
                false
            }
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
            } => self.scroll_region(top, bot, left, right, rows),
        };
        if wrote {
            self.revision = self.revision.wrapping_add(1);
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

    /// Whether any cell holds something other than blank space.
    ///
    /// A grid nvim has sized but not yet drawn into reads `false`: every
    /// cell is [`Cell::default`]'s single space until a `grid_line` lands.
    /// The end-of-buffer `~` fillers count, so a window on an empty file
    /// reads `true` the moment nvim draws it: that draw is the file
    /// appearing, and a start that waited for a glyph the empty file has
    /// none of would never arrive.
    /// Stops at the first non-blank cell, so a grid that has text answers
    /// in the cells before that one and an empty one costs its whole
    /// buffer -- read only while a startup is still waiting for its first
    /// text (see `GridRegistry::window_text_painted`), never per frame.
    #[must_use]
    pub fn has_text(&self) -> bool {
        self.cells.iter().any(|cell| !cell.text.trim().is_empty())
    }

    /// Makes this grid a copy of `source` in its own buffers, and answers
    /// whether anything was copied. Size, cursor and cells are compared
    /// first, so a source that matches already costs that comparison alone.
    pub(crate) fn follow(&mut self, source: &Self) -> bool {
        if self.size() == source.size()
            && self.cursor() == source.cursor()
            && self.cells == source.cells
        {
            return false;
        }
        self.revision = self.revision.wrapping_add(1);
        self.width = source.width;
        self.height = source.height;
        self.cursor_row = source.cursor_row;
        self.cursor_col = source.cursor_col;
        self.cells.clone_from(&source.cells);
        self.dirty_full = source.dirty_full;
        self.dirty_rows.clone_from(&source.dirty_rows);
        true
    }

    /// Makes this grid `size` wide and tall with its cursor at `cursor` and
    /// every cell blank, in its own buffers, and answers whether anything
    /// changed. A cell is blank only with a space or no text in highlight
    /// 0, so whitespace drawn in a colour counts as a change. A grid already
    /// `size` is blanked cell by cell in place, keeping each cell's text
    /// buffer, and a cell with empty text, the right half of a wide
    /// character, keeps it empty, which paints as a space.
    pub(crate) fn blank_to(&mut self, size: (u16, u16), cursor: (u16, u16)) -> bool {
        if self.size() != size {
            self.revision = self.revision.wrapping_add(1);
            (self.width, self.height) = size;
            self.cells.clear();
            self.cells
                .resize(usize::from(size.0) * usize::from(size.1), Cell::default());
            self.dirty_rows.clear();
            self.dirty_rows.resize(usize::from(size.1), false);
            self.cursor_goto(cursor.0, cursor.1);
            self.dirty_full = true;
            return true;
        }
        let mut changed = self.cursor() != cursor;
        self.cursor_goto(cursor.0, cursor.1);
        for cell in &mut self.cells {
            // an empty String has no buffer, and pushing into it allocates
            let blank = cell.text.is_empty() || cell.text == " ";
            if !blank || cell.hl_id != 0 {
                if !blank {
                    cell.text.clear();
                    cell.text.push(' ');
                }
                cell.hl_id = 0;
                changed = true;
            }
        }
        if changed {
            self.revision = self.revision.wrapping_add(1);
            self.dirty_rows.fill(false);
            self.dirty_full = true;
        }
        changed
    }

    /// Where the cell buffer lives, so a test can tell a grid copied into
    /// in place from one replaced by a fresh copy.
    #[cfg(test)]
    pub(crate) fn buffer(&self) -> *const Cell {
        self.cells.as_ptr()
    }

    /// Renumbers every cell's highlight id through `to`.
    pub(crate) fn map_hl(&mut self, to: impl Fn(u64) -> u64) {
        self.revision = self.revision.wrapping_add(1);
        for cell in &mut self.cells {
            cell.hl_id = to(cell.hl_id);
        }
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

    /// Resizes the grid, keeping the cells both sizes share, and answers
    /// whether the size changed.
    fn resize_cells(&mut self, width: u16, height: u16) -> bool {
        let resized = self.size() != (width, height);
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
        resized
    }

    fn clear_cells(&mut self) {
        for cell in &mut self.cells {
            *cell = Cell::default();
        }
        self.dirty_full = true;
    }

    fn cursor_goto(&mut self, row: u16, col: u16) {
        self.cursor_row = row.min(self.height.saturating_sub(1));
        self.cursor_col = col.min(self.width.saturating_sub(1));
    }

    /// Writes `cells` into `row` from `col_start`, and answers whether any
    /// cell landed inside the grid.
    fn put_line(&mut self, row: u16, col_start: u16, cells: &[(String, u64, u64)]) -> bool {
        if row >= self.height {
            return false;
        }
        self.mark_row(row);
        let mut wrote = false;
        let mut col = col_start;
        for (text, hl_id, repeat) in cells {
            for _ in 0..*repeat {
                if col >= self.width {
                    return wrote;
                }
                if let Some(idx) = self.index(row, col) {
                    if let Some(slot) = self.cells.get_mut(idx) {
                        *slot = Cell {
                            text: text.clone(),
                            hl_id: *hl_id,
                        };
                        wrote = true;
                    }
                }
                col = col.saturating_add(1);
            }
        }
        wrote
    }

    /// Scrolls the region by `rows`, and answers whether the region holds
    /// any cell to move.
    fn scroll_region(&mut self, top: u16, bot: u16, left: u16, right: u16, rows: i32) -> bool {
        let top = top.min(self.height);
        let bot = bot.min(self.height);
        let left = left.min(self.width);
        let right = right.min(self.width);
        if top >= bot || left >= right || rows == 0 {
            return false;
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
                return true;
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
        true
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

    /// Blanking a grid already the right size keeps its cell buffer and
    /// every cell's text buffer.
    ///
    /// Disconfirm: rebuilding the cells with `Cell::default()` moves the
    /// text buffers.
    #[test]
    fn blanking_a_grid_of_the_same_size_allocates_no_cell() {
        let mut g = grid_10x3();
        g.apply(GridOp::PutLine {
            row: 0,
            col_start: 0,
            cells: vec![("x".into(), 4, 9), (String::new(), 4, 1)],
        });
        let texts = |g: &Grid| g.cells.iter().map(|c| c.text.as_ptr()).collect::<Vec<_>>();
        let (buffer, before) = (g.buffer(), texts(&g));
        let revision = g.revision();

        assert!(g.blank_to((10, 3), (1, 2)));
        assert_ne!(
            g.revision(),
            revision,
            "the blanked cells kept the revision"
        );
        assert_eq!((g.buffer(), texts(&g)), (buffer, before));
        assert_eq!(
            g.cell(0, 9).map(|c| (c.text.capacity(), c.hl_id)),
            Some((0, 0))
        );
        assert!(!g.has_text());
        assert_eq!(g.cell(0, 0).map(|c| c.hl_id), Some(0));
        assert_eq!(g.cursor(), (1, 2));
        let revision = g.revision();
        assert!(!g.blank_to((10, 3), (1, 2)));
        assert_eq!(g.revision(), revision, "nothing changed");
    }

    /// An op that changes no cell leaves the revision alone, so a copy of
    /// the cells is kept across it.
    #[test]
    fn an_op_that_changes_no_cell_keeps_the_revision() {
        let mut g = grid_10x3();
        let line = |row, col_start| GridOp::PutLine {
            row,
            col_start,
            cells: vec![("x".into(), 1, 1)],
        };
        let scroll = |top, bot, rows| GridOp::Scroll {
            top,
            bot,
            left: 0,
            right: 10,
            rows,
        };
        let revision = g.revision();
        for op in [
            GridOp::Resize {
                width: 10,
                height: 3,
            },
            line(3, 0),
            line(0, 10),
            scroll(0, 3, 0),
            scroll(2, 2, 1),
            GridOp::CursorGoto { row: 1, col: 1 },
        ] {
            g.apply(op.clone());
            assert_eq!(g.revision(), revision, "{op:?} moved the revision");
        }
        for op in [
            line(0, 0),
            scroll(0, 3, 1),
            GridOp::Clear,
            GridOp::Resize {
                width: 9,
                height: 3,
            },
        ] {
            let revision = g.revision();
            g.apply(op.clone());
            assert_ne!(g.revision(), revision, "{op:?} kept the revision");
        }
    }

    /// Disconfirm: a derived `Clone` for `Cell` moves every text pointer.
    #[test]
    fn following_a_grid_of_the_same_size_keeps_each_cells_text_buffer() {
        let mut g = grid_10x3();
        let mut source = grid_10x3();
        source.apply(GridOp::PutLine {
            row: 1,
            col_start: 0,
            cells: vec![("y".into(), 2, 10)],
        });
        let texts = |g: &Grid| g.cells.iter().map(|c| c.text.as_ptr()).collect::<Vec<_>>();
        let (buffer, before) = (g.buffer(), texts(&g));
        let revision = g.revision();

        assert!(g.follow(&source));
        assert_ne!(g.revision(), revision, "the copied cells kept the revision");
        assert_eq!((g.buffer(), texts(&g)), (buffer, before));
        assert_eq!(g.row_text(1), "yyyyyyyyyy");
        assert_eq!(g.cell(1, 0).map(|c| c.hl_id), Some(2));
    }

    /// The source of every file under `grid/`, blanked, with its test
    /// modules taken out, by path.
    fn grid_module_sources() -> Vec<(String, String)> {
        use view_test_support::rust_source::{blank_non_code, without_test_modules};
        fn walk(dir: &std::path::Path, into: &mut Vec<(String, String)>) {
            for entry in std::fs::read_dir(dir).expect("a readable source directory") {
                let path = entry.expect("a readable directory entry").path();
                if path.is_dir() {
                    walk(&path, into);
                } else if path.extension().is_some_and(|ext| ext == "rs") {
                    let source = std::fs::read_to_string(&path).expect("a readable source");
                    let code = blank_non_code(&without_test_modules(&source));
                    into.push((path.display().to_string(), code));
                }
            }
        }
        let mut sources = Vec::new();
        walk(
            &std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/grid"),
            &mut sources,
        );
        sources
    }

    /// Every `&mut self` method of `Grid` that writes cells moves the
    /// revision, so a copy of something drawn from the cells is never
    /// reused past a change. A method that writes no cell names its ground
    /// here. A private method is reached only through the methods walked
    /// here, and no other file under `grid/` calls it, which the walk
    /// checks.
    #[test]
    fn every_method_that_writes_cells_moves_the_revision() {
        use view_test_support::rust_source::{blank_non_code, closing, without_test_modules};
        const WRITES_NO_CELL: &[(&str, &str)] = &[
            (
                "take_dirty",
                "drains the damage record and leaves every cell",
            ),
            ("mark_row", "marks a row for repaint and leaves every cell"),
        ];
        let code = blank_non_code(&without_test_modules(include_str!("grid.rs")));
        let open = code
            .find("impl Grid {")
            .expect("Grid has its own impl block")
            + "impl Grid ".len();
        let close = closing(&code, open).expect("the impl block closes");
        let block = &code[open..close];
        let elsewhere = grid_module_sources();
        let mut walked = Vec::new();
        let mut missing = Vec::new();
        let mut at = 0;
        while let Some(offset) = block[at..].find("fn ") {
            let fn_at = at + offset;
            at = fn_at + 3;
            let head = block[..fn_at].trim_end_matches(|c: char| c != '\n');
            let declaration = block[head.len()..fn_at].trim();
            if !matches!(declaration, "" | "pub" | "pub(crate)" | "pub(super)") {
                continue;
            }
            let name = block[at..]
                .split(['(', '<'])
                .next()
                .unwrap_or_default()
                .trim();
            let body_open = fn_at + block[fn_at..].find('{').expect("a method has a body");
            if !block[fn_at..body_open].contains("&mut self") {
                continue;
            }
            let body_close = closing(block, body_open).expect("the body closes");
            walked.push(name);
            if WRITES_NO_CELL.iter().any(|(exempt, _)| *exempt == name)
                || block[body_open..body_close].contains("self.revision")
            {
                continue;
            }
            let callers: Vec<&str> = elsewhere
                .iter()
                .filter(|(_, code)| {
                    code.contains(&format!(".{name}(")) || code.contains(&format!("::{name}("))
                })
                .map(|(path, _)| path.as_str())
                .collect();
            if !declaration.is_empty() {
                missing.push(format!("{name}, which other modules call"));
            } else if !callers.is_empty() {
                missing.push(format!("{name}, private and called from {callers:?}"));
            }
        }
        for expected in [
            "apply",
            "follow",
            "blank_to",
            "map_hl",
            "take_dirty",
            "mark_row",
            "put_line",
            "scroll_region",
        ] {
            assert!(
                walked.contains(&expected),
                "the walk missed {expected}: {walked:?}"
            );
        }
        assert!(
            missing.is_empty(),
            "these write cells and leave the revision where it was: {missing:?}"
        );
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
