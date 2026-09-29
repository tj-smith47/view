//! The frame a restart holds on screen, and the reads the compositor paints
//! through so that it shows the dead engine's last frame until the
//! replacement puts a window up, then the replacement's windows in the dead
//! engine's slots until the replacement's own layout has settled.

use std::collections::HashMap;

use super::{EngineModel, Model, MouseCapture, WindowStatus};
use crate::events::WinHandle;
use crate::grid::registry::{Dock, GridRegistry, WindowSlot};
use crate::grid::Grid;
use crate::hl::{HlAttr, HlTable};
use crate::msg::Effect;

/// How long the replacement's windows are drawn in the dead engine's slots
/// at most, from the first window it puts on screen. A config opens its
/// sidebars from startup autocmds a plugin manager runs after the attach,
/// which takes a few hundred milliseconds on a loaded config; a replacement
/// that never reopens one hands the screen back to its own layout here.
pub(crate) const RESTART_LAYOUT_HOLD: std::time::Duration = std::time::Duration::from_secs(2);

/// What the dead engine last reported for each window of the held frame, by
/// handle: the buffer a replacement window is matched by, and the title and
/// segments its tile keeps until the replacement reports its own.
type Shown = HashMap<WinHandle, WindowStatus>;

/// Where the dead engine's highlight ids are moved to in the table a held
/// layout paints with, clear of the ids the replacement defines, which
/// nvim counts up from 1.
const HELD_HL_BASE: u64 = 1 << 32;

/// A dead engine's highlight id as the held layout's table numbers it. Id 0
/// is the default colours in every engine, so it stays.
fn held_hl(id: u64) -> u64 {
    if id == 0 {
        0
    } else {
        id.saturating_add(HELD_HL_BASE)
    }
}

/// What a restart keeps on screen in place of the replacement's own frame.
#[derive(Debug, Clone, Default)]
pub(crate) struct Held {
    /// The frame or layout held.
    hold: Hold,
    /// Whether the live registry carries part of a batch nvim has not
    /// flushed yet, which a hold settled from a window's status report
    /// would draw half written.
    torn: bool,
}

/// The frame or layout a restart holds.
#[derive(Debug, Clone, Default)]
enum Hold {
    /// The replacement's own frame is on screen.
    #[default]
    Nothing,
    /// The registry the dead engine last painted and the highlight table
    /// its cells were drawn with, until the replacement puts a window on
    /// screen that the dead engine's slots can hold.
    ///
    /// The replacement's attach reaches that frame over several flushes, and
    /// the ones before it carry a cleared grid and no window, so painting
    /// the live registry through them paints an empty screen. Its first
    /// batch also redefines the highlight ids the held cells carry.
    Frame {
        /// The dead engine's registry.
        grids: GridRegistry,
        /// The dead engine's highlight table.
        hl: HlTable,
        /// What each held window showed.
        shown: Shown,
        /// Whether `shown` has been read from the dead engine's statuses.
        /// A frame held over from a failed attempt has been, and a status
        /// reported since then names a window of that attempt.
        named: bool,
        /// Whether the replacement has put a window up, which is when
        /// [`RESTART_LAYOUT_HOLD`] starts.
        armed: bool,
        /// Whether the dead engine's slots are held once the replacement
        /// puts a window up: a resize leaves them sized for a screen that
        /// is gone.
        layout: bool,
    },
    /// The windows the dead engine last had on screen, and the
    /// replacement's registry drawn in them
    /// ([`GridRegistry::laid_out_as`]).
    ///
    /// The attach draws the file alone, and a config reopens its sidebars
    /// from its own startup autocmds after that, so painting the live
    /// registry puts the file across the whole width and then back. A slot
    /// shows the dead engine's cells until the replacement draws text in
    /// it, and its tile the dead engine's title and segments until the
    /// replacement reports its window.
    Layout {
        /// The dead engine's windows, as handle and slot.
        slots: Vec<WindowSlot>,
        /// What each of those windows showed.
        shown: Shown,
        /// The cells each slot showed, by index into `slots`, with their
        /// highlight ids moved by [`held_hl`].
        cells: Vec<Option<Grid>>,
        /// The dead engine's highlight attributes, under the ids `cells`
        /// carries.
        attrs: Vec<(u64, HlAttr)>,
        /// The live registry drawn in `slots`.
        drawn: GridRegistry,
        /// The live highlight table with `attrs` added.
        hl: HlTable,
        /// The live table's [`HlTable::revision`] `hl` was built from, or
        /// `None` before it was built.
        hl_from: Option<u64>,
    },
}

