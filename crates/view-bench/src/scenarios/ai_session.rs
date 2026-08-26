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

/// A character typed into the open panel, and how the composer draws it.
///
/// The preamble probes with a keystroke rather than by looking for the
/// panel's chrome: chrome only says a panel is on screen, while this says
/// keys are reaching its composer, which is the state the row samples.
const COMPOSER_PROBE: &[u8] = b"z";
const COMPOSER_PROBE_ECHO: &str = "> z";

/// Where that fixture records how many chunks it has written.
const PROGRESS_FILE: &str = "view-ai-stub-stream-progress.txt";

/// The progress path an AI row spawns its agent with, given the project
/// root view runs in: beside that root rather than inside it.
///
/// Inside is what the fixture would pick on its own, and it is the one
/// place the file cannot go: the root is also the directory view watches
/// for external writes, so a count rewritten every 20ms becomes a
/// steady stream of detection round trips to the engine -- traffic that
/// exists only in the rows holding a stream live, which is precisely what
/// those rows compare against rows that have none.
///
/// A root at the filesystem root has no beside, and keeps the fixture's
/// own convention rather than failing a row over a path that cannot occur
/// in a scratch world.
#[must_use]
pub fn progress_path(cwd: &Path) -> PathBuf {
    cwd.parent().unwrap_or(cwd).join(PROGRESS_FILE)
}

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

/// How long ago the fixture may last have written its count when the
/// window's liveness is read, before the turn reads as over.
///
/// A count alone cannot see the end of the window: a turn killed halfway
/// through still added thousands of chunks, and every one of them was
/// added before the samples that followed it. The last write's age is the
/// half that can, which is why it is read while the session is still up.
/// Two seconds is a hundred of the fixture's 20ms cadences, so it is a
/// liveness question rather than a load threshold.
///
/// The window it cannot cover is its own length: a stream that died
/// within the last two seconds of sampling still reads as live, so the
/// samples taken in that window are attested by the count alone. What the
/// pair rules out is a turn that ended early and left the rest of the run
/// measuring the session-absent path under this name.
const STREAM_STALE: Duration = Duration::from_secs(2);

/// What the fixture writes into its progress file when it stops on its own
/// ceiling rather than because view went away (see that fixture's header;
/// the word is duplicated here for the same reason the file name is -- two
/// crates, one convention, no shared dependency between them).
const CEILING_SENTINEL: &str = "ceiling";

/// The bracketed-paste envelope a terminal puts around pasted text, which
/// is how view is told a burst of bytes is one paste and not typing.
const PASTE_OPEN: &[u8] = b"\x1b[200~";
const PASTE_CLOSE: &[u8] = b"\x1b[201~";

/// Bytes per write inside that envelope. Small enough that a pty buffer
/// smaller than the text never has to hold all of it at once.
const PASTE_CHUNK: usize = 4096;

/// Consecutive letters on one screen row that say the row belongs to a
/// pasted prompt rather than to the panel's own chrome. Comfortably under
/// the narrowest composer this runs at and far past any word in the
/// panel's own text.
const SEEDED_ROW_RUN: usize = 20;

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
/// Opens the agent panel and answers the trust prompt, leaving focus in
/// the composer with no turn in flight.
///
/// The composer-echo row's subject is a keystroke that never leaves view,
/// so it deliberately does not start a turn: a stream repainting the
/// transcript underneath would put frames in the pipe that the sampled
/// keystroke did not cause.
///
/// # Errors
///
/// Returns [`BenchError::Desync`] when the panel never reaches the screen,
/// which is the one way the two keys above can fail.
pub fn open_panel(session: &mut BenchSession) -> Result<(), BenchError> {
    session.send(b"\x1b")?;
    session.send(OPEN_PANEL)?;
    quiet(session);
    session.send(TRUST_YES)?;
    quiet(session);
    session.send(COMPOSER_PROBE)?;
    quiet(session);
    if !session.screen_text().contains(COMPOSER_PROBE_ECHO) {
        return Err(BenchError::Desync {
            context: format!(
                "a character typed after opening the agent panel did not echo in its composer, \
                 so this row would have measured a keystroke the panel never saw; screen:\n{}",
                session.screen_text()
            ),
        });
    }
    // back to an empty composer, which is where the sampling loop's own
    // type/delete alternation starts
    session.send(b"\x7f")?;
    quiet(session);
    Ok(())
}

