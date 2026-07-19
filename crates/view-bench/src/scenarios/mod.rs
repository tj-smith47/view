//! Scenario drivers: each module drives real editor processes through one
//! measurement scenario and returns raw per-sample data plus the
//! aggregated statistics the gate consumes. Drivers receive fully
//! resolved [`crate::session::SpawnSpec`]s; which fixture/environment
//! those represent is the caller's concern.

pub mod echo;

use std::time::Duration;

/// Sampling protocol knobs shared by the paired latency scenarios.
#[derive(Debug, Clone, Copy)]
pub struct Protocol {
    /// Measured samples per side per trial.
    pub samples: usize,
    /// Warmup samples per side per trial, excluded from every statistic.
    pub warmup: usize,
    /// Samples taken from one side before switching to the other.
    pub block: usize,
    /// Full interleaved trials per invocation; the gated statistic is the
    /// median across trials.
    pub trials: usize,
    /// Bound on one sample's wait before the run is declared desynced.
    pub sample_timeout: Duration,
    /// Gap between samples so one response's tail cannot bleed into the
    /// next sample's measurement.
    pub inter_sample: Duration,
}

impl Default for Protocol {
    fn default() -> Self {
        Self {
            samples: 1000,
            warmup: 100,
            block: 25,
            trials: 3,
            sample_timeout: Duration::from_secs(5),
            inter_sample: Duration::from_millis(10),
        }
    }
}
