//! Sample collection discipline for the measurement protocol: warmup
//! exclusion, percentile extraction, interleaved view/nvim scheduling, and
//! median-of-trials aggregation.
//!
//! Aggregation exists because a single run's ratio statistic is not stable
//! enough to gate on: repeated identical-binary-pair runs on a 12-core dev
//! host produced single-run p50 ratios spanning 1.14-1.55 under identical
//! conditions. A gate on one run would flap; the gated statistic is
//! therefore the median across repeated full trials (or, for scenarios
//! whose every sample is an independent process spawn, a pooled percentile
//! over the whole interleaved run).

use crate::BenchError;

/// Which editor a sample (or a schedule block) belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Side {
    View,
    Nvim,
}

impl Side {
    #[must_use]
    pub fn other(self) -> Self {
        match self {
            Self::View => Self::Nvim,
            Self::Nvim => Self::View,
        }
    }
}

/// One contiguous block of samples taken from a single side before the
/// harness switches to the other side.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Block {
    pub side: Side,
    pub count: usize,
}

/// Builds the alternating per-side block schedule for one interleaved
/// trial: `samples_per_side` samples for each side, taken `block_size` at a
/// time, strictly alternating, starting with `start`. Interleaving within
/// one run is what cancels slow drift (thermal, scheduler) that would
/// otherwise land entirely on whichever side ran second.
#[must_use]
pub fn interleave_schedule(samples_per_side: usize, block_size: usize, start: Side) -> Vec<Block> {
    if samples_per_side == 0 || block_size == 0 {
        return Vec::new();
    }
    let mut blocks = Vec::new();
    let mut taken = 0;
    let mut side = start;
    while taken < samples_per_side {
        let count = block_size.min(samples_per_side - taken);
        blocks.push(Block { side, count });
        // the same-position block for the other side, so both sides always
        // hold identical totals no matter where samples_per_side falls
        // relative to the block size
        blocks.push(Block {
            side: side.other(),
            count,
        });
        taken += count;
        side = start;
    }
    blocks
}

/// Sorted measured sample values (unit is the caller's: ms, us, MB) with
/// warmup already excluded;
/// the only object percentiles are ever read from, so no call site can
/// accidentally include warmup samples in a reported number.
#[derive(Debug, Clone)]
pub struct Distribution {
    sorted: Vec<f64>,
}

impl Distribution {
    /// Builds a distribution from raw per-sample values, dropping the
    /// first `warmup` samples.
    ///
    /// # Errors
    ///
    /// Returns [`BenchError::NotEnoughSamples`] if fewer than
    /// `warmup + 1` samples were collected: percentiles over an empty
    /// measured set would silently report 0.0 and pass any gate.
    pub fn from_samples(raw: &[f64], warmup: usize) -> Result<Self, BenchError> {
        if raw.len() <= warmup {
            return Err(BenchError::NotEnoughSamples {
                collected: raw.len(),
                warmup,
            });
        }
        let mut sorted: Vec<f64> = raw[warmup..].to_vec();
        sorted.sort_by(f64::total_cmp);
        Ok(Self { sorted })
    }

    /// Linear-rank percentile over the sorted measured samples:
    /// `index = round(pct/100 * (n-1))`. No interpolation; with >= 1000
    /// samples the rank granularity is finer than the timer noise being
    /// measured.
    #[must_use]
    pub fn percentile(&self, pct: f64) -> f64 {
        let n = self.sorted.len();
        let idx = ((pct / 100.0) * ((n - 1) as f64)).round();
        // the index is clamped from a finite pct in [0,100], so the cast
        // cannot lose meaningful range
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let idx = (idx.max(0.0) as usize).min(n - 1);
        self.sorted[idx]
    }

    #[must_use]
    pub fn p50(&self) -> f64 {
        self.percentile(50.0)
    }

    #[must_use]
    pub fn p99(&self) -> f64 {
        self.percentile(99.0)
    }

    #[must_use]
    pub fn max(&self) -> f64 {
        *self.sorted.last().unwrap_or(&0.0)
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.sorted.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.sorted.is_empty()
    }

    /// The measured samples in sorted order, for pooling several trials
    /// into one distribution.
    #[must_use]
    pub fn samples(&self) -> &[f64] {
        &self.sorted
    }
}

