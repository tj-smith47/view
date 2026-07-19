//! Performance measurement harness: the sampling/pairing/report core of
//! the scenario-by-fixture latency matrix. Deliberately serde-free:
//! baseline file I/O lives in `view-harness` (the one package sanctioned
//! to parse TOML); this crate only measures and computes.

pub mod pairing;
pub mod report;
pub mod sampling;

use thiserror::Error;

/// Errors from the measurement core. Every variant is a protocol
/// violation the caller must surface, never silently coerce to a number a
/// gate could pass.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum BenchError {
    #[error(
        "only {collected} samples collected with a {warmup}-sample warmup; \
         no measured samples remain"
    )]
    NotEnoughSamples { collected: usize, warmup: usize },
    #[error("no trials to aggregate")]
    NoTrials,
    #[error("paired sides collected different sample counts (view {view}, nvim {nvim})")]
    SampleCountMismatch { view: usize, nvim: usize },
    #[error("nvim-side p99 is {p99}, not a positive finite number; ratio would be meaningless")]
    DegenerateBaselineSide { p99: f64 },
}
