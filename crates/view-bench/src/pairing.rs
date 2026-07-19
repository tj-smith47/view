//! Pairing math for view-vs-nvim runs: per-sample paired deltas and the
//! p99 ratio, the two statistics the design spec gates on instead of
//! absolute tail latencies (absolute numbers on shared runners are noise;
//! the ratio and the paired delta cancel machine-wide slowness).

use crate::sampling::Distribution;
use crate::BenchError;

/// The paired comparison extracted from one interleaved trial.
#[derive(Debug, Clone)]
pub struct PairedSummary {
    pub view: Distribution,
    pub nvim: Distribution,
    /// view p99 divided by nvim p99.
    pub ratio_p99: f64,
    /// p99 of per-sample `view[i] - nvim[i]` deltas, in milliseconds.
    pub paired_delta_p99_ms: f64,
}

/// Builds the paired summary from both sides' raw per-sample milliseconds
/// (warmup still included; `warmup` is excluded here so no caller can pair
/// a warmed distribution with an unwarmed one).
///
/// # Errors
///
/// Returns [`BenchError::NotEnoughSamples`] if either side has no measured
/// samples after warmup exclusion, [`BenchError::SampleCountMismatch`] if
/// the sides' measured counts differ (per-sample deltas would silently
/// misalign), or [`BenchError::DegenerateBaselineSide`] if the nvim p99 is
/// not a positive finite number (the ratio would be Inf/NaN and could slip
/// through a numeric gate comparison).
pub fn paired_summary(
    view_raw: &[f64],
    nvim_raw: &[f64],
    warmup: usize,
) -> Result<PairedSummary, BenchError> {
    if view_raw.len() != nvim_raw.len() {
        return Err(BenchError::SampleCountMismatch {
            view: view_raw.len(),
            nvim: nvim_raw.len(),
        });
    }
    let view = Distribution::from_samples(view_raw, warmup)?;
    let nvim = Distribution::from_samples(nvim_raw, warmup)?;

    let nvim_p99 = nvim.p99();
    if !(nvim_p99.is_finite() && nvim_p99 > 0.0) {
        return Err(BenchError::DegenerateBaselineSide { p99: nvim_p99 });
    }
    let ratio_p99 = view.p99() / nvim_p99;

    // deltas pair sample i with sample i: the interleaved schedule takes
    // the two sides' i-th samples close together in time, so the delta
    // cancels drift the two absolute values share
    let deltas: Vec<f64> = view_raw[warmup..]
        .iter()
        .zip(&nvim_raw[warmup..])
        .map(|(v, n)| v - n)
        .collect();
    let delta_dist = Distribution::from_samples(&deltas, 0)?;

    Ok(PairedSummary {
        view,
        nvim,
        ratio_p99,
        paired_delta_p99_ms: delta_dist.p99(),
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::float_cmp)]
    use super::*;

    fn constant(n: usize, value: f64) -> Vec<f64> {
        vec![value; n]
    }

    #[test]
    fn ratio_and_delta_match_hand_computed_values() {
        let view = constant(200, 2.0);
        let nvim = constant(200, 1.0);
        let summary = paired_summary(&view, &nvim, 100).unwrap();
        assert_eq!(summary.view.len(), 100);
        assert_eq!(summary.nvim.len(), 100);
        assert_eq!(summary.ratio_p99, 2.0);
        assert_eq!(summary.paired_delta_p99_ms, 1.0);
    }

    #[test]
    fn warmup_is_excluded_on_both_sides_and_from_deltas() {
        // a warmup spike on either side must reach neither the ratio nor
        // the delta percentile
        let mut view = vec![100.0];
        view.extend(constant(50, 2.0));
        let mut nvim = vec![0.5];
        nvim.extend(constant(50, 1.0));
        let summary = paired_summary(&view, &nvim, 1).unwrap();
        assert_eq!(summary.ratio_p99, 2.0);
        assert_eq!(summary.paired_delta_p99_ms, 1.0);
    }

    #[test]
    fn mismatched_sample_counts_are_an_error() {
        let err = paired_summary(&constant(10, 1.0), &constant(9, 1.0), 0).unwrap_err();
        assert!(matches!(
            err,
            BenchError::SampleCountMismatch { view: 10, nvim: 9 }
        ));
    }

    #[test]
    fn a_zero_nvim_p99_cannot_produce_an_infinite_ratio() {
        let err = paired_summary(&constant(10, 1.0), &constant(10, 0.0), 0).unwrap_err();
        assert!(matches!(err, BenchError::DegenerateBaselineSide { .. }));
    }

    #[test]
    fn per_sample_deltas_can_be_negative_when_view_wins() {
        let view = constant(20, 0.5);
        let nvim = constant(20, 1.0);
        let summary = paired_summary(&view, &nvim, 0).unwrap();
        assert_eq!(summary.paired_delta_p99_ms, -0.5);
        assert_eq!(summary.ratio_p99, 0.5);
    }
}
