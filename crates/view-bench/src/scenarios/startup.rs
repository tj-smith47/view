//! The startup scenario: cold process spawn until the screen the user's
//! config opens is showing, view paired against bare nvim on the same
//! config. The boundary `first_paint` does not cover.
//!
//! `first_paint`'s marker is the opened file's own text, which a config
//! that opens nothing reaches on nvim's very first frame. The screen a
//! real config produces is a later one: nvim draws it once every
//! `VimEnter` autocommand has run, and view attaches only then, so the two
//! editors' first content frames are the same screen
//! (`view_core::model::Model::takes_attach`). This row is that screen's
//! own boundary -- the one a regression in the attach order, or in the
//! takeover's place on the critical path, moves.
//!
//! Beside it sits the one diagnostic that decomposes it: what the embedded
//! engine's own startup costs against the same engine under its own TUI,
//! read off `--startuptime`'s `NVIM STARTED` line on both sides
//! ([`SERVER_DELTA_METRIC`]). The settled screen cannot be faster than the
//! server that draws it, so a regression that lands inside nvim's startup
//! -- a surface externalized too early, a hook that makes a plugin do more
//! work -- shows here as a positive delta before it shows anywhere else.
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

use std::path::Path;

use crate::pairing::PairedSummary;
use crate::sampling::Distribution;
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

/// The felt cell this row publishes: view's median time to the settled
/// screen over bare nvim's, under the same config on the same host in the
/// same interleaved run.
pub const SETTLED_RATIO_METRIC: &str = "settled_ratio_p50";

/// The diagnostic that decomposes [`SETTLED_RATIO_METRIC`]: the embedded
/// engine's own median startup minus the same engine's under its own TUI,
/// in milliseconds. Signed -- an embedding that costs nvim nothing reads at
/// or below zero -- and never a claim of its own.
pub const SERVER_DELTA_METRIC: &str = "server_delta_ms";

/// The `--startuptime` line whose figure is the engine's whole startup.
const STARTED_LINE: &str = "--- NVIM STARTED ---";

/// What a `--startuptime` section header reads up to the process it names.
const SECTION_PREFIX: &str = "--- Startup times for process: ";

/// The process whose section holds the editor's own startup.
///
/// A tty nvim is two processes and writes a section for each: the UI client
/// that owns the terminal, and the embedded server that is the editor. Only
/// the second is the same thing view's own engine writes, and the two are
/// nothing like each other -- the client reaches its own `NVIM STARTED` in a
/// couple of milliseconds while the server is still loading a config.
const EDITOR_PROCESS: &str = "Embedded";

/// Every `NVIM STARTED` time the editor process wrote in one
/// `--startuptime` log, in milliseconds.
///
/// nvim appends a whole timing section per run to the file it is given, so
/// one log per side holds one figure per sample of that side's series, in
/// sample order -- and one figure per *process*, which is what
/// [`EDITOR_PROCESS`] selects between: reading both would give a tty side
/// twice the samples of an embedded one, alternating two figures an order
/// of magnitude apart, and a median over the pair is a number neither
/// process ever took.
///
/// A log with no section header at all is one process throughout, which is
/// what an engine that writes no header wrote.
///
/// Line-stateful over a file two processes append to concurrently, which
/// holds exactly while each process's whole report fits one flush: the
/// engine's `profile.c` gives the stream an 8 KiB fully-buffered `bufsize`
/// "big enough for the entire --startuptime report". A report past that
/// auto-flushes mid-write, and a `Primary` line can then land under an
/// `Embedded` header and be read as the editor's -- a cell whose fixture
/// sources enough scripts to cross 8 KiB (`heavy`, `user`) is where that
/// happens. The structure says so on its own: one editor section is one
/// run and writes exactly one [`STARTED_LINE`], so a section that yields
/// two figures has taken one from the process interleaved into it and a
/// section that yields none has lost its own to a split. Both are refused
/// here, where the attribution is made, rather than inferred downstream
/// from two sides disagreeing about how many samples survived -- an
/// interleave that swaps one figure for another leaves those counts equal
/// and the published median mixing the UI client's own figure, an order of
/// magnitude the smaller of the two, into the editor ones.
///
/// The figure is the first field of the line carrying [`STARTED_LINE`]:
/// `clock` in `--startuptime`'s own `clock  self+sourced self` header,
/// which for this line is the elapsed milliseconds since the process began.
///
/// # Errors
///
/// [`BenchError::Desync`] if an editor section holds anything other than
/// exactly one [`STARTED_LINE`].
pub fn started_times_ms(log: &str) -> Result<Vec<f64>, BenchError> {
    let mut editor_section = true;
    let mut headed = false;
    let mut in_section = 0usize;
    let mut times = Vec::new();
    let close = |editor_section: bool, in_section: usize| {
        (!editor_section || in_section == 1)
            .then_some(())
            .ok_or_else(|| BenchError::Desync {
                context: format!(
                    "a {EDITOR_PROCESS} --startuptime section holds {in_section} \
                     {STARTED_LINE} lines instead of one, so the report crossed the engine's \
                     8 KiB buffer and interleaved with the tty side's UI-client section"
                ),
            })
    };
    for line in log.lines() {
        if let Some(process) = line.trim().strip_prefix(SECTION_PREFIX) {
            if headed {
                close(editor_section, in_section)?;
            }
            headed = true;
            in_section = 0;
            editor_section = process.starts_with(EDITOR_PROCESS);
        } else if editor_section && line.contains(STARTED_LINE) {
            if let Some(clock) = line
                .split_whitespace()
                .next()
                .and_then(|field| field.parse::<f64>().ok())
            {
                in_section += 1;
                times.push(clock);
            }
        }
    }
    if headed {
        close(editor_section, in_section)?;
    }
    Ok(times)
}

