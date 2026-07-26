//! Performance measurement harness: the sampling/pairing/report core of
//! the scenario-by-fixture latency matrix. Deliberately serde-free:
//! baseline file I/O lives in `view-harness` (the one package sanctioned
//! to parse TOML); this crate only measures and computes.

pub mod boundaries;
pub mod pairing;
pub mod remote_ui;
pub mod report;
pub mod sampling;
pub mod scenarios;
pub mod session;

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
    #[error(
        "nvim-side {statistic} is {value}, not a positive finite number; \
         ratio would be meaningless"
    )]
    DegenerateBaselineSide { statistic: &'static str, value: f64 },
    #[error("pty session error: {0}")]
    Session(#[from] view_oracle::OracleError),
    #[error("measurement desync (a harness fault, not a latency reading): {context}")]
    Desync { context: String },
    #[error(
        "{metric} measured {value:.4} ms, within {factor}x of the harness's own \
         {resolution:.4} ms probe period: the number describes the instrument, not \
         the editor"
    )]
    BelowInstrumentResolution {
        metric: &'static str,
        value: f64,
        resolution: f64,
        factor: f64,
    },
}
