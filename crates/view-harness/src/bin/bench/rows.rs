//! The matrix-cell driver: one function per bench row, dispatched by
//! scenario name, each returning the metrics its cell records.
//!
//! Split from bench.rs so the file that owns argument parsing, the hermetic
//! fixture world and the gate verdicts does not also own every row's
//! sampling and reporting.

use super::*;

/// Runs one matrix cell and returns the metrics the baseline records for
/// it, refusing any metric name the gate policy has not classified.
///
/// The refusal lands here rather than at record time because a name gets a
/// gate policy from a rule over its components: an undeclared one is not
/// rejected downstream, it is silently gated on every class under whichever
/// arm its spelling happens to reach.
pub(super) fn run_cell(cell: &CellId, bins: &Bins, protocol: &Protocol) -> Result<CellMetrics> {
    let (scenario, fixture) = (&cell.scenario, &cell.fixture);
    let metrics = measure_cell(cell, bins, protocol)?;
    let undeclared = baselines::undeclared_metrics(&metrics);
    ensure!(
        undeclared.is_empty(),
        "{scenario}/{fixture} recorded {}, which baselines::RECORDED_METRICS does not declare; \
         add the name there and give it a policy row before the row can record it",
        undeclared.join(", ")
    );
    Ok(metrics)
}

