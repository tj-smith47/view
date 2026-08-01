//! The first-paint scenario: cold process spawn until the first frame
//! with visible content, view paired against bare nvim on the same
//! resolved config. Every sample is its own process spawn, so samples
//! are naturally independent trials; the run interleaves view/nvim
//! spawn-for-spawn and gates on pooled percentiles over the whole run
//! rather than repeating full sub-trials.
//!
//! Two boundaries are timed, and they are different events the spec
//! states separate budgets for:
//!
//! - **shell visible** -- view's own pre-attach chrome
//!   (`view_surface::SHELL_PLACEHOLDER`) reaching the terminal. view
//!   paints it before it has even spawned the nvim child, so bare nvim
//!   has no counterpart and this is a view-side absolute bar, never a
//!   ratio.
//! - **marker cold** -- the opened fixture's own content reaching the
//!   terminal. Both sides reach this one, so it is what the paired ratio
//!   is taken over.
//!
//! Both come from the same spawn: the sample loop watches for the shell
//! first and keeps polling to the content marker, so the two series are
//! measured on one process and their difference is that process's own
//! attach window rather than a subtraction across spawns.
//!
//! "Cold" here means a fresh process against an untouched fixture copy
//! each sample; the OS page cache stays warm (dropping kernel caches
//! needs privileges a dev run does not have), and the recorded baseline
//! carries that meaning.

use std::time::{Duration, Instant};

use crate::boundaries::screen_holds;
use crate::pairing::{paired_summary, NvimSamples, PairedSummary, ViewSamples};
use crate::sampling::{Distribution, Side};
use crate::scenarios::Protocol;
use crate::session::{BenchSession, NvimSpec, SpawnSpec, ViewSpec};
use crate::BenchError;

/// Bound on one spawn-to-first-frame wait before the run is declared
/// desynced; generous because a plugin-heavy nvim legitimately takes
/// hundreds of milliseconds to its first frame.
const FIRST_FRAME_TIMEOUT: Duration = Duration::from_secs(30);

/// Both boundary times from one cold spawn, in milliseconds since process
/// start. `shell_ms` stays `None` for a side that paints no startup shell.
#[derive(Debug, Clone, Copy)]
struct SampleTimes {
    shell_ms: Option<f64>,
    marker_ms: f64,
}

/// One cold spawn: process start until `marker` -- content only the
/// opened fixture buffer can supply -- is on screen, recording on the way
/// past when `shell` (if watched) first appeared, then a bounded
/// kill+reap so sample teardown can never leak processes across the run.
fn sample_once(
    spec: &SpawnSpec,
    shell: Option<&str>,
    marker: &str,
) -> Result<SampleTimes, BenchError> {
    let start = Instant::now();
    let mut session = BenchSession::spawn(spec)?;
    let deadline = start + FIRST_FRAME_TIMEOUT;
    let mut shell_ms = None;
    let mut polls = 0_u64;
    loop {
        polls += 1;
        // the shell frame stays on screen for the whole attach window
        // (nothing repaints until the engine's first flush), so a poll
        // this tight cannot step over it into the content frame
        if let Some(shell) = shell.filter(|_| shell_ms.is_none()) {
            if session.with_screen(|screen| screen_holds(screen, shell)) {
                shell_ms = Some(elapsed_ms(start));
            }
        }
        if session.with_screen(|screen| screen_holds(screen, marker)) {
            break;
        }
        if Instant::now() >= deadline {
            return Err(BenchError::Desync {
                context: format!(
                    "buffer content {marker:?} never painted within {FIRST_FRAME_TIMEOUT:?} of \
                     spawn; screen:\n{}",
                    session.screen_text()
                ),
            });
        }
        std::thread::yield_now();
    }
    let marker_ms = elapsed_ms(start);
    // content without a shell frame means view stopped painting its
    // pre-attach chrome at all -- the regression the shell bar exists to
    // catch -- so it fails the run rather than silently shortening the series
    if shell.is_some() && shell_ms.is_none() {
        return Err(BenchError::Desync {
            context: format!(
                "buffer content painted at {marker_ms:.3}ms without the startup shell ever \
                 reaching the screen across {polls} polls; screen:\n{}",
                session.screen_text()
            ),
        });
    }
    drop(session);
    Ok(SampleTimes {
        shell_ms,
        marker_ms,
    })
}

