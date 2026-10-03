//! The session DVR's painted frames: a ring of keyframe groups held inside
//! the recording's memory bound, read back one frame at a time to repaint
//! what the screen showed.
//!
//! The public API carries no `ratatui` type. A recorded cell leaves this
//! crate as a [`CellView`], its colors packed into `u32`s.

use std::collections::VecDeque;
use std::sync::Arc;
use std::time::Duration;

use ratatui::buffer::Cell;
use ratatui::style::{Color, Modifier};
use view_core::native::dvr::{input_log_bytes, ScrubStep};

/// The frames one group holds before the next frame opens a new keyframe.
const GROUP_FRAMES: usize = 256;

/// The one-second scrub step, in the microseconds frames are dated in.
const SECOND_US: i64 = 1_000_000;

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

#[derive(Debug, Clone, Copy)]
struct Frame {
    seq: u64,
    at_us: u64,
    cursor: Option<(u16, u16)>,
    /// Where this frame's changed cells end in the group's delta list. A
    /// frame's own cells start where the frame before it ends.
    deltas_end: usize,
}

/// A keyframe and the delta frames painted on top of it, evicted whole.
#[derive(Debug, Default)]
pub(crate) struct Group {
    area: (u16, u16),
    key: Vec<Cell>,
    deltas: Vec<(u16, u16, Cell)>,
    frames: Vec<Frame>,
}

impl Group {
    fn bytes(&self) -> usize {
        self.key.capacity() * std::mem::size_of::<Cell>()
            + self.deltas.capacity() * std::mem::size_of::<(u16, u16, Cell)>()
            + self.frames.capacity() * std::mem::size_of::<Frame>()
    }

    /// The bytes a group reserved for `area` holds.
    fn reserved_bytes(area: (u16, u16)) -> usize {
        let cells = usize::from(area.0) * usize::from(area.1);
        cells * std::mem::size_of::<Cell>()
            + cells * std::mem::size_of::<(u16, u16, Cell)>()
            + GROUP_FRAMES * std::mem::size_of::<Frame>()
    }

    /// Appends one changed cell to the frame being built. Returns false,
    /// holding nothing new, when the reserved delta list is full.
    pub(crate) fn push_cell(&mut self, x: u16, y: u16, cell: &Cell) -> bool {
        if self.deltas.len() == self.deltas.capacity() {
            return false;
        }
        self.deltas.push((x, y, cell.clone()));
        true
    }

    /// Drops the cells of the frame being built.
    pub(crate) fn abort(&mut self) {
        let end = self.frames.last().map_or(0, |f| f.deltas_end);
        self.deltas.truncate(end);
    }

    /// Calls `put` with every cell that builds frame `index`: the keyframe
    /// cells, then each delta in paint order.
    pub(crate) fn replay(&self, index: usize, mut put: impl FnMut(u16, u16, &Cell)) {
        let width = usize::from(self.area.0.max(1));
        for (i, cell) in self.key.iter().enumerate() {
            let (x, y) = (i % width, i / width);
            if let (Ok(x), Ok(y)) = (u16::try_from(x), u16::try_from(y)) {
                put(x, y, cell);
            }
        }
        let end = self.frames.get(index).map_or(0, |f| f.deltas_end);
        for (x, y, cell) in self.deltas.get(..end).unwrap_or_default() {
            put(*x, *y, cell);
        }
    }

    fn first_seq(&self) -> u64 {
        self.frames.first().map_or(0, |f| f.seq)
    }
}

/// The painted frames of the session, oldest whole groups dropped once the
/// recording's memory bound is reached.
///
/// Every group reserves its keyframe, its delta list and its frame list when
/// it opens, and an evicted group's storage opens the next one, so a capture
/// in steady state allocates nothing.
// ponytail: a keyframe stores every cell, so a full-screen scroll costs a
// whole screen per frame. Row dedup or compression of closed groups is the
// upgrade when a longer rewind under scrolling matters.
#[derive(Debug)]
pub struct FrameRing {
    /// The retained groups, oldest first. The newest is the open one,
    /// written through `Arc::get_mut` while no snapshot shares it.
    groups: VecDeque<Arc<Group>>,
    pool: Vec<Group>,
    budget: usize,
    /// The bytes the retained and pooled groups reserve.
    held: usize,
    next_seq: u64,
}

