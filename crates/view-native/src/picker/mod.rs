//! The picker's matcher worker (spec section 18): the one thing about the
//! picker that genuinely needs a crate. `view-core::native::picker` owns
//! the pure session/query state (see that module's doc); everything here
//! is a background thread and the `nucleo`/`ignore` handles it drives.

pub mod matcher;
mod sources;
