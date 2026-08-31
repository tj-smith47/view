//! The replicate-campaign mode: N gated measurements of the same cells on
//! one host, a pre-registered load exclusion, and a proposal file carrying
//! the seats, factors and draws they resolve to.
//!
//! Split from bench.rs for the reason `rows.rs` and `cell_world.rs` are:
//! the file that owns argument parsing and the gate verdicts does not also
//! own a mode's own loop. The arithmetic is not here either -- it is
//! [`view_harness::campaign`], the same code the characterization walk
//! re-checks a published factor with.

use std::time::Duration;

use view_harness::campaign::{self, Campaign, Progress, Replicate, ReplicateDraw, Verdict};
use view_harness::results::today_date_string;

use super::*;

/// Every cell one pass measured, and what each cell that did not produce a
/// trusted number said about it.
pub(super) struct Pass {
    pub(super) measured: Vec<baselines::MeasuredCell>,
    pub(super) refused: Vec<CellId>,
    pub(super) failed: Vec<String>,
}

/// Measures `cells` once, keeping going past a cell that fails so the run
/// is refused at the end with the whole picture rather than at the first
/// thing that broke: one cell failing says nothing about the cells that
/// already measured, and the cells still queued behind it are worth the
/// minutes the matrix has already spent getting here.
pub(super) fn measure_pass(
    cells: &[CellId],
    bins: &Bins,
    protocol: &Protocol,
    controlled: bool,
    under_gha: bool,
) -> Pass {
    let mut pass = Pass {
        measured: Vec::new(),
        refused: Vec::new(),
        failed: Vec::new(),
    };
    for cell in cells {
        let (scenario, fixture) = (&cell.scenario, &cell.fixture);
        match run_cell(cell, bins, protocol, controlled) {
            Ok(outcome) => {
                if let Some(reason) = &outcome.refused {
                    // a refusal is quieter than a failure by design, so on CI
                    // it gets the same checks-page annotation a platform skip
                    // does: visible without opening the run log
                    if under_gha {
                        println!(
                            "::warning::bench cell {scenario}/{fixture} refused its own \
                             measurement: {reason}"
                        );
                    }
                    pass.refused.push(cell.clone());
                }
                pass.measured.push(baselines::MeasuredCell {
                    id: cell.clone(),
                    metrics: outcome.metrics,
                });
            }
            Err(err) => {
                eprintln!("CELL FAILED [{scenario}.{fixture}]: {err:#}");
                pass.failed.push(format!("{scenario}.{fixture}: {err:#}"));
            }
        }
    }
    pass
}

/// Runs one replicate: the whole `--record`/`--gate` measurement path,
/// bracketed by the null-pair calibration at both ends.
///
/// Every way a replicate can fail to produce trustworthy draws -- a noisy
/// bracket, a row that withheld its own number, a cell that failed, even a
/// calibration that could not run -- comes back as a refusal rather than as
/// an error, because the campaign's answer to all of them is the same: say
/// so, and run another replicate in its place.
fn replicate(
    cells: &[CellId],
    bins: &Bins,
    protocol: &Protocol,
    controlled: bool,
    under_gha: bool,
) -> ReplicateDraw {
    let load = host_load();
    let refusal = |reason: String| ReplicateDraw {
        load,
        brackets: None,
        cells: Vec::new(),
        refusal: Some(reason),
    };
    let start = match null_calibration(bins) {
        Ok(ratio) => null_pair_deviation(ratio),
        Err(err) => return refusal(format!("start calibration could not run: {err:#}")),
    };
    if start > NULL_RATIO_FLOOR {
        return refusal(format!(
            "start null-pair deviation {start:.4} exceeds the calibration floor {NULL_RATIO_FLOOR}"
        ));
    }
    let pass = measure_pass(cells, bins, protocol, controlled, under_gha);
    if !pass.failed.is_empty() {
        return refusal(format!("cell(s) failed: {}", pass.failed.join(", ")));
    }
    if !pass.refused.is_empty() {
        let named = pass
            .refused
            .iter()
            .map(|id| format!("{}.{}", id.scenario, id.fixture))
            .collect::<Vec<_>>()
            .join(", ");
        return refusal(format!("row(s) withheld their own measurement: {named}"));
    }
    let end = match null_calibration(bins) {
        Ok(ratio) => null_pair_deviation(ratio),
        Err(err) => return refusal(format!("end calibration could not run: {err:#}")),
    };
    if !run_stayed_quiet(start, end) {
        return refusal(format!(
            "host became noisy while the cells ran: end null-pair deviation {end:.4} exceeds the \
             calibration floor {NULL_RATIO_FLOOR}"
        ));
    }
    ReplicateDraw {
        load,
        brackets: Some((start, end)),
        cells: pass.measured,
        refusal: None,
    }
}

/// A duration as a campaign's own projection states it: hours and minutes
/// once there are hours, minutes and seconds otherwise. Whole units only --
/// a projection built from one replicate is not precise to the second, and
/// printing it as though it were would overstate what the tool knows.
fn spell(elapsed: Duration) -> String {
    let secs = elapsed.as_secs();
    if secs >= 3600 {
        format!("{}h{:02}m", secs / 3600, (secs % 3600) / 60)
    } else {
        format!("{}m{:02}s", secs / 60, secs % 60)
    }
}

