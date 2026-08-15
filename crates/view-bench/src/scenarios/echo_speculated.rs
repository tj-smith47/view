//! The speculated-echo row: keypress to the predicted glyph appearing,
//! measured against bare nvim by the identical protocol the `echo` row
//! uses.
//!
//! # Why this is a row of its own
//!
//! The `echo` row's boundary is the round trip: a keystroke leaves the
//! terminal, reaches nvim, and comes back as a redraw view paints. Nothing
//! view can do shrinks the engine-side segment inside it, which is the
//! majority of the interval. Speculation does not shrink it either -- it
//! answers the keystroke ahead of it, on view's own tick, and lets the
//! round trip land underneath afterwards. That is a different event, so it
//! is measured as one and gated on its own metric names rather than being
//! allowed to move the honest row's number.
//!
//! # What makes the number trustworthy
//!
//! On screen a predicted glyph and the authoritative one are the same
//! character in the same cell, so no vt100 parse can say which one a sample
//! watched appear. The instrumented build can: the painter announces every
//! frame carrying a predicted cell, and a sample is attributed only when
//! such an announced write landed inside its own window and ahead of the
//! redraw answering the keystroke (see
//! [`taps::answered_by_prediction`](crate::scenarios::taps::answered_by_prediction)).
//! The attributed share is reported every run, and a run that attributed
//! nothing refuses its numbers instead of publishing the round trip under a
//! speculated name.
//!
//! Unattributed samples are kept rather than dropped. A sample the
//! prediction did not answer measured the honest round trip, which is the
//! larger of the two quantities, so keeping it can only understate this
//! row's advantage -- while dropping it would let a build that speculates
//! on one keystroke in ten report the same number as one that speculates on
//! all of them.

use std::time::Duration;

use crate::scenarios::echo::{self, EchoOutcome};
use crate::scenarios::taps::{self, TapPipe};
use crate::scenarios::Protocol;
use crate::session::{NvimSpec, ViewSpec};
use crate::BenchError;

/// This row's outcome: the paired echo statistics, plus how many of the
/// measured samples a prediction is what answered.
#[derive(Debug)]
pub struct SpeculatedEchoOutcome {
    pub echo: EchoOutcome,
    /// Measured view samples whose observed glyph was written by a
    /// prediction ahead of the engine's redraw.
    pub attributed: usize,
    /// Measured view samples in total, attributed or not.
    pub samples: usize,
}

impl SpeculatedEchoOutcome {
    /// The share of measured samples a prediction answered, as a fraction.
    /// Zero samples reports zero rather than a division by none, and the
    /// refusal below is what a caller reads for that state.
    #[must_use]
    #[allow(clippy::cast_precision_loss)]
    pub fn attributed_share(&self) -> f64 {
        if self.samples == 0 {
            return 0.0;
        }
        self.attributed as f64 / self.samples as f64
    }

    /// Why this run's numbers cannot be published under this row's names,
    /// when they cannot.
    ///
    /// One state qualifies: nothing at all was attributed. Then every
    /// sample measured the same round trip the `echo` row measures, and
    /// recording it as a speculated paint would put the honest number in
    /// the baseline under a name that claims the round trip was hidden.
    /// Every other share is a real reading of a real build, understated in
    /// proportion to the keystrokes speculation did not answer, and it is
    /// reported rather than refused -- a bar between the two would be a
    /// number chosen before anything measured this row, which is the defect
    /// this project withdrew two budgets over.
    #[must_use]
    pub fn refusal(&self) -> Option<String> {
        (self.attributed == 0).then(|| {
            format!(
                "no prediction answered any of the {} measured keystrokes, so every sample timed \
                 the engine round trip this row exists to measure around",
                self.samples
            )
        })
    }
}

/// Runs the speculated-echo row: the instrumented view build `view_spec`
/// names, paired against the bare nvim `nvim_spec` names, with `pipe`
/// carrying the announcements that attribute each sample.
///
/// `view_spec` must be the `bench-taps` build reached through the tap
/// shim; a plain build announces nothing, which this row reports as an
/// attribution of zero and refuses, rather than silently recording the
/// round trip.
///
/// # Errors
///
/// Returns [`BenchError::Desync`] on a dropped tap record (the
/// announcements a sample was attributed by cannot be trusted with a hole
/// in the stream), and anything the echo protocol itself raises.
pub fn run(
    view_spec: ViewSpec<'_>,
    nvim_spec: NvimSpec<'_>,
    pipe: &TapPipe,
    protocol: &Protocol,
    settle_deadline: Duration,
) -> Result<SpeculatedEchoOutcome, BenchError> {
    let mut windows: Vec<(i64, i64)> = Vec::new();
    let echo = echo::run_observed(
        view_spec,
        nvim_spec,
        protocol,
        settle_deadline,
        &mut |window| {
            windows.push(window);
        },
    )?;
    // attribution runs after the session, over the whole record set,
    // rather than inside the sample loop: a sample's own announcement is
    // still crossing the pipe at the instant its glyph is parsed off the
    // screen, so classifying there would read a live prediction as absent
    // for whichever samples lost that race
    let records = pipe.drain();
    taps::verify_no_drops(&records)?;
    let attributed = windows
        .iter()
        .filter(|(start, seen)| taps::answered_by_prediction(&records, *start, *seen))
        .count();
    Ok(SpeculatedEchoOutcome {
        echo,
        attributed,
        samples: windows.len(),
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;
    use crate::pairing::{paired_summary, NvimSamples, ViewSamples};

    fn outcome(attributed: usize, samples: usize) -> SpeculatedEchoOutcome {
        let trial = paired_summary(
            ViewSamples(&[1.0, 2.0, 3.0]),
            NvimSamples(&[2.0, 4.0, 6.0]),
            0,
        )
        .unwrap();
        SpeculatedEchoOutcome {
            echo: EchoOutcome {
                gated_ratio_p50: trial.ratio_p50,
                gated_ratio_p99: trial.ratio_p99,
                gated_paired_delta_p99_ms: trial.paired_delta_p99_ms,
                gated_view_p99_ms: trial.view.p99(),
                trials: vec![trial],
            },
            attributed,
            samples,
        }
    }

    /// The one state whose numbers are not this row's quantity at all: a
    /// build that answered nothing from a prediction measured the round
    /// trip, and publishing that under a speculated name is the confusion
    /// the whole row exists to prevent.
    #[test]
    fn a_run_that_attributed_nothing_refuses_its_numbers() {
        let reason = outcome(0, 1000)
            .refusal()
            .expect("nothing attributed must refuse");
        assert!(
            reason.contains("1000 measured keystrokes"),
            "the reason must carry what it measured instead, got: {reason}"
        );
    }

    /// Partial attribution is a real reading of a real build, understated
    /// by the keystrokes speculation did not answer -- reported, never
    /// refused against a bar nothing measured.
    #[test]
    fn a_partly_attributed_run_reports_its_share_instead_of_refusing() {
        let partial = outcome(1, 1000);
        assert_eq!(partial.refusal(), None);
        assert!((partial.attributed_share() - 0.001).abs() < 1e-9);

        let full = outcome(1000, 1000);
        assert_eq!(full.refusal(), None);
        assert!((full.attributed_share() - 1.0).abs() < 1e-9);
    }

    /// A run with no measured samples cannot divide, and must not read as
    /// fully attributed either.
    #[test]
    fn an_empty_run_reports_no_share_and_still_refuses() {
        let empty = outcome(0, 0);
        assert!((empty.attributed_share() - 0.0).abs() < 1e-9);
        assert!(empty.refusal().is_some());
    }
}
