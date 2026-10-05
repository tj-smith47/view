//! The session DVR's painted frames: a ring of keyframe groups held inside
//! the recording's memory bound, read back one frame at a time to repaint
//! what the screen showed.
//!
//! The public API carries no `ratatui` type. A recorded cell leaves this
//! crate as a [`CellView`], its colors packed into `u32`s.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use ratatui::buffer::Cell;
use view_core::native::dvr::{input_log_bytes, ScrubStep};

use crate::paint::fitted_symbol;

mod builder;
mod cell;
mod group;

pub use builder::RingBuilder;
pub(crate) use cell::row_hash;
pub use cell::CellView;
pub(crate) use group::Group;
pub use group::Scroll;
use group::{Frame, GROUP_FRAMES};

/// The one-second scrub step, in the microseconds frames are dated in.
const SECOND_US: i64 = 1_000_000;

/// The painted frames of the session, oldest whole groups dropped once the
/// recording's memory bound is reached.
///
/// A cell is stored packed, its symbol entered once in its group's table,
/// and a frame that shows the one before it shifted stores the shift and
/// the cells the shift does not explain. Every group reserves its keyframe,
/// its delta list and its frame list when it opens, and an evicted group,
/// its `Arc` included, opens the next one once no snapshot holds it. A
/// capture in steady state allocates only for a symbol its group has not
/// seen.
// ponytail: closed groups stay uncompressed. Compressing them is the
// upgrade when a longer rewind matters more than a scrub step's cost.
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
    /// The bytes the snapshots' copies of the open group hold, given back
    /// as each snapshot drops, on whatever thread it drops.
    copied: Arc<AtomicUsize>,
    next_seq: u64,
    /// Row hashes and votes scroll detection reuses from frame to frame.
    pub(crate) scratch: Vec<u64>,
    /// What the newest capture stored.
    pub(crate) last: Capture,
}

/// What one capture compared and stored, the line the recording's
/// diagnostic log writes per frame.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct Capture {
    /// The frame's seq, `None` when the ring kept no frame.
    pub seq: Option<u64>,
    /// Whether the frame was stored whole.
    pub key: bool,
    /// The painted cells that differ from the frame before, 0 on a frame
    /// stored whole with no recorded frame of its size before it.
    pub changed: usize,
    /// The columns scroll detection judged to have moved.
    pub span: Option<(u16, u16)>,
    /// The shift stored.
    pub scroll: Option<Scroll>,
    /// The rows the shift's distance explains cell for cell.
    pub explained: usize,
    /// The cells stored, 0 when the ring kept no frame.
    pub pushed: usize,
}

