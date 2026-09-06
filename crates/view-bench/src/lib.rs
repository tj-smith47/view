//! Performance measurement harness: the sampling/pairing/report core of
//! the scenario-by-fixture latency matrix. Deliberately serde-free:
//! baseline file I/O lives in `view-harness` (the one package sanctioned
//! to parse TOML); this crate only measures and computes.

pub mod boundaries;
pub mod notices;
pub mod pairing;
pub mod remote_ui;
pub mod report;
pub mod sampling;
pub mod scenarios;
pub mod session;

use thiserror::Error;

/// Errors from the measurement core. Every variant is a protocol
/// violation the caller must surface, never silently coerce to a number a
/// gate could pass.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum BenchError {
    #[error(
        "only {collected} samples collected with a {warmup}-sample warmup; \
         no measured samples remain"
    )]
    NotEnoughSamples { collected: usize, warmup: usize },
    #[error("no trials to aggregate")]
    NoTrials,
    #[error("paired sides collected different sample counts (view {view}, nvim {nvim})")]
    SampleCountMismatch { view: usize, nvim: usize },
    #[error(
        "nvim-side {statistic} is {value}, not a positive finite number; \
         ratio would be meaningless"
    )]
    DegenerateBaselineSide { statistic: &'static str, value: f64 },
    #[error(
        "the {side} side observed {collected} frame-change gaps, under this row's \
         {floor}-gap floor; a cadence percentile over that few gaps describes the \
         window's luck, not the editor"
    )]
    TooFewCadenceGaps {
        side: &'static str,
        collected: usize,
        floor: usize,
    },
    #[error("pty session error: {0}")]
    Session(#[from] view_oracle::OracleError),
    #[error("picker corpus setup at {path}: {context}")]
    CorpusSetup { path: String, context: String },
    #[error(
        "measurement desync (a harness fault, not a latency reading){}: {context}",
        parked_note(.context)
    )]
    Desync { context: String },
    #[error(
        "{metric} measured {value:.4} ms, within {factor}x of the harness's own \
         {resolution:.4} ms probe period: the number describes the instrument, not \
         the editor"
    )]
    BelowInstrumentResolution {
        metric: &'static str,
        value: f64,
        resolution: f64,
        factor: f64,
    },
}

/// The prompts an editor parks on: a screen it will not leave until
/// somebody answers it, which is a keypress no measurement has.
///
/// Every one of them reads as a stalled boundary to the wait that hits it
/// -- the marker simply never arrives -- so the desync says only that the
/// screen never changed, and the prompt itself is one line inside a
/// forty-row dump nobody reads to the end. The phrases are nvim's own,
/// each a fragment short enough to survive what the two shapes a prompt
/// arrives in put around it: the box drawing of view's Command Line float,
/// and the trailing legend nvim's own bottom-row pager writes after the
/// phrase. The table is the population: a prompt outside it
/// still reaches the reader in the screen dump the message carries, and
/// joins the note by being written down here.
const BLOCKING_PROMPTS: [&str; 5] = [
    "Enter number of swap file to use",
    "(R)ecover",
    "Press ENTER or type command to continue",
    "-- More --",
    "(y/n)",
];

/// The note a desync message carries when the screen it dumps is parked on
/// one of [`BLOCKING_PROMPTS`], and nothing when it is not.
///
/// Reads the message's own text rather than the live screen, so every
/// desync raised anywhere in this crate gets the note without its own call
/// site: each one already renders the screen into the context it carries.
fn parked_note(context: &str) -> String {
    context
        .lines()
        .map(str::trim)
        .find(|line| BLOCKING_PROMPTS.iter().any(|prompt| line.contains(prompt)))
        .map_or_else(String::new, |line| format!(", parked at a prompt: {line}"))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    /// The screen the `startup` row parked on for thirty seconds, one row
    /// of it: the prompt view's swap recovery reaches when more than one
    /// stale swap file is on offer, inside the float that draws it.
    const PARKED_SCREEN: &str = "\
buffer content \"VIEWBENCHVIMENTERMARKER\" never painted within 30s of spawn; screen:
                  ╭─ Command Line ────────────────────────────╮
                  │ > Enter number of swap file to use (0 to quit):    │
                  ╰───────────────────────────────────────────╯";

    /// The other shape the same prompt arrives in: the bare bottom row an
    /// editor with no float to draw in parks on, which is what the nvim
    /// side of every pair would hold, with the pager legend nvim writes
    /// after the phrase.
    const BARE_PARKED_SCREEN: &str = "\
buffer content \"VIEWBENCHVIMENTERMARKER\" never painted within 30s of spawn; screen:
E325: ATTENTION
Found a swap file by the name \".scratch.txt.swp\"
Swap file \".scratch.txt.swp\" already exists!
-- More -- SPACE/d/j: screen/page/line down, b/u/k: up, q: quit";

    /// The shape `(y/n)` actually names: nvim's `ask_yesno` appends it to
    /// whatever question it is answering (`input.c`, `%s (y/n)?`), which is
    /// what a harness with nothing to type parks on the same way it parks
    /// on the swap prompts above.
    const YESNO_PARKED_SCREEN: &str = "\
buffer content \"VIEWBENCHVIMENTERMARKER\" never painted within 30s of spawn; screen:
WARNING: The file has been changed since reading it!!!
Do you really want to write to it (y/n)?";

    #[test]
    fn a_desync_on_a_prompt_names_the_prompt_before_the_screen_dump() {
        let rendered = BenchError::Desync {
            context: PARKED_SCREEN.to_string(),
        }
        .to_string();
        let note = rendered
            .split_once(": buffer content")
            .map(|(head, _)| head.to_string())
            .unwrap_or_default();
        assert!(
            note.contains("parked at a prompt")
                && note.contains("Enter number of swap file to use"),
            "the prompt belongs ahead of the dump, not inside it: {rendered}"
        );
    }

    #[test]
    fn a_desync_on_a_bare_bottom_row_prompt_names_it_too() {
        let rendered = BenchError::Desync {
            context: BARE_PARKED_SCREEN.to_string(),
        }
        .to_string();
        assert!(
            rendered.contains("parked at a prompt: -- More --"),
            "a prompt an editor draws on the bottom row rather than in a float is the same \
             park, and the legend after the phrase must not hide it: {rendered}"
        );
    }

    #[test]
    fn a_desync_on_a_yesno_prompt_names_it_too() {
        let rendered = BenchError::Desync {
            context: YESNO_PARKED_SCREEN.to_string(),
        }
        .to_string();
        assert!(
            rendered.contains("parked at a prompt: Do you really want to write to it (y/n)?"),
            "the phrase generic enough to match other screens has to be shown proven against \
             the one nvim actually parks on: {rendered}"
        );
    }

    #[test]
    fn a_desync_with_no_prompt_on_screen_reads_exactly_as_it_did() {
        let rendered = BenchError::Desync {
            context: "the engine never painted PIDMARKER; screen:\n(blank)".to_string(),
        }
        .to_string();
        assert_eq!(
            rendered,
            "measurement desync (a harness fault, not a latency reading): the engine never \
             painted PIDMARKER; screen:\n(blank)"
        );
    }
}
