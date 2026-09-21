//! Where each of view's own surfaces sits this session, and the counter
//! that tells one window-open from the next.
//!
//! The layouts are config, read once at startup and re-read when a user
//! changes one mid-session. They live beside the model rather than inside
//! it because every surface reads the same four, and a field per surface
//! would be four facts that have to agree.

use crate::native::geometry::{NativeSurface, SurfaceLayout, SurfacePlacement};

/// The placement of every surface, and the generation the next window-open
/// carries.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SurfaceState {
    layouts: [SurfaceLayout; 4],
    /// What `view.toml` (its layers already applied) put each surface at,
    /// captured once by [`Self::set_layouts`] and never touched by a resize
    /// or a cycle step -- the `config` stop [`Self::advance_ring`]'s ring
    /// always returns to, whatever the ring has done to
    /// [`Self::layouts`] since.
    configured: [SurfacePlacement; 4],
    /// The shared three-position ring's own position: 0 is `config`, 1 is
    /// `windowed`, 2 is `overlay`. Starts at 0 -- every surface begins at
    /// its configured placement, so the first `cycle_surfaces` press has
    /// somewhere to advance *from* that is not itself.
    ring: u8,
    /// One counter per surface, never one shared by all four: a ring step
    /// that carries two surfaces to `windowed` in the same fold issues two
    /// `OpenNativeWindow` calls before either reply lands, and a shared
    /// counter would answer for whichever call went last, dropping the
    /// other's handle and leaving nvim holding a scratch window view never
    /// claims.
    generation: [u64; 4],
    /// Whether `generation`'s own open is still awaiting its reply. Nvim's
    /// `cmdline_show` can arrive twice in the one redraw batch one `update`
    /// call folds, and the palette's own open guard reads
    /// `grids().native_window`, which stays `None` until that reply is
    /// *applied* -- a step this same `update` call has not reached yet when
    /// it sees the batch's second `cmdline_show`. Without this, that second
    /// event read the guard as "not open yet" and issued a second
    /// `OpenNativeWindow`, opening two windows in nvim for one surface and
    /// leaving the first orphaned once the second's reply overwrote the
    /// claim.
    pending: [bool; 4],
}

impl Default for SurfaceState {
    fn default() -> Self {
        let layouts = SurfaceLayout::defaults();
        Self {
            configured: layouts.map(|layout| layout.placement),
            layouts,
            ring: 0,
            generation: [0; 4],
            pending: [false; 4],
        }
    }
}

impl SurfaceState {
    /// One surface's layout.
    #[must_use]
    pub fn layout(&self, surface: NativeSurface) -> SurfaceLayout {
        self.layouts
            .get(surface.index())
            .copied()
            .unwrap_or_else(|| SurfaceLayout::default_for(surface))
    }

    /// Whether `surface` takes a window in nvim's layout rather than
    /// floating over the buffer.
    #[must_use]
    pub fn windowed(&self, surface: NativeSurface) -> bool {
        matches!(self.layout(surface).placement, SurfacePlacement::Windowed)
    }

    /// Replaces every layout, which is what a config read answers with, and
    /// re-captures [`Self::configured`] from it -- a config reload
    /// (`:View` has none today, but a session that gains one owes the ring
    /// the new file's own answer, not the one it booted with) moves the
    /// ring's `config` stop along with everything else.
    pub fn set_layouts(&mut self, layouts: [SurfaceLayout; 4]) {
        self.configured = layouts.map(|layout| layout.placement);
        self.layouts = layouts;
    }

    /// Replaces one surface's layout. Leaves [`Self::configured`] alone: a
    /// resize or a cycle step is runtime drift from the file's own answer,
    /// never a new one.
    pub fn set_layout(&mut self, surface: NativeSurface, layout: SurfaceLayout) {
        if let Some(slot) = self.layouts.get_mut(surface.index()) {
            *slot = layout;
        }
    }

