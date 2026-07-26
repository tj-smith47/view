//! The flood scenario: a `:terminal` buffer draining an unbounded producer
//! for a fixed wall-clock window, run paired against the same flood in bare
//! nvim on the same fixture. Two things are measured, not assumed: paint
//! cadence over the window (the gaps between successive observed frame
//! changes) and drain throughput (lines drained in the window, paired: the
//! pace ratio gates). A blocked UI thread shows up as a long no-paint gap
//! while output is still pending, so the cadence percentile IS the coalescing
//! invariant, observed from outside the process. The window (not a line
//! count) bounds the run because hosts drain a fixed count at wildly
//! different rates, which left a fixed-count flood with far too few cadence
//! samples on fast hosts to form a percentile.

use std::time::{Duration, Instant};

use crate::sampling::{median_of_trials, Distribution};
use crate::session::{BenchSession, SpawnSpec};
use crate::BenchError;

/// The `:terminal` line that starts one flood's producer.
///
/// Runs the producer under a NON-interactive `sh -c` rather than typing it
/// into `:terminal`'s default interactive `$SHELL`: the interactive shell is
/// a cross-host measurement variable (zsh's ZLE makes a Linux flood ~20x
/// slower or never finish; macOS ships an ancient slow-interactive bash),
/// while a non-interactive `sh -c` is fast on every host. `yes | cat -n` is
/// the producer: unbounded (the wall-clock window, not a line count, bounds
/// the run so sample counts are comparable across hosts) and line-varying
/// (`cat -n`'s incrementing counter changes the visible screen every scroll,
/// where a bare `yes` would print identical rows and freeze the frame hash).
/// The counter doubles as the drain-progress meter (see [`max_screen_line`]).
#[must_use]
pub fn flood_command() -> String {
    String::from(":terminal sh -c 'yes | cat -n'\r")
}

/// The largest line number visible on the terminal, i.e. how many lines the
/// producer has drained so far. `cat -n` right-justifies the counter in a
/// tab-delimited field; this reads the max integer token on screen, so a
/// widening field or a partially scrolled top row cannot mislead it.
#[must_use]
pub fn max_screen_line(screen_text: &str) -> Option<u64> {
    screen_text
        .split(|c: char| !c.is_ascii_digit())
        .filter_map(|t| t.parse::<u64>().ok())
        .max()
}

/// One side's flood measurement over the wall-clock window.
#[derive(Debug)]
pub struct FloodSide {
    /// Lines the producer drained through the terminal during the window
    /// (the highest `cat -n` counter reached). The drain-throughput meter:
    /// with the window fixed, more lines means the side kept pace better.
    pub lines_drained: f64,
    /// Gaps between successive observed frame changes during the window,
    /// in milliseconds.
    pub cadence_gaps_ms: Vec<f64>,
    /// Mean wall time one probe iteration took during the window, in
    /// milliseconds: the resolution floor on every gap above, since a frame
    /// change is only ever observed on a probe.
    pub probe_period_ms: f64,
}

/// How far above the probe loop's own period a cadence measurement must
/// sit to be treated as a measurement of view rather than of the harness.
///
/// 2x: at the floor itself every observed gap is one probe iteration and
/// the number is pure instrument; one factor of two above it, the observed
/// distribution has room to hold at least two distinguishable outcomes per
/// gap, so a real change in view's coalescing can still move it.
const CADENCE_RESOLUTION_FACTOR: f64 = 2.0;

/// Whether a cadence percentile sits far enough above the probe loop's own
/// period to describe view rather than the harness.
fn cadence_is_measurable(cadence_p99_ms: f64, probe_period_ms: f64) -> bool {
    cadence_p99_ms >= probe_period_ms * CADENCE_RESOLUTION_FACTOR
}

/// The idle span with no frame change that declares the producer started
/// (or, if it elapses first, that it never did): the flood changes the
/// screen continuously once running, so a quiet gap this long at the head
/// means the terminal command never produced output.
const PRODUCER_START_DEADLINE: Duration = Duration::from_secs(15);

