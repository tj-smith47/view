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
use crate::session::{BenchSession, SettleBound, SpawnSpec};
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

/// A cadence percentile and the probe period it has to clear, both in
/// milliseconds.
///
/// Named fields rather than two adjacent `f64` parameters: transposed, the
/// comparison inverts, and it then refuses every side whose cadence is well
/// resolved while accepting every side reporting pure instrument.
#[derive(Debug, Clone, Copy)]
struct CadenceResolution {
    p99_ms: f64,
    probe_period_ms: f64,
}

/// Whether a cadence percentile sits far enough above the probe loop's own
/// period to describe view rather than the harness.
fn cadence_is_measurable(cadence: CadenceResolution) -> bool {
    cadence.p99_ms >= cadence.probe_period_ms * CADENCE_RESOLUTION_FACTOR
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
    session.with_screen(|screen| max_screen_line(&crate::boundaries::screen_lines(screen)))
}

/// Spawns `spec`, opens `:terminal` on the pinned producer, and samples
/// paint cadence plus drain throughput for one window of steady flood.
///
/// The window (not a line count) bounds the run, so the sample count is a
/// property of duration and is comparable across hosts that drain at wildly
/// different rates (the reason [`flood_command`] runs an unbounded producer).
///
/// Reads both durations off `run_spec` rather than taking them as adjacent
/// parameters of the same type, which a caller can transpose silently: a
/// settle deadline in the window's place measures nothing and a window in
/// the deadline's place refuses every startup.
fn flood_once(spec: &SpawnSpec, run_spec: &RunSpec<'_>) -> Result<FloodSide, BenchError> {
    let window = run_spec.window;
    let mut session = BenchSession::spawn(spec)?;
    if !session.settle(SettleBound {
        quiet: Duration::from_secs(2),
        deadline: run_spec.settle_deadline,
    }) {
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

/// One side's cadence distribution for one trial, in milliseconds.
///
/// p50 and p90 are carried alongside the gated p99 because they are what
/// separates the two readings this row can produce: a coalescing failure
/// detaches the p99 from the bulk, while a redraw cadence puts its p99 on
/// the jitter edge of a p50 it stays close to. A p99 reported alone cannot
/// be told apart.
#[derive(Debug, Clone, Copy)]
pub struct SideCadence {
    pub p50_ms: f64,
    pub p90_ms: f64,
    pub p99_ms: f64,
    pub max_ms: f64,
}

/// One paired trial: both sides' raw measurement, both sides' cadence
/// distributions, and the two quotients the trial contributes.
#[derive(Debug)]
pub struct FloodTrial {
    pub view: FloodSide,
    pub nvim: FloodSide,
    pub view_cadence: SideCadence,
    pub nvim_cadence: SideCadence,
    /// nvim lines drained over view lines drained, this trial.
    pub pace_ratio: f64,
    /// `view_cadence.p99_ms / nvim_cadence.p99_ms`, this trial.
    pub cadence_p99_ratio: f64,
}

/// The flood run's outcome.
#[derive(Debug)]
pub struct FloodOutcome {
    pub trials: Vec<FloodTrial>,
    /// Median across trials of nvim lines drained over view lines drained.
    /// Lower is better: view keeping pace makes the two counts equal (~1.0);
    /// view falling behind drains fewer lines in the window and lifts it.
    pub gated_pace_ratio: f64,
    /// Median across trials of the view side's cadence-gap p99 (ms).
    pub gated_cadence_p99_ms: f64,
    /// Median across trials of the nvim side's cadence-gap p99 (ms), the
    /// denominator of [`gated_cadence_p99_ratio`](Self::gated_cadence_p99_ratio).
    pub nvim_cadence_p99_ms: f64,
    /// Median across trials of the view side's cadence-gap p99 over the
    /// nvim side's, both taken on the same host inside the same run.
    ///
    /// The cross-class statistic for this row, because the absolute one
    /// cannot be: how many bytes a flood delivers per read into the editor
    /// is set by the kernel's pty output buffer, which no producer can
    /// choose (measured: neither a block-writing producer nor disabling
    /// output post-processing moves either host onto the other's value --
    /// see the flood-stimulus measurement note). Both arms meet whatever
    /// this host's buffer does, so the quotient survives a difference the
    /// millisecond number cannot.
    ///
    /// A ratio of two p99s, not the p99 of a paired ratio: the two sides
    /// run in sequence over one window each and their gaps have no
    /// per-sample correspondence to pair. The name says which it is.
    pub gated_cadence_p99_ratio: f64,
    /// Worst single view-side no-paint gap observed across all trials
    /// (ms); reported, not gated: the max of a scheduler-noisy quantity.
    pub view_stall_max_ms: f64,
}

/// The two names one side is reported under: the side itself, and the
/// metric name its cadence percentile is refused and reported under (the
/// nvim side's label is a report line, not a recorded metric). Paired so no
/// call site can raise a refusal that names one side while carrying the
/// other's number.
#[derive(Clone, Copy)]
struct SideNames {
    side: &'static str,
    cadence_metric: &'static str,
}

const VIEW_NAMES: SideNames = SideNames {
    side: "view",
    cadence_metric: "cadence_p99_ms",
};
const NVIM_NAMES: SideNames = SideNames {
    side: "nvim",
    cadence_metric: "nvim cadence_p99_ms",
};

/// How many paired trials a run measures, and the gap floor each side of
/// each trial must clear.
///
/// The two counts are carried as named fields rather than as adjacent
/// `usize` parameters: transposing them compiles, lints clean and reads as
/// a plausible call, while turning a three-trial run with a floor of 200
/// into a two-hundred-trial run with a floor of three.
#[derive(Debug, Clone, Copy)]
pub struct TrialPlan {
    pub trials: usize,
    pub min_gap_samples: usize,
}

/// One trial's two measured sides.
///
/// Named fields rather than a `(FloodSide, FloodSide)` tuple: the two are
/// the same type, so producing or destructuring the pair in the wrong order
/// compiles, keeps every guard satisfied, and inverts every quotient the
/// trial forms -- view's drain rate over nvim's becomes nvim's over view's,
/// with both numbers still entirely plausible.
#[derive(Debug)]
struct TrialPair {
    view: FloodSide,
    nvim: FloodSide,
}

/// Everything one flood run measures against: the two sides, how many
/// trials to take, and the two durations that bound each one.
///
/// Named fields for the same reason [`TrialPlan`] has them. The two
/// `&SpawnSpec` and the two [`Duration`] arguments this replaces were
/// adjacent and same-typed, so a transposition compiled: swapping the specs
/// inverts the whole view-against-nvim comparison the row exists to make,
/// and the gate cannot see it because both sides still measure something.
#[derive(Debug, Clone, Copy)]
pub struct RunSpec<'a> {
    /// The side under measurement.
    pub view: &'a SpawnSpec,
    /// The bare-editor control the view side is read against.
    pub nvim: &'a SpawnSpec,
    pub plan: TrialPlan,
    /// How long a side's startup has to go quiet before it is refused.
    pub settle_deadline: Duration,
    /// The wall-clock span of steady flood each side is measured over.
    pub window: Duration,
}

/// Runs the paired floods `run_spec` describes. A flood is one long macro
/// operation, so pairing is per trial (view and nvim floods back to back
/// within the same invocation, order alternating per trial) rather than
/// per-sample interleaving, which has no meaning inside a single window.
///
/// # Errors
///
/// Returns [`BenchError::Desync`] if a producer never scrolls the terminal
/// or a session misbehaves, and propagates every refusal [`aggregate`]
/// raises.
pub fn run(run_spec: &RunSpec<'_>) -> Result<FloodOutcome, BenchError> {
    aggregate(run_spec.plan, |trial| {
        flood_pair(run_spec, trial, |spec| flood_once(spec, run_spec))
    })
}

/// Which side floods first in `trial`. Alternated, so a systematic
/// difference between the first and second window of an invocation (a
/// warmer page cache, a busier host by the time the second one starts)
/// falls on both sides equally instead of on whichever is always second.
fn view_goes_first(trial: usize) -> bool {
    trial.is_multiple_of(2)
}

/// Measures one trial's two sides in this trial's order, returning them in
/// view-then-nvim order whichever way round they ran.
///
/// Takes the per-side measurement as a function so both the order and the
/// attribution are observable without a pty: the two are independent, and
/// swapping the returned pair on the trials that run nvim first would carry
/// nvim's flood as view's through every quotient the run reports, with
/// nothing in the numbers to show it.
///
/// # Errors
///
/// Propagates whatever `measure` refuses, labelled with the side that
/// raised it.
fn flood_pair<F>(
    run_spec: &RunSpec<'_>,
    trial: usize,
    mut measure: F,
) -> Result<TrialPair, BenchError>
where
    F: FnMut(&SpawnSpec) -> Result<FloodSide, BenchError>,
{
    if view_goes_first(trial) {
        let view = measure(run_spec.view).map_err(|e| label("view", e))?;
        let nvim = measure(run_spec.nvim).map_err(|e| label("nvim", e))?;
        Ok(TrialPair { view, nvim })
    } else {
        let nvim = measure(run_spec.nvim).map_err(|e| label("nvim", e))?;
        let view = measure(run_spec.view).map_err(|e| label("view", e))?;
        Ok(TrialPair { view, nvim })
    }
}

/// Reduces the trials `plan` asks for to the run's outcome, refusing the
/// whole run at the first trial any guard refuses.
///
/// Takes a function producing one trial's pair rather than the pairs
/// themselves, for two reasons. It makes this path exercisable without a
/// pty, and the reduction is where a refused trial could be dropped and the
/// survivors aggregated anyway -- which would build a clean-looking median
/// out of a contaminated run, the exact outcome the per-trial guards exist
/// to prevent. And a trial is only produced once the previous one has been
/// accepted, so a refusal costs nothing beyond the trial that raised it
/// rather than the remaining windows of a run whose result is already gone.
///
/// # Errors
///
/// Propagates whatever `pair` refuses and every per-trial refusal
/// [`paired_trial`] raises, and returns [`BenchError::NoTrials`] if the
/// plan asks for no trials.
fn aggregate<F>(plan: TrialPlan, mut pair: F) -> Result<FloodOutcome, BenchError>
where
    F: FnMut(usize) -> Result<TrialPair, BenchError>,
{
    let mut paired = Vec::with_capacity(plan.trials);
    for trial in 0..plan.trials {
        paired.push(paired_trial(pair(trial)?, plan.min_gap_samples)?);
    }

    let across = |pick: fn(&FloodTrial) -> f64| {
        median_of_trials(&paired.iter().map(pick).collect::<Vec<_>>())
    };
    Ok(FloodOutcome {
        gated_pace_ratio: across(|t| t.pace_ratio)?,
        gated_cadence_p99_ms: across(|t| t.view_cadence.p99_ms)?,
        nvim_cadence_p99_ms: across(|t| t.nvim_cadence.p99_ms)?,
        gated_cadence_p99_ratio: across(|t| t.cadence_p99_ratio)?,
        view_stall_max_ms: paired
            .iter()
            .fold(0.0_f64, |worst, t| worst.max(t.view_cadence.max_ms)),
        trials: paired,
    })
}

/// Reduces one trial's two measurements to the statistics the row records.
///
/// Every refusal here is per trial and per side rather than run-level: the
/// quotients the run aggregates are formed at this granularity, so a single
/// contaminated trial can carry the median ratio while a run-level check on
/// the median of p99s still reads clean.
///
/// # Errors
///
/// Returns [`BenchError::DegenerateBaselineSide`] if the view side drained
/// no lines to pace against, or if the nvim side's percentile is not a
/// positive finite number to divide by; and propagates the per-side
/// refusals [`side_cadence`] raises.
///
/// Those per-side refusals come first, so a side that never repainted is
/// reported as sitting under the instrument's own floor -- naming both the
/// percentile and the probe period it failed against -- rather than as a
/// bare unusable denominator. The denominator check still runs, because a
/// probe period of zero is the one arrangement that clears the floor with a
/// zero percentile.
fn paired_trial(pair: TrialPair, min_gap_samples: usize) -> Result<FloodTrial, BenchError> {
    let TrialPair { view, nvim } = pair;
    if !(view.lines_drained.is_finite() && view.lines_drained > 0.0) {
        return Err(BenchError::DegenerateBaselineSide {
            statistic: "lines_drained",
            value: view.lines_drained,
        });
    }
    let view_cadence = side_cadence(&view, VIEW_NAMES, min_gap_samples)?;
    let nvim_cadence = side_cadence(&nvim, NVIM_NAMES, min_gap_samples)?;
    if !(nvim_cadence.p99_ms.is_finite() && nvim_cadence.p99_ms > 0.0) {
        return Err(BenchError::DegenerateBaselineSide {
            statistic: NVIM_NAMES.cadence_metric,
            value: nvim_cadence.p99_ms,
        });
    }
    Ok(FloodTrial {
        pace_ratio: nvim.lines_drained / view.lines_drained,
        cadence_p99_ratio: view_cadence.p99_ms / nvim_cadence.p99_ms,
        view,
        nvim,
        view_cadence,
        nvim_cadence,
    })
}

/// One side's cadence distribution for one trial, refused rather than
/// returned when the window cannot support one.
///
/// # Errors
///
/// Returns [`BenchError::TooFewCadenceGaps`] if this side observed fewer
/// gaps than `min_gap_samples`, or none at all -- a distribution needs one
/// gap however low the configured floor is, and a floor of zero would
/// otherwise let an empty side reach a sampling refusal phrased in terms of
/// a warmup this scenario does not have. Both sides are checked, because a
/// ratio is only as trustworthy as its denominator, and a percentile built
/// on a handful of nvim gaps would let the paired statistic look precise
/// while resting on nothing. Returns
/// [`BenchError::BelowInstrumentResolution`] if this side's p99 sits within
/// [`CADENCE_RESOLUTION_FACTOR`] of *this side's own* probe period: a frame
/// change is only ever seen on a probe, so a cadence number down at that
/// floor reports how fast this harness can hash a screen, not how well the
/// side coalesces a flood. Refused rather than recorded, because a bar set
/// from the instrument gates every later run against the instrument.
fn side_cadence(
    side: &FloodSide,
    names: SideNames,
    min_gap_samples: usize,
) -> Result<SideCadence, BenchError> {
    let floor = min_gap_samples.max(1);
    if side.cadence_gaps_ms.len() < floor {
        return Err(BenchError::TooFewCadenceGaps {
            side: names.side,
            collected: side.cadence_gaps_ms.len(),
            floor,
        });
    }
    let dist = Distribution::from_samples(&side.cadence_gaps_ms, 0)?;
    let p99_ms = dist.p99();
    if !cadence_is_measurable(CadenceResolution {
        p99_ms,
        probe_period_ms: side.probe_period_ms,
    }) {
        return Err(BenchError::BelowInstrumentResolution {
            metric: names.cadence_metric,
            value: p99_ms,
            resolution: side.probe_period_ms,
            factor: CADENCE_RESOLUTION_FACTOR,
        });
    }
    Ok(SideCadence {
        p50_ms: dist.p50(),
        p90_ms: dist.percentile(90.0),
        p99_ms,
        max_ms: dist.max(),
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

    fn side(gaps_ms: &[f64]) -> FloodSide {
        side_probing_every(gaps_ms, 0.05)
    }

    /// A side whose probe loop ran at `probe_period_ms`, for the guards that
    /// read one side's resolution floor rather than the gap values.
    fn side_probing_every(gaps_ms: &[f64], probe_period_ms: f64) -> FloodSide {
        FloodSide {
            lines_drained: 1000.0,
            cadence_gaps_ms: gaps_ms.to_vec(),
            probe_period_ms,
        }
    }

    /// A gap series repeated enough times to clear any sample-count floor a
    /// test sets, without the values themselves varying.
    fn repeated(gaps_ms: &[f64], times: usize) -> Vec<f64> {
        gaps_ms
            .iter()
            .copied()
            .cycle()
            .take(gaps_ms.len() * times)
            .collect()
    }

    #[test]
    fn the_cadence_ratio_is_the_two_sides_percentiles_divided() {
        let view = side(&repeated(&[2.0, 4.0, 20.0], 40));
        let nvim = side(&repeated(&[1.0, 2.0, 10.0], 40));
        let trial = paired_trial(TrialPair { view, nvim }, 100).unwrap();
        assert!(
            (trial.cadence_p99_ratio - trial.view_cadence.p99_ms / trial.nvim_cadence.p99_ms).abs()
                < 1e-12
        );
        assert!(
            trial.cadence_p99_ratio > 1.9 && trial.cadence_p99_ratio < 2.1,
            "a side with uniformly doubled gaps should read about 2x, got {}",
            trial.cadence_p99_ratio
        );
    }

    // The property the row exists for: a host that delivers a flood in
    // coarser chunks lifts BOTH sides' gaps, and no producer-side pin can
    // equalize that across kernels. The quotient is what survives it.
    #[test]
    fn scaling_both_sides_gaps_leaves_the_cadence_ratio_where_it_was() {
        let view_gaps = repeated(&[2.0, 4.0, 20.0], 40);
        let nvim_gaps = repeated(&[1.0, 2.0, 10.0], 40);
        let coarse = |gaps: &[f64]| gaps.iter().map(|g| g * 7.5).collect::<Vec<_>>();

        let fine = paired_trial(
            TrialPair {
                view: side(&view_gaps),
                nvim: side(&nvim_gaps),
            },
            100,
        )
        .unwrap();
        let coarser = paired_trial(
            TrialPair {
                view: side(&coarse(&view_gaps)),
                nvim: side(&coarse(&nvim_gaps)),
            },
            100,
        )
        .unwrap();

        assert!(
            (coarser.view_cadence.p99_ms - fine.view_cadence.p99_ms * 7.5).abs() < 1e-9,
            "the absolute number must move with the stimulus, or this test proves nothing"
        );
        assert!((coarser.cadence_p99_ratio - fine.cadence_p99_ratio).abs() < 1e-12);
    }

    #[test]
    fn a_thin_nvim_side_refuses_the_pair_rather_than_dividing_by_it() {
        let view = side(&repeated(&[2.0, 4.0, 20.0], 40));
        let nvim = side(&[1.0, 2.0]);
        assert!(matches!(
            paired_trial(TrialPair { view, nvim }, 100),
            Err(BenchError::TooFewCadenceGaps {
                side: "nvim",
                collected: 2,
                floor: 100,
            })
        ));
    }

    #[test]
    fn a_thin_view_side_names_the_view_side_in_the_refusal() {
        // the two sides share one refusal path, so which side tripped it is
        // only in the error's own field: a report naming the wrong side sends
        // the reader to the wrong half of the run
        let thin = side(&[1.0, 2.0]);
        let nvim = side(&repeated(&[1.0, 2.0, 10.0], 40));
        assert!(matches!(
            paired_trial(TrialPair { view: thin, nvim }, 100),
            Err(BenchError::TooFewCadenceGaps { side: "view", .. })
        ));
    }

    #[test]
    fn an_nvim_side_that_never_moved_is_degenerate_not_infinite() {
        // probe period 0 is the one arrangement that reaches the denominator
        // guard: with any real probe period a zero percentile is refused one
        // step earlier, as sitting under the instrument's own floor
        let view = side(&repeated(&[2.0, 4.0, 20.0], 40));
        let nvim = side_probing_every(&repeated(&[0.0], 120), 0.0);
        assert!(matches!(
            paired_trial(TrialPair { view, nvim }, 100),
            Err(BenchError::DegenerateBaselineSide {
                statistic: "nvim cadence_p99_ms",
                ..
            })
        ));
    }

    #[test]
    fn each_side_is_held_to_its_own_probe_period_not_the_other_sides() {
        // the nvim side probed 10x coarser than the view side here. Judged
        // against view's period its p99 clears the floor; against its own it
        // does not, and the refusal must carry the period that tripped it or
        // the diagnostic points at the wrong instrument
        let view = side_probing_every(&repeated(&[2.0, 4.0, 20.0], 40), 0.05);
        let nvim = side_probing_every(&repeated(&[0.4, 0.4, 0.6], 40), 0.5);
        let refused = paired_trial(TrialPair { view, nvim }, 100);
        assert!(
            matches!(
                refused,
                Err(BenchError::BelowInstrumentResolution {
                    metric: "nvim cadence_p99_ms",
                    resolution: 0.5,
                    ..
                })
            ),
            "expected the nvim side refused against its own 0.5ms period, got {refused:?}"
        );
    }

    #[test]
    fn a_floor_bound_side_is_refused_in_the_trial_that_measured_it() {
        assert!(paired_trial(
            TrialPair {
                view: clean_side(),
                nvim: clean_side(),
            },
            100
        )
        .is_ok());
        assert!(matches!(
            paired_trial(
                TrialPair {
                    view: floor_bound_side(),
                    nvim: clean_side(),
                },
                100
            ),
            Err(BenchError::BelowInstrumentResolution {
                metric: "cadence_p99_ms",
                ..
            })
        ));
    }

    fn plan(trials: usize, min_gap_samples: usize) -> TrialPlan {
        TrialPlan {
            trials,
            min_gap_samples,
        }
    }

    /// A spec naming a program that cannot be spawned, so any test whose
    /// subject is supposed to refuse before reaching a session fails loudly
    /// if it reaches one instead of quietly measuring something.
    fn unspawnable(name: &str) -> SpawnSpec {
        SpawnSpec {
            program: std::path::PathBuf::from(format!("/nonexistent/view-bench-{name}")),
            args: Vec::new(),
            env: Vec::new(),
            cwd: None,
        }
    }

    fn run_spec_over<'a>(view: &'a SpawnSpec, nvim: &'a SpawnSpec, plan: TrialPlan) -> RunSpec<'a> {
        RunSpec {
            view,
            nvim,
            plan,
            settle_deadline: Duration::from_millis(1),
            window: Duration::from_millis(1),
        }
    }

    fn clean_side() -> FloodSide {
        side_probing_every(&repeated(&[2.0, 4.0, 20.0], 40), 0.05)
    }

    fn clean_pair() -> TrialPair {
        TrialPair {
            view: clean_side(),
            nvim: clean_side(),
        }
    }

    /// A side whose p99 sits under its own probe period, so the trial
    /// carrying it is refused as instrument-bound.
    fn floor_bound_side() -> FloodSide {
        side_probing_every(&repeated(&[0.4, 0.4, 0.6], 40), 0.5)
    }

    #[test]
    fn one_contaminated_trial_is_refused_where_a_median_would_have_absorbed_it() {
        // the run aggregates medians across trials, so a floor-contaminated
        // trial can sit under a clean median and still be the median RATIO.
        // Aggregating the survivors of a refusal is therefore not an option:
        // the refusal has to take the whole run down with it
        let mut queue = vec![
            clean_pair(),
            TrialPair {
                view: floor_bound_side(),
                nvim: clean_side(),
            },
            clean_pair(),
        ]
        .into_iter();
        let refused = aggregate(plan(3, 100), |_| queue.next().ok_or(BenchError::NoTrials));
        assert!(
            matches!(
                refused,
                Err(BenchError::BelowInstrumentResolution {
                    metric: "cadence_p99_ms",
                    ..
                })
            ),
            "the run must be refused whole, not aggregated from the two clean \
             trials, got {refused:?}"
        );

        // and the same three trials read clean at the run level, which is
        // what makes dropping the refused one so quiet: 0.6ms is the middle
        // trial's p99, so the median p99 is a clean 20.0 and the median
        // probe period a clean 0.05
        let median_p99 = median_of_trials(&[20.0, 0.6, 20.0]).unwrap();
        let median_probe = median_of_trials(&[0.05, 0.5, 0.05]).unwrap();
        assert!(
            cadence_is_measurable(CadenceResolution {
                p99_ms: median_p99,
                probe_period_ms: median_probe,
            }),
            "a run-level check would have passed this run"
        );
    }

    #[test]
    fn every_trial_the_run_was_asked_for_is_measured_and_reduced() {
        let mut measured = 0_usize;
        let outcome = aggregate(plan(3, 100), |_| {
            measured += 1;
            Ok(clean_pair())
        })
        .unwrap();
        assert_eq!(measured, 3, "the run must measure the trials it was given");
        assert_eq!(
            outcome.trials.len(),
            3,
            "and reduce over all of them, not a prefix"
        );
    }

    #[test]
    fn a_refused_trial_ends_the_run_before_the_next_one_is_measured() {
        // a flood trial is two 15-second windows, so producing them eagerly
        // would spend the rest of the run on a result already thrown away
        let mut measured = 0_usize;
        let mut queue = vec![
            clean_pair(),
            TrialPair {
                view: floor_bound_side(),
                nvim: clean_side(),
            },
            clean_pair(),
        ]
        .into_iter();
        let refused = aggregate(plan(3, 100), |_| {
            measured += 1;
            queue.next().ok_or(BenchError::NoTrials)
        });
        assert!(refused.is_err());
        assert_eq!(
            measured, 2,
            "the third trial must never be measured once the second is refused"
        );
    }

    #[test]
    fn the_gap_floor_the_run_was_given_reaches_every_trial() {
        // 120 gaps per side clears a floor of 100 and misses one of 200, so
        // a run that forwarded the wrong floor reads as a clean pass
        let mut queue = vec![clean_pair(), clean_pair()].into_iter();
        assert!(aggregate(plan(2, 100), |_| queue.next().ok_or(BenchError::NoTrials)).is_ok());

        let refused = aggregate(plan(2, 200), |_| Ok(clean_pair()));
        assert!(
            matches!(
                refused,
                Err(BenchError::TooFewCadenceGaps {
                    collected: 120,
                    floor: 200,
                    ..
                })
            ),
            "expected the floor the run was given, got {refused:?}"
        );
    }

    #[test]
    fn a_run_of_no_trials_is_refused_rather_than_reduced() {
        // a median over nothing would be a number no measurement produced,
        // and every gate this row feeds is lower-is-better
        let mut measured = 0_usize;
        let refused = aggregate(plan(0, 100), |_| {
            measured += 1;
            Ok(clean_pair())
        });
        assert!(matches!(refused, Err(BenchError::NoTrials)));
        assert_eq!(measured, 0);
    }

    #[test]
    fn the_side_that_floods_first_alternates_by_trial() {
        let order: Vec<bool> = (0..4).map(view_goes_first).collect();
        assert_eq!(order, vec![true, false, true, false]);
    }

    #[test]
    fn each_trial_returns_each_sides_own_measurement_whichever_ran_first() {
        // the alternation exists so neither side is always second, and it is
        // only sound while the returned pair still says which side is which:
        // a swap on the trials that run nvim first would carry nvim's flood
        // as view's through every quotient, with nothing in the numbers to
        // show it
        let view = unspawnable("view");
        let nvim = unspawnable("nvim");
        let run_spec = run_spec_over(&view, &nvim, plan(2, 100));
        let marked = |spec: &SpawnSpec| FloodSide {
            lines_drained: if spec.program == view.program {
                1.0
            } else {
                2.0
            },
            cadence_gaps_ms: Vec::new(),
            probe_period_ms: 0.0,
        };

        for (trial, expected_order) in [(0, ["view", "nvim"]), (1, ["nvim", "view"])] {
            let mut measured = Vec::new();
            let measured_pair = flood_pair(&run_spec, trial, |spec| {
                measured.push(if spec.program == view.program {
                    "view"
                } else {
                    "nvim"
                });
                Ok(marked(spec))
            })
            .unwrap();
            assert_eq!(
                measured, expected_order,
                "trial {trial} measured the sides in the wrong order"
            );
            assert_eq!(
                (
                    measured_pair.view.lines_drained,
                    measured_pair.nvim.lines_drained,
                ),
                (1.0, 2.0),
                "trial {trial} attributed a side's measurement to the other side"
            );
        }
    }

    #[test]
    fn a_run_of_no_trials_refuses_before_it_spawns_anything() {
        // run's own forwarding of its plan is otherwise only observable
        // through a pty; a zero-trial run reaches the refusal without one,
        // and an unspawnable program makes any spawn a different error
        let view = unspawnable("view");
        let nvim = unspawnable("nvim");
        let refused = run(&run_spec_over(&view, &nvim, plan(0, 100)));
        assert!(
            matches!(refused, Err(BenchError::NoTrials)),
            "a zero-trial run must refuse before any spawn, got {refused:?}"
        );
    }

    #[test]
    fn a_side_that_never_repainted_is_refused_against_the_instrument_not_the_denominator() {
        // a real probe period with an all-zero cadence is the case the two
        // guards disagree on: the denominator guard would report only that
        // the number cannot be divided by, while the resolution guard names
        // the percentile, the period it failed against and the factor, which
        // tells the operator the run was instrument-bound
        let view = side(&repeated(&[2.0, 4.0, 20.0], 40));
        let nvim = side_probing_every(&repeated(&[0.0], 120), 0.05);
        let refused = paired_trial(TrialPair { view, nvim }, 100);
        assert!(
            matches!(
                refused,
                Err(BenchError::BelowInstrumentResolution {
                    metric: "nvim cadence_p99_ms",
                    value: 0.0,
                    resolution: 0.05,
                    ..
                })
            ),
            "expected the resolution refusal ahead of the denominator one, got {refused:?}"
        );
    }

    #[test]
    fn a_side_with_no_gaps_at_all_is_refused_as_a_gap_floor() {
        // a configured floor of zero admits an empty side, which would then
        // be refused by the sampling layer in terms of a warmup this
        // scenario has none of
        let view = side(&[]);
        let nvim = side(&repeated(&[1.0, 2.0, 10.0], 40));
        assert!(matches!(
            paired_trial(TrialPair { view, nvim }, 0),
            Err(BenchError::TooFewCadenceGaps {
                side: "view",
                collected: 0,
                floor: 1,
            })
        ));
    }

    #[test]
    fn the_reported_percentiles_separate_a_jitter_tail_from_a_stall() {
        // the two readings this row can produce sit at the SAME p99: a
        // redraw cadence whose tail is a jitter edge above its bulk, and a
        // fast side that stalls one gap in four. Only the p50 tells them
        // apart, which is why the trial report carries it
        let jitter_gaps = repeated(&[12.0, 12.5, 13.0, 16.5], 40);
        let stall_gaps = repeated(&[0.5, 0.5, 0.5, 16.5], 40);
        let jitter = paired_trial(
            TrialPair {
                view: side(&jitter_gaps),
                nvim: side(&jitter_gaps),
            },
            100,
        )
        .unwrap()
        .view_cadence;
        let stall = paired_trial(
            TrialPair {
                view: side(&stall_gaps),
                nvim: side(&stall_gaps),
            },
            100,
        )
        .unwrap()
        .view_cadence;

        assert!(
            (jitter.p99_ms - stall.p99_ms).abs() < 1e-9,
            "the two shapes must share a p99 or this test proves nothing: {} vs {}",
            jitter.p99_ms,
            stall.p99_ms
        );
        assert!(
            jitter.p99_ms / jitter.p50_ms < 1.5,
            "a cadence tail hugs its bulk, got p99 {} over p50 {}",
            jitter.p99_ms,
            jitter.p50_ms
        );
        assert!(
            stall.p99_ms / stall.p50_ms > 10.0,
            "a stall detaches from its bulk, got p99 {} over p50 {}",
            stall.p99_ms,
            stall.p50_ms
        );
        for cadence in [jitter, stall] {
            assert!(cadence.p50_ms <= cadence.p90_ms && cadence.p90_ms <= cadence.p99_ms);
            assert!(cadence.p99_ms <= cadence.max_ms);
        }
    }

    #[test]
    fn a_cadence_at_the_probe_period_is_not_a_measurement_of_view() {
        // every observed gap is one probe iteration here: the number is the
        // harness's screen-hash rate, and recording it would gate every
        // later run against this machine's hashing speed
        assert!(!cadence_is_measurable(CadenceResolution {
            p99_ms: 0.40,
            probe_period_ms: 0.40,
        }));
        assert!(!cadence_is_measurable(CadenceResolution {
            p99_ms: 0.79,
            probe_period_ms: 0.40,
        }));
    }

    #[test]
    fn a_cadence_clear_of_the_probe_period_is_measurable() {
        assert!(cadence_is_measurable(CadenceResolution {
            p99_ms: 0.80,
            probe_period_ms: 0.40,
        }));
        assert!(cadence_is_measurable(CadenceResolution {
            p99_ms: 16.0,
            probe_period_ms: 0.40,
        }));
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
