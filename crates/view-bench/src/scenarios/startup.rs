//! The startup scenario: cold process spawn until the screen the user's
//! config opens is showing, view paired against bare nvim on the same
//! config. The boundary `first_paint` does not cover.
//!
//! `first_paint`'s marker is the opened file's own text, which a config
//! that opens nothing reaches on nvim's very first frame. The screen a
//! real config produces is a later one: nvim draws it once every
//! `VimEnter` autocommand has run, and view withholds its grid until then
//! so the two editors' first content frames are the same screen
//! (`view_core::model::Model::withholds_grid`). This row is that screen's
//! own boundary -- the one a regression in the hold, in the attach order,
//! or in the takeover's place on the critical path moves.
//!
//! `marker` is therefore content only the config's own `VimEnter` window
//! can supply, never the opened buffer's: a marker either editor could
//! paint before `VimEnter` measures `first_paint`'s boundary a second
//! time.
//!
//! The sampling, the pairing and the cold-spawn discipline are
//! [`first_paint::run`]'s, unchanged: the two rows differ in the screen
//! they wait for and in nothing else, and a second copy of that loop would
//! be two definitions of one measurement.

use crate::pairing::PairedSummary;
use crate::scenarios::{first_paint, Protocol};
use crate::session::{NvimSpec, ViewSpec};
use crate::BenchError;

/// The gated metric this row publishes: view's own p99 cold time, in
/// milliseconds, to the first frame carrying the config's windows.
///
/// The `cold` component is what earns the metric its gate policy: the
/// classification rule reads name components, and a cold-spawn absolute is
/// recorded on a shared class and gated on a controlled one
/// (`view_harness::baselines::gate_headroom`).
pub const FIRST_FRAME_METRIC: &str = "first_frame_cold_ms";

/// One startup run: the paired summary over every interleaved spawn pair,
/// plus the two figures the gate reads.
#[derive(Debug)]
pub struct StartupOutcome {
    pub summary: PairedSummary,
    /// view's p99 cold time to the post-`VimEnter` screen, in milliseconds.
    /// Recorded under [`FIRST_FRAME_METRIC`].
    pub gated_first_frame_ms: f64,
    /// view p50 over nvim p50 at that boundary -- the bar S1.7 states, and
    /// the one a paired row can hold across load regimes.
    pub gated_first_frame_ratio_p50: f64,
    /// view p99 over nvim p99 at the same boundary. Recorded everywhere,
    /// gated only on a load-controlled class, the same treatment
    /// `first_paint`'s tail ratio gets.
    pub gated_first_frame_ratio_p99: f64,
}

/// Runs `protocol.warmup + protocol.samples` cold spawns per side against
/// a config whose `VimEnter` opens the window `marker` names.
///
/// # Errors
///
/// As [`first_paint::run`]: [`BenchError::Desync`] if a spawn never paints
/// `marker`, if a view spawn reaches it without ever showing its startup
/// shell, or any underlying session error.
pub fn run(
    view_spec: ViewSpec<'_>,
    nvim_spec: NvimSpec<'_>,
    protocol: &Protocol,
    marker: &str,
) -> Result<StartupOutcome, BenchError> {
    let outcome = first_paint::run(view_spec, nvim_spec, protocol, marker)?;
    Ok(StartupOutcome {
        gated_first_frame_ms: outcome.gated_marker_cold_ms,
        gated_first_frame_ratio_p50: outcome.gated_marker_ratio_p50,
        gated_first_frame_ratio_p99: outcome.gated_marker_ratio_p99,
        summary: outcome.summary,
    })
}