    /// Steps the shared ring to its next position and reports every
    /// surface's placement there, together with whether that is a change
    /// from the placement it had a moment ago -- what a caller needs to
    /// know whether anything open under it has to move.
    ///
    /// The ring runs `config -> windowed -> overlay -> config`, all four
    /// surfaces at the one shared position: a per-surface ring would let
    /// two surfaces drift out of step with a press that names no surface at
    /// all to aim it at.
    pub fn advance_ring(&mut self) -> [(NativeSurface, SurfacePlacement, bool); 4] {
        self.ring = (self.ring + 1) % 3;
        let ring = self.ring;
        NativeSurface::ALL.map(|surface| {
            let index = surface.index();
            let target = match ring {
                1 => SurfacePlacement::Windowed,
                2 => SurfacePlacement::Overlay,
                _ => self.configured[index],
            };
            let changed = self.layouts[index].placement != target;
            self.layouts[index].placement = target;
            (surface, target, changed)
        })
    }

    /// The generation `surface`'s next window-open carries. Monotonic per
    /// surface, so a handle answering a call that surface's own open has
    /// since superseded is told from a live one by its number alone, and a
    /// second surface's own open in the same fold never touches this one's
    /// counter.
    pub fn next_generation(&mut self, surface: NativeSurface) -> u64 {
        let slot = &mut self.generation[surface.index()];
        *slot = slot.wrapping_add(1);
        self.pending[surface.index()] = true;
        *slot
    }

    /// The generation `surface`'s last window-open carried.
    #[must_use]
    pub fn generation(&self, surface: NativeSurface) -> u64 {
        self.generation[surface.index()]
    }

    /// Whether `surface`'s current generation is still waiting on its
    /// `OpenNativeWindow` reply -- the guard a caller that reacts to a
    /// redraw event (rather than a single keystroke) must add to "is there
    /// a window already", since the latter stays false until the reply is
    /// applied, several steps after the request that answers it was sent.
    #[must_use]
    pub fn pending_open(&self, surface: NativeSurface) -> bool {
        self.pending[surface.index()]
    }