/// The campaign's own log line for one replicate as it lands.
fn announce(landed: &Replicate, at: Progress, class: &str, max_load: f64) {
    let load = landed
        .load
        // never "0.00": a load nobody could read and a load of zero are
        // opposite statements about the host
        .map_or_else(|| "unreadable".to_string(), |load| format!("{load:.2}"));
    let brackets = landed.brackets.map_or_else(
        || "brackets not taken".to_string(),
        |(start, end)| format!("brackets {start:.4}/{end:.4}"),
    );
    let verdict = match landed.verdict {
        Verdict::Included => "INCLUDED".to_string(),
        Verdict::LoadExcluded => format!("EXCLUDED (load > {max_load}), replacing"),
        Verdict::Refused => "REFUSED, replacing".to_string(),
    };
    // the run index and the included count answer different questions and
    // only the first advances on a replacement, so both are printed rather
    // than one standing in for the other
    let where_at = format!(
        "replicate {} (included {}/{})",
        at.run, at.included, at.target
    );
    if let Some(reason) = &landed.refusal {
        println!("CAMPAIGN {class}: {where_at}  load {load}  {verdict}: {reason}");
    } else {
        for cell in &landed.cells {
            let readings = cell
                .metrics
                .iter()
                .map(|(metric, value)| format!("{metric} {value:.4}"))
                .collect::<Vec<_>>()
                .join("  ");
            println!(
                "CAMPAIGN {}/{} {class}: {where_at}  load {load}  {brackets}  {readings}  \
                 {verdict}",
                cell.id.scenario, cell.id.fixture
            );
        }
    }
    // once, off the first replicate: the cost of the rest of this campaign
    // is now a measurement of this host rather than a guess, and an
    // operator who asked for a full-matrix campaign learns what it costs
    // before it has spent the afternoon
    if at.run == 1 {
        println!(
            "CAMPAIGN {class}: replicate 1 took {}; {} included ~= {}, the {}-run cap ~= {}",
            spell(landed.elapsed),
            at.target,
            spell(landed.elapsed * u32::try_from(at.target).unwrap_or(u32::MAX)),
            at.cap,
            spell(landed.elapsed * u32::try_from(at.cap).unwrap_or(u32::MAX)),
        );
    }
}

/// The tree the campaign measured, where the tool can read one.
///
/// `--dirty` rather than a bare `rev-parse HEAD`: the binary under
/// measurement is built from the working tree, not from the last commit, so
/// a campaign run over an uncommitted change stamped with its parent sha
/// would be a derived value that is wrong -- and a wrong derived value is
/// worse than a hand-typed one, because it is trusted. Measuring a dirty
/// tree is a legitimate campaign; it only has to say so, and a committed
/// sidecar carrying `<sha>-dirty` states exactly what can and cannot be
/// checked out again.
fn measured_commit() -> Option<String> {
    let out = std::process::Command::new("git")
        .args(["describe", "--always", "--dirty"])
        .current_dir(workspace_root())
        .output()
        .ok()?;
    let sha = String::from_utf8(out.stdout).ok()?.trim().to_string();
    (out.status.success() && !sha.is_empty()).then_some(sha)
}

/// Runs `target` included replicates of `cells` and writes the proposal
/// file, which is the whole of what a campaign produces: it never touches a
/// committed baseline or sidecar.
pub(super) fn run(
    target: usize,
    cli: &Cli,
    cells: &[CellId],
    bins: &Bins,
    protocol: &Protocol,
    controlled: bool,
    pin: &str,
) -> Result<()> {
    ensure!(target > 0, "--campaign needs at least one replicate");
    let class = &cli.class;
    let cap = target.saturating_mul(CAMPAIGN_RUN_CAP);
    // the magnitude the operator just asked for, stated before it is spent:
    // a campaign measures every selected cell in every replicate, so a
    // full-matrix campaign is the cell count times the cap, and nothing but
    // this line tells the operator that before the hours start
    println!(
        "campaign on class {class}: {target} included replicate(s) wanted, at most {cap} run(s) \
         over {} cell(s) = up to {} cell measurement(s), excluding any replicate whose pre-run \
         1-min load exceeds {}",
        cells.len(),
        cap.saturating_mul(cells.len()),
        cli.max_load
    );
    let under_gha = std::env::var("GITHUB_ACTIONS").is_ok_and(|v| v == "true");
    let campaign = Campaign::collect(
        target,
        cap,
        cli.max_load,
        |_| replicate(cells, bins, protocol, controlled, under_gha),
        |landed, at| announce(landed, at, class, cli.max_load),
    )?;

    let proposals = campaign.proposals()?;
    let runs = campaign.replicates.len();
    println!(
        "CAMPAIGN {class}: {} included of {runs} run",
        campaign.included().count()
    );
    for proposal in &proposals {
        let stats = proposal.sized.stats;
        println!(
            "  {}/{} {}: median {:.4}  half-width {:.2}%  worst {:.4}  proposes \"{}\" = {}",
            proposal.cell.scenario,
            proposal.cell.fixture,
            proposal.metric,
            stats.median,
            stats.half_width_fraction() * 100.0,
            stats.high,
            proposal.key,
            proposal.published,
        );
    }

    let provenance = campaign::Provenance {
        class: class.clone(),
        engine_pin: pin.to_string(),
        date: today_date_string(),
        commit: measured_commit(),
        max_load: cli.max_load,
        samples: protocol.samples,
        warmup: protocol.warmup,
        trials: protocol.trials,
    };
    let path = baseline_path(class).with_extension("campaign.toml");
    std::fs::write(&path, campaign::render(&campaign, &provenance, &proposals))
        .with_context(|| format!("writing the campaign proposal to {}", path.display()))?;
    println!(
        "CAMPAIGN wrote {} (seats, factors, draws). Nothing reads it: review it and commit its \
         contents into {} and its headroom sidecar",
        path.display(),
        baseline_path(class).display()
    );
    Ok(())
}
