//! The internal-boundary bench rows (`input_path`, `output_path`) and the
//! `echo_path` decomposition, driven through the tap channel. That channel is
//! FIFO + raw-`CLOCK_MONOTONIC` based (see [`view_bench::scenarios::taps`]) and
//! exists only on unix, so the whole module is `#[cfg(unix)]` and bench.rs
//! loud-skips these rows off unix through `platform_block`.

use super::*;
use view_bench::scenarios::taps;

/// Sampling for the pty floor control that runs alongside the `echo_path`
/// row: a bare round trip has no editor in it, so a few hundred samples
/// settle its median while keeping the control to seconds.
const PTY_FLOOR_SAMPLES: usize = 500;
const PTY_FLOOR_WARMUP: usize = 50;

/// Ceiling on the measured tap-operation p99 before the taps rows are
/// allowed to run at all: 5% of the input-path row's 100 microsecond
/// budget. Above this, the instrumentation itself would materially
/// distort the number it measures.
const TAP_OVERHEAD_BAR_US: f64 = 5.0;

/// Wraps a view spawn in a `sh` shim that opens the tap FIFO at a fixed
/// descriptor before exec'ing the real binary. The pty spawn path closes
/// every inherited fd above stdio in the child, so the descriptor
/// `VIEW_BENCH_TAP_FD` names must be (re)opened after that point; the
/// shell's own `exec 9>` runs post-exec, exactly late enough.
fn shim_taps_spec(inner: SpawnSpec, tap_path: &Path) -> SpawnSpec {
    let mut args = vec![
        OsString::from("-c"),
        OsString::from("exec 9>\"$VIEW_BENCH_TAP_PATH\"; exec \"$0\" \"$@\""),
        inner.program.into_os_string(),
    ];
    args.extend(inner.args);
    let mut env = inner.env;
    env.push((
        OsString::from("VIEW_BENCH_TAP_PATH"),
        tap_path.as_os_str().to_os_string(),
    ));
    env.push((OsString::from("VIEW_BENCH_TAP_FD"), OsString::from("9")));
    SpawnSpec {
        program: PathBuf::from("sh"),
        args,
        env,
        cwd: inner.cwd,
    }
}

/// Runs one taps row (`input_path` or `output_path`) end to end:
/// characterizes tap overhead first and refuses to measure through taps
/// that would distort the row's own budget.
pub(crate) fn run_taps_row(
    scenario: &str,
    fixture: &str,
    world: &CellWorld,
    bins: &Bins,
    protocol: &Protocol,
) -> Result<CellMetrics> {
    let (pipe, spec, _cwd) = taps_side(fixture, world, bins)?;
    let deadline = settle_deadline(fixture);
    let (outcome, metric_key, unit) = match scenario {
        "input_path" => (
            taps::run_input_path(&spec, &pipe, protocol, deadline)
                .with_context(|| format!("input_path/{fixture} run failed"))?,
            "p99_us",
            "us",
        ),
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
    println!(
        "{}",
        report::aggregate_line(metric_key, outcome.gated_p99, protocol.trials)
    );
    let mut metrics = CellMetrics::new();
    metrics.insert(metric_key.to_string(), outcome.gated_p99);
    Ok(metrics)
}

/// Prepares one instrumented-build side: the tap FIFO, the overhead
/// characterization that guards it, and the shimmed spawn spec.
fn taps_side(
    fixture: &str,
    world: &CellWorld,
    bins: &Bins,
) -> Result<(taps::TapPipe, SpawnSpec, PathBuf)> {
    if !bins.taps_view.exists() {
        bail!(
            "taps view binary {} does not exist; run via `task bench` (which builds it) or pass \
             --taps-view-bin",
            bins.taps_view.display()
        );
    }
    let side = world.side(fixture, "view")?;
    let cwd = side.cwd.clone();
    let tap_path = cwd.join("tap.fifo");
    let pipe = taps::TapPipe::create(&tap_path)?;
    let overhead = taps::characterize_overhead(&tap_path, 100_000)?;
    println!(
        "      tap overhead p50 {:.3}us p99 {:.3}us over {} iterations (bar {TAP_OVERHEAD_BAR_US}us)",
        overhead.p50(),
        overhead.p99(),
        overhead.len()
    );
    if overhead.p99() > TAP_OVERHEAD_BAR_US {
        bail!(
            "measured tap overhead p99 {:.3}us exceeds {TAP_OVERHEAD_BAR_US}us (5% of the \
             input-path budget); the tap design must change before this row can be trusted",
            overhead.p99()
        );
    }
    let spec = shim_taps_spec(view_spec_from(side, &bins.taps_view, &bins.nvim), &tap_path);
    Ok((pipe, spec, cwd))
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
    let (pipe, view_spec, cwd) = taps_side(fixture, world, bins)?;
    let nvim_spec = nvim_spec_from(world.side(fixture, "nvim")?, &bins.nvim);
    let outcome = taps::run_echo_path(
        &view_spec,
        &nvim_spec,
        &pipe,
        protocol,
        settle_deadline(fixture),
    )
    .with_context(|| format!("echo_path/{fixture} run failed"))?;

    for summary in &outcome.trials {
        println!(
            "{}",
            report::paired_cell("echo_path", fixture, summary, protocol.warmup)
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
