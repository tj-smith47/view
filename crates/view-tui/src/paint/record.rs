//! Moves frames between the shadow and the session DVR's ring: the frame
//! just committed goes in as the cells it changed, and a recorded frame
//! comes back as the shadow's next frame to emit.

use std::ops::Range;

use ratatui::buffer::Cell;
use ratatui::layout::Rect;
use view_core::model::Model;
use view_core::theme::{ChromeGroup, Theme};

use super::{paint_text_row, ratatui_style, Damage, Shadow};
use crate::dvr::{place, row_hash, Capture, FrameRing, Group, Scroll};

/// Records the frame [`Shadow::commit`] just promoted into `into` and
/// returns its seq, or `None` when the ring could not hold it.
///
/// `delta` says `back` holds the newest recorded frame, so the frame can be
/// stored as the cells that differ from it. The rows the frame repainted
/// are the only rows where `front` and `back` can differ, so a delta
/// compares those rows alone and clones a cell only where it changed. A
/// frame that shows `back` shifted is stored as the shift and the cells it
/// does not explain. A frame with no recorded base, a new size, or more
/// changed cells than the open group has room for is a keyframe.
///
/// Scroll detection runs only when more than one row's worth of cells
/// changed. It reads the screen's cells about six times and compares at
/// most height squared row hashes.
pub(crate) fn capture(
    shadow: &Shadow,
    into: &mut FrameRing,
    at_us: u64,
    cursor: Option<(u16, u16)>,
    delta: bool,
) -> Option<u64> {
    let area = shadow.front.area;
    let size = (area.width, area.height);
    let mut last = Capture::default();
    if delta && into.open_delta(size).is_some() {
        let scroll = scroll_of(shadow, &mut into.scratch, &mut last);
        if let Some(group) = into.open_delta(size) {
            if let Some(pushed) = fill(shadow, group, scroll) {
                last.pushed = pushed;
                last.scroll = scroll;
                last.seq = into.close_delta(at_us, cursor, scroll);
                into.last = last;
                return last.seq;
            }
            group.abort();
        }
    }
    last.key = true;
    last.seq = into.push_key(at_us, size, cursor, shadow.front.content.iter().cloned());
    last.pushed = last.seq.map_or(0, |_| shadow.front.content.len());
    into.last = last;
    last.seq
}

/// Row `y`'s columns `cols` of a row-major `content` `width` cells wide.
fn row(content: &[Cell], width: usize, y: usize, cols: Range<usize>) -> Option<&[Cell]> {
    content.get(y * width + cols.start..y * width + cols.end)
}

/// Pushes the cells of `front` that differ from what `scroll` leaves of
/// `back` into `group`: a cell the shift fills is compared with the cell it
/// is filled from, every other cell with the cell it replaces. Returns the
/// cells pushed, or `None` when the group has no room for them.
///
/// A row the shift fills is read whether or not it was painted, since the
/// shift moves its cells either way.
fn fill(shadow: &Shadow, group: &mut Group, scroll: Option<Scroll>) -> Option<usize> {
    let width = usize::from(shadow.front.area.width);
    let (front, back) = (&shadow.front.content, &shadow.back.content);
    let mut pushed = 0;
    for y in 0..shadow.front.area.height {
        let shifted = scroll.filter(|s| (s.top..s.bottom).contains(&y));
        if !shadow.painted.covers(y) && shifted.is_none() {
            continue;
        }
        let at = usize::from(y);
        let (Some(now), Some(was)) = (
            row(front, width, at, 0..width),
            row(back, width, at, 0..width),
        ) else {
            continue;
        };
        let source = shifted
            .and_then(|s| s.source(y))
            .and_then(|from| row(back, width, usize::from(from), 0..width));
        for (x, (now, was)) in (0..).zip(now.iter().zip(was)) {
            let base = match (shifted, source) {
                (Some(s), Some(source)) if s.covers(x, y) => {
                    source.get(usize::from(x)).unwrap_or(was)
                }
                _ => was,
            };
            if now != base {
                if !group.push_cell(x, y, now) {
                    return None;
                }
                pushed += 1;
            }
        }
    }
    Some(pushed)
}

