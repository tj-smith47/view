//! Putting a live agent session into a bench session, and proving it was
//! live for the whole measurement.
//!
//! The AI rows measure the same boundaries the session-absent rows do
//! (`taps::sample_input_path`, `taps::sample_output_path`) with an agent
//! turn streaming underneath them. That claim is only worth recording if
//! the turn really was in flight: a panel that never opened, an agent that
//! never spawned, or a turn that ended after the first frame all leave a
//! row that measures the session-absent path under a name saying
//! otherwise, and reads as an improvement. So the turn is asserted at both
//! ends -- streaming before the first sample, still streaming after the
//! last one -- and the row fails rather than records when either is false.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::session::{BenchSession, SettleBound};
use crate::BenchError;

/// The ex-command that opens view's agent panel, and the answer to the
/// trust prompt that opening it raises for a project root the user has
/// not trusted before (every bench cell runs in a fresh scratch root, so
/// the prompt is always the next thing on screen).
const OPEN_PANEL: &[u8] = b":View ai open\r";
const TRUST_YES: &[u8] = b"y";

/// The prompt text `view-ai-stub-agent` reads as "stream until the client
/// stops reading" (see that fixture's own header), and the word its chunks
/// carry. The word is what proves view *rendered* the stream rather than
/// only that the agent wrote it: the count file below is written by the
/// agent process, and an editor that dropped every update on the floor
/// would still let that count climb.
const SUSTAINED_PROMPT: &[u8] = b"stream-forever\r";
const STREAM_MARKER: &str = "chunk";

/// Where that fixture records how many chunks it has written, inside the
/// working directory view spawned it in.
const PROGRESS_FILE: &str = "view-ai-stub-stream-progress.txt";

/// How long the preamble waits for the panel, the agent and the first
/// rendered chunk. Generous: it covers a cold agent spawn on a loaded
/// host, and it is paid once per cell rather than per sample.
const TURN_DEADLINE: Duration = Duration::from_secs(60);

/// How long the preamble lets the screen go quiet between the two keys
/// that open the panel. Both are answered by a repaint, and neither is
/// racing the agent: the stream has not started yet.
const KEY_QUIET: Duration = Duration::from_millis(500);

/// Chunks the stream must add across a sampling run for the run to count
/// as measured under a live turn. One would prove only that the turn had
/// not ended before sampling started; a sampling run is seconds long and
/// the fixture's cadence is 20ms, so anything that kept streaming
/// throughout clears this by orders of magnitude.
const SUSTAINED_MINIMUM: u64 = 5;

/// A turn confirmed in flight, holding what the stream had produced when
/// sampling was allowed to start.
#[derive(Debug)]
pub struct LiveTurn {
    progress: PathBuf,
    at_start: u64,
}

/// Opens the agent panel, trusts the root, submits the prompt the stub
/// fixture streams on, and returns once view has rendered a chunk of it --
/// leaving the session back in insert mode with the panel open behind it,
/// which is the state the sampling loops expect.
///
/// # Errors
///
/// Returns [`BenchError::Desync`] when no chunk is rendered within
/// [`TURN_DEADLINE`], which is the one failure this preamble has: every
/// step before it (the panel, the trust answer, the agent spawn, the
/// prompt) can only fail by leaving that chunk unrendered.
pub fn start(session: &mut BenchSession, cwd: &Path) -> Result<LiveTurn, BenchError> {
    let progress = cwd.join(PROGRESS_FILE);
    let _ = std::fs::remove_file(&progress);

    // out of the insert mode `prepare` left the session in: the panel is
    // opened by an ex-command
    session.send(b"\x1b")?;
    session.send(OPEN_PANEL)?;
    quiet(session);
    session.send(TRUST_YES)?;
    quiet(session);
    session.send(SUSTAINED_PROMPT)?;

    let deadline = Instant::now() + TURN_DEADLINE;
    loop {
        let written = chunks_written(&progress);
        if written > 0 && session.screen_text().contains(STREAM_MARKER) {
            // the first Esc leaves the panel (returning focus to the
            // engine), the second clears any operator the keys above left
            // pending in nvim, and `i` restores the insert mode the
            // sampling loops type into
            session.send(b"\x1b")?;
            session.send(b"\x1b")?;
            session.send(b"i")?;
            return Ok(LiveTurn {
                progress,
                at_start: written,
            });
        }
        if Instant::now() >= deadline {
            return Err(BenchError::Desync {
                context: format!(
                    "no agent chunk reached the screen within {TURN_DEADLINE:?} of opening the \
                     panel ({written} chunk(s) written by the agent itself), so this row would \
                     have measured a session with no live turn in it; screen:\n{}",
                    session.screen_text()
                ),
            });
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

impl LiveTurn {
    /// Confirms the stream kept running across the sampling it bracketed.
    ///
    /// # Errors
    ///
    /// Returns [`BenchError::Desync`] when fewer than [`SUSTAINED_MINIMUM`]
    /// chunks were added, which is a turn that ended (or an agent that
    /// died) partway through a run whose whole subject is what a live turn
    /// costs.
    pub fn ended_still_streaming(&self) -> Result<u64, BenchError> {
        let added = chunks_written(&self.progress).saturating_sub(self.at_start);
        if added < SUSTAINED_MINIMUM {
            return Err(BenchError::Desync {
                context: format!(
                    "the agent added {added} chunk(s) across the sampling run, under the \
                     {SUSTAINED_MINIMUM} a live turn produces, so the samples were not all taken \
                     under one"
                ),
            });
        }
        Ok(added)
    }
}

/// How many chunks the fixture has recorded, or zero for a file that is
/// not there yet or holds something unreadable -- both of which are the
/// same thing to every caller here: no evidence of a stream.
fn chunks_written(progress: &Path) -> u64 {
    std::fs::read_to_string(progress)
        .ok()
        .and_then(|text| text.trim().parse().ok())
        .unwrap_or(0)
}

fn quiet(session: &mut BenchSession) {
    let _ = session.settle(SettleBound {
        quiet: KEY_QUIET,
        deadline: TURN_DEADLINE,
    });
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    #[test]
    fn a_missing_or_unreadable_progress_file_is_no_evidence_of_a_stream() {
        let dir = view_test_support::ScratchDir::new("ai-session-progress").unwrap();
        let progress = dir.path().join(PROGRESS_FILE);
        assert_eq!(chunks_written(&progress), 0, "a file that is not there");
        std::fs::write(&progress, "not a number").unwrap();
        assert_eq!(chunks_written(&progress), 0, "a file that holds nonsense");
        std::fs::write(&progress, " 42\n").unwrap();
        assert_eq!(chunks_written(&progress), 42, "a count, whitespace and all");
    }

    #[test]
    fn a_turn_that_stopped_streaming_is_refused_rather_than_recorded() {
        let dir = view_test_support::ScratchDir::new("ai-session-sustained").unwrap();
        let progress = dir.path().join(PROGRESS_FILE);
        std::fs::write(&progress, "10").unwrap();
        let turn = LiveTurn {
            progress: progress.clone(),
            at_start: 10,
        };
        let err = turn
            .ended_still_streaming()
            .expect_err("a stream that added nothing must refuse the row");
        assert!(
            format!("{err}").contains("0 chunk(s) across the sampling run"),
            "the refusal must say how little arrived: {err}"
        );

        std::fs::write(&progress, (10 + SUSTAINED_MINIMUM).to_string()).unwrap();
        assert_eq!(
            turn.ended_still_streaming()
                .expect("a stream at the minimum must stand"),
            SUSTAINED_MINIMUM
        );
    }
}
