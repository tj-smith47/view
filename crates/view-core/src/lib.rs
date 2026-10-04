//! Pure application state: Model, Msg, and update(). No I/O, no rendering.

pub mod config;
pub mod events;
pub mod grid;
pub mod hash;
pub mod hl;
pub mod model;
pub mod msg;
pub mod native;
pub mod osc52;
pub mod sink;
pub mod theme;
pub mod update;

// counts per thread, so a test reads the allocations its own code made
// between a reset and a count
#[cfg(test)]
#[global_allocator]
static ALLOCATOR: view_test_support::CountingAllocator =
    view_test_support::CountingAllocator::new();