/// The shift of `back` that the frame in `front` shows: the columns where
/// most changed rows differ are the span that moved, rows whose span
/// equals another row of `back` vote for the distance between them, and
/// the shift covers every row from the first the winning distance explains
/// cell for cell and that changed in place to the last. Each edge of the
/// span then grows outward over the columns where every explained row
/// agrees with its source, to the furthest of them that changed, and a row
/// past either end of the shift joins it while fewer of its cells differ
/// from their source than from the cells they replace.
/// `None` when a row's worth of cells or fewer changed, or fewer than two
/// rows are explained. `last` takes the counts.
///
/// A row inside the shift that the distance does not explain, the cursor
/// line or a guide that changed beside a block of rows, keeps the shift
/// and [`fill`] stores the cells of it that differ from their source. A
/// gutter of relative line numbers keeps its digits while the text beside
/// it moves, so the majority leaves it outside the span and [`fill`] stores
/// it as plain cells. The pass that counts the changed cells is one more
/// read of the painted rows than [`fill`] makes on its own.
// ponytail: one column span for the whole frame, so a pane that scrolls
// beside another one that scrolls by a different distance stores the second
// as cells. Per-pane spans are the upgrade when that shows in a retention
// measurement.
fn scroll_of(shadow: &Shadow, scratch: &mut Vec<u64>, last: &mut Capture) -> Option<Scroll> {
    let (front, back) = (&shadow.front.content, &shadow.back.content);
    let w = usize::from(shadow.front.area.width);
    let h = usize::from(shadow.front.area.height);
    let (mut changed, mut rows) = (0, 0u64);
    scratch.clear();
    scratch.resize(w, 0);
    for y in 0..h {
        if !u16::try_from(y).is_ok_and(|y| shadow.painted.covers(y)) {
            continue;
        }
        let (Some(now), Some(was)) = (row(front, w, y, 0..w), row(back, w, y, 0..w)) else {
            continue;
        };
        let before = changed;
        for (x, _) in now.iter().zip(was).enumerate().filter(|(_, (a, b))| a != b) {
            changed += 1;
            if let Some(count) = scratch.get_mut(x) {
                *count += 1;
            }
        }
        rows += u64::from(changed > before);
    }
    last.changed = changed;
    if changed <= w {
        return None;
    }
    let moved = |count: &u64| 2 * *count > rows;
    let mut left = scratch.iter().position(moved)?;
    let mut right = scratch.iter().rposition(moved)? + 1;
    for content in [front, back] {
        for y in 0..h {
            scratch.push(row(content, w, y, left..right).map_or(0, row_hash));
        }
    }
    scratch.resize(w + 4 * h, 0);
    let (counts, rest) = scratch.split_at_mut(w);
    let (now, rest) = rest.split_at_mut(h);
    let (was, votes) = rest.split_at_mut(h);
    for (y, hash) in now.iter().enumerate() {
        if was.get(y) == Some(hash) {
            continue;
        }
        for (from, _) in was.iter().enumerate().filter(|&(_, other)| other == hash) {
            if let Some(count) = votes.get_mut(h + from - y) {
                *count += 1;
            }
        }
    }
    let (at, &count) = votes
        .iter()
        .enumerate()
        .filter(|&(at, _)| at != h)
        .max_by_key(|&(at, count)| (*count, std::cmp::Reverse(at.abs_diff(h))))?;
    if count < 2 {
        return None;
    }
    let by = isize::try_from(at).ok()? - isize::try_from(h).ok()?;
    let (moved_left, moved_right) = (left, right);
    let explained = |y: usize| {
        y.checked_add_signed(by)
            .filter(|&f| f < h)
            .is_some_and(|from| {
                let cols = moved_left..moved_right;
                now.get(y) == was.get(from)
                    && row(front, w, y, cols.clone()) == row(back, w, from, cols)
            })
    };
    // the votes are spent, so their first `h` slots mark the explained rows.
    // A row that stood still is explained whenever it equals its source, as
    // `~` filler under a window does, so only a row that moved sets an end:
    // a still row the shift leaves out costs nothing
    let mut hull = None::<Range<usize>>;
    for y in 0..h {
        let hit = explained(y);
        if let Some(mark) = votes.get_mut(y) {
            *mark = u64::from(hit);
        }
        last.explained += usize::from(hit);
        if hit && now.get(y) != was.get(y) {
            hull = Some(hull.map_or(y..y + 1, |r| r.start..y + 1));
        }
    }
    let mut hull = hull.filter(|_| last.explained >= 2)?;
    let source = |y: usize, x: usize| {
        let from = y.checked_add_signed(by).filter(|&f| f < h)?;
        Some((
            front.get(y * w + x)?,
            back.get(from * w + x)?,
            back.get(y * w + x)?,
        ))
    };
    // an edge reaches over the columns where every explained row shows its
    // source's cell, as far as the last of them that changed, so it stops
    // where the shift stops being true and leaves out columns that stood still
    let agrees = |x: usize| {
        (0..h)
            .filter(|&y| votes.get(y) == Some(&1))
            .all(|y| source(y, x).is_some_and(|(now, from, _)| now == from))
    };
    let touched = |x: usize| counts.get(x).is_some_and(|&c| c > 0);
    let mut x = left;
    while x > 0 && agrees(x - 1) {
        x -= 1;
        if touched(x) {
            left = x;
        }
    }
    let mut x = right;
    while x < w && agrees(x) {
        x += 1;
        if touched(x - 1) {
            right = x;
        }
    }
    // the cells a row saves when the shift covers it: those differing from
    // the cells they replace less those differing from their source. The
    // shift reaches as far past either end as the rows' savings add up to
    // the most, so a cursor line or a guide that changed beside rows that
    // still moved costs its own cells and no more
    let saves = |y: usize| {
        let mut saved = 0isize;
        for x in left..right {
            let (now, from, was) = source(y, x)?;
            saved += isize::from(now != was) - isize::from(now != from);
        }
        Some(saved)
    };
    let reach = |rows: &mut dyn Iterator<Item = usize>| {
        let (mut sum, mut best, mut at) = (0, 0, None);
        for y in rows {
            let Some(saved) = saves(y) else { break };
            sum += saved;
            if sum > best {
                (best, at) = (sum, Some(y));
            }
        }
        at
    };
    if let Some(top) = reach(&mut (0..hull.start).rev()) {
        hull.start = top;
    }
    if let Some(bottom) = reach(&mut (hull.end..h)) {
        hull.end = bottom + 1;
    }
    last.span = Some((u16::try_from(left).ok()?, u16::try_from(right).ok()?));
    let range = |r: Range<usize>| Some(u16::try_from(r.start).ok()?..u16::try_from(r.end).ok()?);
    Some(Scroll::new(
        range(hull)?,
        range(left..right)?,
        i16::try_from(by).ok()?,
    ))
}

