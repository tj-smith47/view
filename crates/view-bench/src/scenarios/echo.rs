//! The echo scenario: keypress to cell change under steady insert-mode
//! typing, view and nvim interleaved within one run. The response
//! boundary is the spec's "first vt100-parsed frame where the target cell
//! differs": each sample waits for the typed character to appear in the
//! exact cell the cursor occupied, discovered per line by a probe
//! character rather than assumed from any chrome layout.

use std::time::{Duration, Instant};

use crate::pairing::{paired_summary, NvimSamples, PairedSummary, ViewSamples};
use crate::sampling::{interleave_schedule, median_of_trials, Side};
use crate::scenarios::clock::monotonic_nanos;
use crate::scenarios::Protocol;
use crate::session::{BenchSession, NvimSpec, SettleBound, SpawnSpec, ViewSpec};
use crate::BenchError;

/// Characters typed per line before the driver opens a fresh line:
/// keeps every line shorter than the grid width so a wrap can never move
/// the target cell mid-line, and keeps single-line redraw cost flat.
const LINE_LIMIT: usize = 100;

/// The typed sample character: not a Vim motion/digraph trigger on its
/// own in insert mode.
const SAMPLE_CHAR: &str = "x";

/// The first settle's quiet span for a spawn with no injected transport
/// latency: right for a local attach, which finishes well inside it. A
/// caller measuring under injected latency (the RTT-acceptance harness)
/// must widen this the same way it widens `probe_timeout`, since a fixed
/// span this small is satisfied by the pre-attach shell frame's own static
/// paint before a slow attach's real content ever lands; see
/// [`SideState::prepare`]'s doc comment.
pub const DEFAULT_STARTUP_QUIET: Duration = Duration::from_millis(500);

/// One side's typing state: where the next character will land.
pub(crate) struct SideState {
    session: BenchSession,
    at: crate::boundaries::CellPos,
    origin_col: u16,
    line_len: usize,
    windows: Vec<(i64, i64)>,
}

