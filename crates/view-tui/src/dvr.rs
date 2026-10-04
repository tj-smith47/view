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
use view_core::native::text::clusters;

use crate::paint::fitted_symbol;

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
    /// The bytes the group's symbols hold on the heap.
    heap: usize,
    /// `heap` as the last closed frame left it.
    heap_closed: usize,
}

/// The bytes a symbol stores inline in its cell.
const INLINE_SYMBOL: usize = 24;

/// The bytes `cell`'s symbol holds on the heap.
fn heap_of(cell: &Cell) -> usize {
    let len = cell.symbol().len();
    if len > INLINE_SYMBOL {
        len
    } else {
        0
    }
}

impl Group {
    fn bytes(&self) -> usize {
        self.key.capacity() * std::mem::size_of::<Cell>()
            + self.deltas.capacity() * std::mem::size_of::<(u16, u16, Cell)>()
            + self.frames.capacity() * std::mem::size_of::<Frame>()
            + self.heap
    }

    /// Empties the group and reserves it for `area`.
    fn reset(&mut self, area: (u16, u16)) {
        let cells = usize::from(area.0) * usize::from(area.1);
        self.key.clear();
        self.deltas.clear();
        self.frames.clear();
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
        self.heap = 0;
        self.heap_closed = 0;
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
        self.heap += heap_of(cell);
        self.deltas.push((x, y, cell.clone()));
        true
    }