/// Pastes `text` into the open panel's composer as one bracketed paste,
/// and confirms it reached the screen.
///
/// One paste rather than a keystroke per character: the row's subject is
/// what a keystroke costs against a composer that already holds this text,
/// and typing it in would spend that whole cost n times before the first
/// sample.
///
/// Written in chunks because the pty's own buffer is smaller than the text
/// -- the decoder accumulates until the closing marker, so where the writes
/// fall inside the envelope does not change what it decodes.
///
/// # Errors
///
/// Returns [`BenchError::Desync`] when no wrapped row of the pasted text
/// is on screen afterwards, which is the one way the paste can fail
/// without the write itself failing.
pub fn seed_composer(session: &mut BenchSession, text: &str) -> Result<(), BenchError> {
    session.send(PASTE_OPEN)?;
    for chunk in text.as_bytes().chunks(PASTE_CHUNK) {
        session.send(chunk)?;
    }
    session.send(PASTE_CLOSE)?;
    quiet(session);
    if longest_letter_run(&session.screen_text()) < SEEDED_ROW_RUN {
        return Err(BenchError::Desync {
            context: format!(
                "no wrapped row of the {} byte(s) pasted into the composer reached the screen, \
                 so this row would have measured a keystroke against an empty prompt; screen:\n{}",
                text.len(),
                session.screen_text()
            ),
        });
    }
    Ok(())
}

/// The longest run of consecutive ASCII letters on any one row of `screen`.
///
/// A wrapped composer row of the seed is a full row of them; the panel's
/// own chrome and any transcript row above it are words and spaces. So one
/// run past [`SEEDED_ROW_RUN`] says a painted row belongs to the pasted
/// prompt, without depending on where the wrap happens to fall.
fn longest_letter_run(screen: &str) -> usize {
    screen
        .lines()
        .map(|line| {
            line.chars()
                .fold((0_usize, 0_usize), |(best, run), ch| {
                    let run = if ch.is_ascii_alphabetic() { run + 1 } else { 0 };
                    (best.max(run), run)
                })
                .0
        })
        .max()
        .unwrap_or(0)
}

pub fn start(session: &mut BenchSession, cwd: &Path) -> Result<LiveTurn, BenchError> {
    let progress = progress_path(cwd);
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
    /// Confirms the stream is still running at the end of the sampling it
    /// bracketed, and returns what it added.
    ///
    /// Read while the session is still up, deliberately: after teardown
    /// every one of these signals reads the same whether the turn ran to
    /// the last sample or died at the first, and the row would stand
    /// behind a number nothing supports.
    ///
    /// # Errors
    ///
    /// Returns [`BenchError::Desync`] when the fixture stopped on its own
    /// ceiling, when fewer than [`SUSTAINED_MINIMUM`] chunks were added, or
    /// when the last chunk is older than [`STREAM_STALE`] -- a turn that
    /// ended partway through a run whose whole subject is what a live turn
    /// costs.
    pub fn still_streaming(&self) -> Result<u64, BenchError> {
        let refuse = |why: String| {
            Err(BenchError::Desync {
                context: format!("{why}, so the samples were not all taken under a live turn"),
            })
        };
        if std::fs::read_to_string(&self.progress).is_ok_and(|text| text.trim() == CEILING_SENTINEL)
        {
            return refuse("the agent stopped on its own streaming ceiling".to_string());
        }
        let added = chunks_written(&self.progress).saturating_sub(self.at_start);
        if added < SUSTAINED_MINIMUM {
            return refuse(format!(
                "the agent added {added} chunk(s) across the sampling run, under the \
                 {SUSTAINED_MINIMUM} a live turn produces"
            ));
        }
        match last_write_age(&self.progress) {
            Some(idle) if idle <= STREAM_STALE => Ok(added),
            Some(idle) => refuse(format!(
                "the agent's last chunk is {idle:?} old, past the {STREAM_STALE:?} a \
                 stream at the fixture's cadence stays inside"
            )),
            None => refuse("the agent's progress file cannot be read at all".to_string()),
        }
    }
}

