//! The internal-boundary bench rows (`input_path`, `output_path`) and the
//! `echo_path` decomposition, driven through the tap channel. That channel is
//! FIFO + raw-`CLOCK_MONOTONIC` based (see [`view_bench::scenarios::taps`]) and
//! exists only on unix, so the whole module is `#[cfg(unix)]` and bench.rs
//! loud-skips these rows off unix through `platform_block`.

use super::*;
use view_bench::scenarios::{echo_speculated, taps};

/// Sampling for the pty floor control that runs alongside the `echo_path`
/// row: a bare round trip has no editor in it, so a few hundred samples
/// settle its median while keeping the control to seconds.
const PTY_FLOOR_SAMPLES: usize = 500;
const PTY_FLOOR_WARMUP: usize = 50;

/// Ceiling on the measured tap-operation p99 before a taps row may stand
/// behind its number. The gated input-path interval carries two
/// interior taps (loop-wake, rpc-handoff) plus the tail of the one that
/// opens it, so a tap at this bar puts ~15 microseconds of
/// instrumentation inside a boundary whose per-class p99 floors are
/// ~150-300; above it, the measurement would be reporting mostly itself.
///
/// What exceeding the bar costs depends on what the row's number is for.
/// The taps rows' only metrics are tail statistics, which gate only on a
/// controlled class (see [`baselines::gate_headroom`]): there the run
/// fails outright, because an untrusted tap invalidates a number the gate
/// enforces. On a shared class the row refuses its own measurement and
/// the rest of the matrix stands -- failing eleven trusted cells over a
/// number that never gates there would hand one runner's contention spike
/// the whole matrix's verdict. The echo_path decomposition always fails,
/// because everything it reports flows through the taps.
const TAP_OVERHEAD_BAR_US: f64 = 5.0;

/// Share of a row's keystrokes that may answer from view's own chrome
/// before the engine's redraw is parsed, above which the output-path row
/// is no longer measuring the thing it names.
///
/// The row's interval opens at the keystroke's parsed redraw, so a paint
/// that lands before that redraw is outside the boundary by construction
/// and is counted rather than timed. That accounting is honest only while
/// such paints are rare: a view that answered every keystroke from its
/// own chrome would still produce a clean percentile here, measured over
/// the engine round trips the user never waited for.
///
/// One paint per 3300 keystrokes in each of three full dev-linux matrix
/// runs is the measured rate, so this share sits ~33x above what a
/// healthy build produces and ~100x below the per-keystroke regime it
/// exists to catch. A share rather than a count, because `--samples` and
/// `--trials` are operator-set and a count would silently change meaning
/// with them; and floored at one event, so that no run length can be
/// short enough for a single stray paint to fail it.
///
/// The rate above is what this counter measures *now* as well, because the
/// predicted paint speculation produces between a keypress and its
/// authoritative redraw is counted separately
/// ([`taps::PaintSplit`](view_bench::scenarios::taps::PaintSplit)) rather
/// than here. It is a per-keystroke event by design, so counting it here
/// would refuse the row on every accelerated burst -- and would raise the
/// bar so far that the chrome-answered regime this share exists to catch
/// could hide underneath it.
const UNEXPLAINED_PAINT_SHARE: f64 = 0.01;