    /// Marks `surface`'s current generation as answered, whether or not the
    /// reply's window handle was ultimately claimed.
    pub fn clear_pending(&mut self, surface: NativeSurface) {
        self.pending[surface.index()] = false;
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use crate::native::geometry::Anchor;

    #[test]
    fn a_surface_answers_its_own_layout_and_no_neighbours() {
        let mut state = SurfaceState::default();
        assert!(!state.windowed(NativeSurface::Tree));
        state.set_layout(
            NativeSurface::Tree,
            SurfaceLayout::new(SurfacePlacement::Windowed, Anchor::Right, 25),
        );
        assert!(state.windowed(NativeSurface::Tree));
        assert_eq!(state.layout(NativeSurface::Tree).size, 25);
        assert!(!state.windowed(NativeSurface::Agent));
    }

    #[test]
    fn every_open_carries_a_generation_of_its_own() {
        let mut state = SurfaceState::default();
        let first = state.next_generation(NativeSurface::Tree);
        assert_eq!(state.generation(NativeSurface::Tree), first);
        assert_ne!(state.next_generation(NativeSurface::Tree), first);
    }

    // C2: a ring step opening the tree and the agent panel in the same
    // fold issues one `next_generation` call per surface before either
    // reply lands -- a shared counter answers only the last call, and the
    // other surface's reply is dropped as stale, orphaning its window.
    #[test]
    fn two_surfaces_opened_in_one_step_keep_their_own_generation() {
        let mut state = SurfaceState::default();
        let tree_gen = state.next_generation(NativeSurface::Tree);
        let agent_gen = state.next_generation(NativeSurface::Agent);
        assert_eq!(state.generation(NativeSurface::Tree), tree_gen);
        assert_eq!(state.generation(NativeSurface::Agent), agent_gen);
        assert_eq!(
            state.generation(NativeSurface::Notifications),
            0,
            "a third surface's own counter must not move"
        );
    }

    // a single redraw batch can carry two `cmdline_show` events before
    // either one's `OpenNativeWindow` reply has been applied -- a guard
    // reading only `generation`/the grid registry sees both as "no window
    // yet" and opens the surface twice; `pending_open` is what a second
    // request within the same batch has to check.
    #[test]
    fn a_second_open_of_the_same_surface_reads_pending_until_its_reply_lands() {
        let mut state = SurfaceState::default();
        assert!(!state.pending_open(NativeSurface::Palette));
        let generation = state.next_generation(NativeSurface::Palette);
        assert!(
            state.pending_open(NativeSurface::Palette),
            "a request just issued must read pending until its reply lands"
        );
        state.clear_pending(NativeSurface::Palette);
        assert!(!state.pending_open(NativeSurface::Palette));
        assert_eq!(state.generation(NativeSurface::Palette), generation);
    }

    // an error reply clears `pending` too (there is no window to claim,
    // but the flag still has to let a later open through) -- the same
    // `clear_pending` call `native_window_open_failed` makes.
    #[test]
    fn a_failed_open_still_clears_pending_for_a_later_request() {
        let mut state = SurfaceState::default();
        state.next_generation(NativeSurface::Tree);
        assert!(state.pending_open(NativeSurface::Tree));
        state.clear_pending(NativeSurface::Tree);
        assert!(
            !state.pending_open(NativeSurface::Tree),
            "an error reply must not leave the surface refusing every \
             later open for the rest of the session"
        );
    }

    #[test]
    fn the_ring_runs_config_then_windowed_then_overlay_then_config() {
        let mut state = SurfaceState::default();
        state.set_layouts([
            SurfaceLayout::new(SurfacePlacement::Windowed, Anchor::Left, 30),
            SurfaceLayout::new(SurfacePlacement::Overlay, Anchor::Right, 30),
            SurfaceLayout::new(SurfacePlacement::Overlay, Anchor::Center, 30),
            SurfaceLayout::new(SurfacePlacement::Overlay, Anchor::TopRight, 30),
        ]);
        // windowed, every surface, whatever it was configured at
        for (_, placement, _) in state.advance_ring() {
            assert_eq!(placement, SurfacePlacement::Windowed);
        }
        // overlay, every surface
        for (_, placement, _) in state.advance_ring() {
            assert_eq!(placement, SurfacePlacement::Overlay);
        }
        // back to what `view.toml` named, per surface
        let back = state.advance_ring();
        assert_eq!(
            back[NativeSurface::Tree.index()].1,
            SurfacePlacement::Windowed
        );
        assert_eq!(
            back[NativeSurface::Agent.index()].1,
            SurfacePlacement::Overlay
        );
        // windowed again: the ring wraps rather than stopping at `config`
        for (_, placement, _) in state.advance_ring() {
            assert_eq!(placement, SurfacePlacement::Windowed);
        }
    }

    #[test]
    fn a_ring_step_reports_no_change_for_a_surface_already_there() {
        let mut state = SurfaceState::default();
        // Every surface starts at `Overlay` (the shipped default), so
        // `config` and `overlay` are the same placement here: the step from
        // `windowed` onto `overlay` is a real move, but the step from
        // `overlay` back onto `config` never moves anything -- only the
        // `changed` bit can tell those two arrivals apart, since the
        // placement value alone reads the same either way.
        state.advance_ring(); // windowed
        state.advance_ring(); // overlay (changed = true, from windowed)
        let unchanged = state.advance_ring(); // config, which is Overlay too
        for (_, placement, changed) in unchanged {
            assert_eq!(placement, SurfacePlacement::Overlay);
            assert!(
                !changed,
                "config and overlay are the same placement with nothing \
                 configured, so this step must report no change"
            );
        }
    }
}