/// The engine-startup delta between the two sides of a run, in
/// milliseconds: view's median `NVIM STARTED` minus nvim's.
///
/// `warmup` drops the same leading samples the paired series drops, and is
/// what keeps the two comparable: the timings a side wrote while its plugin
/// cache was still cold are in this log exactly as they are in that series.
///
/// # Errors
///
/// [`BenchError::Desync`] if either log holds no `NVIM STARTED` line at
/// all, which means the side never wrote one -- an editor that failed to
/// start, or a `--startuptime` argument that never reached it -- if the two
/// sides timed different numbers of startups, or if [`started_times_ms`]
/// refuses a side's log. Otherwise whatever
/// [`Distribution::from_samples`] returns for a series shorter than the
/// warmup it is asked to drop.
pub fn server_delta_ms(view_log: &Path, nvim_log: &Path, warmup: usize) -> Result<f64, BenchError> {
    let mut series = Vec::new();
    for (side, path) in [("view", view_log), ("nvim", nvim_log)] {
        let text = std::fs::read_to_string(path).unwrap_or_default();
        let times = started_times_ms(&text)?;
        if times.is_empty() {
            return Err(BenchError::Desync {
                context: format!(
                    "the {side} side wrote no {STARTED_LINE} line to {}, so its engine's own \
                     startup was never timed",
                    path.display()
                ),
            });
        }
        series.push((side, times));
    }
    // The structural rule above grades a report against its own header, so
    // a sample whose whole report never reached the file leaves nothing to
    // grade -- and view's side writes no header at all, having one process
    // to report. What is left of it is the count the other side still has.
    if series[0].1.len() != series[1].1.len() {
        return Err(BenchError::Desync {
            context: format!(
                "the {} side timed {} engine startups and the {} side timed {}, so at least one \
                 sample's whole report is missing and the two medians would be taken over \
                 different runs",
                series[0].0,
                series[0].1.len(),
                series[1].0,
                series[1].1.len()
            ),
        });
    }
    let mut sides = Vec::new();
    for (_, times) in &series {
        sides.push(Distribution::from_samples(times, warmup)?.p50());
    }
    Ok(sides[0] - sides[1])
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    /// One `--startuptime` section per run, appended, so the figures come
    /// out in sample order -- and only the `NVIM STARTED` line's own clock
    /// is read, never the `self+sourced` columns beside it or any of the
    /// step lines above it.
    #[test]
    fn every_nvim_started_clock_is_read_in_run_order() {
        let log = "\
times in msec\n\
clock   self+sourced   self:  sourced script\n\
005.100  002.000 002.000: sourcing /etc/vimrc\n\
112.345  000.010: --- NVIM STARTED ---\n\
\n\
times in msec\n\
004.900  001.900 001.900: sourcing /etc/vimrc\n\
098.760  000.011: --- NVIM STARTED ---\n";
        assert_eq!(started_times_ms(log).unwrap(), vec![112.345, 98.760]);
    }

    /// A tty side writes a UI-client section beside the editor's, and the
    /// client's own figure is not a startup any editor took: reading it
    /// would double that side's sample count and drag its median toward a
    /// process that loads no config.
    #[test]
    fn a_ui_clients_own_section_is_not_the_editors() {
        let log = "\
--- Startup times for process: Primary (or UI client) ---\n\
\n\
times in msec\n\
003.195  000.002: --- NVIM STARTED ---\n\
\n\
--- Startup times for process: Embedded ---\n\
\n\
times in msec\n\
112.148  000.028: --- NVIM STARTED ---\n";
        assert_eq!(started_times_ms(log).unwrap(), vec![112.148]);
    }

    /// The interleave the 8 KiB flush boundary produces, in the shape that
    /// survives a count check: the first editor section holds the UI
    /// client's own figure as well as its own, and a later one holds none,
    /// so both sides still report the same number of samples while the
    /// median mixes a process that loads no config into one that does.
    #[test]
    fn a_client_figure_read_under_an_editor_header_is_refused() {
        let log = "\
--- Startup times for process: Embedded ---\n\
\n\
times in msec\n\
003.195  000.002: --- NVIM STARTED ---\n\
112.148  000.028: --- NVIM STARTED ---\n\
\n\
--- Startup times for process: Embedded ---\n\
\n\
times in msec\n\
005.100  002.000 002.000: sourcing /etc/vimrc\n";
        assert!(matches!(
            started_times_ms(log),
            Err(BenchError::Desync { .. })
        ));
    }

    /// A log with no timing section at all is the shape a side that never
    /// received the argument writes, and it must fail the run rather than
    /// publish a delta against an empty series.
    #[test]
    fn a_side_that_timed_nothing_fails_the_run() {
        let dir = view_test_support::ScratchDir::new("startup-server-delta").unwrap();
        let view_log = dir.join("view.log");
        let nvim_log = dir.join("nvim.log");
        std::fs::write(&view_log, "090.000  000.010: --- NVIM STARTED ---\n").unwrap();
        std::fs::write(&nvim_log, "times in msec\n").unwrap();
        assert!(matches!(
            server_delta_ms(&view_log, &nvim_log, 0),
            Err(BenchError::Desync { .. })
        ));
    }

    /// The refusal a misattributed figure earns reaches the row that
    /// publishes the delta, rather than stopping at the parser.
    #[test]
    fn an_interleaved_log_fails_the_run_it_was_measured_for() {
        let dir = view_test_support::ScratchDir::new("startup-server-delta-counts").unwrap();
        let view_log = dir.join("view.log");
        let nvim_log = dir.join("nvim.log");
        std::fs::write(&view_log, "110.000  000.010: --- NVIM STARTED ---\n").unwrap();
        std::fs::write(
            &nvim_log,
            "--- Startup times for process: Embedded ---\n\
             003.195  000.002: --- NVIM STARTED ---\n\
             100.000  000.010: --- NVIM STARTED ---\n",
        )
        .unwrap();
        assert!(matches!(
            server_delta_ms(&view_log, &nvim_log, 0),
            Err(BenchError::Desync { .. })
        ));
    }

    /// A spawn whose whole report never reached the file leaves no section
    /// for the structural rule to grade, and view's own side writes no
    /// header to grade against at all, so the only witness left is that
    /// one side timed fewer startups than the other.
    #[test]
    fn a_side_missing_a_whole_report_fails_the_run() {
        let dir = view_test_support::ScratchDir::new("startup-server-delta-lost").unwrap();
        let view_log = dir.join("view.log");
        let nvim_log = dir.join("nvim.log");
        std::fs::write(
            &view_log,
            "110.000  000.010: --- NVIM STARTED ---\n\
             112.000  000.010: --- NVIM STARTED ---\n",
        )
        .unwrap();
        std::fs::write(&nvim_log, "100.000  000.010: --- NVIM STARTED ---\n").unwrap();
        assert!(matches!(
            server_delta_ms(&view_log, &nvim_log, 0),
            Err(BenchError::Desync { .. })
        ));
    }

    /// The delta is signed and view-minus-nvim, so an embedding that costs
    /// the engine nothing reads at or below zero.
    #[test]
    fn the_delta_is_view_minus_nvim() {
        let dir = view_test_support::ScratchDir::new("startup-server-delta-sign").unwrap();
        let view_log = dir.join("view.log");
        let nvim_log = dir.join("nvim.log");
        std::fs::write(&view_log, "110.000  000.010: --- NVIM STARTED ---\n").unwrap();
        std::fs::write(&nvim_log, "100.000  000.010: --- NVIM STARTED ---\n").unwrap();
        let delta = server_delta_ms(&view_log, &nvim_log, 0).unwrap();
        assert!((delta - 10.0).abs() < 1e-9, "{delta}");
    }
}

/// One startup run: the paired summary over every interleaved spawn pair,
/// plus the two figures the gate reads.
#[derive(Debug)]
pub struct StartupOutcome {
    pub summary: PairedSummary,
    /// view's p99 cold time to the post-`VimEnter` screen, in milliseconds.
    /// Recorded under [`FIRST_FRAME_METRIC`].
    pub gated_first_frame_ms: f64,
    /// view p50 over nvim p50 at that boundary -- the bar this row is held
    /// to, and the one a paired row can hold across load regimes.
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