fn elapsed_ms(start: Instant) -> f64 {
    start.elapsed().as_secs_f64() * 1000.0
}

/// The first-paint run's outcome: one pooled paired summary over every
/// interleaved spawn pair, plus the view-only shell series.
#[derive(Debug)]
pub struct FirstPaintOutcome {
    pub summary: PairedSummary,
    /// The view side's p99 cold time to the fixture's own content, in
    /// milliseconds.
    pub gated_marker_cold_ms: f64,
    /// view p50 over nvim p50 at the content-marker boundary. The median
    /// is what every class gates: it held within x1.07 across load regimes
    /// whose tails swung x300.
    pub gated_marker_ratio_p50: f64,
    /// view p99 over nvim p99 at the same boundary. Recorded everywhere,
    /// gated only on a load-controlled class.
    pub gated_marker_ratio_p99: f64,
    /// The view side's pre-attach shell frame times. Unpaired: bare nvim
    /// paints nothing before its buffer window, so there is no counterpart
    /// series to divide by.
    pub shell: Distribution,
    /// The view side's p99 time to the startup shell, in milliseconds.
    pub gated_shell_visible_ms: f64,
}

/// Runs `protocol.warmup + protocol.samples` cold spawns per side,
/// strictly alternating view/nvim (per-sample interleaving: each sample
/// is one whole trial, so pair members sit adjacent in time).
///
/// `shell` is the view side's startup-chrome text and `marker` the fixture
/// content both sides open; the nvim side is timed to `marker` alone.
///
/// # Errors
///
/// Returns [`BenchError::Desync`] if any spawn never paints `marker`, if a
/// view spawn reaches `marker` without ever showing `shell`, or any
/// underlying session error.
pub fn run(
    view_spec: ViewSpec<'_>,
    nvim_spec: NvimSpec<'_>,
    protocol: &Protocol,
    shell: &str,
    marker: &str,
) -> Result<FirstPaintOutcome, BenchError> {
    let ViewSpec(view) = view_spec;
    let NvimSpec(nvim) = nvim_spec;
    let per_side = protocol.warmup + protocol.samples;
    let mut view_ms = Vec::with_capacity(per_side);
    let mut shell_ms = Vec::with_capacity(per_side);
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
            let (spec, watch_shell, name) = match side {
                Side::View => (view, Some(shell), "view"),
                Side::Nvim => (nvim, None, "nvim"),
            };
            let times = sample_once(spec, watch_shell, marker).map_err(|e| label(name, e))?;
            match side {
                Side::View => {
                    view_ms.push(times.marker_ms);
                    // sample_once has already refused a view spawn that
                    // reached content with no shell, so the two view
                    // series stay index-aligned by construction
                    shell_ms.extend(times.shell_ms);
                }
                Side::Nvim => nvim_ms.push(times.marker_ms),
            }
        }
        std::thread::sleep(protocol.inter_sample);
    }

    let summary = paired_summary(
        ViewSamples(&view_ms),
        NvimSamples(&nvim_ms),
        protocol.warmup,
    )?;
    let shell = Distribution::from_samples(&shell_ms, protocol.warmup)?;
    let gated_marker_cold_ms = summary.view.p99();
    let gated_marker_ratio_p50 = summary.ratio_p50;
    let gated_marker_ratio_p99 = summary.ratio_p99;
    let gated_shell_visible_ms = shell.p99();
    Ok(FirstPaintOutcome {
        summary,
        gated_marker_cold_ms,
        gated_marker_ratio_p50,
        gated_marker_ratio_p99,
        shell,
        gated_shell_visible_ms,
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