    /// Drops the cells of the frame being built.
    pub(crate) fn abort(&mut self) {
        let end = self.frames.last().map_or(0, |f| f.deltas_end);
        self.deltas.truncate(end);
        self.heap = self.heap_closed;
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
/// it opens, and an evicted group, its `Arc` included, opens the next one
/// once no snapshot holds it. A capture in steady state allocates only for
/// a symbol longer than 24 bytes.
// ponytail: a keyframe stores every cell, so a full-screen scroll costs a
// whole screen per frame. Row dedup or compression of closed groups is the
// upgrade when a longer rewind under scrolling matters.
#[derive(Debug)]
pub struct FrameRing {
    /// The retained groups, oldest first. The newest is the open one,
    /// written through `Arc::get_mut` while no snapshot shares it.
    groups: VecDeque<Arc<Group>>,
    /// Evicted groups, some still held by a snapshot, kept counted until one
    /// opens a new group or is dropped.
    retired: Vec<Arc<Group>>,
    budget: usize,
    /// The bytes the retained and retired groups hold.
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
            retired: Vec::new(),
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

    /// Closes the delta frame built on the open group and returns its seq,
    /// or `None` when the frame left the ring to keep it inside its budget.
    pub(crate) fn close_delta(&mut self, at_us: u64, cursor: Option<(u16, u16)>) -> Option<u64> {
        let seq = self.next_seq;
        let group = self.groups.back_mut().and_then(Arc::get_mut)?;
        group.frames.push(Frame {
            seq,
            at_us,
            cursor,
            deltas_end: group.deltas.len(),
        });
        self.held += group.heap - group.heap_closed;
        group.heap_closed = group.heap;
        self.next_seq += 1;
        self.trim();
        self.newest().filter(|&newest| newest == seq)
    }

    /// Records a keyframe of size `area` from `cells`, row by row, opening a
    /// new group. Returns its seq, or `None` when the frame goes unrecorded:
    /// one group of this size cannot fit the budget, or snapshots hold the
    /// memory it would take.
    pub(crate) fn push_key(
        &mut self,
        at_us: u64,
        area: (u16, u16),
        cursor: Option<(u16, u16)>,
        cells: impl IntoIterator<Item = Cell>,
    ) -> Option<u64> {
        let mut shared = self.take_group(area)?;
        let seq = self.next_seq;
        let Some(group) = Arc::get_mut(&mut shared) else {
            self.retired.push(shared);
            return None;
        };
        group.key.extend(cells);
        group.heap = group.key.iter().map(heap_of).sum();
        group.heap_closed = group.heap;
        group.frames.push(Frame {
            seq,
            at_us,
            cursor,
            deltas_end: 0,
        });
        self.held += group.heap;
        self.next_seq += 1;
        self.groups.push_back(shared);
        self.trim();
        self.newest().filter(|&newest| newest == seq)
    }

    /// Storage for a new group of size `area`, counted in `held`: a retired
    /// group no snapshot holds first, then a fresh one once evicting the
    /// oldest groups makes room for it.
    fn take_group(&mut self, area: (u16, u16)) -> Option<Arc<Group>> {
        let fresh = Group::reserved_bytes(area);
        if fresh > self.budget {
            return None;
        }
        loop {
            if let Some(at) = self
                .retired
                .iter_mut()
                .position(|g| Arc::get_mut(g).is_some())
            {
                let mut shared = self.retired.swap_remove(at);
                let Some(group) = Arc::get_mut(&mut shared) else {
                    self.retired.push(shared);
                    return None;
                };
                self.held -= group.bytes();
                group.reset(area);
                self.held += group.bytes();
                self.trim();
                if self.held > self.budget {
                    self.held -= shared.bytes();
                    return None;
                }
                return Some(shared);
            }
            if self.held + fresh <= self.budget {
                let mut group = Group::default();
                group.reset(area);
                self.held += group.bytes();
                return Some(Arc::new(group));
            }
            // evicting a group a snapshot holds frees nothing, so while
            // snapshots hold every group the ring keeps its frames
            if self.groups.iter().all(|g| Arc::strong_count(g) > 1) {
                return None;
            }
            let oldest = self.groups.pop_front()?;
            self.retired.push(oldest);
        }
    }

    /// Drops retired groups no snapshot holds, then evicts the oldest
    /// groups, while the ring holds more than its budget. A retired group
    /// grown for a larger screen, or symbols stored on the heap, can cause
    /// that.
    fn trim(&mut self) {
        while self.held > self.budget {
            if let Some(at) = self
                .retired
                .iter_mut()
                .position(|g| Arc::get_mut(g).is_some())
            {
                let dropped = self.retired.swap_remove(at);
                self.held -= dropped.bytes();
            } else if let Some(oldest) = self.groups.pop_front() {
                self.retired.push(oldest);
            } else {
                return;
            }
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

/// The cell `view` was recorded from, holding the symbol's first grapheme
/// cluster. A symbol carrying a control character, which only a damaged
/// clip holds, comes back blank.
fn restore(view: CellView<'_>) -> Cell {
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
/// `area`, dropping it when it falls outside. A symbol wider than the
/// columns left in its row is placed as a blank, since the shadow's diff
/// relies on no cell running past its row.
pub(crate) fn place(screen: &mut [Cell], area: (u16, u16), x: u16, y: u16, cell: &Cell) {
    if x < area.0 && y < area.1 {
        if let Some(slot) = screen.get_mut(usize::from(y) * usize::from(area.0) + usize::from(x)) {
            slot.clone_from(cell);
            let fitted = fitted_symbol(cell.symbol(), area.0 - x);
            if fitted != cell.symbol() {
                slot.set_symbol(fitted);
            }
        }
    }
}

/// Builds a ring from frames decoded out of a clip.
#[derive(Debug)]
pub struct RingBuilder {
    ring: FrameRing,
    /// Whether the last frame went unrecorded, which leaves the deltas
    /// after it nothing to build on until the next keyframe.
    dropped: bool,
}

impl RingBuilder {
    /// An empty builder whose ring holds a recording bound of `max_bytes`.
    #[must_use]
    pub fn new(max_bytes: usize) -> Self {
        Self {
            ring: FrameRing::new(max_bytes),
            dropped: false,
        }
    }

    /// Adds a keyframe of size `area`. A cell outside the area is dropped
    /// and a cell the frame does not name is blank. Returns its seq, or
    /// `None` when the ring could not hold it.
    pub fn push_key<'c>(
        &mut self,
        at_us: u64,
        area: (u16, u16),
        cursor: Option<(u16, u16)>,
        cells: impl IntoIterator<Item = CellView<'c>>,
    ) -> Option<u64> {
        let mut screen = blank(area);
        for view in cells {
            place(&mut screen, area, view.x, view.y, &restore(view));
        }
        let seq = self.ring.push_key(at_us, area, cursor, screen);
        self.dropped = seq.is_none();
        seq
    }

    /// Adds a delta frame on the newest frame. A delta past what the open
    /// group holds becomes a keyframe of the frame it builds. Returns its
    /// seq, or `None` before any keyframe, after a frame the ring could not
    /// hold until the next keyframe, or when the ring cannot hold this one.
    pub fn push_delta<'c>(
        &mut self,
        at_us: u64,
        cursor: Option<(u16, u16)>,
        cells: impl IntoIterator<Item = CellView<'c>> + Clone,
    ) -> Option<u64> {
        let seq = self.delta(at_us, cursor, cells);
        self.dropped = seq.is_none();
        seq
    }

    fn delta<'c>(
        &mut self,
        at_us: u64,
        cursor: Option<(u16, u16)>,
        cells: impl IntoIterator<Item = CellView<'c>> + Clone,
    ) -> Option<u64> {
        if self.dropped {
            return None;
        }
        let newest = self.ring.newest()?;
        let area = self.ring.locate(newest)?.0.area;
        if let Some(group) = self.ring.open_delta(area) {
            let fits = cells
                .clone()
                .into_iter()
                .filter(|v| v.x < area.0 && v.y < area.1)
                .all(|v| group.push_cell(v.x, v.y, &restore(v)));
            if fits {
                return self.ring.close_delta(at_us, cursor);
            }
            group.abort();
        }
        let mut screen = blank(area);
        let (group, index) = self.ring.locate(newest)?;
        group.replay(index, |x, y, cell| place(&mut screen, area, x, y, cell));
        for view in cells {
            place(&mut screen, area, view.x, view.y, &restore(view));
        }
        self.ring.push_key(at_us, area, cursor, screen)
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
            return ring.push_key(at_us, AREA, None, screen("k")).unwrap();
        };
        let changed = Cell::new("d");
        if (0..cells).all(|i| group.push_cell(u16::try_from(i % 4).unwrap(), 0, &changed)) {
            return ring.close_delta(at_us, None).unwrap();
        }
        group.abort();
        ring.push_key(at_us, AREA, None, screen("k")).unwrap()
    }

    fn is_key(ring: &FrameRing, seq: u64) -> bool {
        ring.locate(seq).is_some_and(|(_, index)| index == 0)
    }

    /// The bytes every group the ring keeps alive holds, counted from the
    /// groups themselves.
    fn counted(ring: &FrameRing) -> usize {
        ring.groups
            .iter()
            .chain(&ring.retired)
            .map(|g| g.bytes())
            .sum()
    }

    /// Where the open group's header and storage live.
    fn storage(ring: &FrameRing) -> (*const Group, *const Cell, *const (u16, u16, Cell)) {
        let open = ring.groups.back().unwrap();
        (Arc::as_ptr(open), open.key.as_ptr(), open.deltas.as_ptr())
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
            let live = counted(&ring);
            assert_eq!(live, ring.held, "frame {frame}");
            assert!(live <= ring.budget, "frame {frame}: {live}");
        }
        assert!(ring.groups.len() > 1);
        assert!(ring.oldest().unwrap() > 1, "the oldest groups were dropped");
    }

    #[test]
    fn a_resize_opens_a_new_keyframe() {
        let mut ring = FrameRing::new(64 << 20);
        let first = ring.push_key(0, AREA, None, screen("a")).unwrap();
        let second = delta(&mut ring, 1, 1);
        assert!(!is_key(&ring, second));
        assert!(ring.open_delta((5, 2)).is_none());
        let resized = ring
            .push_key(2, (5, 2), None, vec![Cell::new("b"); 10])
            .unwrap();
        assert!(is_key(&ring, resized));
        assert_eq!((first, second, resized), (1, 2, 3));
        assert_eq!(ring.locate(resized).unwrap().0.area, (5, 2));
    }

    #[test]
    fn steady_capture_reuses_pooled_storage() {
        let mut ring = FrameRing::with_budget(Group::reserved_bytes(AREA) * 3);
        let mut seen = Vec::new();
        for frame in 0..3 {
            ring.push_key(frame, AREA, None, screen("k")).unwrap();
            seen.push(storage(&ring));
        }
        for frame in 3..20 {
            ring.push_key(frame, AREA, None, screen("k")).unwrap();
            assert!(seen.contains(&storage(&ring)), "frame {frame} allocated");
        }
    }

    #[test]
    fn a_group_reused_for_a_smaller_screen_gives_its_memory_back() {
        let mut group = Group::default();
        group.reset((40, 20));
        group.reset(AREA);
        assert_eq!(group.bytes(), Group::reserved_bytes(AREA));
    }

    #[test]
    fn a_held_snapshot_keeps_the_ring_inside_its_budget() {
        let mut ring = FrameRing::with_budget(Group::reserved_bytes(AREA) * 4);
        let mut seen = Vec::new();
        for frame in 0..2 {
            ring.push_key(frame, AREA, None, screen("k")).unwrap();
            seen.push(storage(&ring));
        }
        let snapshot = ring.snapshot();
        for frame in 2..40 {
            assert!(ring.push_key(frame, AREA, None, screen("k")).is_some());
            if frame < 4 {
                seen.push(storage(&ring));
            }
            assert!(seen.contains(&storage(&ring)), "frame {frame} allocated");
            let live = counted(&ring);
            assert_eq!(live, ring.held, "frame {frame}");
            assert!(live <= ring.budget, "frame {frame}: {live}");
        }
        assert_eq!(snapshot.frames().count(), 2, "the snapshot kept its frames");
        drop(snapshot);
        for frame in 40..60 {
            ring.push_key(frame, AREA, None, screen("k")).unwrap();
            assert!(seen.contains(&storage(&ring)), "frame {frame} allocated");
            assert!(counted(&ring) <= ring.budget);
        }
    }

    #[test]
    fn a_ring_every_snapshot_holds_keeps_its_frames_and_records_nothing() {
        let mut ring = FrameRing::with_budget(Group::reserved_bytes(AREA) * 2);
        ring.push_key(0, AREA, None, screen("a")).unwrap();
        ring.push_key(1, AREA, None, screen("b")).unwrap();
        let snapshot = ring.snapshot();
        assert_eq!(ring.push_key(2, AREA, None, screen("c")), None);
        assert_eq!((ring.oldest(), ring.newest()), (Some(1), Some(2)));
        drop(snapshot);
        assert_eq!(ring.push_key(3, AREA, None, screen("d")), Some(3));
    }

    #[test]
    fn a_restored_cell_holds_one_grapheme_cluster() {
        let view = CellView {
            x: 0,
            y: 0,
            symbol: "e\u{301}x",
            fg: 0,
            bg: 0,
            ul: 0,
            modifier: 0,
        };
        assert_eq!(restore(view).symbol(), "e\u{301}");
    }

    #[test]
    fn symbols_stored_on_the_heap_count_toward_the_budget() {
        let long: &'static str =
            "e\u{301}e\u{301}e\u{301}e\u{301}e\u{301}e\u{301}e\u{301}e\u{301}e\u{301}";
        assert!(long.len() > INLINE_SYMBOL);
        let mut ring = FrameRing::new(64 << 20);
        ring.push_key(0, AREA, None, vec![Cell::new(long); 8])
            .unwrap();
        let reserved = Group::reserved_bytes(AREA);
        assert_eq!(ring.held, reserved + 8 * long.len());
        let group = ring.open_delta(AREA).unwrap();
        assert!(group.push_cell(0, 0, &Cell::new(long)));
        ring.close_delta(1, None).unwrap();
        assert_eq!(ring.held, reserved + 9 * long.len());
        assert_eq!(counted(&ring), ring.held);
    }

    #[test]
    fn a_frame_the_ring_cannot_hold_is_reported_and_no_delta_builds_on_it() {
        let small = (2, 1);
        let mut builder = RingBuilder::new(Group::reserved_bytes(small) * 3);
        let cell = |x| CellView {
            x,
            y: 0,
            symbol: "a",
            fg: 0,
            bg: 0,
            ul: 0,
            modifier: 0,
        };
        assert_eq!(builder.push_key(0, small, None, [cell(0)]), Some(1));
        assert_eq!(builder.push_key(1, (40, 40), None, [cell(0)]), None);
        assert_eq!(builder.push_delta(2, None, [cell(1)]), None);
        assert_eq!(builder.push_key(3, small, None, [cell(1)]), Some(2));
        assert_eq!(builder.push_delta(4, None, [cell(0)]), Some(3));
        let mut ring = FrameRing::with_budget(1);
        assert_eq!(ring.push_key(0, AREA, None, screen("k")), None);
        assert_eq!(ring.newest(), None);
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
