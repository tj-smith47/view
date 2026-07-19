//! Terminal frontend: paints the surface, reads input, owns the terminal.

pub mod keys;
pub mod mouse;
pub mod paint;
#[cfg(feature = "bench-taps")]
mod tap;
pub mod terminal;
pub mod tiers;
