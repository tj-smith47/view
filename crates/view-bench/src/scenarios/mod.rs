//! Scenario drivers: each module drives real editor processes through one
//! measurement scenario and returns raw per-sample data plus the
//! aggregated statistics the gate consumes. Drivers receive fully
//! resolved [`crate::session::SpawnSpec`]s; which fixture/environment
//! those represent is the caller's concern.

pub mod clock;
pub mod echo;
// The control arm reaches its headless server over a unix socket path; the
// named-pipe equivalent is untested here, so the row loud-skips off unix
// rather than recording a number nobody has validated.
#[cfg(unix)]
pub mod echo_control;
pub mod first_paint;
pub mod flood;
pub mod memory;
pub mod scroll;
// The tap channel is FIFO + raw-CLOCK_MONOTONIC based (see taps.rs); neither
// exists on Windows, so the module and the rows built on it are unix-only and
// the driver loud-skips them off-unix, the same way the memory row skips where
// its metric is undefined.
#[cfg(unix)]
pub mod taps;

use std::time::Duration;

/// Sampling protocol knobs shared by the paired latency scenarios.
#[derive(Debug, Clone, Copy)]
pub struct Protocol {
    /// Measured samples per side per trial.
    pub samples: usize,
    /// Warmup samples per side per trial, excluded from every statistic.
    pub warmup: usize,
    /// Samples taken from one side before switching to the other.
    /// Per-sample alternation (block = 1) couples ambient noise bursts
    /// into both sides' tail percentiles equally, which is what keeps
    /// the gated ratio stable across host-load regimes; larger blocks
    /// let a burst shorter than one block inflate a single side.
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
            block: 1,
            trials: 3,
            sample_timeout: Duration::from_secs(5),
            inter_sample: Duration::from_millis(10),
        }
    }
}
