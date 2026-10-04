//! Moves frames between the shadow and the session DVR's ring: the frame
//! just committed goes in as the cells it changed, and a recorded frame
//! comes back as the shadow's next frame to emit.

use std::ops::Range;

use ratatui::buffer::Cell;
use ratatui::layout::Rect;
use view_core::model::Model;
use view_core::theme::{ChromeGroup, Theme};

use super::{paint_text_row, ratatui_style, Damage, Shadow};
use crate::dvr::{place, row_hash, FrameRing, Group, Scroll};

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
/// changed. It reads the screen's cells about five times and compares at
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
    if delta && into.open_delta(size).is_some() {
        let scroll = scroll_of(shadow, &mut into.scratch);
        if let Some(group) = into.open_delta(size) {
            if fill(shadow, group, scroll) {
                return into.close_delta(at_us, cursor, scroll);
            }
            group.abort();
        }
    }
    into.push_key(at_us, size, cursor, shadow.front.content.iter().cloned())
}

/// Row `y`'s columns `cols` of a row-major `content` `width` cells wide.
fn row(content: &[Cell], width: usize, y: usize, cols: Range<usize>) -> Option<&[Cell]> {
    content.get(y * width + cols.start..y * width + cols.end)
}

/// Pushes the painted cells that differ from what `scroll` leaves of
/// `back` into `group`. A cell the shift fills matches by the shift's own
/// check. Returns false when the group has no room for them.
fn fill(shadow: &Shadow, group: &mut Group, scroll: Option<Scroll>) -> bool {
    let width = usize::from(shadow.front.area.width);
    for y in 0..shadow.front.area.height {
        if !shadow.painted.covers(y) {
            continue;
        }
        let at = usize::from(y);
        let (Some(now), Some(was)) = (
            row(&shadow.front.content, width, at, 0..width),
            row(&shadow.back.content, width, at, 0..width),
        ) else {
            continue;
        };
        for (x, (now, was)) in (0..).zip(now.iter().zip(was)) {
            if scroll.is_some_and(|s| s.covers(x, y)) {
                continue;
            }
            if now != was && !group.push_cell(x, y, now) {
                return false;
            }
        }
    }
    true
}