/// Runs one taps row (`input_path` or `output_path`) end to end, then
/// refuses to report through taps that would distort the row's own
/// budget. `controlled` selects what that refusal does -- fail the run
/// where the number gates, withhold only the number where it never does
/// (see [`TAP_OVERHEAD_BAR_US`]).
pub(crate) fn run_taps_row(
    cell: &CellId,
    world: &CellWorld,
    bins: &Bins,
    protocol: &Protocol,
    controlled: bool,
) -> Result<RowOutcome> {
    let (scenario, fixture) = (cell.scenario.as_str(), cell.fixture.as_str());
    let agent = ai_row(scenario).then(stub_agent_bin).transpose()?;
    let (pipe, spec, cwd) = taps_side(fixture, world, bins.taps_bins()?, agent.as_deref())?;
    let deadline = settle_deadline(fixture);
    let (outcome, metric_key, unit) = match scenario {
        "input_path" => (
            taps::run_input_path(&spec, &pipe, protocol, deadline)
                .with_context(|| format!("input_path/{fixture} run failed"))?,
            "key_to_rpc_p99_us",
            "us",
        ),
        "ai_session_active" => {
            let (outcome, streamed) =
                taps::run_ai_session_active(&spec, &pipe, protocol, deadline, &cwd)
                    .with_context(|| format!("ai_session_active/{fixture} run failed"))?;
            report_live_turn(streamed);
            (outcome, "key_to_rpc_p99_us", "us")
        }
        "ai_streaming" => {
            let (outcome, streamed) =
                taps::run_ai_streaming(&spec, &pipe, protocol, deadline, &cwd)
                    .with_context(|| format!("ai_streaming/{fixture} run failed"))?;
            report_live_turn(streamed);
            (outcome, "p99_ms", "ms")
        }
        _ => (
            taps::run_output_path(&spec, &pipe, protocol, deadline)
                .with_context(|| format!("output_path/{fixture} run failed"))?,
            "p99_ms",
            "ms",
        ),
    };
    for dist in &outcome.trial_distributions {
        println!(
            "{}",
            report::absolute_cell(
                scenario,
                fixture,
                "boundary-delta",
                report::AbsoluteStats {
                    p50: dist.p50(),
                    p99: dist.p99(),
                    max: dist.max(),
                    unit,
                    samples: dist.len(),
                    warmup: protocol.warmup,
                }
            )
        );
    }
    for segment in &outcome.segments {
        println!(
            "      segment {}: p50 {:.1}us p99 {:.1}us over {} samples",
            segment.label, segment.p50_us, segment.p99_us, segment.samples
        );
    }
    let keystrokes = protocol.trials * (protocol.warmup + protocol.samples);
    if outcome.paints.speculated > 0 {
        println!(
            "      predicted paints before the answering redraw: {} across {keystrokes} \
             keystrokes (speculation put the typed glyph on screen on view's own tick); a count \
             and not a share, since one keystroke's prediction can reach the terminal in more \
             than one write. Outside this row's boundary by the same construction, and explained \
             -- no bar",
            outcome.paints.speculated
        );
    }
    if outcome.paints.unexplained > 0 {
        println!(
            "      paints with no redraw behind them: {} of {keystrokes} keystrokes (view \
             answered the keystroke from its own chrome before the engine's redraw was parsed); \
             outside this row's boundary, bar {}",
            outcome.paints.unexplained,
            unexplained_paint_bound(keystrokes)
        );
    }
    if let Some(reason) = unexplained_paint_refusal(outcome.paints.unexplained, keystrokes) {
        if controlled {
            bail!(
                "{reason}; the row's interval opens at the parsed redraw, so these keystrokes \
                 were answered outside the boundary this class gates and the percentile no \
                 longer describes what the user waited for"
            );
        }
        println!(
            "      UNEXPLAINED PAINTS PAST BAR [{scenario}.{fixture}]: {reason}; recorded and \
             printed only -- this row's metric is a tail statistic, which never gates on a \
             shared class"
        );
    }
    report_overhead(&outcome.overhead, outcome.overhead_pace);
    if let Some(reason) = overhead_refusal(&outcome.overhead) {
        if controlled {
            bail!(
                "{reason}; this class gates the row's p99, so the tap design must change before \
                 the row can be trusted"
            );
        }
        println!(
            "      TAPS ROW REFUSED [{scenario}.{fixture}]: {reason}; the row records and \
             compares nothing this run -- its only metric is a tail statistic, which never gates \
             on a shared class"
        );
        return Ok(RowOutcome {
            metrics: CellMetrics::new(),
            refused: Some(reason),
        });
    }
    println!(
        "{}",
        report::aggregate_line(metric_key, outcome.gated_p99, protocol.trials)
    );
    let mut metrics = CellMetrics::new();
    metrics.insert(metric_key.to_string(), outcome.gated_p99);
    Ok(RowOutcome::trusted(metrics))
}