/// Writes frame `seq` of `ring` into the shadow's `back` and marks every
/// row painted, so the next emission repaints the whole screen from it. A
/// cell outside the current screen is dropped. Returns false, leaving the
/// shadow untouched, when the ring no longer holds the frame.
pub(crate) fn load(shadow: &mut Shadow, ring: &FrameRing, seq: u64) -> bool {
    let Some((group, index)) = ring.locate(seq) else {
        return false;
    };
    let mut screen = Vec::new();
    group.replay(index, &mut screen);
    let back = &mut shadow.back;
    back.reset();
    let area = (back.area.width, back.area.height);
    let width = usize::from(group.area.0.max(1));
    for (i, cell) in screen.iter().enumerate() {
        if let (Ok(x), Ok(y)) = (u16::try_from(i % width), u16::try_from(i / width)) {
            place(&mut back.content, area, x, y, cell);
        }
    }
    shadow.painted = Damage::full();
    true
}

/// Paints `text` across one row of the shadow's `back` in the status line's
/// colors: the last row when the recorded frame left it blank, the top row
/// when it holds text, since that is where a message was printed.
pub(crate) fn paint_bar(shadow: &mut Shadow, model: &Model, text: &str) {
    let area = shadow.back.area;
    let Some(last) = area.bottom().checked_sub(1).filter(|_| area.width > 0) else {
        return;
    };
    let blank = (area.x..area.right()).all(|x| {
        shadow
            .back
            .cell((x, last))
            .is_none_or(|c| c.symbol().trim().is_empty())
    });
    let y = if blank { last } else { area.y };
    let style =
        ratatui_style(Theme::from_hl(model.engine.painted_hl()).chrome(ChromeGroup::StatusLine));
    let row = Rect::new(area.x, y, area.width, 1);
    for x in area.x..area.right() {
        if let Some(cell) = shadow.back.cell_mut((x, y)) {
            *cell = Cell::EMPTY;
        }
    }
    shadow.back.set_style(row, style);
    paint_text_row(text, style, row, 0, &mut shadow.back);
}

/// A screen shaped like a working session for the recording tests: a
/// tabline, a file tree beside the editor, numbered code lines, a cursor
/// line, a status line and an empty command line.
#[cfg(test)]
mod fixture {
    use ratatui::buffer::Buffer;
    use ratatui::style::{Color, Style};

    /// The width of the tree column for a screen `width` wide.
    pub(super) fn tree_width(width: u16) -> u16 {
        width * 3 / 10
    }

    /// Lines of the kind a source file holds, picked so neighbours differ
    /// the way neighbouring lines of real code do.
    const LINES: [&str; 12] = [
        "fn resolve(&self, from: u64, step: ScrubStep) -> u64 {",
        "    let (Some(oldest), Some(newest)) = (self.oldest(), self.newest()) else {",
        "        return from;",
        "    };",
        "    match step {",
        "        ScrubStep::Oldest => oldest,",
        "        // a frame painted before the oldest kept is gone",
        "            .clamp(oldest, newest),",
        "    }",
        "}",
        "",
        "impl Drop for Guard { fn drop(&mut self) { restore(); } }",
    ];