impl SideState {
    /// Prepares a spawned session for sampling: settle, enter insert
    /// mode, then locate the buffer origin by typing and erasing a probe
    /// character (the one observation that works identically across both
    /// editors and any chrome/gutter layout).
    ///
    /// `probe_timeout` bounds the probe's own two round trips (type, then
    /// erase) the same way `protocol.sample_timeout` bounds a regular
    /// sample: a caller measuring under injected transport latency passes
    /// the same widened bound here, since the probe pays that latency too.
    ///
    /// `startup_quiet` is the first settle's own quiet span -- see its note
    /// below for why it cannot share `probe_timeout`'s value.
    pub(crate) fn prepare(
        spec: &SpawnSpec,
        settle_deadline: Duration,
        probe_timeout: Duration,
        startup_quiet: Duration,
    ) -> Result<Self, BenchError> {
        let mut session = BenchSession::spawn(spec)?;
        // a fixed 500ms quiet span (right for a local spawn, where the
        // engine attach that replaces the pre-attach shell frame finishes
        // well inside it) is satisfied by that same shell frame BEFORE the
        // attach it is waiting on completes once the transport carrying
        // attach's own handshake/register/ui_attach round trips is
        // injected-latency: the frame paints once, holds bit-for-bit
        // static while attach is still in flight, and 500ms of "no change"
        // reads as settled on a screen that has not attached yet. The
        // caller passes a quiet span already widened for its transport
        // (the same shape as `probe_timeout`'s), so real attach traffic
        // landing before the span elapses resets the quiet clock instead
        // of racing it.
        if !session.settle(SettleBound {
            quiet: startup_quiet,
            deadline: settle_deadline,
        }) {
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
        if !session.settle(SettleBound {
            quiet: Duration::from_secs(2),
            deadline: settle_deadline,
        }) {
            return Err(BenchError::Desync {
                context: format!(
                    "insert-mode entry never went quiet within {settle_deadline:?}; screen:\n{}",
                    session.screen_text()
                ),
            });
        }
        let at = probe_origin(&mut session, probe_timeout)?;
        Ok(Self {
            session,
            at,
            origin_col: at.col,
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
    ///
    /// Reads its own bound and gap off the protocol instead of taking them
    /// as two `Duration`s: handed over the other way round, the sample
    /// still measures the same window and the row still prints a plausible
    /// ratio, paying a 5s gap per sample against a 10ms bound with nothing
    /// but wall-clock time to say so.
    pub(crate) fn sample_one(&mut self, protocol: &Protocol) -> Result<(i64, i64), BenchError> {
        if self.line_len >= LINE_LIMIT {
            self.open_fresh_line()?;
        }
        let start = monotonic_nanos();
        self.session.send(SAMPLE_CHAR.as_bytes())?;
        if !self
            .session
            .wait_cell(self.at, SAMPLE_CHAR, protocol.sample_timeout)
        {
            return Err(BenchError::Desync {
                context: format!(
                    "sample never appeared at ({}, {}); screen:\n{}",
                    self.at.row,
                    self.at.col,
                    self.session.screen_text()
                ),
            });
        }
        let seen = monotonic_nanos();
        self.windows.push((start, seen));
        self.at.col += 1;
        self.line_len += 1;
        std::thread::sleep(protocol.inter_sample);
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
        self.at.row += 1;
        self.at.col = self.origin_col;
        self.line_len = 0;
        // the new line starts empty; give the paint a bounded moment so
        // the first sample of the line cannot race the line-open redraw
        std::thread::sleep(Duration::from_millis(50));
        Ok(())
    }

    /// Clears the buffer between trials so cell positions and line count
    /// reset; keeps a trial's screen state identical to the first
    /// trial's.
    pub(crate) fn reset_buffer(&mut self, probe_timeout: Duration) -> Result<(), BenchError> {
        self.session.send(b"\x1b:%d _\r")?;
        std::thread::sleep(Duration::from_millis(200));
        self.session.send(b"i")?;
        std::thread::sleep(Duration::from_millis(100));
        self.at = probe_origin(&mut self.session, probe_timeout)?;
        self.origin_col = self.at.col;
        self.line_len = 0;
        Ok(())
    }
}

/// Locates the cell the next typed character will occupy by typing one
/// probe character and diffing sample-character cell positions against
/// the pre-probe screen, then erasing the probe again. A whole-screen
/// uniqueness search would race chrome: a statusline rendering
/// `scratch.txt` already holds an `x` cell before the probe ever paints.
fn probe_origin(
    session: &mut BenchSession,
    probe_timeout: Duration,
) -> Result<crate::boundaries::CellPos, BenchError> {
    let baseline =
        session.with_screen(|screen| crate::boundaries::char_cell_positions(screen, SAMPLE_CHAR));
    session.send(SAMPLE_CHAR.as_bytes())?;
    let deadline = Instant::now() + probe_timeout;
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
        // a real sleep, not `yield_now`: this wait can run for the whole
        // probe timeout under an injected-latency spawn (a startup
        // handshake stretched by transport RTT, not a fixed few hundred
        // ms), and a tight single-thread busy-spin held that long starves
        // the pty's own reader of scheduling time on a host without cores
        // to spare, which starves the very screen state this loop reads
        std::thread::sleep(Duration::from_millis(5));
    };
    session.send(b"\x7f")?;
    // erased means the probed cell no longer holds the sample character,
    // not that the screen equals the pre-probe baseline: chrome changes
    // legitimately between the snapshot and the erase (a notification
    // toast times out and vanishes, the modified flag appears in the
    // statusline the moment the probe types), so baseline equality can
    // become permanently unreachable on a host slow enough for a toast
    // to outlive the settle window
    let deadline = Instant::now() + probe_timeout;
    loop {
        let cleared = session.with_screen(|screen| {
            !crate::boundaries::char_cell_positions(screen, SAMPLE_CHAR).contains(&position)
        });
        if cleared {
            return Ok(position);
        }
        if Instant::now() >= deadline {
            return Err(BenchError::Desync {
                context: format!(
                    "probe character never erased from ({}, {}); screen:\n{}",
                    position.row,
                    position.col,
                    session.screen_text()
                ),
            });
        }
        // see the identical note on the wait above: a real sleep, not a
        // busy-spin, for a wait that can run the full probe timeout
        std::thread::sleep(Duration::from_millis(5));
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
/// `startup_quiet` is [`SideState::prepare`]'s first-settle quiet span; see
/// its doc comment for why a caller under injected transport latency must
/// widen it rather than pass the local-spawn default.
///
/// # Errors
///
/// Returns [`BenchError::Desync`] if either editor stops responding to
/// typed input within the sample timeout, or any underlying session error.
pub fn run(
    view_spec: ViewSpec<'_>,
    nvim_spec: NvimSpec<'_>,
    protocol: &Protocol,
    settle_deadline: Duration,
    startup_quiet: Duration,
) -> Result<EchoOutcome, BenchError> {
    run_observed(
        view_spec,
        nvim_spec,
        protocol,
        settle_deadline,
        startup_quiet,
        &mut |_| {},
    )
}

/// The echo scenario with every measured view sample's monotonic window
/// handed to `observe_view` as it is taken.
///
/// The window is the same `(start, seen)` pair the row's own milliseconds
/// are derived from, so a caller reading a second source about the same
/// sample -- the tap stream, for a row that has to say *what* answered the
/// keystroke -- intersects it against the identical interval this row
/// timed, rather than against a window measured a second time.
///
/// Warmup samples are not offered: they are excluded from every statistic
/// the row reports, and an observer counting them would describe a
/// population the numbers beside it were not taken from.
///
/// # Errors
///
/// As [`run`].
pub(crate) fn run_observed(
    view_spec: ViewSpec<'_>,
    nvim_spec: NvimSpec<'_>,
    protocol: &Protocol,
    settle_deadline: Duration,
    startup_quiet: Duration,
    observe_view: &mut dyn FnMut((i64, i64)),
) -> Result<EchoOutcome, BenchError> {
    let ViewSpec(view) = view_spec;
    let NvimSpec(nvim) = nvim_spec;
    let mut view_state = SideState::prepare(
        view,
        settle_deadline,
        protocol.sample_timeout,
        startup_quiet,
    )
    .map_err(|e| label("view", e))?;
    let mut nvim_state = SideState::prepare(
        nvim,
        settle_deadline,
        protocol.sample_timeout,
        startup_quiet,
    )
    .map_err(|e| label("nvim", e))?;

    let mut trials = Vec::with_capacity(protocol.trials);
    for trial in 0..protocol.trials {
        if trial > 0 {
            view_state
                .reset_buffer(protocol.sample_timeout)
                .map_err(|e| label("view", e))?;
            nvim_state
                .reset_buffer(protocol.sample_timeout)
                .map_err(|e| label("nvim", e))?;
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
        let mut view_taken = 0;
        for block in interleave_schedule(per_side, protocol.block, start) {
            let (state, side_name) = match block.side {
                Side::View => (&mut view_state, "view"),
                Side::Nvim => (&mut nvim_state, "nvim"),
            };
            for _ in 0..block.count {
                let window = state
                    .sample_one(protocol)
                    .map_err(|e| label(side_name, e))?;
                if block.side == Side::View {
                    view_taken += 1;
                    if view_taken > protocol.warmup {
                        observe_view(window);
                    }
                }
            }
        }
        trials.push(paired_summary(
            ViewSamples(&view_state.raw_ms()),
            NvimSamples(&nvim_state.raw_ms()),
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