impl std::fmt::Display for Capture {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let seq = self.seq.map_or_else(|| "-".to_owned(), |s| s.to_string());
        write!(f, "seq={seq} key={} changed={}", self.key, self.changed)?;
        if let Some((left, right)) = self.span {
            write!(f, " span={left}..{right}")?;
        }
        if let Some(s) = self.scroll {
            write!(f, " by={} run={}..{}", s.by, s.top, s.bottom)?;
        }
        write!(f, " explained={} pushed={}", self.explained, self.pushed)
    }
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
            copied: Arc::default(),
            next_seq: 1,
            scratch: Vec::new(),
            last: Capture::default(),
        }
    }

    /// What the newest capture compared and stored.
    #[must_use]
    pub fn last_capture(&self) -> Capture {
        self.last
    }

    /// The bytes the ring holds, snapshot copies included.
    #[must_use]
    pub fn held(&self) -> usize {
        self.held
            .saturating_add(self.copied.load(Ordering::Relaxed))
    }

    /// How many frames the ring holds.
    #[must_use]
    pub fn frame_count(&self) -> usize {
        self.groups.iter().map(|group| group.frames.len()).sum()
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

    /// The retained groups, the closed ones shared with the caller at the
    /// cost of one `Arc` clone each and the open one copied, so frames
    /// recorded while the snapshot is held keep building on the open group.
    /// The copy counts toward the ring's budget until the snapshot drops,
    /// and the oldest groups are evicted to make room for it before it is
    /// taken. `None`, evicting nothing, when no group the ring may evict
    /// makes that room: the open group, or earlier snapshots, hold what the
    /// copy would need.
    #[must_use]
    pub fn snapshot(&mut self) -> Option<RingSnapshot> {
        let bytes = self.groups.back().map_or(0, |g| g.bytes());
        let needed = self.held().saturating_add(bytes);
        if needed.saturating_sub(self.freeable(1)) > self.budget || !self.trim_to(bytes, 1) {
            return None;
        }
        let closed = self.groups.len().saturating_sub(1);
        let open = self.groups.back().map(|g| Arc::new(Group::clone(g)));
        let copy = open.as_ref().map(|_| {
            self.copied.fetch_add(bytes, Ordering::Relaxed);
            Arc::new(CopyHeld {
                copied: Arc::clone(&self.copied),
                bytes,
            })
        });
        Some(RingSnapshot {
            groups: self
                .groups
                .iter()
                .take(closed)
                .cloned()
                .chain(open)
                .collect(),
            _copy: copy,
        })
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

    /// Closes the delta frame built on the open group, `scroll` applied
    /// ahead of its cells, and returns its seq, or `None` when the frame
    /// left the ring to keep it inside its budget.
    pub(crate) fn close_delta(
        &mut self,
        at_us: u64,
        cursor: Option<(u16, u16)>,
        scroll: Option<Scroll>,
    ) -> Option<u64> {
        let seq = self.next_seq;
        let group = self.groups.back_mut().and_then(Arc::get_mut)?;
        group.frames.push(Frame {
            seq,
            at_us,
            cursor,
            deltas_end: group.deltas.len(),
            scroll,
        });
        self.held = settle(self.held, group);
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
        // symbols the open group entered since its last frame closed are
        // counted before the group can be evicted to make room
        if let Some(open) = self.groups.back_mut().and_then(Arc::get_mut) {
            self.held = settle(self.held, open);
        }
        let mut shared = self.take_group(area)?;
        let seq = self.next_seq;
        let Some(group) = Arc::get_mut(&mut shared) else {
            self.retired.push(shared);
            return None;
        };
        for cell in cells {
            let packed = group.symbols.pack(&cell);
            group.key.push(packed);
        }
        group.frames.push(Frame {
            seq,
            at_us,
            cursor,
            deltas_end: 0,
            scroll: None,
        });
        self.held = settle(self.held, group);
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
                group.reset(area);
                self.held = settle(self.held, group);
                self.trim();
                if self.held() > self.budget {
                    self.held -= shared.counted;
                    return None;
                }
                return Some(shared);
            }
            if self.held() + fresh <= self.budget {
                let mut group = Group::default();
                group.reset(area);
                self.held = settle(self.held, &mut group);
                return Some(Arc::new(group));
            }
            self.evict_oldest()?;
        }
    }

    /// Drops retired groups no snapshot holds, then evicts the oldest
    /// groups, while the ring holds more than its budget. A retired group
    /// grown for a larger screen, the symbols a group entered, or a
    /// snapshot's copy of the open group can cause that.
    fn trim(&mut self) {
        self.trim_to(0, 0);
    }

    /// Frees memory the way [`Self::trim`] does until `extra` more bytes
    /// fit the budget, evicting no group while `keep` or fewer remain.
    /// Returns whether they fit.
    fn trim_to(&mut self, extra: usize, keep: usize) -> bool {
        while self.held().saturating_add(extra) > self.budget {
            if let Some(at) = self
                .retired
                .iter_mut()
                .position(|g| Arc::get_mut(g).is_some())
            {
                let dropped = self.retired.swap_remove(at);
                self.held -= dropped.counted;
            } else if self.groups.len() <= keep || self.evict_oldest().is_none() {
                return false;
            }
        }
        true
    }

    /// The bytes [`Self::trim_to`] can free while leaving `keep` groups:
    /// the retired groups no snapshot holds and the oldest groups up to the
    /// first one a snapshot holds.
    fn freeable(&self, keep: usize) -> usize {
        let unheld = |g: &&Arc<Group>| Arc::strong_count(g) == 1 && Arc::weak_count(g) == 0;
        let evictable = self.groups.len().saturating_sub(keep);
        let oldest = self.groups.iter().take(evictable).take_while(unheld);
        self.retired
            .iter()
            .filter(unheld)
            .chain(oldest)
            .map(|g| g.counted)
            .sum()
    }

    /// Moves the oldest group to the retired list. `None`, evicting
    /// nothing, when there is none or a snapshot holds it: its memory stays
    /// held either way, so the ring keeps its frames and records no new
    /// group until the snapshot drops.
    fn evict_oldest(&mut self) -> Option<()> {
        if Arc::strong_count(self.groups.front()?) > 1 {
            return None;
        }
        let oldest = self.groups.pop_front()?;
        self.retired.push(oldest);
        Some(())
    }
}