impl FrameRing {
    /// An empty ring holding what a recording bound of `max_bytes` leaves
    /// once the input log has taken its share.
    #[must_use]
    pub fn new(max_bytes: usize) -> Self {
        Self::with_budget(max_bytes - input_log_bytes(max_bytes))
    }

    fn with_budget(budget: usize) -> Self {
        Self {
            groups: VecDeque::new(),
            pool: Vec::new(),
            budget,
            held: 0,
            next_seq: 1,
        }
    }

    /// The newest recorded frame.
    #[must_use]
    pub fn newest(&self) -> Option<u64> {
        self.groups.back()?.frames.last().map(|f| f.seq)
    }

    /// The oldest retained frame, which is always a keyframe.
    #[must_use]
    pub fn oldest(&self) -> Option<u64> {
        self.groups.front()?.frames.first().map(|f| f.seq)
    }

    /// How long before the newest frame frame `seq` was painted.
    #[must_use]
    pub fn age(&self, seq: u64) -> Option<Duration> {
        let newest = self.frame(self.newest()?)?.at_us;
        let at = self.frame(seq)?.at_us;
        Some(Duration::from_micros(newest.saturating_sub(at)))
    }

    /// The retained frame one scrub `step` away from `from`, clamped to the
    /// retained range. An empty ring answers `from`.
    #[must_use]
    pub fn resolve(&self, from: u64, step: ScrubStep) -> u64 {
        let (Some(oldest), Some(newest)) = (self.oldest(), self.newest()) else {
            return from;
        };
        let from = from.clamp(oldest, newest);
        match step {
            ScrubStep::Oldest => oldest,
            ScrubStep::Newest => newest,
            ScrubStep::Frames(n) => from
                .saturating_add_signed(i64::from(n))
                .clamp(oldest, newest),
            ScrubStep::Seconds(n) => {
                let Some(at) = self.frame(from).map(|f| f.at_us) else {
                    return from;
                };
                let target = at.saturating_add_signed(i64::from(n) * SECOND_US);
                let mut frames = self.groups.iter().flat_map(|g| g.frames.iter());
                if n < 0 {
                    frames
                        .take_while(|f| f.seq <= from)
                        .filter(|f| f.at_us <= target)
                        .last()
                        .map_or(oldest, |f| f.seq)
                } else {
                    frames
                        .find(|f| f.seq >= from && f.at_us >= target)
                        .map_or(newest, |f| f.seq)
                }
            }
            _ => from,
        }
    }

    /// The retained groups, shared with the caller at the cost of one `Arc`
    /// clone each. A frame recorded after this opens a new group.
    #[must_use]
    pub fn snapshot(&self) -> RingSnapshot {
        RingSnapshot {
            groups: self.groups.iter().cloned().collect(),
        }
    }

    /// The group holding frame `seq` and the frame's index inside it.
    pub(crate) fn locate(&self, seq: u64) -> Option<(&Group, usize)> {
        let at = self.groups.partition_point(|g| g.first_seq() <= seq);
        let group = self.groups.get(at.checked_sub(1)?)?;
        let index = usize::try_from(seq - group.first_seq()).ok()?;
        (index < group.frames.len()).then_some((&**group, index))
    }

    fn frame(&self, seq: u64) -> Option<&Frame> {
        let (group, index) = self.locate(seq)?;
        group.frames.get(index)
    }

    /// The open group, when the next frame of size `area` can be a delta
    /// on it.
    pub(crate) fn open_delta(&mut self, area: (u16, u16)) -> Option<&mut Group> {
        let group = Arc::get_mut(self.groups.back_mut()?)?;
        (group.area == area && group.frames.len() < GROUP_FRAMES).then_some(group)
    }