/// Median across per-trial values of one statistic; the aggregation the
/// gate reads for scenarios that repeat full interleaved trials.
///
/// # Errors
///
/// Returns [`BenchError::NoTrials`] on an empty slice rather than
/// defaulting to 0.0, which would silently pass any lower-is-better gate.
pub fn median_of_trials(values: &[f64]) -> Result<f64, BenchError> {
    if values.is_empty() {
        return Err(BenchError::NoTrials);
    }
    let mut sorted: Vec<f64> = values.to_vec();
    sorted.sort_by(f64::total_cmp);
    let n = sorted.len();
    if n % 2 == 1 {
        Ok(sorted[n / 2])
    } else {
        Ok((sorted[n / 2 - 1] + sorted[n / 2]) / 2.0)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::float_cmp)]
    use super::*;

    #[test]
    fn percentiles_match_a_known_uniform_distribution() {
        // 1.0..=1000.0 ms: p50 must land mid-range and p99 near the top,
        // by the documented linear-rank rule idx = round(pct/100 * (n-1))
        let raw: Vec<f64> = (1..=1000).map(f64::from).collect();
        let dist = Distribution::from_samples(&raw, 0).unwrap();
        assert_eq!(dist.len(), 1000);
        assert_eq!(dist.p50(), 501.0); // round(0.50 * 999) = 500 -> value 501
        assert_eq!(dist.p99(), 990.0); // round(0.99 * 999) = 989 -> value 990
        assert_eq!(dist.max(), 1000.0);
        assert_eq!(dist.percentile(0.0), 1.0);
        assert_eq!(dist.percentile(100.0), 1000.0);
    }

    #[test]
    fn warmup_samples_are_excluded_from_every_percentile() {
        // warmup carries huge outliers; if exclusion broke, p99/max would
        // report 5000.0 instead of the measured range's top
        let mut raw = vec![5000.0, 4000.0, 3000.0];
        raw.extend((1..=100).map(f64::from));
        let dist = Distribution::from_samples(&raw, 3).unwrap();
        assert_eq!(dist.len(), 100);
        assert_eq!(dist.max(), 100.0);
        assert_eq!(dist.p99(), 99.0);
    }

    #[test]
    fn a_run_with_no_measured_samples_is_an_error_not_a_zero() {
        let raw = vec![1.0, 2.0];
        let err = Distribution::from_samples(&raw, 2).unwrap_err();
        assert!(matches!(
            err,
            BenchError::NotEnoughSamples {
                collected: 2,
                warmup: 2
            }
        ));
    }

    #[test]
    fn unsorted_input_still_yields_sorted_percentiles() {
        let raw = vec![9.0, 1.0, 5.0, 3.0, 7.0];
        let dist = Distribution::from_samples(&raw, 0).unwrap();
        assert_eq!(dist.p50(), 5.0);
        assert_eq!(dist.max(), 9.0);
    }

    #[test]
    fn interleave_schedule_strictly_alternates_and_balances_sides() {
        let blocks = interleave_schedule(100, 25, Side::View);
        assert_eq!(blocks.first().map(|b| b.side), Some(Side::View));
        for pair in blocks.windows(2) {
            assert_ne!(pair[0].side, pair[1].side, "two adjacent same-side blocks");
        }
        let view_total: usize = blocks
            .iter()
            .filter(|b| b.side == Side::View)
            .map(|b| b.count)
            .sum();
        let nvim_total: usize = blocks
            .iter()
            .filter(|b| b.side == Side::Nvim)
            .map(|b| b.count)
            .sum();
        assert_eq!(view_total, 100);
        assert_eq!(nvim_total, 100);
    }

    #[test]
    fn interleave_schedule_handles_a_ragged_final_block() {
        let blocks = interleave_schedule(10, 4, Side::Nvim);
        let counts: Vec<usize> = blocks.iter().map(|b| b.count).collect();
        assert_eq!(counts, vec![4, 4, 4, 4, 2, 2]);
        assert_eq!(blocks[0].side, Side::Nvim);
        assert_eq!(blocks[1].side, Side::View);
    }

    #[test]
    fn interleave_schedule_is_empty_for_zero_inputs() {
        assert!(interleave_schedule(0, 25, Side::View).is_empty());
        assert!(interleave_schedule(100, 0, Side::View).is_empty());
    }

    #[test]
    fn median_of_trials_takes_the_middle_run_not_the_mean() {
        // one wild outlier trial (the observed 1.55-style run) must not
        // drag the gated statistic the way a mean would
        assert_eq!(median_of_trials(&[1.16, 1.55, 1.14]).unwrap(), 1.16);
        assert_eq!(median_of_trials(&[2.0, 4.0]).unwrap(), 3.0);
        assert_eq!(median_of_trials(&[7.0]).unwrap(), 7.0);
    }

    #[test]
    fn median_of_no_trials_is_an_error() {
        assert!(matches!(
            median_of_trials(&[]).unwrap_err(),
            BenchError::NoTrials
        ));
    }
}