    /// File line `line` of the fixture's source.
    fn source_line(line: u64) -> &'static str {
        let at = usize::try_from(line.wrapping_mul(2_654_435_761) >> 7).unwrap_or(0);
        LINES.get(at % LINES.len()).copied().unwrap_or("")
    }

    /// Draws the screen of size `area` with the editor scrolled to file
    /// line `top` and `typed` characters typed into its sixth row.
    pub(super) fn draw(buf: &mut Buffer, area: (u16, u16), top: u64, typed: u16) {
        let (w, h) = area;
        let tree = tree_width(w);
        let text = Style::default().fg(Color::Rgb(248, 248, 242));
        let dim = Style::default().fg(Color::Rgb(98, 114, 164));
        let chrome = Style::default()
            .fg(Color::Rgb(248, 248, 242))
            .bg(Color::Rgb(33, 34, 44));
        let cursorline = text.bg(Color::Rgb(68, 71, 90));
        let width = usize::from(w);
        let tabs = format!("{:<width$}", "  a.rs   b.rs   c.toml");
        buf.set_stringn(0, 0, tabs, width, chrome);
        for y in 1..h.saturating_sub(2) {
            let name = format!("{:<1$}", format!("  src/module_{y}.rs"), usize::from(tree));
            buf.set_stringn(0, y, name, usize::from(tree), dim);
            buf.set_stringn(tree, y, "│", 1, dim);
            let line = top + u64::from(y);
            let style = if y + 3 == h { cursorline } else { text };
            let code = format!("{line:>4} {:<1$}", source_line(line), width);
            buf.set_stringn(tree + 1, y, code, usize::from(w - tree - 1), style);
            if y == 6 && typed > 0 {
                let at = tree + 6 + (typed % (w - tree - 7));
                buf.set_stringn(at, y, "x", 1, text);
            }
        }
        let status = format!(
            " NORMAL  a.rs{:>1$}",
            format!("{}:{} ", top + 1, typed + 1),
            width - 13
        );
        buf.set_stringn(0, h - 2, status, width, chrome);
        buf.set_stringn(0, h - 1, " ".repeat(width), width, Style::default());
    }

    /// Redraws the gutter of a screen [`draw`] drew at file line `top` the
    /// way `relativenumber` does: the cursor stays on the cursor line's
    /// row, which shows its file line, every other row shows its distance
    /// from it, and a git sign marks file line 20 wherever it is on screen.
    pub(super) fn relative(buf: &mut Buffer, area: (u16, u16), top: u64) {
        let (w, h) = area;
        let tree = tree_width(w);
        let dim = Style::default().fg(Color::Rgb(98, 114, 164));
        let cursor = h - 3;
        for y in 1..h.saturating_sub(2) {
            let line = top + u64::from(y);
            let number = if y == cursor {
                line
            } else {
                u64::from(y.abs_diff(cursor))
            };
            let sign = if line == 20 { "▎" } else { " " };
            let style = if y == cursor {
                dim.bg(Color::Rgb(68, 71, 90))
            } else {
                dim
            };
            buf.set_stringn(tree + 1, y, format!("{sign}{number:>3} "), 5, style);
        }
    }

    /// The text a wrapped file line shows on its first row and its second.
    const WRAPPED: [&str; 2] = [
        "    let reach = self.ring.oldest().and_then(|s| self.ring.age(s)).unwrap_or_default();",
        "        .as_secs_f64();",
    ];

    /// Redraws the editor of a screen [`draw`] drew at file line `top` the
    /// way a working config shows it: every seventh file line wraps onto a
    /// second row, the cursor line sits eight rows above the last text row,
    /// and an indent guide turns solid beside the six-line block holding
    /// the cursor line.
    pub(super) fn real(buf: &mut Buffer, area: (u16, u16), top: u64) {
        let (w, h) = area;
        let tree = tree_width(w);
        let text = Style::default()
            .fg(Color::Rgb(248, 248, 242))
            .bg(Color::Reset);
        let dim = Style::default().fg(Color::Rgb(98, 114, 164));
        let cursor = usize::from(h - 3 - 8);
        let width = usize::from(w - tree - 1);
        // each text row's file line, and whether it is a wrapped line's
        // second row
        let mut rows = Vec::new();
        let mut line = top + 1;
        while rows.len() < usize::from(h - 3) {
            rows.push((line, false));
            if line.is_multiple_of(7) {
                rows.push((line, true));
            }
            line += 1;
        }
        let block = rows.get(cursor - 1).map_or(0, |&(line, _)| line / 6);
        for (y, &(line, second)) in (1u16..h - 2).zip(&rows) {
            let row = if usize::from(y) == cursor {
                text.bg(Color::Rgb(68, 71, 90))
            } else {
                text
            };
            let code = match (line.is_multiple_of(7), second) {
                (true, false) => format!("{line:>4} {}", WRAPPED[0]),
                (true, true) => format!("     {}", WRAPPED[1]),
                _ => format!("{line:>4} {}", source_line(line)),
            };
            buf.set_stringn(tree + 1, y, format!("{code:<width$}"), width, row);
            let guide = if line / 6 == block { "│" } else { "┊" };
            buf.set_stringn(tree + 10, y, guide, 1, row.patch(dim));
        }
    }

    /// The first row of the lower window [`split`] draws.
    pub(super) fn lower_window(height: u16) -> u16 {
        height / 2 + 1
    }

    /// Splits the editor of a screen [`draw`] drew into two windows: the
    /// upper one keeps the scrolled text, a status line of its own closes
    /// it, and the lower one holds a short file that stands still, nine
    /// lines of text and `~` on every row past them.
    pub(super) fn split(buf: &mut Buffer, area: (u16, u16), _: u64) {
        let (w, h) = area;
        let tree = tree_width(w);
        let width = usize::from(w - tree - 1);
        let text = Style::default().fg(Color::Rgb(248, 248, 242));
        let dim = Style::default().fg(Color::Rgb(98, 114, 164));
        let chrome = Style::default()
            .fg(Color::Rgb(248, 248, 242))
            .bg(Color::Rgb(33, 34, 44));
        let lower = lower_window(h);
        let status = format!("{:<width$}", " b.rs  [+]");
        buf.set_stringn(tree + 1, lower - 1, status, width, chrome);
        for (y, n) in (lower..h - 2).zip(0u64..) {
            let (row, style) = match LINES.get(usize::try_from(n).unwrap_or(0)) {
                Some(line) if n < 9 => (format!("{:>4} {line}", n + 1), text),
                _ => ("~".to_string(), dim),
            };
            buf.set_stringn(tree + 1, y, format!("{row:<width$}"), width, style);
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use ratatui::buffer::Buffer;
    use ratatui::style::{Color, Modifier, Style};
    use view_core::grid::GridOp;

    use super::*;

    const W: u16 = 20;
    const H: u16 = 6;

    fn model() -> Model {
        let mut model = Model::with_term_size(W, H);
        model.engine.apply_grid(GridOp::Resize {
            width: W,
            height: H,
        });
        model
    }

    fn put(model: &mut Model, row: u16, text: &str) {
        model.engine.apply_grid(GridOp::PutLine {
            row,
            col_start: 0,
            cells: vec![(text.into(), 0, 1)],
        });
    }

    /// Composes, emits and commits one frame the way the terminal does.
    fn paint(shadow: &mut Shadow, model: &Model, damage: &Damage) {
        let area = Rect::new(0, 0, W, H);
        shadow.resize(area);
        let surface = view_surface::render(model);
        let mut rows = shadow.overlay_damage(&surface);
        shadow.native_pane_damage(model, &surface, area, &mut rows);
        shadow.compose(model, &surface, damage);
        shadow.emit_updates(&mut Vec::new()).unwrap();
        shadow.commit();
    }

    fn rows(rows: &[u16]) -> Damage {
        Damage {
            full: false,
            rows: rows.to_vec(),
        }
    }

    #[test]
    fn a_frame_reconstructs_cell_for_cell_from_its_keyframe_and_deltas() {
        let mut model = model();
        let mut shadow = Shadow::new();
        let mut ring = FrameRing::new(64 << 20);
        put(&mut model, 0, "a");
        paint(&mut shadow, &model, &Damage::full());
        assert_eq!(capture(&shadow, &mut ring, 0, None, false), Some(1));
        // a frame repaints the rows the frame before it did as well, so the
        // frame after a full one is a whole-screen repaint stored as a delta
        put(&mut model, 2, "z");
        paint(&mut shadow, &model, &rows(&[2]));
        assert!(shadow.painted.full);
        assert_eq!(capture(&shadow, &mut ring, 5, None, true), Some(2));
        put(&mut model, 1, "b");
        paint(&mut shadow, &model, &rows(&[1]));
        let third = shadow.front.clone();
        assert_eq!(capture(&shadow, &mut ring, 10, Some((0, 1)), true), Some(3));
        put(&mut model, 0, "c");
        paint(&mut shadow, &model, &rows(&[0]));
        let fourth = shadow.front.clone();
        assert_eq!(capture(&shadow, &mut ring, 20, None, true), Some(4));
        assert_ne!(third, fourth);

        // a wide glyph, a combining cluster and a styled cell, then the wide
        // glyph replaced by narrow text
        let mut styled = Cell::new("s");
        styled.fg = Color::Rgb(1, 2, 3);
        styled.bg = Color::Indexed(17);
        styled.underline_color = Color::LightRed;
        styled.modifier = Modifier::BOLD | Modifier::UNDERLINED;
        hand_paint(
            &mut shadow,
            &[
                (0, 3, Cell::new("世")),
                (1, 3, Cell::EMPTY),
                (0, 4, Cell::new("e\u{301}")),
                (2, 4, styled.clone()),
            ],
        );
        assert_eq!(capture(&shadow, &mut ring, 30, None, true), Some(5));
        let fifth = shadow.front.clone();
        hand_paint(
            &mut shadow,
            &[(0, 3, Cell::new("a")), (1, 3, Cell::new("b"))],
        );
        assert_eq!(capture(&shadow, &mut ring, 40, None, true), Some(6));
        let sixth = shadow.front.clone();

        let deltas = ring.snapshot().unwrap();
        let keys: Vec<_> = deltas.frames().map(|f| (f.seq, f.key)).collect();
        assert_eq!(
            keys,
            [
                (1, true),
                (2, false),
                (3, false),
                (4, false),
                (5, false),
                (6, false)
            ]
        );

        assert!(load(&mut shadow, &ring, 3));
        assert_eq!(shadow.back, third);
        assert!(shadow.painted.full);
        assert!(load(&mut shadow, &ring, 4));
        assert_eq!(shadow.back, fourth);
        assert!(load(&mut shadow, &ring, 5));
        assert_eq!(shadow.back, fifth);
        assert_eq!(shadow.back.content[usize::from(W) * 4 + 2], styled);
        assert!(load(&mut shadow, &ring, 6));
        assert_eq!(shadow.back, sixth);
        assert!(!load(&mut shadow, &ring, 7));
    }

    /// Commits a frame that changes `cells` by hand, the way
    /// [`Shadow::commit`] leaves the frame before it in `back`.
    fn hand_paint(shadow: &mut Shadow, cells: &[(u16, u16, Cell)]) {
        shadow.back = shadow.front.clone();
        let mut painted = Vec::new();
        for (x, y, cell) in cells {
            *shadow.front.cell_mut((*x, *y)).unwrap() = cell.clone();
            painted.push(*y);
        }
        shadow.painted = rows(&painted);
    }

    #[test]
    fn a_keyframe_the_ring_refuses_counts_no_cell_stored() {
        let mut shadow = Shadow::new();
        shadow.resize(Rect::new(0, 0, W, H));
        shadow.painted = Damage::full();
        let mut ring = FrameRing::new(64);
        assert_eq!(capture(&shadow, &mut ring, 0, None, false), None);
        let last = ring.last_capture();
        assert!(last.key && last.seq.is_none(), "{last:?}");
        assert_eq!(last.pushed, 0);
    }

    #[test]
    fn a_placed_cell_never_runs_past_its_row() {
        let mut ring = FrameRing::new(64 << 20);
        let mut cells = vec![Cell::EMPTY; 8];
        cells[5] = Cell::new("世");
        ring.push_key(0, (8, 1), None, cells).unwrap();
        let mut shadow = Shadow::new();
        shadow.resize(Rect::new(0, 0, 6, 1));
        assert!(load(&mut shadow, &ring, 1));
        assert_eq!(shadow.back.content[5].symbol(), " ");
    }

    /// A drawing laid over the fixture screen.
    type Decorate = fn(&mut Buffer, (u16, u16), u64);

    /// A drawing laid over the fixture screen, and its name.
    type Decoration = (&'static str, Decorate);

    /// The fixture screen as it is, under relative line numbers, under a
    /// working config's cursor line, wrapped lines and indent guide, and
    /// split over a lower window that stands still.
    const DECORATIONS: [Decoration; 4] = [
        ("plain", |_, _, _| {}),
        ("relative", fixture::relative),
        ("real", fixture::real),
        ("split", fixture::split),
    ];

    /// Feeds frames of the fixture screen of size `area`, one every
    /// `period_us`, until the ring first drops its oldest group or holds
    /// `until` frames. Returns the frames and the seconds the ring held at
    /// that moment. `decorate` is drawn over every frame.
    fn retention(
        area: (u16, u16),
        period_us: u64,
        (scroll, decorate): (bool, Decorate),
        max_bytes: usize,
        until: u64,
    ) -> (u64, f64) {
        let mut shadow = Shadow::new();
        shadow.resize(Rect::new(0, 0, area.0, area.1));
        let mut ring = FrameRing::new(max_bytes);
        let draw = |buf: &mut Buffer, top: u64, typed: u16| {
            fixture::draw(buf, area, top, typed);
            decorate(buf, area, top);
        };
        draw(&mut shadow.front, 0, 0);
        shadow.painted = Damage::full();
        capture(&shadow, &mut ring, 0, None, false).unwrap();
        for n in 1..until {
            let held = (ring.newest().unwrap(), ring.age(1).unwrap().as_secs_f64());
            std::mem::swap(&mut shadow.front, &mut shadow.back);
            if scroll {
                draw(&mut shadow.front, n, 0);
                shadow.painted = Damage::full();
            } else {
                let typed = u16::try_from(n % 1000).unwrap();
                draw(&mut shadow.front, 0, typed);
                shadow.painted = rows(&[6, area.1 - 2]);
            }
            capture(&shadow, &mut ring, n * period_us, None, true);
            if ring.oldest() != Some(1) {
                return held;
            }
        }
        (ring.newest().unwrap(), ring.age(1).unwrap().as_secs_f64())
    }

    #[test]
    #[ignore = "a measurement, run by hand with --ignored --nocapture"]
    fn zz_measure_retention() {
        for (fixture, decorate) in DECORATIONS {
            for (name, area, period_us, scroll) in [
                ("typing at 10 keys/s, 100x30", (100, 30), 100_000, false),
                ("held j at 30 Hz, 100x30", (100, 30), 33_333, true),
                ("held j at 30 Hz, 200x60", (200, 60), 33_333, true),
            ] {
                let shape = (scroll, decorate);
                let (frames, secs) = retention(area, period_us, shape, 64 << 20, u64::MAX);
                println!("{fixture} {name}: {frames} frames, {secs:.1} s");
            }
        }
        for (area, scroll) in [((100, 30), true), ((200, 60), true), ((200, 60), false)] {
            let mut shadow = Shadow::new();
            shadow.resize(Rect::new(0, 0, area.0, area.1));
            let mut ring = FrameRing::new(64 << 20);
            let mut spent = std::time::Duration::ZERO;
            for n in 0..2000u64 {
                std::mem::swap(&mut shadow.front, &mut shadow.back);
                let (top, typed) = if scroll { (n, 0) } else { (0, n) };
                fixture::draw(&mut shadow.front, area, top, u16::try_from(typed).unwrap());
                shadow.painted = Damage::full();
                let started = std::time::Instant::now();
                capture(&shadow, &mut ring, n, None, n > 0);
                spent += started.elapsed();
            }
            println!(
                "capture {area:?} scroll={scroll}: {:?} per frame",
                spent / 2000
            );
        }
    }

    /// Records one fixture frame per entry of `screens`, each an area and
    /// the file line its editor is scrolled to, `decorate` drawn over each.
    /// Returns the ring and every frame as it was painted.
    fn record(
        screens: &[((u16, u16), u64)],
        decorate: impl Fn(&mut ratatui::buffer::Buffer, (u16, u16), u64),
    ) -> (FrameRing, Vec<ratatui::buffer::Buffer>) {
        let mut shadow = Shadow::new();
        let mut ring = FrameRing::new(64 << 20);
        let mut painted = Vec::new();
        for (n, &(area, top)) in (0u64..).zip(screens) {
            if shadow.front.area != Rect::new(0, 0, area.0, area.1) {
                shadow.resize(Rect::new(0, 0, area.0, area.1));
            }
            std::mem::swap(&mut shadow.front, &mut shadow.back);
            fixture::draw(&mut shadow.front, area, top, 0);
            decorate(&mut shadow.front, area, top);
            shadow.painted = Damage::full();
            let seq = capture(&shadow, &mut ring, n * 33_333, None, n > 0);
            assert_eq!(seq, Some(n + 1));
            painted.push(shadow.front.clone());
        }
        (ring, painted)
    }

    /// Asserts every frame of `ring` loads back as the frame `painted`
    /// holds, and returns each frame's shift.
    fn reloaded(ring: &mut FrameRing, painted: &[ratatui::buffer::Buffer]) -> Vec<Option<Scroll>> {
        let mut shadow = Shadow::new();
        for (seq, frame) in (1u64..).zip(painted) {
            shadow.resize(frame.area);
            assert!(load(&mut shadow, ring, seq));
            assert_eq!(&shadow.back, frame, "frame {seq}");
        }
        ring.snapshot()
            .unwrap()
            .frames()
            .map(|f| f.scroll)
            .collect()
    }

    #[test]
    fn scrolls_up_and_down_inside_a_split_rebuild_cell_for_cell() {
        let area = (40, 12);
        let tops = [0, 1, 2, 3, 5, 4, 3, 1, 0, 0, 1];
        let screens: Vec<_> = tops.iter().map(|&t| (area, t)).collect();
        let (mut ring, painted) = record(&screens, |_, _, _| {});
        let scrolls = reloaded(&mut ring, &painted);
        let tree = fixture::tree_width(area.0);
        for (i, pair) in tops.windows(2).enumerate() {
            let by = i16::try_from(pair[1]).unwrap() - i16::try_from(pair[0]).unwrap();
            let scroll = scrolls[i + 1];
            if by == 0 {
                assert_eq!(scroll, None, "frame {}", i + 2);
                continue;
            }
            let scroll = scroll.expect("a scrolled frame stores its shift");
            assert_eq!(scroll.by, by, "frame {}", i + 2);
            assert!(scroll.left > tree, "the tree column stays out: {scroll:?}");
            assert!(scroll.right <= area.0 && scroll.bottom < area.1 - 1);
        }
    }

    #[test]
    fn a_scroll_over_wide_glyphs_rebuilds_cell_for_cell() {
        let area = (40, 12);
        let tree = fixture::tree_width(area.0);
        let wide = |buf: &mut ratatui::buffer::Buffer, area: (u16, u16), top: u64| {
            for y in 1..area.1 - 2 {
                // one wide glyph moves with its line and one stays on the
                // edge of the scrolled columns
                if (top + u64::from(y)).is_multiple_of(3) {
                    buf.set_string(tree + 6, y, "世界", Style::default());
                }
                buf.set_string(tree - 1, y, "漢", Style::default());
            }
        };
        let screens: Vec<_> = [0, 1, 2, 1, 4].iter().map(|&t| (area, t)).collect();
        let (mut ring, painted) = record(&screens, wide);
        let scrolls = reloaded(&mut ring, &painted);
        assert!(scrolls[1..].iter().all(Option::is_some), "{scrolls:?}");
    }

    #[test]
    fn a_resize_between_scrolls_rebuilds_cell_for_cell() {
        let (big, small) = ((40, 12), (30, 9));
        let screens = [(big, 0), (big, 1), (small, 1), (small, 2), (small, 0)];
        let (mut ring, painted) = record(&screens, |_, _, _| {});
        let scrolls = reloaded(&mut ring, &painted);
        let keys: Vec<_> = ring.snapshot().unwrap().frames().map(|f| f.key).collect();
        assert_eq!(keys, [true, false, true, false, false]);
        assert!(scrolls[1].is_some() && scrolls[3].is_some() && scrolls[4].is_some());
    }

    #[test]
    fn a_run_of_single_row_scrolls_keeps_six_times_the_frames() {
        let area = (100, 30);
        let max = 64 << 20;
        let cells = usize::from(area.0) * usize::from(area.1);
        // the frames the recording held when every cell was stored whole:
        // a group of a keyframe and its deltas cost a screen of cells twice,
        // and took deltas until its list of a screen of cells was full
        let (mut before, mut after) = (
            Buffer::empty(Rect::new(0, 0, 100, 30)),
            Buffer::empty(Rect::new(0, 0, 100, 30)),
        );
        fixture::draw(&mut before, area, 0, 0);
        fixture::draw(&mut after, area, 1, 0);
        let changed = before
            .content
            .iter()
            .zip(&after.content)
            .filter(|(a, b)| a != b)
            .count();
        let whole =
            cells * std::mem::size_of::<Cell>() + cells * std::mem::size_of::<(u16, u16, Cell)>();
        let per_group = 1 + (cells / changed).min(255);
        let budget = max - view_core::native::dvr::input_log_bytes(max);
        let old = u64::try_from(budget / whole * per_group).unwrap();
        // the first frame still held at six times the old figure is enough,
        // and costs a fifth of running on to the eviction
        for (fixture, decorate) in DECORATIONS {
            let (frames, _) = retention(area, 33_333, (true, decorate), max, 6 * old);
            assert_eq!(
                frames,
                6 * old,
                "{fixture}: the first frame was dropped first"
            );
        }
    }

    #[test]
    fn a_wide_glyph_straddling_the_left_of_a_shift_rebuilds_cell_for_cell() {
        let area = (40, 12);
        let (mut plain, _) = record(&[(area, 0), (area, 1)], |_, _, _| {});
        let left = plain
            .snapshot()
            .unwrap()
            .frames()
            .nth(1)
            .unwrap()
            .scroll
            .unwrap()
            .left;
        // the glyph's lead cell sits left of the shift and its second half
        // inside it, on a row that changes from frame to frame; detection
        // grows the edge away from such a glyph, so the shift is given
        let mut shadow = Shadow::new();
        shadow.resize(Rect::new(0, 0, area.0, area.1));
        let mut ring = FrameRing::new(64 << 20);
        fixture::draw(&mut shadow.front, area, 0, 0);
        shadow.front.set_string(left - 1, 4, "世", Style::default());
        shadow.painted = Damage::full();
        assert_eq!(capture(&shadow, &mut ring, 0, None, false), Some(1));
        shadow.back = shadow.front.clone();
        fixture::draw(&mut shadow.front, area, 1, 0);
        shadow.front.set_string(left - 1, 6, "世", Style::default());
        let scroll = Scroll::new(1..area.1 - 3, left..area.0, 1);
        let group = ring.open_delta(area).unwrap();
        assert!(fill(&shadow, group, Some(scroll)).is_some());
        assert_eq!(ring.close_delta(1, None, Some(scroll)), Some(2));
        let mut loaded = Shadow::new();
        loaded.resize(Rect::new(0, 0, area.0, area.1));
        assert!(load(&mut loaded, &ring, 2));
        assert_eq!(loaded.back, shadow.front);
    }

    #[test]
    fn a_frame_typed_after_a_scroll_stores_its_cells_and_no_shift() {
        let area = (40, 12);
        let mut shadow = Shadow::new();
        shadow.resize(Rect::new(0, 0, area.0, area.1));
        let mut ring = FrameRing::new(64 << 20);
        let mut painted = Vec::new();
        for (n, (top, typed)) in (0u64..).zip([(0, 0), (1, 0), (1, 3)]) {
            std::mem::swap(&mut shadow.front, &mut shadow.back);
            fixture::draw(&mut shadow.front, area, top, typed);
            shadow.painted = Damage::full();
            assert_eq!(capture(&shadow, &mut ring, n, None, n > 0), Some(n + 1));
            painted.push(shadow.front.clone());
        }
        let scrolls = reloaded(&mut ring, &painted);
        assert!(scrolls[1].is_some(), "{scrolls:?}");
        assert_eq!(scrolls[2], None);
    }

    #[test]
    fn a_scroll_past_the_cursor_line_shifts_the_rows_under_it() {
        let area = (100, 30);
        let tops: Vec<_> = (0..40).collect();
        let screens: Vec<_> = tops.iter().map(|&t| (area, t)).collect();
        let (mut ring, painted) = record(&screens, fixture::real);
        let scrolls = reloaded(&mut ring, &painted);
        let keys: Vec<_> = ring.snapshot().unwrap().frames().map(|f| f.key).collect();
        let under = area.1 - 3 - 8 + 1;
        for (i, scroll) in scrolls.iter().enumerate().filter(|&(i, _)| !keys[i]) {
            let scroll = scroll.expect("every delta frame scrolled");
            assert!(
                scroll.top < under && scroll.bottom > under + 4,
                "frame {}: {scroll:?}",
                i + 1
            );
        }
    }

    #[test]
    fn scrolling_one_window_of_a_split_stores_no_cell_of_the_other() {
        let area = (100, 30);
        let screens: Vec<_> = (0..40).map(|t| (area, t)).collect();
        let (mut ring, painted) = record(&screens, fixture::split);
        reloaded(&mut ring, &painted);
        // the upper window's status line and every row of the lower window
        let still = fixture::lower_window(area.1) - 1..area.1 - 2;
        let snapshot = ring.snapshot().unwrap();
        for frame in snapshot.frames().filter(|f| !f.key) {
            let scroll = frame.scroll.expect("every delta frame scrolled");
            assert!(
                scroll.bottom <= still.start,
                "frame {}: {scroll:?}",
                frame.seq
            );
            let stored: Vec<_> = frame
                .cells()
                .filter(|c| still.contains(&c.y))
                .map(|c| (c.x, c.y))
                .collect();
            assert!(stored.is_empty(), "frame {}: {stored:?}", frame.seq);
        }
    }

    #[test]
    fn a_row_left_unpainted_inside_a_shift_rebuilds_cell_for_cell() {
        let (w, h) = (20u16, 8u16);
        let fill_row = |buf: &mut Buffer, y: u16, c: char| {
            buf.set_string(0, y, c.to_string().repeat(usize::from(w)), Style::default());
        };
        let mut shadow = Shadow::new();
        shadow.resize(Rect::new(0, 0, w, h));
        let mut ring = FrameRing::new(64 << 20);
        for (y, c) in (0..h).zip('a'..) {
            fill_row(&mut shadow.front, y, c);
        }
        shadow.painted = Damage::full();
        assert_eq!(capture(&shadow, &mut ring, 0, None, false), Some(1));
        // every row moves up one but the fourth, which keeps what it showed
        // and so is neither painted nor explained by the shift
        shadow.back = shadow.front.clone();
        for (y, c) in (0..h).zip('b'..) {
            if y != 3 {
                fill_row(&mut shadow.front, y, c);
            }
        }
        shadow.painted = rows(&[0, 1, 2, 4, 5, 6, 7]);
        assert_eq!(capture(&shadow, &mut ring, 1, None, true), Some(2));
        let scroll = ring
            .last_capture()
            .scroll
            .expect("the frame stores a shift");
        assert!(scroll.top < 3 && scroll.bottom > 4, "{scroll:?}");
        let mut loaded = Shadow::new();
        loaded.resize(Rect::new(0, 0, w, h));
        assert!(load(&mut loaded, &ring, 2));
        assert_eq!(loaded.back, shadow.front);
    }

    #[test]
    fn a_scroll_beside_relative_line_numbers_stores_its_shift() {
        let area = (40, 12);
        let tops = [0, 1, 2, 3, 2, 5];
        let screens: Vec<_> = tops.iter().map(|&t| (area, t)).collect();
        let (mut ring, painted) = record(&screens, fixture::relative);
        let scrolls = reloaded(&mut ring, &painted);
        let gutter = fixture::tree_width(area.0) + 6;
        for (i, scroll) in scrolls.iter().enumerate().skip(1) {
            let scroll = scroll.expect("every frame after the first scrolled");
            assert!(scroll.left >= gutter, "frame {}: {scroll:?}", i + 1);
        }
        // the edge grows out of the text to the first column it holds
        let lefts: Vec<_> = scrolls.iter().flatten().map(|s| s.left).collect();
        assert!(lefts.contains(&gutter), "{lefts:?}");
    }

    #[test]
    fn a_pane_changing_right_of_the_scroll_stays_out_of_its_span() {
        let area = (40, 12);
        let pane = area.0 - 6;
        // a pane on the right whose rows differ from each other and whose
        // second row counts the frames
        let beside = move |buf: &mut Buffer, _: (u16, u16), top: u64| {
            for y in 1..area.1 - 2 {
                buf.set_string(pane, y, format!("│ p{y:<3}"), Style::default());
            }
            buf.set_string(pane + 2, 2, format!("n{top:<3}"), Style::default());
        };
        let screens: Vec<_> = (0..6).map(|t| (area, t)).collect();
        let (mut ring, painted) = record(&screens, beside);
        let scrolls = reloaded(&mut ring, &painted);
        for (i, scroll) in scrolls.iter().enumerate().skip(1) {
            let scroll = scroll.expect("every frame after the first scrolled");
            assert!(scroll.right <= pane, "frame {}: {scroll:?}", i + 1);
        }
    }

    #[test]
    fn paint_bar_writes_the_last_row_only() {
        let model = model();
        let mut shadow = Shadow::new();
        paint(&mut shadow, &model, &Damage::full());
        let before = shadow.back.clone();
        paint_bar(&mut shadow, &model, "DVR");
        let last = usize::from(H - 1) * usize::from(W);
        assert_eq!(shadow.back.content[last].symbol(), "D");
        assert_eq!(shadow.back.content[..last], before.content[..last]);
    }

    #[test]
    fn the_bar_leaves_a_message_on_the_last_row_readable() {
        let model = model();
        let mut shadow = Shadow::new();
        paint(&mut shadow, &model, &Damage::full());
        shadow.back.cell_mut((3, H - 1)).unwrap().set_symbol("E");
        let before = shadow.back.clone();
        paint_bar(&mut shadow, &model, "DVR");
        let last = usize::from(H - 1) * usize::from(W);
        assert_eq!(shadow.back.content[0].symbol(), "D");
        assert_eq!(shadow.back.content[last..], before.content[last..]);
    }
}