/// Where the replacement's windows stand against the held slots.
enum Fit {
    /// The held slots, each relabelled with the live window that fills it.
    Slots(Vec<WindowSlot>),
    /// A live window has no slot by handle, and its buffer is not known yet.
    Unnamed,
    /// A live window fills no held slot.
    Outside,
}

/// Matches each live window to a held slot: by handle, or else by the
/// buffer it shows, since a fresh engine numbers its windows from 1000
/// again and the dead engine's file window may have had another handle.
fn fit(
    slots: &[WindowSlot],
    shown: &Shown,
    live: &[WindowSlot],
    status: &HashMap<WinHandle, WindowStatus>,
) -> Fit {
    let mut fitted = slots.to_vec();
    for (win, _) in live {
        if fitted.iter().any(|(held, _)| held == win) {
            continue;
        }
        let taken = |held: &WinHandle| live.iter().any(|(w, _)| w == held);
        let Some(name) = status.get(win).map(|s| &s.name) else {
            // waiting on the report is worth it only for a slot that has a
            // buffer to match
            let open = fitted
                .iter()
                .any(|(held, _)| shown.contains_key(held) && !taken(held));
            return if open { Fit::Unnamed } else { Fit::Outside };
        };
        let Some(slot) = fitted
            .iter_mut()
            .find(|(held, _)| shown.get(held).map(|s| &s.name) == Some(name) && !taken(held))
        else {
            return Fit::Outside;
        };
        slot.0 = *win;
    }
    Fit::Slots(fitted)
}

impl EngineModel {
    /// The engine grid the screen shows, for the compositor: the dead
    /// engine's last one while a restart holds its frame.
    #[must_use]
    pub fn painted_grid(&self) -> &Grid {
        self.painted_grids().global()
    }

    /// Every grid and pane the screen shows, for the compositor: the dead
    /// engine's last registry while a restart holds its frame, the live one
    /// drawn in the dead engine's slots while it holds the layout, and
    /// [`Self::grids`] otherwise.
    #[must_use]
    #[inline]
    pub fn painted_grids(&self) -> &GridRegistry {
        match &self.held.hold {
            Hold::Nothing => &self.grids,
            Hold::Frame { grids, .. } => grids,
            Hold::Layout { drawn, .. } => drawn,
        }
    }

    /// The highlight table the screen's cells are drawn with, for the
    /// compositor: the dead engine's while a restart holds its frame, the
    /// live one with the dead engine's added while it holds the layout, and
    /// [`Self::hl`] otherwise.
    #[must_use]
    #[inline]
    pub fn painted_hl(&self) -> &HlTable {
        match &self.held.hold {
            Hold::Frame { hl, .. } | Hold::Layout { hl, .. } => hl,
            Hold::Nothing => &self.hl,
        }
    }

    /// Whether a restart keeps something other than the live registry on
    /// screen.
    #[must_use]
    pub fn holds_the_screen(&self) -> bool {
        !matches!(self.held.hold, Hold::Nothing)
    }

    /// Whether the screen shows the dead engine's own frame, whose grid ids
    /// name none of the replacement's grids.
    #[must_use]
    pub(crate) fn holds_the_frame(&self) -> bool {
        matches!(self.held.hold, Hold::Frame { .. })
    }

    /// Records the floats docked to a side of the screen on the live
    /// registry and on the one a restart holds, and answers whether that
    /// changed the live one.
    pub(crate) fn set_docks(&mut self, docks: &[Dock]) -> bool {
        match &mut self.held.hold {
            Hold::Nothing => {}
            Hold::Frame { grids, .. } => {
                grids.set_docks(docks);
            }
            Hold::Layout { drawn, .. } => {
                drawn.set_docks(docks);
            }
        }
        self.grids.set_docks(docks)
    }