/// Runs the speculated-echo row: view's predicted paint against bare
/// nvim's round trip, each sample attributed through the tap channel.
///
/// Both of this row's metrics carry the `speculated_` prefix rather than
/// the `echo` row's names. They are a different event measured over the
/// same protocol, and a shared name would let one be read against a
/// recorded value taken from the other -- the confusion the whole row
/// exists to keep out of the baseline.
pub(crate) fn run_echo_speculated_row(
    fixture: &str,
    world: &CellWorld,
    bins: &Bins,
    protocol: &Protocol,
) -> Result<RowOutcome> {
    let (pipe, view_spec, _cwd) = taps_side(fixture, world, bins.taps_bins()?, None)?;
    let nvim_spec = nvim_spec_from(world.side(fixture, "nvim")?, &bins.nvim);
    let outcome = echo_speculated::run(
        ViewSpec(&view_spec),
        NvimSpec(&nvim_spec),
        &pipe,
        protocol,
        settle_deadline(fixture),
        echo::DEFAULT_STARTUP_QUIET,
    )
    .with_context(|| format!("echo_speculated/{fixture} run failed"))?;

    for summary in &outcome.echo.trials {
        println!(
            "{}",
            report::paired_cell("echo_speculated", fixture, "view", summary, protocol.warmup)
        );
    }
    println!(
        "      keystrokes a prediction answered: {} of {} measured samples ({:.1}%); the rest \
         timed the engine round trip and can only understate this row",
        outcome.attributed,
        outcome.samples,
        100.0 * outcome.attributed_share()
    );
    if let Some(reason) = outcome.refusal() {
        println!(
            "      SPECULATED ECHO REFUSED [echo_speculated.{fixture}]: {reason}; the row records \
             and compares nothing this run rather than publishing the round trip under a name \
             that claims it was hidden"
        );
        return Ok(RowOutcome {
            metrics: CellMetrics::new(),
            refused: Some(reason),
        });
    }
    let mut metrics = CellMetrics::new();
    for (metric, value) in [
        ("speculated_ratio_p50", outcome.echo.gated_ratio_p50),
        ("speculated_paint_p99_ms", outcome.echo.gated_view_p99_ms),
    ] {
        println!(
            "{}",
            report::aggregate_line(metric, value, outcome.echo.trials.len())
        );
        metrics.insert(metric.to_string(), value);
    }
    Ok(RowOutcome::trusted(metrics))
}

/// Prepares one instrumented-build side: the tap FIFO and the shimmed
/// spawn spec.
///
/// `editor` rather than a build chosen here: the tap channel is compiled
/// into more than one arm, and which of them a row measures is the row's
/// own decision (see [`Bins::echo_path_bins`]). Each accessor checks its
/// own binary and names its own flag, so a missing arm is reported as the
/// arm it is.
/// Whether `scenario` measures its boundary with a live agent session in
/// the editor. The one place that question is answered, because three
/// things downstream turn on it: which binary the session spawns as its
/// agent, whether the fixture copy gets an `[ai]` table, and which run
/// function the row calls.
pub(crate) fn ai_row(scenario: &str) -> bool {
    matches!(scenario, "ai_session_active" | "ai_streaming")
}

/// The stub agent the AI rows measure against.
///
/// A fixture rather than the real pinned adapter: the rows price what an
/// agent session costs *view*, so the agent has to stream on a cadence the
/// row controls, on every host, with no network and no credentials. The
/// real adapter's own conformance is `scripts/acceptance/ai-conformance.sh`.
fn stub_agent_bin() -> Result<PathBuf> {
    let path = workspace_root()
        .join("target")
        .join("release")
        .join("view-ai-stub-agent");
    ensure!(
        path.exists(),
        "the AI rows measure against the stub agent at {}, which is not built; `task bench` \
         builds it, so a hand-run bench binary needs `cargo build --release -p view-ai \
         --features test-support --bin view-ai-stub-agent` first",
        path.display()
    );
    Ok(path)
}

/// Prints what the live turn produced across a row's sampling: the row's
/// own evidence that it measured what its name says, alongside the number
/// it reports.
fn report_live_turn(streamed: u64) {
    println!("      agent chunks streamed across the sampling run: {streamed}");
}

fn taps_side(
    fixture: &str,
    world: &CellWorld,
    editor: EditorBins<'_>,
    agent: Option<&Path>,
) -> Result<(taps::TapPipe, SpawnSpec, PathBuf)> {
    let side = world.side(fixture, "view")?;
    if let Some(agent) = agent {
        side.enable_ai_agent(agent)?;
    }
    let cwd = side.cwd.clone();
    let tap_path = cwd.join("tap.fifo");
    let pipe = taps::TapPipe::create(&tap_path)?;
    let spec = taps::shim_taps_spec(view_spec_from(side, editor), &tap_path);
    Ok((pipe, spec, cwd))
}

/// Prints the row's own tap-overhead characterization.
///
/// The characterization runs inside the live session, not before it: an
/// idle-host number cannot see the contention the tap sites run under, and
/// a bar compared against it is a bar against a different operation.
fn report_overhead(overhead: &view_bench::sampling::Distribution, pace: std::time::Duration) {
    println!(
        "      tap overhead p50 {:.3}us p99 {:.3}us over {} iterations at {}us pace \
         (bar {TAP_OVERHEAD_BAR_US}us)",
        overhead.p50(),
        overhead.p99(),
        overhead.len(),
        pace.as_micros()
    );
}

