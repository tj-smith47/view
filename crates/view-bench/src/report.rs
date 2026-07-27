//! Report-line rendering: one place defines the printed shape of every
//! matrix cell, so a consumer scripting against the output (or a human
//! comparing two runs) sees one stable format regardless of which
//! scenario produced the numbers.

use crate::pairing::PairedSummary;
use crate::scenarios::flood::FloodTrial;

/// The two-line paired-cell report:
///
/// ```text
/// echo/minimal: view p50 0.612ms p99 0.941ms max 1.203ms | nvim p50 0.550ms p99 0.713ms max 0.881ms
///       ratio(p99) 1.318  paired-delta p99 0.291ms  samples 1000 (+100 warmup)
/// ```
///
/// `measured` names the side under test, which is not always `view`: the
/// control row pairs nvim's own remote UI against bare nvim, and a line
/// calling that side `view` would attribute nvim's overhead to view in
/// every report a reader ever compares.
#[must_use]
pub fn paired_cell(
    scenario: &str,
    fixture: &str,
    measured: &str,
    summary: &PairedSummary,
    warmup: usize,
) -> String {
    format!(
        "{scenario}/{fixture}: {measured} p50 {:.3}ms p99 {:.3}ms max {:.3}ms | \
         nvim p50 {:.3}ms p99 {:.3}ms max {:.3}ms\n      \
         ratio(p99) {:.3}  paired-delta p99 {:.3}ms  samples {} (+{warmup} warmup)",
        summary.view.p50(),
        summary.view.p99(),
        summary.view.max(),
        summary.nvim.p50(),
        summary.nvim.p99(),
        summary.nvim.max(),
        summary.ratio_p99,
        summary.paired_delta_p99_ms,
        summary.view.len(),
    )
}

/// The two-line paired flood trial report:
///
/// ```text
/// flood/minimal: view 41233 lines p50 12.40ms p90 14.90ms p99 16.43ms | nvim 41102 lines p50 12.35ms p90 14.85ms p99 16.45ms
///       pace 0.997  cadence(p99) 0.999  gaps 1180/1190  probe 0.42/0.41ms (view/nvim)
/// ```
///
/// Every number is read off the trial the run aggregates, so the log and
/// the gate cannot disagree about what this trial measured.
///
/// The p50 and p90 are here because the p99 alone cannot be read: a
/// coalescing failure detaches the tail from the bulk, a redraw cadence
/// keeps it a jitter edge above a p50 it stays near, and those are opposite
/// findings at the same p99. Both sides' gap counts print for the same
/// reason -- a low count on the view side alone points at view's painting,
/// while a low count on both points at what the harness can observe through
/// the pty on this host -- and each side's probe period prints because it is
/// the resolution floor under that side's own percentiles.
#[must_use]
pub fn flood_trial(scenario: &str, fixture: &str, trial: &FloodTrial) -> String {
    format!(
        "{scenario}/{fixture}: view {:.0} lines p50 {:.2}ms p90 {:.2}ms p99 {:.2}ms | \
         nvim {:.0} lines p50 {:.2}ms p90 {:.2}ms p99 {:.2}ms\n      \
         pace {:.3}  cadence(p99) {:.3}  gaps {}/{}  probe {:.2}/{:.2}ms (view/nvim)",
        trial.view.lines_drained,
        trial.view_cadence.p50_ms,
        trial.view_cadence.p90_ms,
        trial.view_cadence.p99_ms,
        trial.nvim.lines_drained,
        trial.nvim_cadence.p50_ms,
        trial.nvim_cadence.p90_ms,
        trial.nvim_cadence.p99_ms,
        trial.pace_ratio,
        trial.cadence_p99_ratio,
        trial.view.cadence_gaps_ms.len(),
        trial.nvim.cadence_gaps_ms.len(),
        trial.view.probe_period_ms,
        trial.nvim.probe_period_ms,
    )
}

/// The numbers behind one unpaired cell line, with the unit spelled by
/// the caller so microsecond rows don't print as 0.00ms.
#[derive(Debug, Clone, Copy)]
pub struct AbsoluteStats<'a> {
    pub p50: f64,
    pub p99: f64,
    pub max: f64,
    pub unit: &'a str,
    pub samples: usize,
    pub warmup: usize,
}

