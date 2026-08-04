//! Pure, consumer-facing description of view's native features. No I/O and
//! no serialization live here: the crate that reads `view.toml` resolves
//! these descriptions against a user's config, and the crates that paint or
//! drive a feature read the same table for its name and its off switch.

pub mod registry;