    /// Closes the delta frame built on the open group and returns its seq.
    pub(crate) fn close_delta(&mut self, at_us: u64, cursor: Option<(u16, u16)>) -> u64 {
        let seq = self.next_seq;
        if let Some(group) = self.groups.back_mut().and_then(Arc::get_mut) {
            group.frames.push(Frame {
                seq,
                at_us,
                cursor,
                deltas_end: group.deltas.len(),
            });
            self.next_seq += 1;
        }
        seq
    }

    /// Records a keyframe of size `area` from `cells`, row by row, opening a
    /// new group. Returns its seq, or the newest seq when one group of this
    /// size cannot fit the bound at all and the frame goes unrecorded.
    pub(crate) fn push_key(
        &mut self,
        at_us: u64,
        area: (u16, u16),
        cursor: Option<(u16, u16)>,
        cells: impl IntoIterator<Item = Cell>,
    ) -> u64 {
        let Some(mut group) = self.take_group(area) else {
            return self.newest().unwrap_or(0);
        };
        group.key.extend(cells);
        let seq = self.next_seq;
        self.next_seq += 1;
        group.frames.push(Frame {
            seq,
            at_us,
            cursor,
            deltas_end: 0,
        });
        self.groups.push_back(Arc::new(group));
        seq
    }

    /// Storage for a new group of size `area`: a pooled group first, then
    /// a fresh one once evicting the oldest groups makes room for it.
    fn take_group(&mut self, area: (u16, u16)) -> Option<Group> {
        let fresh = Group::reserved_bytes(area);
        if fresh > self.budget {
            return None;
        }
        let cells = usize::from(area.0) * usize::from(area.1);
        loop {
            if let Some(mut group) = self.pool.pop() {
                let before = group.bytes();
                group.key.clear();
                group.deltas.clear();
                group.frames.clear();
                group.key.reserve(cells);
                group.deltas.reserve(cells);
                group.frames.reserve(GROUP_FRAMES);
                group.area = area;
                self.held -= before;
                if group.bytes() > self.budget {
                    continue;
                }
                self.held += group.bytes();
                self.trim();
                return Some(group);
            }
            if self.held + fresh <= self.budget {
                let group = Group {
                    area,
                    key: Vec::with_capacity(cells),
                    deltas: Vec::with_capacity(cells),
                    frames: Vec::with_capacity(GROUP_FRAMES),
                };
                self.held += group.bytes();
                return Some(group);
            }
            let oldest = self.groups.pop_front()?;
            match Arc::try_unwrap(oldest) {
                Ok(group) => self.pool.push(group),
                Err(shared) => self.held -= shared.bytes(),
            }
        }
    }

    /// Drops pooled and then the oldest groups while the ring holds more
    /// than its budget, which a pooled group grown for a larger screen can
    /// cause.
    fn trim(&mut self) {
        while self.held > self.budget {
            let dropped = match self.pool.pop() {
                Some(group) => group.bytes(),
                None => match self.groups.pop_front() {
                    Some(group) => group.bytes(),
                    None => return,
                },
            };
            self.held -= dropped;
        }
    }
}

/// The groups of a ring at one moment, readable while the ring records on.
#[derive(Debug, Clone)]
pub struct RingSnapshot {
    groups: Vec<Arc<Group>>,
}

impl RingSnapshot {
    /// Every frame of the snapshot, oldest first.
    pub fn frames(&self) -> impl Iterator<Item = FrameView<'_>> {
        self.groups.iter().flat_map(|group| {
            let mut start = 0;
            group.frames.iter().enumerate().map(move |(i, f)| {
                let cells = if i == 0 {
                    Cells::Key(&group.key, group.area.0)
                } else {
                    let slice = group.deltas.get(start..f.deltas_end).unwrap_or_default();
                    Cells::Delta(slice)
                };
                start = f.deltas_end;
                FrameView {
                    seq: f.seq,
                    at_us: f.at_us,
                    area: group.area,
                    cursor: f.cursor,
                    key: i == 0,
                    cells,
                }
            })
        })
    }
}