/// The highest `cat -n` counter currently on screen.
///
/// Read once at the window's end rather than every probe: the counters only
/// grow and the producer's output scrolls, so the final screen already holds
/// the maximum, while building the whole screen's text on every iteration
/// only widens the probe loop -- and the probe period is the floor on every
/// cadence gap this loop can observe.
fn drained_lines(session: &mut BenchSession) -> Option<u64> {
    session.with_screen(|screen| {
        let (rows, cols) = screen.size();
        let mut text = String::new();
        for row in 0..rows {
            text.push_str(&crate::boundaries::row_text(screen, row, cols));
            text.push('\n');
        }
        max_screen_line(&text)
    })
}

/// Spawns `spec`, opens `:terminal` on the pinned producer, and samples
/// paint cadence plus drain throughput for `window` of steady flood.
///
/// The window (not a line count) bounds the run, so the sample count is a
/// property of duration and is comparable across hosts that drain at wildly
/// different rates (the reason [`flood_command`] runs an unbounded producer).
fn flood_once(
    spec: &SpawnSpec,
    settle_deadline: Duration,
    window: Duration,
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
    session.send(flood_command().as_bytes())?;

    // wait for the producer to begin scrolling before starting the window, so
    // it measures steady flood rather than the terminal-open transient
    let mut last_hash = session.with_screen(crate::boundaries::screen_hash);
    let armed = Instant::now();
    loop {
        let hash = session.with_screen(crate::boundaries::screen_hash);
        if hash != last_hash {
            last_hash = hash;
            break;
        }
        if armed.elapsed() >= PRODUCER_START_DEADLINE {
            return Err(BenchError::Desync {
                context: format!(
                    "flood producer never scrolled the terminal; screen:\n{}",
                    session.screen_text()
                ),
            });
        }
        std::thread::yield_now();
    }

    let start = Instant::now();
    let deadline = start + window;
    let mut last_change: Option<Instant> = Some(start);
    let mut gaps_ms = Vec::new();
    let mut probes = 0_u32;
    let elapsed = loop {
        let hash = session.with_screen(crate::boundaries::screen_hash);
        let now = Instant::now();
        probes = probes.saturating_add(1);
        if hash != last_hash {
            if let Some(previous) = last_change {
                gaps_ms.push(now.duration_since(previous).as_secs_f64() * 1000.0);
            }
            last_change = Some(now);
            last_hash = hash;
        }
        if now >= deadline {
            break now.duration_since(start);
        }
        std::thread::yield_now();
    };
    let lines_drained = drained_lines(&mut session).unwrap_or(0);

    session.shutdown();
    Ok(FloodSide {
        #[allow(clippy::cast_precision_loss)]
        lines_drained: lines_drained as f64,
        cadence_gaps_ms: gaps_ms,
        #[allow(clippy::cast_lossless)]
        probe_period_ms: elapsed.as_secs_f64() * 1000.0 / f64::from(probes.max(1)),
    })
}

/// The flood run's outcome.
#[derive(Debug)]
pub struct FloodOutcome {
    pub view_trials: Vec<FloodSide>,
    pub nvim_trials: Vec<FloodSide>,
    /// Median across trials of nvim lines drained over view lines drained.
    /// Lower is better: view keeping pace makes the two counts equal (~1.0);
    /// view falling behind drains fewer lines in the window and lifts it.
    pub gated_pace_ratio: f64,
    /// Median across trials of the view side's cadence-gap p99 (ms).
    pub gated_cadence_p99_ms: f64,
    /// Worst single view-side no-paint gap observed across all trials
    /// (ms); reported, not gated: the max of a scheduler-noisy quantity.
    pub view_stall_max_ms: f64,
}