    /// Takes the frame on screen as the one a restart holds.
    pub(crate) fn hold_frame(&mut self) {
        // a failed attempt comes back here holding the frame already, and
        // the registry it would copy now is the empty one
        if let Hold::Frame { armed, .. } = &mut self.held.hold {
            *armed = false;
            return;
        }
        // a stand-in's buffer carries over even once the engine now dying
        // filled its slot: that window may die again before it ever
        // reports its own name, and the carried one is all the next
        // replacement has to match it by
        let shown = match &self.held.hold {
            Hold::Layout { shown, .. } => shown.clone(),
            Hold::Nothing | Hold::Frame { .. } => Shown::new(),
        };
        self.held.hold = Hold::Frame {
            grids: self.painted_grids().clone(),
            hl: self.painted_hl().clone(),
            shown,
            named: false,
            armed: false,
            layout: true,
        };
    }

    /// Records what each window of the held frame showed, from the statuses
    /// the dead engine reported, so that a replacement window numbered
    /// differently still finds its slot and each tile keeps its title. A
    /// window's own report here replaces any status it was carried into
    /// this frame with, since the carried one was only ever a placeholder
    /// for this one.
    fn name_held_windows(&mut self, status: &HashMap<WinHandle, WindowStatus>) {
        if let Hold::Frame {
            grids,
            shown,
            named,
            ..
        } = &mut self.held.hold
        {
            if *named {
                return;
            }
            for (win, _) in grids.window_layout() {
                if let Some(status) = status.get(&win) {
                    shown.insert(win, status.clone());
                }
            }
            *named = true;
        }
    }

    /// Records whether the redraw batch just applied ended on a flush.
    pub(crate) fn note_batch(&mut self, flushed: bool) {
        self.held.torn = !flushed;
    }

    /// Moves the hold on: from the dead frame to the dead layout once the
    /// replacement puts up a window the slots can hold, and from the layout
    /// to the live registry once the replacement's own windows fill every
    /// slot, have each reported their status and drawn text, or one of
    /// them stands outside every slot. Answers whether the replacement put
    /// its first window up here, which is when [`RESTART_LAYOUT_HOLD`]
    /// starts.
    fn settle_held(&mut self, status: &HashMap<WinHandle, WindowStatus>) -> bool {
        let mut began = false;
        let live = self.grids.window_layout();
        if let Hold::Frame {
            grids,
            hl,
            shown,
            armed,
            layout,
            ..
        } = &mut self.held.hold
        {
            let slots = grids.window_layout();
            // a multigrid replacement's global grid carries text before any
            // window is up, and the slots drawn from it would all be empty
            if !self.grids.shows_a_window() || (!slots.is_empty() && live.is_empty()) {
                return false;
            }
            began = !*armed;
            // a single-grid frame has no slots to hold
            if slots.is_empty() || !*layout {
                self.held.hold = Hold::Nothing;
                return false;
            }
            // the dead frame is a whole screen, and stays up until the
            // window it cannot place yet reports its buffer
            if matches!(fit(&slots, shown, &live, status), Fit::Unnamed) {
                *armed = true;
                return began;
            }
            let cells = slots
                .iter()
                .map(|(win, _)| {
                    let mut grid = grids.window_grid(*win)?.clone();
                    grid.map_hl(held_hl);
                    Some(grid)
                })
                .collect();
            let attrs = hl.attrs().map(|(id, attr)| (held_hl(id), attr)).collect();
            self.held.hold = Hold::Layout {
                slots,
                shown: std::mem::take(shown),
                cells,
                attrs,
                drawn: GridRegistry::new(),
                hl: HlTable::new(),
                hl_from: None,
            };
        }
        let settled = |fitted: &[WindowSlot]| {
            fitted.iter().all(|(held, _)| {
                status.contains_key(held)
                    && self.grids.window_grid(*held).is_some_and(Grid::has_text)
            })
        };
        if let Hold::Layout {
            slots,
            shown,
            cells,
            drawn,
            ..
        } = &mut self.held.hold
        {
            let next = match fit(slots, shown, &live, status) {
                Fit::Slots(fitted) if !settled(&fitted) => self.grids.laid_out_as(&fitted, cells),
                Fit::Slots(_) | Fit::Unnamed | Fit::Outside => None,
            };
            match next {
                Some(next) => *drawn = next,
                None => self.held.hold = Hold::Nothing,
            }
        }
        self.rebuild_held_hl(false);
        began && self.holds_the_screen()
    }