/// How long ago the fixture last wrote its count, or `None` when that
/// cannot be established -- an unreadable file, or a clock that moved
/// backwards under it.
fn last_write_age(progress: &Path) -> Option<Duration> {
    std::fs::metadata(progress)
        .and_then(|meta| meta.modified())
        .ok()?
        .elapsed()
        .ok()
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

    /// The count is rewritten every 20ms for the whole sampling run. Inside
    /// the root, each rewrite is an external write view detects and answers
    /// -- engine traffic the AI rows would carry and the rows they are
    /// compared against would not.
    #[test]
    fn the_progress_file_lands_beside_the_root_view_watches_rather_than_in_it() {
        let dir = view_test_support::ScratchDir::new("ai-session-outside").unwrap();
        let root = dir.path().join("view");
        std::fs::create_dir_all(&root).unwrap();
        let progress = progress_path(&root);
        assert!(
            !progress.starts_with(&root),
            "{} is inside the watched root {}",
            progress.display(),
            root.display()
        );
        assert_eq!(progress.parent(), Some(dir.path()));
    }

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

    /// A turn is live when the count moved AND the last chunk is recent:
    /// one without the other is exactly the state a dead agent leaves.
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
            .still_streaming()
            .expect_err("a stream that added nothing must refuse the row");
        assert!(
            format!("{err}").contains("0 chunk(s) across the sampling run"),
            "the refusal must say how little arrived: {err}"
        );

        std::fs::write(&progress, (10 + SUSTAINED_MINIMUM).to_string()).unwrap();
        assert_eq!(
            turn.still_streaming()
                .expect("a stream at the minimum, written just now, must stand"),
            SUSTAINED_MINIMUM
        );
    }

    #[test]
    fn a_stream_that_added_plenty_and_then_died_is_refused_on_its_last_write() {
        let dir = view_test_support::ScratchDir::new("ai-session-stale").unwrap();
        let progress = dir.path().join(PROGRESS_FILE);
        std::fs::write(&progress, "5000").unwrap();
        let aged = std::fs::File::options()
            .write(true)
            .open(&progress)
            .unwrap();
        aged.set_times(
            std::fs::FileTimes::new()
                .set_modified(std::time::SystemTime::now() - Duration::from_secs(30)),
        )
        .unwrap();
        let turn = LiveTurn {
            progress,
            at_start: 0,
        };
        let err = turn
            .still_streaming()
            .expect_err("a turn that died mid-window must refuse, however much it added");
        assert!(
            format!("{err}").contains("old, past the"),
            "the refusal must name the age of the last chunk: {err}"
        );
    }

    #[test]
    fn a_fixture_that_stopped_on_its_own_ceiling_is_refused() {
        let dir = view_test_support::ScratchDir::new("ai-session-ceiling").unwrap();
        let progress = dir.path().join(PROGRESS_FILE);
        std::fs::write(&progress, CEILING_SENTINEL).unwrap();
        let turn = LiveTurn {
            progress,
            at_start: 0,
        };
        let err = turn
            .still_streaming()
            .expect_err("a fixture at its ceiling is not a live turn");
        assert!(
            format!("{err}").contains("own streaming ceiling"),
            "the refusal must name the ceiling: {err}"
        );
    }
}