/// Why the measured tap overhead disqualifies the instrumentation, when
/// it does: a tap p99 over [`TAP_OVERHEAD_BAR_US`] means the measurement
/// would be a material share of instrumentation rather than editor.
///
/// The reason states the finding alone; each caller appends the
/// consequence, because what an untrusted tap costs depends on what the
/// row's number is for (see [`TAP_OVERHEAD_BAR_US`]).
fn overhead_refusal(overhead: &view_bench::sampling::Distribution) -> Option<String> {
    (overhead.p99() > TAP_OVERHEAD_BAR_US).then(|| {
        format!(
            "measured tap overhead p99 {:.3}us exceeds the {TAP_OVERHEAD_BAR_US}us bar",
            overhead.p99()
        )
    })
}

/// The most unexplained paints `keystrokes` samples may carry before the
/// output-path row stops describing its own boundary, per
/// [`UNEXPLAINED_PAINT_SHARE`].
fn unexplained_paint_bound(keystrokes: usize) -> usize {
    #[allow(clippy::cast_sign_loss, clippy::cast_possible_truncation)]
    let share = (UNEXPLAINED_PAINT_SHARE * keystrokes as f64) as usize;
    share.max(1)
}

/// Why a resolved-nothing decomposition disqualifies the `echo_path` row,
/// when it does.
///
/// Zero of N is the only count refused, for the reason the chrome-paint
/// bound is floored at one event: any bar above zero would be a share
/// invented before a campaign has published what a healthy run resolves,
/// and the discipline this tree holds to is that a bar comes from a
/// recording. Zero needs no recording -- a decomposition that resolved no
/// chain at all explains none of its own total, and every stage it prints
/// is a zero standing where a measurement should be.
///
/// The build arm is named in the reason because it is the cause that has
/// actually occurred: the chain closes on the typed glyph reaching the
/// terminal, and a view that predicts puts that glyph there before the
/// engine answers, leaving the authoritative write outside every window.
fn unresolved_chain_refusal(unresolved: usize, samples: usize) -> Option<String> {
    (samples > 0 && unresolved == samples).then(|| {
        format!(
            "the tag chain resolved on 0 of {samples} measured view samples, so every stage \
             below is zero over zero samples and the decomposition explains none of its own \
             total. The suspected cause is a view build that predicts: echo_path decomposes the \
             echo round trip and must run the bench-taps + bench-no-speculate arm \
             (target/taps-nospec, which `task bench` builds, or --taps-nospec-view-bin)"
        )
    })
}

/// Why the counted chrome paints disqualify the output-path row, when
/// they do.
///
/// The reason states the finding alone; each caller appends the
/// consequence, which depends on whether the row's percentile gates on
/// this class (see [`UNEXPLAINED_PAINT_SHARE`]).
fn unexplained_paint_refusal(unexplained: usize, keystrokes: usize) -> Option<String> {
    let bound = unexplained_paint_bound(keystrokes);
    (unexplained > bound).then(|| {
        format!(
            "{unexplained} of {keystrokes} keystrokes were answered by a paint with no redraw \
             behind it, past the {bound} this row admits ({:.0}% of its samples)",
            UNEXPLAINED_PAINT_SHARE * 100.0
        )
    })
}

