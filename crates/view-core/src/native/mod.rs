//! Pure, consumer-facing description of view's native features. No I/O and
//! no serialization live here: the crate that reads `view.toml` resolves
//! these descriptions against a user's config, and the crates that paint or
//! drive a feature read the same table for its name and its off switch.

pub mod ai_context;
pub mod ai_event;
pub mod ai_fs;
pub mod ai_panel;
pub mod ai_registry;
pub mod diff;
pub mod geometry;
pub mod mappings;
pub mod palette;
pub mod picker;
pub mod prompt;
pub mod registry;
pub mod speculate;
pub mod statusline;
pub mod supervision;
pub mod toast;
pub mod tree;
pub mod views;
