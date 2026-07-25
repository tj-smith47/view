//! The echo scenario: keypress to cell change under steady insert-mode
//! typing, view and nvim interleaved within one run. The response
//! boundary is the spec's "first vt100-parsed frame where the target cell
//! differs": each sample waits for the typed character to appear in the
//! exact cell the cursor occupied, discovered per line by a probe
//! character rather than assumed from any chrome layout.

use std::time::{Duration, Instant};

use crate::pairing::{paired_summary, PairedSummary};
use crate::sampling::{interleave_schedule, median_of_trials, Side};
use crate::scenarios::taps::monotonic_nanos;
use crate::scenarios::Protocol;
use crate::session::{BenchSession, SpawnSpec};
use crate::BenchError;

/// Characters typed per line before the driver opens a fresh line:
/// keeps every line shorter than the grid width so a wrap can never move
/// the target cell mid-line, and keeps single-line redraw cost flat.
const LINE_LIMIT: usize = 100;

/// The typed sample character: not a Vim motion/digraph trigger on its
/// own in insert mode.
const SAMPLE_CHAR: &str = "x";

/// One side's typing state: where the next character will land.
pub(crate) struct SideState {
    session: BenchSession,
    row: u16,
    col: u16,
    origin_col: u16,
    line_len: usize,
    windows: Vec<(i64, i64)>,
}

impl SideState {
    /// Prepares a spawned session for sampling: settle, enter insert
    /// mode, then locate the buffer origin by typing and erasing a probe
    /// character (the one observation that works identically across both
    /// editors and any chrome/gutter layout).
    pub(crate) fn prepare(spec: &SpawnSpec, settle_deadline: Duration) -> Result<Self, BenchError> {
        let mut session = BenchSession::spawn(spec)?;
        if !session.settle(Duration::from_millis(500), settle_deadline) {
            return Err(BenchError::Desync {
                context: format!(
                    "startup never went quiet within {settle_deadline:?}; screen:\n{}",
                    session.screen_text()
                ),
            });
        }
        session.send(b"i")?;
        // entering insert mode is itself a plugin-load trigger under a
        // lazy-loading config (completion engines, noice warning toasts
        // that float over the text area); a second, stricter settle here
        // absorbs that churn so no toast can occlude the sampled cells
        if !session.settle(Duration::from_secs(2), settle_deadline) {
            return Err(BenchError::Desync {
                context: format!(
                    "insert-mode entry never went quiet within {settle_deadline:?}; screen:\n{}",
                    session.screen_text()
                ),
            });
        }
        let (row, col) = probe_origin(&mut session)?;
        Ok(Self {
            session,
            row,
            col,
            origin_col: col,
            line_len: 0,
            windows: Vec::new(),
        })
    }

    /// Types one sample character and records the monotonic window it took
    /// to appear in the cursor's cell, returning that window.
    ///
    /// The window's endpoints come from the same `CLOCK_MONOTONIC` the
    /// instrumented build's tap records carry, so a sample's window can be
    /// intersected directly with the tap stream; the paired milliseconds
    /// below are derived from it rather than timed by a second clock, which
    /// keeps one sample at exactly two clock reads either way.
    pub(crate) fn sample_one(
        &mut self,
        timeout: Duration,
        inter_sample: Duration,
    ) -> Result<(i64, i64), BenchError> {
        if self.line_len >= LINE_LIMIT {
            self.open_fresh_line()?;
        }
        let start = monotonic_nanos();
        self.session.send(SAMPLE_CHAR.as_bytes())?;
        if !self
            .session
            .wait_cell(self.row, self.col, SAMPLE_CHAR, timeout)
        {
            return Err(BenchError::Desync {
                context: format!(
                    "sample never appeared at ({}, {}); screen:\n{}",
                    self.row,
                    self.col,
                    self.session.screen_text()
                ),
            });
        }
        let seen = monotonic_nanos();
        self.windows.push((start, seen));
        self.col += 1;
        self.line_len += 1;
        std::thread::sleep(inter_sample);
        Ok((start, seen))
    }

    /// This side's per-sample latencies in milliseconds, derived from the
    /// recorded windows.
    #[allow(clippy::cast_precision_loss)]
    pub(crate) fn raw_ms(&self) -> Vec<f64> {
        self.windows
            .iter()
            .map(|(start, seen)| (seen - start) as f64 / 1_000_000.0)
            .collect()
    }

    /// Discards the samples collected so far, for the start of a trial.
    pub(crate) fn clear_samples(&mut self) {
        self.windows.clear();
    }

    /// Ends this side's session; see [`BenchSession::shutdown`].
    pub(crate) fn shutdown(&mut self) {
        self.session.shutdown();
    }

    /// Opens a fresh line under the current one (Esc also dismisses any
    /// completion popup a plugin-heavy config raised, so the popup can
    /// never swallow the newline) and re-enters insert mode. Untimed:
    /// line management is driver bookkeeping, not a sample.
    fn open_fresh_line(&mut self) -> Result<(), BenchError> {
        self.session.send(b"\x1bo")?;
        self.row += 1;
        self.col = self.origin_col;
        self.line_len = 0;
        // the new line starts empty; give the paint a bounded moment so
        // the first sample of the line cannot race the line-open redraw
        std::thread::sleep(Duration::from_millis(50));
        Ok(())
    }