/// A one-line unpaired cell (absolute-budget rows: taps, memory).
#[must_use]
pub fn absolute_cell(scenario: &str, fixture: &str, metric: &str, stats: AbsoluteStats) -> String {
    let AbsoluteStats {
        p50,
        p99,
        max,
        unit,
        samples,
        warmup,
    } = stats;
    format!(
        "{scenario}/{fixture}: {metric} p50 {p50:.2}{unit} p99 {p99:.2}{unit} max {max:.2}{unit}  \
         samples {samples} (+{warmup} warmup)"
    )
}

/// The per-invocation aggregation trailer naming the gated statistic, so a
/// reader of the raw output can tell a single-trial number from the value
/// the gate actually compares.
#[must_use]
pub fn aggregate_line(statistic: &str, value: f64, trials: usize) -> String {
    format!("      gated {statistic} {value:.3} (median of {trials} trials)")
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;
    use crate::pairing::paired_summary;
    use crate::scenarios::flood::{FloodSide, SideCadence};

    #[test]
    fn paired_cell_renders_the_documented_two_line_shape() {
        let view: Vec<f64> = vec![2.0; 150];
        let nvim: Vec<f64> = vec![1.0; 150];
        let summary = paired_summary(&view, &nvim, 100).unwrap();
        let rendered = paired_cell("echo", "minimal", "view", &summary, 100);
        let lines: Vec<&str> = rendered.lines().collect();
        assert_eq!(
            lines[0],
            "echo/minimal: view p50 2.000ms p99 2.000ms max 2.000ms | \
             nvim p50 1.000ms p99 1.000ms max 1.000ms"
        );
        assert_eq!(
            lines[1],
            "      ratio(p99) 2.000  paired-delta p99 1.000ms  samples 50 (+100 warmup)"
        );
    }

    #[test]
    fn flood_trial_renders_both_sides_full_distributions_and_both_probe_periods() {
        let trial = FloodTrial {
            view: FloodSide {
                lines_drained: 41233.0,
                cadence_gaps_ms: vec![12.0, 13.0, 16.4],
                probe_period_ms: 0.42,
            },
            nvim: FloodSide {
                lines_drained: 41102.0,
                cadence_gaps_ms: vec![12.0, 13.0, 16.5],
                probe_period_ms: 0.41,
            },
            view_cadence: SideCadence {
                p50_ms: 12.40,
                p90_ms: 14.90,
                p99_ms: 16.43,
                max_ms: 18.0,
            },
            nvim_cadence: SideCadence {
                p50_ms: 12.35,
                p90_ms: 14.85,
                p99_ms: 16.45,
                max_ms: 18.1,
            },
            pace_ratio: 0.9968,
            cadence_p99_ratio: 0.9988,
        };
        let rendered = flood_trial("flood", "minimal", &trial);
        let lines: Vec<&str> = rendered.lines().collect();
        assert_eq!(
            lines[0],
            "flood/minimal: view 41233 lines p50 12.40ms p90 14.90ms p99 16.43ms | \
             nvim 41102 lines p50 12.35ms p90 14.85ms p99 16.45ms"
        );
        assert_eq!(
            lines[1],
            "      pace 0.997  cadence(p99) 0.999  gaps 3/3  probe 0.42/0.41ms (view/nvim)"
        );
    }

    #[test]
    fn absolute_cell_spells_the_caller_unit() {
        let line = absolute_cell(
            "input_path",
            "minimal",
            "key-to-rpc",
            AbsoluteStats {
                p50: 40.0,
                p99: 85.5,
                max: 92.1,
                unit: "us",
                samples: 1000,
                warmup: 100,
            },
        );
        assert_eq!(
            line,
            "input_path/minimal: key-to-rpc p50 40.00us p99 85.50us max 92.10us  \
             samples 1000 (+100 warmup)"
        );
    }

    #[test]
    fn aggregate_line_names_the_statistic_and_trial_count() {
        assert_eq!(
            aggregate_line("ratio_p99", 1.213, 3),
            "      gated ratio_p99 1.213 (median of 3 trials)"
        );
    }
}
