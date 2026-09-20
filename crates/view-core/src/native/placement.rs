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
    generation: u64,
}

impl Default for SurfaceState {
    fn default() -> Self {
        Self {
            layouts: SurfaceLayout::defaults(),
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

    /// Replaces every layout, which is what a config read answers with.
    pub fn set_layouts(&mut self, layouts: [SurfaceLayout; 4]) {
        self.layouts = layouts;
    }

    /// Replaces one surface's layout.
    pub fn set_layout(&mut self, surface: NativeSurface, layout: SurfaceLayout) {
        if let Some(slot) = self.layouts.get_mut(surface.index()) {
            *slot = layout;
        }
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
}
