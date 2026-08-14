//! Terminal frontend: paints the surface, reads input, owns the terminal.

pub mod input;
pub mod keys;
pub mod mouse;
pub mod paint;
#[cfg(all(unix, feature = "bench-taps"))]
pub mod tap;
pub mod terminal;
pub mod tiers;
