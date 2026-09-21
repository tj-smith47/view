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
    generation: u64,
}

impl Default for SurfaceState {
    fn default() -> Self {
        let layouts = SurfaceLayout::defaults();
        Self {
            configured: layouts.map(|layout| layout.placement),
            layouts,
            ring: 0,
            generation: 0,
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

    /// The generation the next window-open carries. Monotonic, so a handle
    /// answering a call the user has already undone is told from a live
    /// one by its number alone.
    pub fn next_generation(&mut self) -> u64 {
        self.generation = self.generation.wrapping_add(1);
        self.generation
    }

    /// The generation the last window-open carried.
    #[must_use]
    pub const fn generation(&self) -> u64 {
        self.generation
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
        let first = state.next_generation();
        assert_eq!(state.generation(), first);
        assert_ne!(state.next_generation(), first);
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
