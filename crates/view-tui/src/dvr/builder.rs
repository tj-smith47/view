//! Builds a ring from frames decoded out of a clip.

use super::cell::{restore, CellView};
use super::group::Scroll;
use super::{blank, place, FrameRing};

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

    /// Adds a delta frame on the newest frame: `scroll` shifts it, then
    /// `cells` are painted on top. A delta past what the open group holds
    /// becomes a keyframe of the frame it builds. Returns its seq, or
    /// `None` before any keyframe, after a frame the ring could not hold
    /// until the next keyframe, for a shift reaching outside the screen, or
    /// when the ring cannot hold this one.
    pub fn push_delta<'c>(
        &mut self,
        at_us: u64,
        cursor: Option<(u16, u16)>,
        scroll: Option<Scroll>,
        cells: impl IntoIterator<Item = CellView<'c>> + Clone,
    ) -> Option<u64> {
        let seq = self.delta(at_us, cursor, scroll, cells);
        self.dropped = seq.is_none();
        seq
    }

    fn delta<'c>(
        &mut self,
        at_us: u64,
        cursor: Option<(u16, u16)>,
        scroll: Option<Scroll>,
        cells: impl IntoIterator<Item = CellView<'c>> + Clone,
    ) -> Option<u64> {
        if self.dropped {
            return None;
        }
        let newest = self.ring.newest()?;
        let area = self.ring.locate(newest)?.0.area;
        if scroll.is_some_and(|s| !s.fits(area)) {
            return None;
        }
        if let Some(group) = self.ring.open_delta(area) {
            let fits = cells
                .clone()
                .into_iter()
                .filter(|v| v.x < area.0 && v.y < area.1)
                .all(|v| group.push_cell(v.x, v.y, &restore(v)));
            if fits {
                return self.ring.close_delta(at_us, cursor, scroll);
            }
            group.abort();
        }
        let mut screen = Vec::new();
        let (group, index) = self.ring.locate(newest)?;
        group.replay(index, &mut screen);
        if let Some(scroll) = scroll {
            scroll.apply(&mut screen, area.0);
        }
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