/// Runs the report-only `echo_path` decomposition: the echo round trip on
/// the instrumented build, paired against bare nvim in the same run, split
/// into the internal stages the tap chain resolves, plus this host's bare
/// pty round trip as the floor both editors pay.
pub(crate) fn run_echo_path_row(
    fixture: &str,
    world: &CellWorld,
    bins: &Bins,
    protocol: &Protocol,
) -> Result<CellMetrics> {
    let (pipe, view_spec, cwd) = taps_side(fixture, world, bins.echo_path_bins()?, None)?;
    let nvim_spec = nvim_spec_from(world.side(fixture, "nvim")?, &bins.nvim);
    let outcome = taps::run_echo_path(
        ViewSpec(&view_spec),
        NvimSpec(&nvim_spec),
        &pipe,
        protocol,
        settle_deadline(fixture),
    )
    .with_context(|| format!("echo_path/{fixture} run failed"))?;

    for summary in &outcome.trials {
        println!(
            "{}",
            report::paired_cell("echo_path", fixture, "view", summary, protocol.warmup)
        );
    }
    println!(
        "{}",
        report::aggregate_line("ratio_p50", outcome.gated_ratio_p50, outcome.trials.len())
    );
    for segment in outcome
        .segments
        .iter()
        .chain([&outcome.view_total, &outcome.nvim_total])
    {
        println!(
            "      segment {}: p50 {:.1}us p99 {:.1}us over {} samples",
            segment.label, segment.p50_us, segment.p99_us, segment.samples
        );
    }
    println!(
        "      stage p50 sum {:.1}us vs measured total p50 {:.1}us; residual {:.1}us ({:.1}% of \
         the total)",
        outcome.stage_p50_sum_us(),
        outcome.view_total.p50_us,
        outcome.residual_p50_us(),
        100.0 * outcome.residual_p50_us() / outcome.view_total.p50_us
    );
    println!(
        "      chain unresolved on {} of {} measured view samples; ambiguous loop wakes: input \
         {}, output {}",
        outcome.unresolved,
        outcome.view_total.samples,
        outcome.ambiguous_input_wakes,
        outcome.ambiguous_output_wakes
    );
    if let Some(reason) = unresolved_chain_refusal(outcome.unresolved, outcome.view_total.samples) {
        bail!("{reason}");
    }
    let per_tag = outcome
        .repeated_round_tags
        .iter()
        .map(|(tag, count)| format!("{} {count}", *tag as char))
        .collect::<Vec<_>>()
        .join(", ");
    println!(
        "      redraw-round multiplicity: {} of {} resolved samples held a repeated chain tag \
         ({per_tag})",
        outcome.multiplicity_flagged,
        outcome.view_total.samples - outcome.unresolved
    );
    if outcome.multiplicity_flagged > 0 {
        println!(
            "      WARNING: a repeated chain tag means the walker may have paired the keystroke \
             with a redraw round it did not cause; the stages between the RPC write and the \
             terminal write are understated by that sample's share and their time reappears in \
             the closing stage. These percentiles are not an attribution until this reads zero."
        );
    }

    report_overhead(&outcome.overhead, outcome.overhead_pace);
    if let Some(reason) = overhead_refusal(&outcome.overhead) {
        bail!(
            "{reason}; the decomposition is measured entirely through the taps, so nothing in \
             this row survives untrusted instrumentation"
        );
    }
    let floor = taps::run_pty_floor(
        &cwd,
        PTY_FLOOR_SAMPLES,
        PTY_FLOOR_WARMUP,
        protocol.sample_timeout,
    )
    .context("pty floor control failed")?;
    println!(
        "      pty floor (raw-mode cat round trip): p50 {:.1}us p99 {:.1}us over {} samples",
        floor.p50(),
        floor.p99(),
        floor.len()
    );
    // report-only: the cell records no metric, so no baseline can be
    // written from it and no gate can read one
    Ok(CellMetrics::new())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;
    use view_bench::sampling::Distribution;
    use view_bench::scenarios::taps::TapRecord;

    #[test]
    fn the_overhead_bar_admits_at_the_bar_and_refuses_above_it() {
        // the bar is inclusive: a tap at exactly the stated ceiling is the
        // design's stated cost, not an excess, so refusing it would make
        // the row unrunnable on a host that meets the design
        let at_bar = Distribution::from_samples(&[TAP_OVERHEAD_BAR_US; 200], 0).unwrap();
        assert_eq!(overhead_refusal(&at_bar), None);

        let over = Distribution::from_samples(&[TAP_OVERHEAD_BAR_US * 3.5; 200], 0).unwrap();
        let reason = overhead_refusal(&over).expect("a tap p99 over the bar must disqualify");
        assert!(
            reason.contains("17.500us") && reason.contains("5us bar"),
            "the reason must carry the measured value and the bar it broke, got: {reason}"
        );
    }

    /// The measured rate on a healthy build -- one chrome paint per full
    /// default run -- must never refuse, or the bar would fail the thing
    /// it was derived from.
    #[test]
    fn the_observed_chrome_paint_rate_is_admitted_at_every_run_length() {
        let default_run = Protocol::default();
        let keystrokes = default_run.trials * (default_run.warmup + default_run.samples);
        assert_eq!(keystrokes, 3300);
        assert_eq!(unexplained_paint_refusal(1, keystrokes), None);
        assert_eq!(unexplained_paint_bound(keystrokes), 33);

        // a run too short for one percent to reach a whole event still
        // admits the single stray paint the rate produces
        assert_eq!(unexplained_paint_bound(30), 1);
        assert_eq!(unexplained_paint_refusal(1, 30), None);
    }

    /// The decomposition's own floor: nothing resolved is refused and
    /// names the arm, one sample resolved is reported as the thin
    /// attribution it is, and an empty run has no verdict to give.
    #[test]
    fn a_decomposition_that_resolved_nothing_refuses_and_names_the_arm() {
        let reason = unresolved_chain_refusal(40, 40)
            .expect("a chain that resolved on no sample at all must refuse");
        assert!(
            reason.contains("0 of 40") && reason.contains("bench-no-speculate"),
            "the reason must carry the count and the build arm it suspects, got: {reason}"
        );

        assert_eq!(
            unresolved_chain_refusal(39, 40),
            None,
            "one resolved sample is a thin attribution, not an unmeasured row: the share is \
             printed and the bar it would take has to come from a recording"
        );
        assert_eq!(
            unresolved_chain_refusal(0, 0),
            None,
            "an empty run resolved nothing because it measured nothing; the run's own sampling \
             refusals are what speak to that"
        );
    }

    /// A view answering keystrokes from its own chrome is what the bar
    /// exists to catch, and the reason must carry the evidence.
    #[test]
    fn painting_every_keystroke_from_chrome_is_refused_with_its_own_numbers() {
        let reason = unexplained_paint_refusal(3300, 3300)
            .expect("a paint for every keystroke must disqualify the row");
        assert!(
            reason.contains("3300 of 3300") && reason.contains("past the 33"),
            "the reason must carry the count, the run length and the bound, got: {reason}"
        );
        assert!(
            unexplained_paint_refusal(34, 3300).is_some(),
            "one past the bound must refuse; the bound is inclusive"
        );
        assert!(
            unexplained_paint_refusal(2, 30).is_some(),
            "a short run refuses at two, the first count its floor does not admit"
        );
    }

    /// The whole seam, composed: tap records in, refusal verdict out. A
    /// speculated keystroke must not consume the budget the chrome-paint
    /// refusal holds, and the refusal must still fire for a paint nothing
    /// explains, at exactly the count it always fired at.
    ///
    /// Driven through the classifier rather than through hand-written
    /// counts because the mistake this guards against lives between the two
    /// pieces: an attribution that credited any pre-redraw write to
    /// speculation would leave both these assertions passing on the same
    /// numbers while the safety net caught nothing.
    #[test]
    fn speculated_paints_leave_the_chrome_paint_refusal_exactly_where_it_was() {
        let default_run = Protocol::default();
        let keystrokes = default_run.trials * (default_run.warmup + default_run.samples);
        let mut split = taps::PaintSplit::default();
        for keystroke in 0..keystrokes {
            // one sample's window: the predicted paint on view's own tick,
            // then the engine's redraw; every keystroke of a burst has this
            // shape and none of them is a chrome paint
            let base = i64::try_from(keystroke).unwrap() * 1_000;
            let sample = [
                TapRecord {
                    tag: b'D',
                    seq: 0,
                    nanos: base + 10,
                },
                TapRecord {
                    tag: b'T',
                    seq: 0,
                    nanos: base + 20,
                },
                TapRecord {
                    tag: b'R',
                    seq: 0,
                    nanos: base + 600,
                },
            ];
            let one = taps::classify_paints_before_redraw(&sample, base, base + 600);
            split.speculated += one.speculated;
            split.unexplained += one.unexplained;
        }
        assert_eq!(split.speculated, keystrokes);
        assert_eq!(
            unexplained_paint_refusal(split.unexplained, keystrokes),
            None,
            "a fully speculated run must not refuse: every one of its pre-redraw paints is \
             explained"
        );

        // the same run with one chrome paint per keystroke on top: the
        // regression the counter exists to catch, unchanged by the split
        let chrome = [
            TapRecord {
                tag: b'D',
                seq: 0,
                nanos: 10,
            },
            TapRecord {
                tag: b'T',
                seq: 0,
                nanos: 20,
            },
            TapRecord {
                tag: b'T',
                seq: 0,
                nanos: 30,
            },
            TapRecord {
                tag: b'R',
                seq: 0,
                nanos: 600,
            },
        ];
        let one = taps::classify_paints_before_redraw(&chrome, 0, 600);
        assert_eq!(one.unexplained, 1, "the second write has nothing behind it");
        assert!(
            unexplained_paint_refusal(one.unexplained * keystrokes, keystrokes).is_some(),
            "a chrome paint per keystroke must still refuse the row, speculation or not"
        );
    }
}