/// The shift of `back` that the frame in `front` shows: the columns where
/// most changed rows differ are the span that moved, rows whose span
/// equals another row of `back` vote for the distance between them, and
/// the longest run of rows the winning distance explains cell for cell is
/// the shift. `None` when a row's worth of cells or fewer changed, or no
/// run of two rows is explained.
///
/// A gutter of relative line numbers keeps its digits while the text
/// beside it moves, and a sign changes on a row or two, so the majority
/// leaves both outside the span and [`fill`] stores them as plain cells.
/// The pass that counts the changed cells is one more read of the painted
/// rows than [`fill`] makes on its own.
// ponytail: one column span for the whole frame, so a pane that changes
// beside the scrolling one widens the span past the shift. Per-pane spans
// are the upgrade when that shows in a retention measurement.
fn scroll_of(shadow: &Shadow, scratch: &mut Vec<u64>) -> Option<Scroll> {
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
    if changed <= w {
        return None;
    }
    let moved = |count: &u64| 2 * *count > rows;
    let left = scratch.iter().position(moved)?;
    let right = scratch.iter().rposition(|&count| count > 0)? + 1;
    scratch.clear();
    for content in [front, back] {
        for y in 0..h {
            scratch.push(row(content, w, y, left..right).map_or(0, row_hash));
        }
    }
    scratch.resize(4 * h, 0);
    let (now, rest) = scratch.split_at_mut(h);
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
    let explained = |y: usize| {
        y.checked_add_signed(by)
            .filter(|&f| f < h)
            .is_some_and(|from| {
                now.get(y) == was.get(from)
                    && row(front, w, y, left..right) == row(back, w, from, left..right)
            })
    };
    let (mut best, mut start) = (0..0, None);
    for y in 0..=h {
        match (y < h && explained(y), start) {
            (true, None) => start = Some(y),
            (false, Some(top)) => {
                if y - top > best.len() {
                    best = top..y;
                }
                start = None;
            }
            _ => {}
        }
    }
    if best.len() < 2 {
        return None;
    }
    let range = |r: Range<usize>| Some(u16::try_from(r.start).ok()?..u16::try_from(r.end).ok()?);
    Some(Scroll::new(
        range(best)?,
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

        let deltas = ring.snapshot();
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

    /// Feeds frames of the fixture screen of size `area`, one every
    /// `period_us`, until the ring first drops its oldest group or holds
    /// `until` frames. Returns the frames and the seconds the ring held at
    /// that moment. `relative` draws the gutter the way `relativenumber`
    /// does.
    fn retention(
        area: (u16, u16),
        period_us: u64,
        (scroll, relative): (bool, bool),
        max_bytes: usize,
        until: u64,
    ) -> (u64, f64) {
        let mut shadow = Shadow::new();
        shadow.resize(Rect::new(0, 0, area.0, area.1));
        let mut ring = FrameRing::new(max_bytes);
        let draw = |buf: &mut Buffer, top: u64, typed: u16| {
            fixture::draw(buf, area, top, typed);
            if relative {
                fixture::relative(buf, area, top);
            }
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
        for relative in [false, true] {
            for (name, area, period_us, scroll) in [
                ("typing at 10 keys/s, 100x30", (100, 30), 100_000, false),
                ("held j at 30 Hz, 100x30", (100, 30), 33_333, true),
                ("held j at 30 Hz, 200x60", (200, 60), 33_333, true),
            ] {
                let shape = (scroll, relative);
                let (frames, secs) = retention(area, period_us, shape, 64 << 20, u64::MAX);
                println!("relative={relative} {name}: {frames} frames, {secs:.1} s");
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
    fn reloaded(ring: &FrameRing, painted: &[ratatui::buffer::Buffer]) -> Vec<Option<Scroll>> {
        let mut shadow = Shadow::new();
        for (seq, frame) in (1u64..).zip(painted) {
            shadow.resize(frame.area);
            assert!(load(&mut shadow, ring, seq));
            assert_eq!(&shadow.back, frame, "frame {seq}");
        }
        ring.snapshot().frames().map(|f| f.scroll).collect()
    }

    #[test]
    fn scrolls_up_and_down_inside_a_split_rebuild_cell_for_cell() {
        let area = (40, 12);
        let tops = [0, 1, 2, 3, 5, 4, 3, 1, 0, 0, 1];
        let screens: Vec<_> = tops.iter().map(|&t| (area, t)).collect();
        let (ring, painted) = record(&screens, |_, _, _| {});
        let scrolls = reloaded(&ring, &painted);
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
        let (ring, painted) = record(&screens, wide);
        let scrolls = reloaded(&ring, &painted);
        assert!(scrolls[1..].iter().all(Option::is_some), "{scrolls:?}");
    }

    #[test]
    fn a_resize_between_scrolls_rebuilds_cell_for_cell() {
        let (big, small) = ((40, 12), (30, 9));
        let screens = [(big, 0), (big, 1), (small, 1), (small, 2), (small, 0)];
        let (ring, painted) = record(&screens, |_, _, _| {});
        let scrolls = reloaded(&ring, &painted);
        let keys: Vec<_> = ring.snapshot().frames().map(|f| f.key).collect();
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
        for relative in [false, true] {
            let (frames, _) = retention(area, 33_333, (true, relative), max, 6 * old);
            assert_eq!(
                frames,
                6 * old,
                "relative={relative}: the first frame was dropped first"
            );
        }
    }

    #[test]
    fn a_wide_glyph_straddling_the_left_of_a_shift_rebuilds_cell_for_cell() {
        let area = (40, 12);
        let (plain, _) = record(&[(area, 0), (area, 1)], |_, _, _| {});
        let left = plain
            .snapshot()
            .frames()
            .nth(1)
            .unwrap()
            .scroll
            .unwrap()
            .left;
        // the glyph's lead cell sits left of the shift and its second half
        // inside it, on a row that changes from frame to frame
        let wide = move |buf: &mut Buffer, _: (u16, u16), top: u64| {
            let y = if top.is_multiple_of(2) { 3 } else { 4 };
            buf.set_string(left - 1, y, "世", Style::default());
        };
        let screens: Vec<_> = [0, 1, 2, 3].iter().map(|&t| (area, t)).collect();
        let (ring, painted) = record(&screens, wide);
        let scrolls = reloaded(&ring, &painted);
        assert!(
            scrolls[1..]
                .iter()
                .all(|s| s.is_some_and(|s| s.left == left)),
            "{scrolls:?}"
        );
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
        let scrolls = reloaded(&ring, &painted);
        assert!(scrolls[1].is_some(), "{scrolls:?}");
        assert_eq!(scrolls[2], None);
    }

    #[test]
    fn a_scroll_beside_relative_line_numbers_stores_its_shift() {
        let area = (40, 12);
        let tops = [0, 1, 2, 3, 2, 5];
        let screens: Vec<_> = tops.iter().map(|&t| (area, t)).collect();
        let (ring, painted) = record(&screens, fixture::relative);
        let scrolls = reloaded(&ring, &painted);
        let gutter = fixture::tree_width(area.0) + 6;
        for (i, scroll) in scrolls.iter().enumerate().skip(1) {
            let scroll = scroll.expect("every frame after the first scrolled");
            assert!(scroll.left >= gutter, "frame {}: {scroll:?}", i + 1);
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
