//! The first-paint scenario: cold process spawn until the first frame
//! with visible content, view paired against bare nvim on the same
//! resolved config. Every sample is its own process spawn, so samples
//! are naturally independent trials; the run interleaves view/nvim
//! spawn-for-spawn and gates on pooled percentiles over the whole run
//! rather than repeating full sub-trials.
//!
//! "Cold" here means a fresh process against an untouched fixture copy
//! each sample; the OS page cache stays warm (dropping kernel caches
//! needs privileges a dev run does not have), and the recorded baseline
//! carries that meaning.

use std::time::{Duration, Instant};

use crate::boundaries::any_visible_cell;
use crate::pairing::{paired_summary, PairedSummary};
use crate::sampling::Side;
use crate::scenarios::Protocol;
use crate::session::{BenchSession, SpawnSpec};
use crate::BenchError;

/// Bound on one spawn-to-first-frame wait before the run is declared
/// desynced; generous because a plugin-heavy nvim legitimately takes
/// hundreds of milliseconds to its first frame.
const FIRST_FRAME_TIMEOUT: Duration = Duration::from_secs(30);

/// One cold spawn: process start to first visible cell, then a bounded
/// kill+reap so sample teardown can never leak processes across the run.
fn sample_once(spec: &SpawnSpec) -> Result<f64, BenchError> {
    let start = Instant::now();
    let mut session = BenchSession::spawn(spec)?;
    let deadline = start + FIRST_FRAME_TIMEOUT;
    loop {
        if session.with_screen(any_visible_cell) {
            break;
        }
        if Instant::now() >= deadline {
            return Err(BenchError::Desync {
                context: format!(
                    "no visible cell within {FIRST_FRAME_TIMEOUT:?} of spawn; screen:\n{}",
                    session.screen_text()
                ),
            });
        }
        std::thread::yield_now();
    }
    let elapsed_ms = start.elapsed().as_secs_f64() * 1000.0;
    drop(session);
    Ok(elapsed_ms)
}

/// The first-paint run's outcome: one pooled paired summary over every
/// interleaved spawn pair.
#[derive(Debug)]
pub struct FirstPaintOutcome {
    pub summary: PairedSummary,
    /// The view side's p99 cold first-frame time in milliseconds.
    pub gated_cold_ms: f64,
    /// view p99 over nvim p99.
    pub gated_ratio_vs_nvim: f64,
}

/// Runs `protocol.warmup + protocol.samples` cold spawns per side,
/// strictly alternating view/nvim (per-sample interleaving: each sample
/// is one whole trial, so pair members sit adjacent in time).
///
/// # Errors
///
/// Returns [`BenchError::Desync`] if any spawn never paints, or any
/// underlying session error.
pub fn run(
    view: &SpawnSpec,
    nvim: &SpawnSpec,
    protocol: &Protocol,
) -> Result<FirstPaintOutcome, BenchError> {
    let per_side = protocol.warmup + protocol.samples;
    let mut view_ms = Vec::with_capacity(per_side);
    let mut nvim_ms = Vec::with_capacity(per_side);
    for index in 0..per_side {
        // alternating which side spawns first within each pair keeps
        // slow drift from always taxing the same side of a pair
        let first = if index % 2 == 0 {
            Side::View
        } else {
            Side::Nvim
        };
        for side in [first, first.other()] {
            let (spec, sink, name) = match side {
                Side::View => (view, &mut view_ms, "view"),
                Side::Nvim => (nvim, &mut nvim_ms, "nvim"),
            };
            sink.push(sample_once(spec).map_err(|e| label(name, e))?);
        }
        std::thread::sleep(protocol.inter_sample);
    }

    let summary = paired_summary(&view_ms, &nvim_ms, protocol.warmup)?;
    let gated_cold_ms = summary.view.p99();
    let gated_ratio_vs_nvim = summary.ratio_p99;
    Ok(FirstPaintOutcome {
        summary,
        gated_cold_ms,
        gated_ratio_vs_nvim,
    })
}

fn label(side: &str, err: BenchError) -> BenchError {
    match err {
        BenchError::Desync { context } => BenchError::Desync {
            context: format!("[{side} side] {context}"),
        },
        other => other,
    }
}
