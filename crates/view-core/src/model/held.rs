//! The frame a restart holds on screen, and the reads the compositor paints
//! through so that it shows the dead engine's last frame until the
//! replacement puts a window up, then the replacement's windows in the dead
//! engine's slots until the replacement's own layout has settled.

use std::collections::HashMap;

use super::{EngineModel, Model, WindowStatus};
use crate::events::WinHandle;
use crate::grid::registry::{GridRegistry, WindowSlot};
use crate::grid::Grid;
use crate::hl::HlTable;
use crate::msg::Effect;

/// How long the replacement's windows are drawn in the dead engine's slots
/// at most, from the first window it puts on screen. A config opens its
/// sidebars from startup autocmds a plugin manager runs after the attach,
/// which takes a few hundred milliseconds on a loaded config; a replacement
/// that never reopens one hands the screen back to its own layout here.
pub(crate) const RESTART_LAYOUT_HOLD: std::time::Duration = std::time::Duration::from_secs(2);

/// The buffer name each window of the held frame showed, by handle.
type Names = HashMap<WinHandle, String>;

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
        /// The buffer each held window showed.
        names: Names,
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
    /// registry puts the file across the whole width and then back. The
    /// cells are the replacement's from its first window: only the slots
    /// are the dead engine's.
    Layout {
        /// The dead engine's windows, as handle and slot.
        slots: Vec<WindowSlot>,
        /// The buffer each of those windows showed.
        names: Names,
        /// The live registry drawn in `slots`.
        drawn: GridRegistry,
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
    names: &Names,
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
                .any(|(held, _)| names.contains_key(held) && !taken(held));
            return if open { Fit::Unnamed } else { Fit::Outside };
        };
        let Some(slot) = fitted
            .iter_mut()
            .find(|(held, _)| names.get(held) == Some(name) && !taken(held))
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
    /// compositor: the dead engine's while a restart holds its frame, and
    /// [`Self::hl`] otherwise.
    #[must_use]
    #[inline]
    pub fn painted_hl(&self) -> &HlTable {
        match &self.held.hold {
            Hold::Frame { hl, .. } => hl,
            Hold::Nothing | Hold::Layout { .. } => &self.hl,
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
    pub fn holds_the_frame(&self) -> bool {
        matches!(self.held.hold, Hold::Frame { .. })
    }

    /// Takes the frame on screen as the one a restart holds.
    pub(crate) fn hold_frame(&mut self) {
        // a failed attempt comes back here holding the frame already, and
        // the registry it would copy now is the empty one
        if let Hold::Frame { armed, .. } = &mut self.held.hold {
            *armed = false;
        } else {
            self.held.hold = Hold::Frame {
                grids: self.painted_grids().clone(),
                hl: self.hl.clone(),
                names: Names::new(),
                armed: false,
                layout: true,
            };
        }
    }

    /// Records the buffer each window of the held frame showed, from the
    /// statuses the dead engine reported, so that a replacement window
    /// numbered differently still finds its slot.
    pub fn name_held_windows(&mut self, status: &HashMap<WinHandle, WindowStatus>) {
        // a frame held over from a failed attempt keeps its names, since a
        // replacement that reported may have reused their handles
        if let Hold::Frame { grids, names, .. } = &mut self.held.hold {
            if !names.is_empty() {
                return;
            }
            *names = grids
                .window_layout()
                .into_iter()
                .filter_map(|(win, _)| status.get(&win).map(|s| (win, s.name.clone())))
                .collect();
        }
    }

    /// Records whether the redraw batch just applied ended on a flush.
    pub(crate) fn note_batch(&mut self, flushed: bool) {
        self.held.torn = !flushed;
    }

    /// Moves the hold on: from the dead frame to the dead layout once the
    /// replacement puts up a window the slots can hold, and from the layout
    /// to the live registry once the replacement's own windows fill every
    /// slot or one of them stands outside every slot. Answers whether the
    /// replacement put its first window up here, which is when
    /// [`RESTART_LAYOUT_HOLD`] starts.
    fn settle_held(&mut self, status: &HashMap<WinHandle, WindowStatus>) -> bool {
        let mut began = false;
        let live = self.grids.window_layout();
        if let Hold::Frame {
            grids,
            names,
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
            if matches!(fit(&slots, names, &live, status), Fit::Unnamed) {
                *armed = true;
                return began;
            }
            self.held.hold = Hold::Layout {
                slots,
                names: std::mem::take(names),
                drawn: GridRegistry::new(),
            };
        }
        if let Hold::Layout { slots, names, .. } = &self.held.hold {
            self.held.hold = match fit(slots, names, &live, status) {
                Fit::Slots(fitted) => {
                    let filled = fitted
                        .iter()
                        .all(|(held, _)| live.iter().any(|(w, _)| w == held));
                    match self.grids.laid_out_as(&fitted) {
                        Some(drawn) if !filled => Hold::Layout {
                            slots: slots.clone(),
                            names: names.clone(),
                            drawn,
                        },
                        _ => Hold::Nothing,
                    }
                }
                Fit::Unnamed | Fit::Outside => Hold::Nothing,
            };
        }
        began && self.holds_the_screen()
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