/// A snapshot's copy of the open group, counted in its ring's budget until
/// the last clone of the snapshot drops.
#[derive(Debug)]
struct CopyHeld {
    copied: Arc<AtomicUsize>,
    bytes: usize,
}

impl Drop for CopyHeld {
    fn drop(&mut self) {
        self.copied.fetch_sub(self.bytes, Ordering::Relaxed);
    }
}

/// The groups of a ring at one moment, readable while the ring records on.
#[derive(Debug, Clone)]
pub struct RingSnapshot {
    groups: Vec<Arc<Group>>,
    _copy: Option<Arc<CopyHeld>>,
}

impl RingSnapshot {
    /// Every frame of the snapshot, oldest first.
    pub fn frames(&self) -> impl Iterator<Item = FrameView<'_>> {
        self.groups.iter().flat_map(|group| {
            let mut start = 0;
            group.frames.iter().enumerate().map(move |(i, f)| {
                let deltas = (i > 0).then_some((start, f.deltas_end));
                start = f.deltas_end;
                FrameView {
                    seq: f.seq,
                    at_us: f.at_us,
                    area: group.area,
                    cursor: f.cursor,
                    key: i == 0,
                    scroll: f.scroll,
                    group,
                    deltas,
                }
            })
        })
    }
}

/// The bytes the ring counts once `group` is settled: `held` with the
/// group's last count replaced by what it holds now.
fn settle(held: usize, group: &mut Group) -> usize {
    let now = group.bytes();
    let held = held - group.counted + now;
    group.counted = now;
    held
}

/// One recorded frame: a whole screen when it is a keyframe, the shift of
/// the frame before it and the cells that shift does not explain otherwise.
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
    /// The shift of the frame before, applied ahead of a delta's cells.
    pub scroll: Option<Scroll>,
    group: &'a Group,
    /// The frame's range of the group's delta list, `None` for a keyframe.
    deltas: Option<(usize, usize)>,
}

