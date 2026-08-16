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
pub(super) fn run_cell(
    cell: &CellId,
    bins: &Bins,
    protocol: &Protocol,
    controlled: bool,
) -> Result<RowOutcome> {
    let (scenario, fixture) = (&cell.scenario, &cell.fixture);
    // the only consumer is the unix-only taps arm, so off unix the
    // parameter has no reader and `-D warnings` fails the build there
    #[cfg(not(unix))]
    let _ = controlled;
    // the taps rows dispatch here rather than in measure_cell because they
    // are the only rows that can refuse their own number; every other row
    // either measures or fails, so measure_cell keeps the simpler contract
    let outcome = match scenario.as_str() {
        #[cfg(unix)]
        "input_path" | "output_path" => {
            let world = CellWorld::create(fixture)?;
            taps_rows::run_taps_row(cell, &world, bins, protocol, controlled)?
        }
        // dispatched here for the same reason the two rows above are: it
        // can refuse its own number, which measure_cell has no lane for
        #[cfg(unix)]
        "echo_speculated" => {
            let world = CellWorld::create(fixture)?;
            taps_rows::run_echo_speculated_row(fixture, &world, bins, protocol)?
        }
        _ => RowOutcome::trusted(measure_cell(cell, bins, protocol)?),
    };
    let undeclared = baselines::undeclared_metrics(&outcome.metrics);
    ensure!(
        undeclared.is_empty(),
        "{scenario}/{fixture} recorded {}, which baselines::RECORDED_METRICS does not declare; \
         add the name there and give it a policy row before the row can record it",
        undeclared.join(", ")
    );
    Ok(outcome)
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
        "echo_path" => taps_rows::run_echo_path_row(fixture, &world, bins, protocol),
        #[cfg(unix)]
        "echo_control" => {
            let control_side = world.side(fixture, "control")?;
            let socket = control_side.cwd.join("control-ui.sock");
            let control_spec = nvim_spec_from(control_side, nvim_bin);
            let nvim_spec = nvim_spec_from(world.side(fixture, "nvim")?, nvim_bin);
            let outcome = echo_control::run(
                ViewSpec(&control_spec),
                NvimSpec(&nvim_spec),
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
            let pair = paired_specs_with(&world, fixture, bins.echo_bins()?, bins)?;
            let outcome = echo::run(
                ViewSpec(&pair.view),
                NvimSpec(&pair.nvim),
                protocol,
                settle_deadline(fixture),
                echo::DEFAULT_STARTUP_QUIET,
            )
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
                // the scratch file's pinned position is the LAST argument
                // on both sides (view's spec leads with --nvim-bin, which
                // `first()` would misread as the file to overwrite)
                let file = spec
                    .args
                    .last()
                    .map(PathBuf::from)
                    .context("scroll spec has no file argument")?;
                std::fs::write(&file, scroll::fixture_content())
                    .with_context(|| format!("writing scroll fixture {}", file.display()))?;
            }
            let outcome = scroll::run(
                ViewSpec(&pair.view),
                NvimSpec(&pair.nvim),
                protocol,
                settle_deadline(fixture),
            )
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
                ViewSpec(&pair.view),
                NvimSpec(&pair.nvim),
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
                report::aggregate_line(
                    "shell_visible_cold_ms",
                    outcome.gated_shell_visible_cold_ms,
                    1
                )
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
                "shell_visible_cold_ms".to_string(),
                outcome.gated_shell_visible_cold_ms,
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
            let outcome = memory::run(ViewSpec(&view_spec), protocol)
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

            // The equivalence-matrix resource leg (ledger E2) rides on the
            // diagnostic-only memory/heavy cell (see DIAGNOSTIC_MATRIX):
            // the gated memory/minimal cell above stays exactly as
            // recorded, unchanged in spawn count or timing, and these two
            // extra readings never enter `metrics` -- the CLI already
            // refuses --record/--gate on a diagnostic cell, and staying
            // out of CellMetrics keeps that true independent of that
            // refusal too, with no RECORDED_METRICS/budgets.toml entry
            // owed for names nothing ever writes to a baseline.
            if fixture == "heavy" {
                let nvim_bin = bins.nvim.as_path();
                let nvim_side = world.side(fixture, "nvim")?;
                for (index, name) in memory::workload_files().iter().enumerate() {
                    std::fs::write(
                        nvim_side.cwd.join(name),
                        memory::workload_content(index + 1),
                    )
                    .with_context(|| format!("writing nvim-side workload buffer {name}"))?;
                }
                let nvim_spec = nvim_spec_from(nvim_side, nvim_bin);
                let nvim_outcome = memory::run_nvim(NvimSpec(&nvim_spec), protocol)
                    .with_context(|| format!("memory/{fixture} bare-nvim run failed"))?;
                println!(
                    "{}",
                    report::absolute_cell(
                        scenario,
                        fixture,
                        &format!("{}_nvim", outcome.metric),
                        report::AbsoluteStats {
                            p50: nvim_outcome.distribution.p50(),
                            p99: nvim_outcome.gated_mb,
                            max: nvim_outcome.distribution.max(),
                            unit: "MB",
                            samples: nvim_outcome.distribution.len(),
                            warmup: protocol.warmup,
                        }
                    )
                );

                let tree_side = world.side(fixture, "view-tree")?;
                for (index, name) in memory::workload_files().iter().enumerate() {
                    std::fs::write(
                        tree_side.cwd.join(name),
                        memory::workload_content(index + 1),
                    )
                    .with_context(|| format!("writing view-tree workload buffer {name}"))?;
                }
                let tree_spec = view_spec_from(tree_side, bins.view_bins());
                let tree_outcome = memory::run_view_tree(ViewSpec(&tree_spec), protocol)
                    .with_context(|| format!("memory/{fixture} view-tree run failed"))?;
                println!(
                    "{}",
                    report::absolute_cell(
                        scenario,
                        fixture,
                        &format!("{}_view_tree", outcome.metric),
                        report::AbsoluteStats {
                            p50: tree_outcome.distribution.p50(),
                            p99: tree_outcome.gated_mb,
                            max: tree_outcome.distribution.max(),
                            unit: "MB",
                            samples: tree_outcome.distribution.len(),
                            warmup: protocol.warmup,
                        }
                    )
                );
                println!(
                    "      equivalence matrix: bare nvim {:.2}MB | view (own process) {:.2}MB | \
                     view tree (own process + embedded nvim engine) {:.2}MB -- view's own-process \
                     number excludes its engine child, so it is not comparable to nvim's \
                     whole-process number; the tree number is the honest apples-to-apples \
                     comparison against bare nvim's whole-process number.",
                    nvim_outcome.gated_mb, outcome.gated_mb, tree_outcome.gated_mb
                );
            }
            Ok(metrics)
        }
        #[cfg(unix)]
        "remote_memory" => {
            super::remote_rows::run_remote_memory_row(&world, fixture, bins, scenario, protocol)
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
        "picker" => {
            let roots = picker::ensure_corpora(&corpus_root())
                .context("generating the picker corpora (see the error for the disk bound)")?;
            let side = world.side(fixture, "view")?;
            let view_spec = view_spec_from(side, bins.view_bins());
            let outcome = picker::run(
                ViewSpec(&view_spec),
                &roots,
                protocol,
                settle_deadline(fixture),
            )
            .with_context(|| format!("picker/{fixture} run failed"))?;
            for (phase, trials, warmup) in [
                ("match-paint", &outcome.match_trials, protocol.warmup),
                ("first-page", &outcome.scan_trials, picker::SCAN_WARMUP),
            ] {
                for trial in trials {
                    println!(
                        "{}",
                        report::absolute_cell(
                            scenario,
                            fixture,
                            phase,
                            report::AbsoluteStats {
                                p50: trial.p50(),
                                p99: trial.p99(),
                                max: trial.max(),
                                unit: "ms",
                                samples: trial.len(),
                                warmup,
                            }
                        )
                    );
                }
            }
            println!(
                "      streaming observed: trial {} probe rows {} -> {} with no input in between",
                outcome.streaming.trial, outcome.streaming.first_seen, outcome.streaming.grew_to
            );
            let trials = outcome.match_trials.len();
            let mut metrics = CellMetrics::new();
            for (metric, value) in [
                ("match_paint_p50_ms", outcome.gated_match_paint_p50_ms),
                ("match_paint_p99_ms", outcome.gated_match_paint_p99_ms),
                ("first_page_p50_ms", outcome.gated_first_page_p50_ms),
                ("first_page_p99_ms", outcome.gated_first_page_p99_ms),
            ] {
                println!("{}", report::aggregate_line(metric, value, trials));
                metrics.insert(metric.to_string(), value);
            }
            Ok(metrics)
        }
        "supervision" => {
            let side = world.side(fixture, "view")?;
            let view_spec = view_spec_from(side, bins.view_bins());
            let outcome =
                supervision::run(ViewSpec(&view_spec), protocol, settle_deadline(fixture))
                    .with_context(|| format!("supervision/{fixture} run failed"))?;
            for (phase, trials) in [
                ("wedge-detect", &outcome.detect_trials),
                ("restart-rehydrate", &outcome.rehydrate_trials),
            ] {
                for trial in trials {
                    println!(
                        "{}",
                        report::absolute_cell(
                            scenario,
                            fixture,
                            phase,
                            report::AbsoluteStats {
                                p50: trial.p50(),
                                p99: trial.p99(),
                                max: trial.max(),
                                unit: "ms",
                                samples: trial.len(),
                                // every sample is its own process against a
                                // failure that happens once per process:
                                // there is no state a warmup sample could
                                // warm, and dropping one would cost a
                                // detection ceiling for nothing
                                warmup: 0,
                            }
                        )
                    );
                }
            }
            let trials = outcome.detect_trials.len();
            let mut metrics = CellMetrics::new();
            for (metric, value) in [
                ("wedge_detect_p99_ms", outcome.gated_wedge_detect_p99_ms),
                (
                    "restart_rehydrate_p99_ms",
                    outcome.gated_restart_rehydrate_p99_ms,
                ),
            ] {
                println!("{}", report::aggregate_line(metric, value, trials));
                metrics.insert(metric.to_string(), value);
            }
            Ok(metrics)
        }
        other => bail!(
            "unknown scenario {other:?}; known: {}",
            known_scenarios().join(", ")
        ),
    }
}