/// Runs `trials` paired floods. A flood is one long macro operation, so
/// pairing is per trial (view and nvim floods back to back within the
/// same invocation, order alternating per trial) rather than per-sample
/// interleaving, which has no meaning inside a single window.
///
/// # Errors
///
/// Returns [`BenchError::Desync`] if a producer never scrolls the terminal
/// or a session misbehaves, [`BenchError::NotEnoughSamples`] if a window
/// produced fewer observed frame changes than the protocol's floor (the
/// cadence percentile would be built on too few gaps to mean anything), and
/// [`BenchError::BelowInstrumentResolution`] if the cadence percentile sits
/// within [`CADENCE_RESOLUTION_FACTOR`] of the probe loop's own period.
pub fn run(
    view: &SpawnSpec,
    nvim: &SpawnSpec,
    trials: usize,
    min_gap_samples: usize,
    settle_deadline: Duration,
    window: Duration,
) -> Result<FloodOutcome, BenchError> {
    let mut view_trials = Vec::with_capacity(trials);
    let mut nvim_trials = Vec::with_capacity(trials);
    for trial in 0..trials {
        if trial % 2 == 0 {
            view_trials
                .push(flood_once(view, settle_deadline, window).map_err(|e| label("view", e))?);
            nvim_trials
                .push(flood_once(nvim, settle_deadline, window).map_err(|e| label("nvim", e))?);
        } else {
            nvim_trials
                .push(flood_once(nvim, settle_deadline, window).map_err(|e| label("nvim", e))?);
            view_trials
                .push(flood_once(view, settle_deadline, window).map_err(|e| label("view", e))?);
        }
    }

    let mut ratios = Vec::with_capacity(trials);
    let mut cadence_p99s = Vec::with_capacity(trials);
    let mut stall_max: f64 = 0.0;
    for (view_side, nvim_side) in view_trials.iter().zip(&nvim_trials) {
        if !(view_side.lines_drained.is_finite() && view_side.lines_drained > 0.0) {
            return Err(BenchError::DegenerateBaselineSide {
                statistic: "lines_drained",
                value: view_side.lines_drained,
            });
        }
        ratios.push(nvim_side.lines_drained / view_side.lines_drained);
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

    let gated_cadence_p99_ms = median_of_trials(&cadence_p99s)?;
    let probe_period_ms = median_of_trials(
        &view_trials
            .iter()
            .map(|t| t.probe_period_ms)
            .collect::<Vec<_>>(),
    )?;
    // a frame change is only ever seen on a probe, so no gap can be finer
    // than the probe loop's own period: a cadence number down at that floor
    // is reporting how fast this harness can hash a screen, not how well
    // view coalesces a flood. Refused rather than recorded, because a bar
    // set from the instrument gates every later run against the instrument.
    if !cadence_is_measurable(gated_cadence_p99_ms, probe_period_ms) {
        return Err(BenchError::BelowInstrumentResolution {
            metric: "cadence_p99_ms",
            value: gated_cadence_p99_ms,
            resolution: probe_period_ms,
            factor: CADENCE_RESOLUTION_FACTOR,
        });
    }

    Ok(FloodOutcome {
        gated_pace_ratio: median_of_trials(&ratios)?,
        gated_cadence_p99_ms,
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
    fn a_cadence_at_the_probe_period_is_not_a_measurement_of_view() {
        // every observed gap is one probe iteration here: the number is the
        // harness's screen-hash rate, and recording it would gate every
        // later run against this machine's hashing speed
        assert!(!cadence_is_measurable(0.40, 0.40));
        assert!(!cadence_is_measurable(0.79, 0.40));
    }

    #[test]
    fn a_cadence_clear_of_the_probe_period_is_measurable() {
        assert!(cadence_is_measurable(0.80, 0.40));
        assert!(cadence_is_measurable(16.0, 0.40));
    }

    #[test]
    fn flood_command_pins_a_noninteractive_shell_and_an_unbounded_varying_producer() {
        let command = flood_command();
        assert!(
            command.contains("sh -c"),
            "the producer must run under a fixed non-interactive shell, not the \
             host's interactive $SHELL (a cross-host variable): {command}"
        );
        assert!(
            command.contains("cat -n"),
            "the producer must number its lines so the visible screen keeps \
             changing (a bare `yes` freezes the frame hash): {command}"
        );
        assert!(
            command.starts_with(":terminal ") && command.ends_with('\r'),
            "the command must be a single :terminal line submitted with CR: {command}"
        );
    }

    #[test]
    fn max_screen_line_reads_the_largest_cat_n_counter_on_screen() {
        let screen = "     8\ty\n     9\ty\n    10\ty\n    11\ty\n";
        assert_eq!(
            max_screen_line(screen),
            Some(11),
            "the drain meter is the highest line number the producer has reached"
        );
    }

    #[test]
    fn max_screen_line_is_none_on_a_screen_with_no_digits() {
        assert_eq!(max_screen_line("waiting for terminal\n~\n~\n"), None);
    }

    #[test]
    fn max_screen_line_ignores_a_partially_scrolled_top_row() {
        // the top row can be clipped mid-number as the buffer scrolls; the
        // max token still comes from a whole lower line, never the fragment
        let screen = "34\ty\n   1201\ty\n   1202\ty\n";
        assert_eq!(max_screen_line(screen), Some(1202));
    }
}