#[derive(Debug, Clone, Copy)]
enum Cells<'a> {
    Key(&'a [Cell], u16),
    Delta(&'a [(u16, u16, Cell)]),
}

/// One recorded frame: a whole screen when it is a keyframe, the cells
/// that changed since the frame before it otherwise.
#[derive(Debug, Clone, Copy)]
#[non_exhaustive]
pub struct FrameView<'a> {
    /// The frame's number, counting from 1 for the session's first.
    pub seq: u64,
    /// When the frame was painted, in microseconds since recording began.
    pub at_us: u64,
    /// The screen's width and height.
    pub area: (u16, u16),
    /// Where the caret stood, when it was shown.
    pub cursor: Option<(u16, u16)>,
    /// Whether the frame holds every cell of the screen.
    pub key: bool,
    cells: Cells<'a>,
}

impl<'a> FrameView<'a> {
    /// The frame's cells, row by row for a keyframe and in paint order for
    /// a delta.
    pub fn cells(&self) -> impl Iterator<Item = CellView<'a>> + 'a {
        let (key, width, delta) = match self.cells {
            Cells::Key(cells, width) => (cells, usize::from(width.max(1)), &[][..]),
            Cells::Delta(cells) => (&[][..], 1, cells),
        };
        let keyed = key.iter().enumerate().map(move |(i, cell)| {
            let x = u16::try_from(i % width).unwrap_or(u16::MAX);
            let y = u16::try_from(i / width).unwrap_or(u16::MAX);
            CellView::of(x, y, cell)
        });
        keyed.chain(delta.iter().map(|(x, y, cell)| CellView::of(*x, *y, cell)))
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
    fn of(x: u16, y: u16, cell: &'a Cell) -> Self {
        Self {
            x,
            y,
            symbol: cell.symbol(),
            fg: pack(cell.fg),
            bg: pack(cell.bg),
            ul: pack(cell.underline_color),
            modifier: cell.modifier.bits(),
        }
    }
}

/// The cell `view` was recorded from. A symbol carrying a control character,
/// which only a damaged clip holds, comes back blank.
fn restore(view: CellView<'_>) -> Cell {
    let mut cell = Cell::EMPTY;
    if !view.symbol.chars().any(char::is_control) {
        cell.set_symbol(view.symbol);
    }
    cell.fg = unpack(view.fg);
    cell.bg = unpack(view.bg);
    cell.underline_color = unpack(view.ul);
    cell.modifier = Modifier::from_bits_truncate(view.modifier);
    cell
}

fn pack(color: Color) -> u32 {
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

fn unpack(packed: u32) -> Color {
    let [tag, r, g, b] = packed.to_be_bytes();
    match tag {
        1 => NAMED.get(usize::from(b)).copied().unwrap_or(Color::Reset),
        2 => Color::Indexed(b),
        3 => Color::Rgb(r, g, b),
        _ => Color::Reset,
    }
}

fn blank(area: (u16, u16)) -> Vec<Cell> {
    vec![Cell::EMPTY; usize::from(area.0) * usize::from(area.1)]
}

/// Copies `cell` to column `x` of row `y` of a row-major `screen` of size
/// `area`, dropping it when it falls outside.
pub(crate) fn place(screen: &mut [Cell], area: (u16, u16), x: u16, y: u16, cell: &Cell) {
    if x < area.0 && y < area.1 {
        if let Some(slot) = screen.get_mut(usize::from(y) * usize::from(area.0) + usize::from(x)) {
            slot.clone_from(cell);
        }
    }
}

/// Builds a ring from frames decoded out of a clip.
#[derive(Debug)]
pub struct RingBuilder {
    ring: FrameRing,
}

impl RingBuilder {
    /// An empty builder whose ring holds a recording bound of `max_bytes`.
    #[must_use]
    pub fn new(max_bytes: usize) -> Self {
        Self {
            ring: FrameRing::new(max_bytes),
        }
    }

    /// Adds a keyframe of size `area`. A cell outside the area is dropped
    /// and a cell the frame does not name is blank. Returns its seq.
    pub fn push_key<'c>(
        &mut self,
        at_us: u64,
        area: (u16, u16),
        cursor: Option<(u16, u16)>,
        cells: impl IntoIterator<Item = CellView<'c>>,
    ) -> u64 {
        let mut screen = blank(area);
        for view in cells {
            place(&mut screen, area, view.x, view.y, &restore(view));
        }
        self.ring.push_key(at_us, area, cursor, screen)
    }

    /// Adds a delta frame on the newest frame. A delta past what the open
    /// group holds becomes a keyframe of the frame it builds. Returns its
    /// seq, or `None` before any keyframe.
    pub fn push_delta<'c>(
        &mut self,
        at_us: u64,
        cursor: Option<(u16, u16)>,
        cells: impl IntoIterator<Item = CellView<'c>> + Clone,
    ) -> Option<u64> {
        let newest = self.ring.newest()?;
        let area = self.ring.locate(newest)?.0.area;
        if let Some(group) = self.ring.open_delta(area) {
            let fits = cells
                .clone()
                .into_iter()
                .filter(|v| v.x < area.0 && v.y < area.1)
                .all(|v| group.push_cell(v.x, v.y, &restore(v)));
            if fits {
                return Some(self.ring.close_delta(at_us, cursor));
            }
            group.abort();
        }
        let mut screen = blank(area);
        let (group, index) = self.ring.locate(newest)?;
        group.replay(index, |x, y, cell| place(&mut screen, area, x, y, cell));
        for view in cells {
            place(&mut screen, area, view.x, view.y, &restore(view));
        }
        Some(self.ring.push_key(at_us, area, cursor, screen))
    }