    /// Rebuilds the held layout's table from the live one with the dead
    /// engine's attributes added. A theme change lands in the live table
    /// between flushes, and the repaint it damages reads this one.
    pub(super) fn refresh_held_hl(&mut self) {
        self.rebuild_held_hl(true);
    }

    /// The held layout's own table, so a test can tell a rebuilt table from
    /// a kept one.
    #[cfg(test)]
    pub(crate) fn held_hl_mut(&mut self) -> Option<&mut HlTable> {
        match &mut self.held.hold {
            Hold::Layout { hl, .. } => Some(hl),
            Hold::Nothing | Hold::Frame { .. } => None,
        }
    }

    /// Rebuilds the held layout's table when `always`, or when the live
    /// table has moved since it was built. A flush that changed no
    /// highlight then copies no table.
    fn rebuild_held_hl(&mut self, always: bool) {
        let revision = self.hl.revision();
        if let Hold::Layout {
            attrs, hl, hl_from, ..
        } = &mut self.held.hold
        {
            if !always && *hl_from == Some(revision) {
                return;
            }
            *hl = self.hl.clone();
            for (id, attr) in attrs.iter() {
                hl.define_attr(*id, *attr);
            }
            *hl_from = Some(revision);
        }
    }

    /// Hands the screen to the replacement's own layout once it has put a
    /// window up. A resize before that keeps the dead frame and drops its
    /// slots, which fit a screen that is gone.
    pub(crate) fn release_held_layout(&mut self) {
        match &mut self.held.hold {
            Hold::Frame {
                armed: false,
                layout,
                ..
            } => *layout = false,
            Hold::Frame { .. } | Hold::Layout { .. } => self.held.hold = Hold::Nothing,
            Hold::Nothing => {}
        }
    }
}

impl Model {
    /// Records what each window of the held frame showed, then drops what
    /// the dead engine's windows leave behind: their statuses, whose
    /// handles the replacement numbers its own windows with, and a mouse
    /// gesture captured on one of their grids.
    pub fn forget_engine_windows(&mut self) {
        self.engine.name_held_windows(&self.window_status);
        self.window_status.clear();
        if matches!(self.mouse_capture, Some(MouseCapture::Engine(_))) {
            self.mouse_capture = None;
        }
    }

    /// The status a tile's frame shows for `win`, a window of
    /// [`EngineModel::painted_grids`]: what the dead engine last reported
    /// while a restart holds its frame, and while it holds the layout, the
    /// replacement's report once there is one and the dead engine's for
    /// its slot until then.
    #[must_use]
    pub fn painted_status(&self, win: WinHandle) -> Option<&WindowStatus> {
        match &self.engine.held.hold {
            Hold::Nothing => self.window_status.get(&win),
            Hold::Frame { shown, .. } => shown.get(&win),
            // a stand-in carries the dead engine's handle, which the
            // replacement may have given a window of its own elsewhere
            Hold::Layout { shown, .. } if self.engine.grids.window_slot(win).is_some() => {
                self.window_status.get(&win).or_else(|| shown.get(&win))
            }
            Hold::Layout { shown, .. } => shown.get(&win),
        }
    }

    /// [`EngineModel::settle_held`] against the statuses the replacement
    /// has reported, and the bound a hold that began here owes. Off a
    /// flush, it waits for one while the live registry is mid-batch.
    pub(crate) fn settle_held(&mut self, at_flush: bool) -> Vec<Effect> {
        if !self.engine.holds_the_screen() || (!at_flush && self.engine.held.torn) {
            return Vec::new();
        }
        self.dirty = true;
        if !self.engine.settle_held(&self.window_status) {
            return Vec::new();
        }
        vec![Effect::ScheduleLayoutHold {
            after: RESTART_LAYOUT_HOLD,
            generation: self.surface_conflicts.engine_generation(),
        }]
    }
}
