//! The flood scenario: a `:terminal` buffer draining a fixed line flood,
//! run paired against the same flood in bare nvim on the same fixture.
//! Two things are measured, not assumed: total drain time (paired, the
//! ratio gates) and paint cadence during the drain: the gaps between
//! successive observed frame changes. A blocked UI thread shows up as a
//! long no-paint gap while output is still pending, so the cadence
//! percentile IS the coalescing invariant, observed from outside the
//! process.

use std::time::{Duration, Instant};

use crate::sampling::{median_of_trials, Distribution};
use crate::session::{BenchSession, SpawnSpec};
use crate::BenchError;

/// The marker the flood command prints when the producer finishes. The
/// TYPED command must never contain this as contiguous cells (the echoed
/// command line would satisfy the wait before the flood even ran), so
/// [`flood_command`] splits it with shell quote concatenation.
const DONE_MARKER: &str = "FLOODMARK-DONE";

/// The shell command one flood runs inside `:terminal`.
#[must_use]
pub fn flood_command(lines: usize) -> String {
    format!("seq -f 'L%.0f' 1 {lines}; printf 'FLOODMARK''-DONE\\n'\r")
}

/// One side's flood measurement.
#[derive(Debug)]
pub struct FloodSide {
    pub drain_ms: f64,
    /// Gaps between successive observed frame changes during the drain,
    /// in milliseconds.
    pub cadence_gaps_ms: Vec<f64>,
}

/// Spawns `spec`, opens `:terminal`, runs the flood, and observes drain
/// time plus paint cadence until the done marker is visible.
fn flood_once(
    spec: &SpawnSpec,
    settle_deadline: Duration,
    lines: usize,
) -> Result<FloodSide, BenchError> {
    let mut session = BenchSession::spawn(spec)?;
    if !session.settle(Duration::from_secs(2), settle_deadline) {
        return Err(BenchError::Desync {
            context: format!(
                "startup never went quiet; screen:\n{}",
                session.screen_text()
            ),
        });
    }
    session.send(b":terminal\r")?;
    if !session.settle(Duration::from_secs(1), Duration::from_secs(30)) {
        return Err(BenchError::Desync {
            context: format!(
                "terminal buffer never settled; screen:\n{}",
                session.screen_text()
            ),
        });
    }
    session.send(b"i")?;
    std::thread::sleep(Duration::from_millis(200));

    session.send(flood_command(lines).as_bytes())?;
    let start = Instant::now();
    let deadline = start + Duration::from_secs(120);
    let mut last_hash = 0_u64;
    let mut last_change: Option<Instant> = None;
    let mut gaps_ms = Vec::new();
    let drain_ms = loop {
        let (hash, done) = session.with_screen(|screen| {
            let hash = crate::boundaries::screen_hash(screen);
            let (rows, cols) = screen.size();
            let mut done = false;
            for row in 0..rows {
                let text = crate::boundaries::row_text(screen, row, cols);
                if text.contains(DONE_MARKER) {
                    done = true;
                    break;
                }
            }
            (hash, done)
        });
        let now = Instant::now();
        if hash != last_hash {
            if let Some(previous) = last_change {
                gaps_ms.push(now.duration_since(previous).as_secs_f64() * 1000.0);
            }
            last_change = Some(now);
            last_hash = hash;
        }
        if done {
            break now.duration_since(start).as_secs_f64() * 1000.0;
        }
        if now >= deadline {
            return Err(BenchError::Desync {
                context: format!(
                    "flood done marker never appeared; screen:\n{}",
                    session.screen_text()
                ),
            });
        }
        std::thread::yield_now();
    };

    // leave terminal-insert mode before the ordinary quit sequence; the
    // shell would otherwise swallow the Esc that starts it
    session.send(b"\x1c\x0e")?;
    session.shutdown();
    Ok(FloodSide {
        drain_ms,
        cadence_gaps_ms: gaps_ms,
    })
}

/// The flood run's outcome.
#[derive(Debug)]
pub struct FloodOutcome {
    pub view_trials: Vec<FloodSide>,
    pub nvim_trials: Vec<FloodSide>,
    /// Median across trials of view drain time over nvim drain time.
    pub gated_drain_ratio: f64,
    /// Median across trials of the view side's cadence-gap p99 (ms).
    pub gated_cadence_p99_ms: f64,
    /// Worst single view-side no-paint gap observed across all trials
    /// (ms); reported, not gated: the max of a scheduler-noisy quantity.
    pub view_stall_max_ms: f64,
}

/// Runs `trials` paired floods. A flood is one long macro operation, so
/// pairing is per trial (view and nvim floods back to back within the
/// same invocation, order alternating per trial) rather than per-sample
/// interleaving, which has no meaning inside a single drain.
///
/// # Errors
///
/// Returns [`BenchError::Desync`] if a flood never completes or a
/// session misbehaves, and [`BenchError::NotEnoughSamples`] if a drain
/// produced fewer observed frame changes than the protocol's floor (the
/// cadence percentile would be built on too few gaps to mean anything).
pub fn run(
    view: &SpawnSpec,
    nvim: &SpawnSpec,
    trials: usize,
    min_gap_samples: usize,
    settle_deadline: Duration,
    lines: usize,
) -> Result<FloodOutcome, BenchError> {
    let mut view_trials = Vec::with_capacity(trials);
    let mut nvim_trials = Vec::with_capacity(trials);
    for trial in 0..trials {
        if trial % 2 == 0 {
            view_trials
                .push(flood_once(view, settle_deadline, lines).map_err(|e| label("view", e))?);
            nvim_trials
                .push(flood_once(nvim, settle_deadline, lines).map_err(|e| label("nvim", e))?);
        } else {
            nvim_trials
                .push(flood_once(nvim, settle_deadline, lines).map_err(|e| label("nvim", e))?);
            view_trials
                .push(flood_once(view, settle_deadline, lines).map_err(|e| label("view", e))?);
        }
    }

    let mut ratios = Vec::with_capacity(trials);
    let mut cadence_p99s = Vec::with_capacity(trials);
    let mut stall_max: f64 = 0.0;
    for (view_side, nvim_side) in view_trials.iter().zip(&nvim_trials) {
        if !(nvim_side.drain_ms.is_finite() && nvim_side.drain_ms > 0.0) {
            return Err(BenchError::DegenerateBaselineSide {
                p99: nvim_side.drain_ms,
            });
        }
        ratios.push(view_side.drain_ms / nvim_side.drain_ms);
        if view_side.cadence_gaps_ms.len() < min_gap_samples {
            return Err(BenchError::NotEnoughSamples {
                collected: view_side.cadence_gaps_ms.len(),
                warmup: min_gap_samples,
            });
        }
        let dist = Distribution::from_samples(&view_side.cadence_gaps_ms, 0)?;
        cadence_p99s.push(dist.p99());
        stall_max = stall_max.max(dist.max());
    }

    Ok(FloodOutcome {
        gated_drain_ratio: median_of_trials(&ratios)?,
        gated_cadence_p99_ms: median_of_trials(&cadence_p99s)?,
        view_stall_max_ms: stall_max,
        view_trials,
        nvim_trials,
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

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    #[test]
    fn typed_flood_command_never_contains_the_done_marker_contiguously() {
        let command = flood_command(1000);
        assert!(
            !command.contains(DONE_MARKER),
            "the echoed command line would satisfy the done wait early: {command}"
        );
    }
}