    /// The ring the frames built.
    #[must_use]
    pub fn finish(self) -> FrameRing {
        self.ring
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    const AREA: (u16, u16) = (4, 2);

    fn screen(symbol: &'static str) -> Vec<Cell> {
        vec![Cell::new(symbol); 8]
    }

    fn delta(ring: &mut FrameRing, at_us: u64, cells: usize) -> u64 {
        let Some(group) = ring.open_delta(AREA) else {
            return ring.push_key(at_us, AREA, None, screen("k"));
        };
        let changed = Cell::new("d");
        if (0..cells).all(|i| group.push_cell(u16::try_from(i % 4).unwrap(), 0, &changed)) {
            return ring.close_delta(at_us, None);
        }
        group.abort();
        ring.push_key(at_us, AREA, None, screen("k"))
    }

    fn is_key(ring: &FrameRing, seq: u64) -> bool {
        ring.locate(seq).is_some_and(|(_, index)| index == 0)
    }

    #[test]
    fn the_ring_and_the_input_log_together_stay_inside_the_bound() {
        for max_bytes in [1, 7, 64 << 20, 1000 << 20, 4096 << 20] {
            let ring = FrameRing::new(max_bytes);
            assert_eq!(ring.budget + input_log_bytes(max_bytes), max_bytes);
        }
    }

    #[test]
    fn eviction_drops_whole_groups_and_never_a_lone_delta() {
        let mut ring = FrameRing::with_budget(Group::reserved_bytes(AREA) * 3);
        for frame in 0..2000u64 {
            delta(&mut ring, frame, usize::try_from(frame % 5).unwrap());
            let oldest = ring.oldest().unwrap();
            assert!(is_key(&ring, oldest), "frame {frame}: oldest {oldest}");
            assert!(ring.held <= ring.budget, "frame {frame}: {}", ring.held);
        }
        assert!(ring.groups.len() > 1);
        assert!(ring.oldest().unwrap() > 1, "the oldest groups were dropped");
    }

    #[test]
    fn a_resize_opens_a_new_keyframe() {
        let mut ring = FrameRing::new(64 << 20);
        let first = ring.push_key(0, AREA, None, screen("a"));
        let second = delta(&mut ring, 1, 1);
        assert!(!is_key(&ring, second));
        assert!(ring.open_delta((5, 2)).is_none());
        let resized = ring.push_key(2, (5, 2), None, vec![Cell::new("b"); 10]);
        assert!(is_key(&ring, resized));
        assert_eq!((first, second, resized), (1, 2, 3));
        assert_eq!(ring.locate(resized).unwrap().0.area, (5, 2));
    }

    #[test]
    fn steady_capture_reuses_pooled_storage() {
        let mut ring = FrameRing::with_budget(Group::reserved_bytes(AREA) * 3);
        let mut seen = Vec::new();
        for frame in 0..3 {
            ring.push_key(frame, AREA, None, screen("k"));
            let open = ring.groups.back().unwrap();
            seen.push((open.key.as_ptr(), open.deltas.as_ptr()));
        }
        for frame in 3..20 {
            ring.push_key(frame, AREA, None, screen("k"));
            let open = ring.groups.back().unwrap();
            let storage = (open.key.as_ptr(), open.deltas.as_ptr());
            assert!(seen.contains(&storage), "frame {frame} allocated");
        }
    }

    #[test]
    fn resolve_clamps_steps_to_the_retained_range() {
        let mut ring = FrameRing::new(64 << 20);
        assert_eq!(ring.resolve(9, ScrubStep::Frames(-1)), 9);
        for frame in 0..10u64 {
            delta(&mut ring, frame * 400_000, 1);
        }
        assert_eq!(ring.resolve(5, ScrubStep::Frames(-2)), 3);
        assert_eq!(ring.resolve(2, ScrubStep::Frames(-5)), 1);
        assert_eq!(ring.resolve(9, ScrubStep::Frames(5)), 10);
        assert_eq!(ring.resolve(99, ScrubStep::Frames(0)), 10);
        assert_eq!(ring.resolve(5, ScrubStep::Oldest), 1);
        assert_eq!(ring.resolve(5, ScrubStep::Newest), 10);
        // frame n is painted at (n - 1) * 0.4 s
        assert_eq!(ring.resolve(10, ScrubStep::Seconds(-1)), 7);
        assert_eq!(ring.resolve(2, ScrubStep::Seconds(1)), 5);
        assert_eq!(ring.resolve(2, ScrubStep::Seconds(-1)), 1);
        assert_eq!(ring.resolve(9, ScrubStep::Seconds(1)), 10);
        assert_eq!(ring.age(7), Some(Duration::from_millis(1200)));
    }

    #[test]
    fn cell_view_packs_every_color_kind() {
        let colors = [
            Color::Reset,
            Color::Black,
            Color::LightCyan,
            Color::White,
            Color::Indexed(200),
            Color::Rgb(1, 2, 3),
        ];
        let mut cells = screen("x");
        for (cell, color) in cells.iter_mut().zip(colors) {
            cell.fg = color;
            cell.bg = color;
            cell.underline_color = color;
            cell.modifier = Modifier::BOLD | Modifier::ITALIC;
        }
        let mut ring = FrameRing::new(64 << 20);
        ring.push_key(0, AREA, Some((1, 1)), cells.clone());
        let snapshot = ring.snapshot();
        let frame = snapshot.frames().next().unwrap();
        let views: Vec<_> = frame.cells().collect();
        let mut builder = RingBuilder::new(64 << 20);
        builder.push_key(0, AREA, Some((1, 1)), views.iter().copied());
        let rebuilt = builder.finish().snapshot();
        let again: Vec<_> = rebuilt.frames().next().unwrap().cells().collect();
        assert_eq!(views, again);
        for (view, color) in views.iter().zip(colors) {
            assert_eq!(unpack(view.fg), color);
            assert_eq!(unpack(view.bg), color);
            assert_eq!(unpack(view.ul), color);
        }
        assert_eq!(views[4].fg, 0x0200_00c8);
        assert_eq!(views[5].fg, 0x0301_0203);
    }
}
