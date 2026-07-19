//! Report-line rendering: one place defines the printed shape of every
//! matrix cell, so a consumer scripting against the output (or a human
//! comparing two runs) sees one stable format regardless of which
//! scenario produced the numbers.

use crate::pairing::PairedSummary;

/// The two-line paired-cell report:
///
/// ```text
/// echo/minimal: view p50 0.61ms p99 0.94ms max 1.20ms | nvim p50 0.55ms p99 0.71ms max 0.88ms
///       ratio(p99) 1.32  paired-delta p99 0.29ms  samples 1000 (+100 warmup)
/// ```
#[must_use]
pub fn paired_cell(
    scenario: &str,
    fixture: &str,
    summary: &PairedSummary,
    warmup: usize,
) -> String {
    format!(
        "{scenario}/{fixture}: view p50 {:.2}ms p99 {:.2}ms max {:.2}ms | \
         nvim p50 {:.2}ms p99 {:.2}ms max {:.2}ms\n      \
         ratio(p99) {:.2}  paired-delta p99 {:.2}ms  samples {} (+{warmup} warmup)",
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

    #[test]
    fn paired_cell_renders_the_documented_two_line_shape() {
        let view: Vec<f64> = vec![2.0; 150];
        let nvim: Vec<f64> = vec![1.0; 150];
        let summary = paired_summary(&view, &nvim, 100).unwrap();
        let rendered = paired_cell("echo", "minimal", &summary, 100);
        let lines: Vec<&str> = rendered.lines().collect();
        assert_eq!(
            lines[0],
            "echo/minimal: view p50 2.00ms p99 2.00ms max 2.00ms | \
             nvim p50 1.00ms p99 1.00ms max 1.00ms"
        );
        assert_eq!(
            lines[1],
            "      ratio(p99) 2.00  paired-delta p99 1.00ms  samples 50 (+100 warmup)"
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