fn measure_cell(cell: &CellId, bins: &Bins, protocol: &Protocol) -> Result<CellMetrics> {
    let (scenario, fixture) = (cell.scenario.as_str(), cell.fixture.as_str());
    // the only arm that reads this is unix-only, so on Windows the binding
    // has no reader at all and `-D warnings` fails the build there
    #[cfg(unix)]
    let nvim_bin = bins.nvim.as_path();
    let world = CellWorld::create(fixture)?;
    match scenario {
        #[cfg(unix)]
        "input_path" | "output_path" => taps_rows::run_taps_row(cell, &world, bins, protocol),
        #[cfg(unix)]
        "echo_path" => taps_rows::run_echo_path_row(fixture, &world, bins, protocol),
        #[cfg(unix)]
        "echo_control" => {
            let control_side = world.side(fixture, "control")?;
            let socket = control_side.cwd.join("control-ui.sock");
            let control_spec = nvim_spec_from(control_side, nvim_bin);
            let nvim_spec = nvim_spec_from(world.side(fixture, "nvim")?, nvim_bin);
            let outcome = echo_control::run(
                &control_spec,
                &nvim_spec,
                socket,
                protocol,
                settle_deadline(fixture),
            )
            .with_context(|| format!("echo_control/{fixture} run failed"))?;
            for summary in &outcome.trials {
                println!(
                    "{}",
                    report::paired_cell(
                        scenario,
                        fixture,
                        echo_control::MEASURED_SIDE,
                        summary,
                        protocol.warmup
                    )
                );
            }
            // named apart from the echo row's `ratio_p50` on purpose: the
            // two are read side by side to split that row's overhead, and
            // a shared name invites the wrong one into a comparison
            for (metric, value) in [
                ("control_ratio_p50", outcome.gated_ratio_p50),
                ("control_ratio_p99", outcome.gated_ratio_p99),
                ("control_delta_p99_ms", outcome.gated_paired_delta_p99_ms),
                ("control_p99_ms", outcome.gated_view_p99_ms),
            ] {
                println!(
                    "{}",
                    report::aggregate_line(metric, value, outcome.trials.len())
                );
            }
            let mut metrics = CellMetrics::new();
            metrics.insert("control_ratio_p50".to_string(), outcome.gated_ratio_p50);
            metrics.insert("control_ratio_p99".to_string(), outcome.gated_ratio_p99);
            metrics.insert(
                "control_delta_p99_ms".to_string(),
                outcome.gated_paired_delta_p99_ms,
            );
            metrics.insert("control_p99_ms".to_string(), outcome.gated_view_p99_ms);
            Ok(metrics)
        }
        "echo" => {
            let pair = paired_specs(&world, fixture, bins)?;
            let outcome = echo::run(&pair.view, &pair.nvim, protocol, settle_deadline(fixture))
                .with_context(|| format!("echo/{fixture} run failed"))?;
            for summary in &outcome.trials {
                println!(
                    "{}",
                    report::paired_cell(scenario, fixture, "view", summary, protocol.warmup)
                );
            }
            println!(
                "{}",
                report::aggregate_line("ratio_p50", outcome.gated_ratio_p50, outcome.trials.len())
            );
            println!(
                "{}",
                report::aggregate_line("ratio_p99", outcome.gated_ratio_p99, outcome.trials.len())
            );
            println!(
                "{}",
                report::aggregate_line(
                    "paired_delta_p99_ms",
                    outcome.gated_paired_delta_p99_ms,
                    outcome.trials.len()
                )
            );
            println!(
                "{}",
                report::aggregate_line(
                    "view_p99_ms",
                    outcome.gated_view_p99_ms,
                    outcome.trials.len()
                )
            );
            let mut metrics = CellMetrics::new();
            metrics.insert("ratio_p50".to_string(), outcome.gated_ratio_p50);
            metrics.insert("ratio_p99".to_string(), outcome.gated_ratio_p99);
            metrics.insert(
                "paired_delta_p99_ms".to_string(),
                outcome.gated_paired_delta_p99_ms,
            );
            metrics.insert("view_p99_ms".to_string(), outcome.gated_view_p99_ms);
            Ok(metrics)
        }
        "scroll" => {
            let mut pair = paired_specs(&world, fixture, bins)?;
            for spec in [&mut pair.view, &mut pair.nvim] {
                let file = spec
                    .args
                    .first()
                    .map(PathBuf::from)
                    .context("scroll spec has no file argument")?;
                std::fs::write(&file, scroll::fixture_content())
                    .with_context(|| format!("writing scroll fixture {}", file.display()))?;
            }
            let outcome = scroll::run(&pair.view, &pair.nvim, protocol, settle_deadline(fixture))
                .with_context(|| format!("scroll/{fixture} run failed"))?;
            for summary in &outcome.trials {
                println!(
                    "{}",
                    report::paired_cell(scenario, fixture, "view", summary, protocol.warmup)
                );
            }
            println!(
                "{}",
                report::aggregate_line(
                    "staleness_p99_ms",
                    outcome.gated_staleness_p99_ms,
                    outcome.trials.len()
                )
            );
            println!(
                "{}",
                report::aggregate_line("ratio_p50", outcome.gated_ratio_p50, outcome.trials.len())
            );
            println!(
                "{}",
                report::aggregate_line("ratio_p99", outcome.gated_ratio_p99, outcome.trials.len())
            );
            let mut metrics = CellMetrics::new();
            metrics.insert(
                "staleness_p99_ms".to_string(),
                outcome.gated_staleness_p99_ms,
            );
            metrics.insert("ratio_p50".to_string(), outcome.gated_ratio_p50);
            metrics.insert("ratio_p99".to_string(), outcome.gated_ratio_p99);
            Ok(metrics)
        }
        "first_paint" => {
            let pair = paired_specs(&world, fixture, bins)?;
            plant_first_paint_marker(&pair.view)?;
            plant_first_paint_marker(&pair.nvim)?;
            let outcome = first_paint::run(
                &pair.view,
                &pair.nvim,
                protocol,
                view_surface::SHELL_PLACEHOLDER,
                FIRST_PAINT_MARKER,
            )
            .with_context(|| format!("first_paint/{fixture} run failed"))?;
            println!(
                "{}",
                report::paired_cell(scenario, fixture, "view", &outcome.summary, protocol.warmup)
            );
            println!(
                "{}",
                report::absolute_cell(
                    scenario,
                    fixture,
                    "shell-visible",
                    report::AbsoluteStats {
                        p50: outcome.shell.p50(),
                        p99: outcome.shell.p99(),
                        max: outcome.shell.max(),
                        unit: "ms",
                        samples: outcome.shell.len(),
                        warmup: protocol.warmup,
                    }
                )
            );
            println!(
                "{}",
                report::aggregate_line("shell_visible_ms", outcome.gated_shell_visible_ms, 1)
            );
            println!(
                "{}",
                report::aggregate_line("marker_cold_ms", outcome.gated_marker_cold_ms, 1)
            );
            println!(
                "{}",
                report::aggregate_line("marker_ratio_p50", outcome.gated_marker_ratio_p50, 1)
            );
            println!(
                "{}",
                report::aggregate_line("marker_ratio_p99", outcome.gated_marker_ratio_p99, 1)
            );
            verify_fixture_copies_untouched(&world, fixture)?;
            let mut metrics = CellMetrics::new();
            metrics.insert(
                "shell_visible_ms".to_string(),
                outcome.gated_shell_visible_ms,
            );
            metrics.insert("marker_cold_ms".to_string(), outcome.gated_marker_cold_ms);
            metrics.insert(
                "marker_ratio_p50".to_string(),
                outcome.gated_marker_ratio_p50,
            );
            metrics.insert(
                "marker_ratio_p99".to_string(),
                outcome.gated_marker_ratio_p99,
            );
            Ok(metrics)
        }
        "memory" => {
            let side = world.side(fixture, "view")?;
            for (index, name) in memory::workload_files().iter().enumerate() {
                std::fs::write(side.cwd.join(name), memory::workload_content(index + 1))
                    .with_context(|| format!("writing workload buffer {name}"))?;
            }
            let view_spec = view_spec_from(side, bins.view_bins());
            let outcome = memory::run(&view_spec, protocol)
                .with_context(|| format!("memory/{fixture} run failed"))?;
            println!(
                "{}",
                report::absolute_cell(
                    scenario,
                    fixture,
                    outcome.metric,
                    report::AbsoluteStats {
                        p50: outcome.distribution.p50(),
                        p99: outcome.gated_mb,
                        max: outcome.distribution.max(),
                        unit: "MB",
                        samples: outcome.distribution.len(),
                        warmup: protocol.warmup,
                    }
                )
            );
            println!(
                "{}",
                report::aggregate_line(outcome.metric, outcome.gated_mb, 1)
            );
            let mut metrics = CellMetrics::new();
            metrics.insert(outcome.metric.to_string(), outcome.gated_mb);
            Ok(metrics)
        }
        "flood" => {
            let pair = paired_specs(&world, fixture, bins)?;
            let outcome = flood::run(&flood::RunSpec {
                view: &pair.view,
                nvim: &pair.nvim,
                plan: flood::TrialPlan {
                    trials: protocol.trials,
                    min_gap_samples: protocol.samples,
                },
                settle_deadline: settle_deadline(fixture),
                window: FLOOD_WINDOW,
            })
            .with_context(|| format!("flood/{fixture} run failed"))?;
            for trial in &outcome.trials {
                println!("{}", report::flood_trial(scenario, fixture, trial));
            }
            let trials = outcome.trials.len();
            println!(
                "{}",
                report::aggregate_line("pace_ratio", outcome.gated_pace_ratio, trials)
            );
            println!(
                "{}",
                report::aggregate_line("cadence_p99_ms", outcome.gated_cadence_p99_ms, trials)
            );
            println!(
                "{}",
                report::aggregate_line(
                    "cadence_p99_ratio",
                    outcome.gated_cadence_p99_ratio,
                    trials
                )
            );
            println!(
                "      nvim cadence p99 {:.2}ms | view worst no-paint gap {:.2}ms (reported, not \
                 gated)",
                outcome.nvim_cadence_p99_ms, outcome.view_stall_max_ms
            );
            let mut metrics = CellMetrics::new();
            metrics.insert("pace_ratio".to_string(), outcome.gated_pace_ratio);
            metrics.insert("cadence_p99_ms".to_string(), outcome.gated_cadence_p99_ms);
            metrics.insert(
                "cadence_p99_ratio".to_string(),
                outcome.gated_cadence_p99_ratio,
            );
            Ok(metrics)
        }
        other => bail!(
            "unknown scenario {other:?}; known: {}",
            known_scenarios().join(", ")
        ),
    }
}