impl<'a> FrameView<'a> {
    /// The frame's cells, row by row for a keyframe and in paint order for
    /// a delta.
    pub fn cells(&self) -> impl Iterator<Item = CellView<'a>> + 'a {
        let group = self.group;
        let (key, delta) = match self.deltas {
            None => (group.key.as_slice(), &[][..]),
            Some((start, end)) => (&[][..], group.deltas.get(start..end).unwrap_or_default()),
        };
        let width = usize::from(group.area.0.max(1));
        let keyed = key.iter().enumerate().map(move |(i, &cell)| {
            let x = u16::try_from(i % width).unwrap_or(u16::MAX);
            let y = u16::try_from(i / width).unwrap_or(u16::MAX);
            group.symbols.view(x, y, cell)
        });
        keyed.chain(
            delta
                .iter()
                .map(move |&(x, y, cell)| group.symbols.view(x, y, cell)),
        )
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

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use ratatui::style::{Color, Modifier};

    use super::cell::{unpack, Packed};
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
            return ring.close_delta(at_us, None, None).unwrap();
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
    fn storage(ring: &FrameRing) -> (*const Group, *const Packed, *const (u16, u16, Packed)) {
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
    fn a_group_reused_after_a_screen_of_distinct_glyphs_gives_its_table_back() {
        let mut group = Group::default();
        group.reset((40, 20));
        for (i, glyph) in ('\u{4e00}'..).take(600).enumerate() {
            let (x, y) = (
                u16::try_from(i % 40).unwrap(),
                u16::try_from(i / 40).unwrap(),
            );
            let mut cell = Cell::EMPTY;
            cell.set_symbol(glyph.encode_utf8(&mut [0; 4]));
            assert!(group.push_cell(x, y, &cell));
        }
        assert!(group.bytes() > Group::reserved_bytes((40, 20)) + 600 * 3);
        group.reset((40, 20));
        assert!(
            group.bytes() <= Group::reserved_bytes((40, 20)) + 50 * 40,
            "{}",
            group.bytes()
        );
    }

    #[test]
    fn a_held_snapshot_keeps_the_ring_inside_its_budget() {
        let mut ring = FrameRing::with_budget(Group::reserved_bytes(AREA) * 4);
        let mut seen = Vec::new();
        for frame in 0..2 {
            ring.push_key(frame, AREA, None, screen("k")).unwrap();
            seen.push(storage(&ring));
        }
        let snapshot = ring.snapshot().unwrap();
        let copy = ring.held() - ring.held;
        assert!(copy >= AREA.0 as usize * AREA.1 as usize, "{copy}");
        // the copy of the open group takes its room and the next group
        // fits; past it the ring keeps the frames the snapshot holds and
        // records none
        assert_eq!(ring.push_key(2, AREA, None, screen("k")), Some(3));
        seen.push(storage(&ring));
        for frame in 3..40 {
            assert_eq!(ring.push_key(frame, AREA, None, screen("k")), None);
            assert_eq!(counted(&ring), ring.held, "frame {frame}");
            assert!(ring.held() <= ring.budget, "frame {frame}");
        }
        assert_eq!(ring.oldest(), Some(1), "the ring kept its frames");
        assert_eq!(snapshot.frames().count(), 2, "the snapshot kept its frames");
        drop(snapshot);
        assert_eq!(ring.held(), ring.held, "the copy is given back");
        for frame in 40..60 {
            ring.push_key(frame, AREA, None, screen("k")).unwrap();
            if frame == 40 {
                seen.push(storage(&ring));
            }
            assert!(seen.contains(&storage(&ring)), "frame {frame} allocated");
            assert!(counted(&ring) <= ring.budget);
        }
    }

    #[test]
    fn a_snapshot_taken_at_budget_makes_room_for_its_copy() {
        let mut ring = FrameRing::with_budget(Group::reserved_bytes(AREA) * 2);
        ring.push_key(0, AREA, None, screen("a")).unwrap();
        ring.push_key(1, AREA, None, screen("b")).unwrap();
        assert_eq!(ring.held(), ring.budget, "the ring is at its budget");
        let snapshot = ring.snapshot().unwrap();
        assert!(ring.held() <= ring.budget, "{} held", ring.held());
        let held: Vec<_> = snapshot.frames().map(|f| f.seq).collect();
        assert_eq!(held, [2], "the oldest group made room for the copy");
        // the open group alone at budget leaves nothing to evict, and a
        // second snapshot finds the first one holding what is left
        assert!(ring.snapshot().is_none());
        drop(snapshot);
        let mut alone = FrameRing::with_budget(Group::reserved_bytes(AREA));
        alone.push_key(0, AREA, None, screen("a")).unwrap();
        assert!(alone.snapshot().is_none());
        assert_eq!(alone.held(), alone.budget);
    }

    #[test]
    fn a_snapshot_with_no_room_for_its_copy_evicts_nothing() {
        let mut ring = FrameRing::new(64 << 20);
        ring.push_key(0, AREA, None, screen("a")).unwrap();
        ring.push_key(1, AREA, None, screen("b")).unwrap();
        let group = ring.open_delta(AREA).unwrap();
        for (x, symbol) in ["世", "界", "漢", "字"].into_iter().enumerate() {
            assert!(group.push_cell(u16::try_from(x).unwrap(), 0, &Cell::new(symbol)));
        }
        ring.close_delta(2, None, None).unwrap();
        ring.budget = ring.held();
        assert!(ring.snapshot().is_none());
        assert_eq!(
            ring.oldest(),
            Some(1),
            "the refused snapshot kept the history"
        );
        assert_eq!(ring.groups.len(), 2);
    }

    #[test]
    fn a_held_snapshot_leaves_the_open_group_taking_deltas() {
        let mut ring = FrameRing::with_budget(Group::reserved_bytes(AREA) * 3);
        ring.push_key(0, AREA, None, screen("a")).unwrap();
        ring.push_key(1, AREA, None, screen("b")).unwrap();
        let snapshot = ring.snapshot().unwrap();
        assert_eq!(delta(&mut ring, 2, 1), 3);
        assert!(!is_key(&ring, 3), "the frame is a delta on the open group");
        // a new group has no room while the snapshot holds the oldest
        assert_eq!(ring.push_key(3, AREA, None, screen("c")), None);
        let held: Vec<_> = snapshot.frames().map(|f| f.seq).collect();
        assert_eq!(held, [1, 2], "the snapshot kept the frames it took");
        drop(snapshot);
        assert_eq!(ring.push_key(4, AREA, None, screen("c")), Some(4));
    }

    #[test]
    fn the_symbols_a_group_enters_count_toward_the_budget() {
        let mut ring = FrameRing::new(64 << 20);
        ring.push_key(0, AREA, None, screen("a")).unwrap();
        let reserved = Group::reserved_bytes(AREA);
        assert_eq!(ring.held, reserved, "ASCII enters no symbol");
        let group = ring.open_delta(AREA).unwrap();
        for (x, symbol) in ["世", "界", "e\u{301}"].into_iter().enumerate() {
            assert!(group.push_cell(u16::try_from(x).unwrap(), 0, &Cell::new(symbol)));
        }
        ring.close_delta(1, None, None).unwrap();
        assert!(ring.held > reserved);
        assert_eq!(counted(&ring), ring.held);
        let group = ring.open_delta(AREA).unwrap();
        assert!(group.push_cell(0, 1, &Cell::new("漢")));
        ring.push_key(2, AREA, None, screen("b")).unwrap();
        assert_eq!(
            counted(&ring),
            ring.held,
            "an aborted open frame is settled"
        );
    }

    #[test]
    fn a_frame_the_ring_cannot_hold_is_reported_and_no_delta_builds_on_it() {
        let small = (2, 1);
        let mut builder = RingBuilder::new(Group::reserved_bytes(small) * 3);
        let cell = |x| CellView::new(x, 0, "a", [0; 3], 0);
        assert_eq!(builder.push_key(0, small, None, [cell(0)]), Some(1));
        assert_eq!(builder.push_key(1, (40, 40), None, [cell(0)]), None);
        assert_eq!(builder.push_delta(2, None, None, [cell(1)]), None);
        // a screen this size is refused before four billion cells are allocated
        let huge = (u16::MAX, u16::MAX);
        assert_eq!(builder.push_key(2, huge, None, [cell(0)]), None);
        assert_eq!(builder.push_delta(2, None, None, [cell(1)]), None);
        assert_eq!(builder.push_key(3, small, None, [cell(1)]), Some(2));
        assert_eq!(builder.push_delta(4, None, None, [cell(0)]), Some(3));
        let mut ring = FrameRing::with_budget(1);
        assert_eq!(ring.push_key(0, AREA, None, screen("k")), None);
        assert_eq!(ring.newest(), None);
    }

    /// Frame `seq` of `ring` as one string of symbols, row-major.
    fn shown(ring: &FrameRing, seq: u64) -> String {
        let (group, index) = ring.locate(seq).unwrap();
        let mut screen = Vec::new();
        group.replay(index, &mut screen);
        screen.iter().map(Cell::symbol).collect()
    }

    #[test]
    fn a_built_shift_fits_falls_back_to_a_keyframe_or_is_refused() {
        let area = (2, 3);
        let mut builder = RingBuilder::new(64 << 20);
        let at = |x, y, symbol| CellView::new(x, y, symbol, [0; 3], 0);
        let rows = [at(0, 0, "a"), at(0, 1, "b"), at(0, 2, "c")];
        assert_eq!(builder.push_key(0, area, None, rows), Some(1));
        let up = Scroll::new(0..2, 0..2, 1);
        assert_eq!(
            builder.push_delta(1, None, Some(up), [at(0, 2, "d")]),
            Some(2)
        );
        // the delta list holds a screen of cells, so this frame fills it
        let all: Vec<_> = (1..6).map(|i| at(i % 2, i / 2, "e")).collect();
        assert_eq!(builder.push_delta(2, None, None, all), Some(3));
        let down = Scroll::new(1..3, 0..2, -1);
        assert_eq!(
            builder.push_delta(3, None, Some(down), [at(1, 0, "f")]),
            Some(4)
        );
        let reaching = Scroll::new(0..3, 0..2, 1);
        assert_eq!(builder.push_delta(4, None, Some(reaching), []), None);
        assert_eq!(builder.push_delta(5, None, None, [at(0, 0, "g")]), None);
        let ring = builder.finish();
        assert_eq!(shown(&ring, 2), "b c d ");
        assert!(!is_key(&ring, 2) && !is_key(&ring, 3));
        assert!(is_key(&ring, 4), "a full group closes into a keyframe");
        assert_eq!(shown(&ring, 3), "beeeee");
        assert_eq!(shown(&ring, 4), "bfbeee");
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
        let snapshot = ring.snapshot().unwrap();
        let frame = snapshot.frames().next().unwrap();
        let views: Vec<_> = frame.cells().collect();
        let mut builder = RingBuilder::new(64 << 20);
        builder.push_key(0, AREA, Some((1, 1)), views.iter().copied());
        let rebuilt = builder.finish().snapshot().unwrap();
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