    /// Clears the buffer between trials so cell positions and line count
    /// reset; keeps a trial's screen state identical to the first
    /// trial's.
    pub(crate) fn reset_buffer(&mut self) -> Result<(), BenchError> {
        self.session.send(b"\x1b:%d _\r")?;
        std::thread::sleep(Duration::from_millis(200));
        self.session.send(b"i")?;
        std::thread::sleep(Duration::from_millis(100));
        let (row, col) = probe_origin(&mut self.session)?;
        self.row = row;
        self.col = col;
        self.origin_col = col;
        self.line_len = 0;
        Ok(())
    }
}

/// Locates the cell the next typed character will occupy by typing one
/// probe character and diffing sample-character cell positions against
/// the pre-probe screen, then erasing the probe again. A whole-screen
/// uniqueness search would race chrome: a statusline rendering
/// `scratch.txt` already holds an `x` cell before the probe ever paints.
fn probe_origin(session: &mut BenchSession) -> Result<(u16, u16), BenchError> {
    let baseline =
        session.with_screen(|screen| crate::boundaries::char_cell_positions(screen, SAMPLE_CHAR));
    session.send(SAMPLE_CHAR.as_bytes())?;
    let deadline = Instant::now() + Duration::from_secs(5);
    let position = loop {
        let now = session
            .with_screen(|screen| crate::boundaries::char_cell_positions(screen, SAMPLE_CHAR));
        if let Some(position) = crate::boundaries::single_new_position(&baseline, &now) {
            break position;
        }
        if Instant::now() >= deadline {
            return Err(BenchError::Desync {
                context: format!(
                    "probe character never appeared as a new cell (chrome baseline {baseline:?}); \
                     screen:\n{}",
                    session.screen_text()
                ),
            });
        }
        std::thread::yield_now();
    };
    session.send(b"\x7f")?;
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let now = session
            .with_screen(|screen| crate::boundaries::char_cell_positions(screen, SAMPLE_CHAR));
        if now == baseline {
            return Ok(position);
        }
        if Instant::now() >= deadline {
            return Err(BenchError::Desync {
                context: format!(
                    "probe character never erased back to the chrome baseline {baseline:?}; \
                     screen:\n{}",
                    session.screen_text()
                ),
            });
        }
        std::thread::yield_now();
    }
}

/// Stamps a desync error with which editor produced it; the two sides'
/// failure screens are otherwise indistinguishable in a report.
pub(crate) fn label(side: &str, err: BenchError) -> BenchError {
    match err {
        BenchError::Desync { context } => BenchError::Desync {
            context: format!("[{side} side] {context}"),
        },
        other => other,
    }
}

/// The echo run's outcome: every trial's paired summary plus the median
/// statistics the gate reads.
#[derive(Debug)]
pub struct EchoOutcome {
    pub trials: Vec<PairedSummary>,
    pub gated_ratio_p50: f64,
    pub gated_ratio_p99: f64,
    pub gated_paired_delta_p99_ms: f64,
    /// Median across trials of the view side's absolute p99, in ms: the
    /// row's gated tail statistic against the spec budget, since the
    /// tail *ratio* is unusable on shared classes.
    pub gated_view_p99_ms: f64,
}

/// Drives the full echo scenario: both editors spawned once, then
/// `protocol.trials` interleaved trials of `warmup + samples` keypresses
/// per side, buffer reset between trials.
///
/// # Errors
///
/// Returns [`BenchError::Desync`] if either editor stops responding to
/// typed input within the sample timeout, or any underlying session error.
pub fn run(
    view: &SpawnSpec,
    nvim: &SpawnSpec,
    protocol: &Protocol,
    settle_deadline: Duration,
) -> Result<EchoOutcome, BenchError> {
    let mut view_state = SideState::prepare(view, settle_deadline).map_err(|e| label("view", e))?;
    let mut nvim_state = SideState::prepare(nvim, settle_deadline).map_err(|e| label("nvim", e))?;

    let mut trials = Vec::with_capacity(protocol.trials);
    for trial in 0..protocol.trials {
        if trial > 0 {
            view_state.reset_buffer().map_err(|e| label("view", e))?;
            nvim_state.reset_buffer().map_err(|e| label("nvim", e))?;
        }
        view_state.clear_samples();
        nvim_state.clear_samples();

        // the starting side alternates per trial so neither editor
        // systematically samples first within a block pattern
        let start = if trial % 2 == 0 {
            Side::View
        } else {
            Side::Nvim
        };
        let per_side = protocol.warmup + protocol.samples;
        for block in interleave_schedule(per_side, protocol.block, start) {
            let (state, side_name) = match block.side {
                Side::View => (&mut view_state, "view"),
                Side::Nvim => (&mut nvim_state, "nvim"),
            };
            for _ in 0..block.count {
                state
                    .sample_one(protocol.sample_timeout, protocol.inter_sample)
                    .map_err(|e| label(side_name, e))?;
            }
        }
        trials.push(paired_summary(
            &view_state.raw_ms(),
            &nvim_state.raw_ms(),
            protocol.warmup,
        )?);
    }

    view_state.shutdown();
    nvim_state.shutdown();

    let median_ratios: Vec<f64> = trials.iter().map(|t| t.ratio_p50).collect();
    let ratios: Vec<f64> = trials.iter().map(|t| t.ratio_p99).collect();
    let deltas: Vec<f64> = trials.iter().map(|t| t.paired_delta_p99_ms).collect();
    let view_p99s: Vec<f64> = trials.iter().map(|t| t.view.p99()).collect();
    Ok(EchoOutcome {
        gated_ratio_p50: median_of_trials(&median_ratios)?,
        gated_ratio_p99: median_of_trials(&ratios)?,
        gated_paired_delta_p99_ms: median_of_trials(&deltas)?,
        gated_view_p99_ms: median_of_trials(&view_p99s)?,
        trials,
    })
}
