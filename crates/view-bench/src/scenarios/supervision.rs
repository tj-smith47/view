//! The availability scenario: how long view takes to say that its engine
//! has stopped serving, and how long a replacement takes to put the
//! session's own text back on screen.
//!
//! Both numbers are read off the terminal rather than out of a model. The
//! oracle already folds the same two schedules through production's own
//! watches and asserts the verdicts they reach (`view_oracle::hang`); what
//! is missing there is the quantity a user actually waits through, which is
//! not a verdict but a painted frame. So this row drives the shipped binary
//! in a pty and stops its clock on the banner text
//! [`WedgeKind::ReadSide`] paints and on the buffer line only a recovered
//! swap can supply.
//!
//! Two boundaries, matching the two failures with two recoveries:
//!
//! - **wedge detect** -- a synchronous Lua loop inside the engine (the same
//!   chunk the oracle's read-side schedule fires, taken from
//!   [`wedge_command`] rather than copied) until the sticky banner naming
//!   the read-side wedge is on screen. The connection stays open and the
//!   engine drains everything view writes, so nothing but the heartbeat can
//!   notice, and the ceiling is the heartbeat's own:
//!   [`view_oracle::hang::DETECTION_BOUND`].
//! - **restart rehydrate** -- the engine killed out of band until the
//!   unsaved line it was holding is painted again by its replacement. That
//!   line is never written to disk, so a screen holding it again is
//!   evidence of the whole chain: the death noticed, a replacement spawned
//!   and attached, its swap prompt answered, and the recovered text
//!   painted.
//!
//! Every sample is its own process. A wedged engine stays wedged for the
//! whole self-bound of its loop and an automatic recovery is granted a
//! bounded number of times per session, so reusing one session across
//! samples would measure a different thing each time.

use std::ffi::OsString;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use view_core::native::supervision::WedgeKind;
use view_oracle::hang::{kill_out_of_band, wedge_command};

use crate::boundaries::screen_holds;
use crate::sampling::{median_of_trials, Distribution};
use crate::scenarios::Protocol;
use crate::session::{BenchSession, SettleBound, SpawnSpec, ViewSpec};
use crate::BenchError;

/// Samples per trial, per boundary.
///
/// Fixed here rather than taken from `--samples`, the way the picker row's
/// scan phase already fixes its own: a wedge sample cannot return before
/// the heartbeat's ceiling has elapsed, so the matrix-wide sample count
/// would price this row in hours. Three is the smallest count a tail can be
/// read from at all -- the linear-rank p99 over three samples is the worst
/// of the three -- and the gated statistic is the median across trials, so
/// one unlucky spawn moves a trial rather than the recorded number.
const SAMPLES: usize = 3;

/// How long the blocking Lua chunk holds the engine for.
///
/// Two bounds meet here. It must outlast [`DETECT_TIMEOUT`], or a loop
/// ending on its own would let the engine answer again before the boundary
/// this row times is reached, and the sample would record a wedge nobody
/// waited out. And it must be short enough that an engine orphaned by a
/// harness that died without running its own teardown -- an out-of-memory
/// kill, an impatient runner -- stops burning a core within the minute
/// rather than at the next reboot.
const WEDGE_SELF_BOUND: Duration = Duration::from_secs(60);

/// Bound on one wedge's detection before the sample is a desync rather than
/// a slow reading.
///
/// Comfortably past [`view_oracle::hang::DETECTION_BOUND`], because a reading at the ceiling
/// is the number this row exists to record and a bound that could refuse it
/// would turn an honest worst case into a harness failure. What it does
/// refuse is a wedge that was never noticed at all.
const DETECT_TIMEOUT: Duration = Duration::from_secs(30);

/// Bound on one restart's rehydration before the sample is a desync.
///
/// Sized against the recovery it waits for rather than against the
/// heartbeat: a closed connection is noticed on the spot, so everything
/// after it is a spawn, a handshake, a swap prompt and a paint.
const REHYDRATE_TIMEOUT: Duration = Duration::from_secs(30);

/// Bound on the preparation steps around a timed boundary -- the engine
/// naming its own pid, the buffer leaving the screen -- none of which is
/// part of any recorded number.
const STEP_TIMEOUT: Duration = Duration::from_secs(15);

/// Gap between screen reads while a boundary is awaited.
///
/// Deliberately a sleep where the paired latency rows spin: those measure
/// sub-millisecond quantities, this one measures seconds, and a poll that
/// spun for twelve of them would take a core away from the editor it is
/// timing on every sample. Five milliseconds is under a twentieth of a
/// percent of the smaller boundary.
const POLL_INTERVAL: Duration = Duration::from_millis(5);

/// How long the screen must hold still before a sample fires.
const SETTLE_QUIET: Duration = Duration::from_millis(400);

/// The buffer line the engine writes its own pid into.
///
/// One line does both jobs. It names the process the sample has to kill,
/// and -- because it is an unsaved edit that only nvim's swap file ever
/// holds -- its reappearance after a restart is the rehydration boundary
/// itself. A marker with no space in it, so a diffing painter's blank cells
/// cannot break the match.
const PID_MARKER: &str = "VIEWENGINEPID=";

/// Which buffer line carries the marker.
///
/// Not the first one. A restart's swap recovery prints messages nvim's
/// caller shows as a native notification, drawn over the top of the grid --
/// nine rows of it, observed covering the recovered line while the
/// rehydration boundary waited for a row it could not see. A line below the
/// overlay is painted by the same frame and is visible in it.
const PID_LINE: usize = 21;

/// The environment variable holding the state directory nvim keeps its swap
/// files under.
const STATE_HOME_VAR: &str = "XDG_STATE_HOME";

/// Distinguishes one sample's state directory from the last one's.
static SAMPLE_SERIAL: AtomicU64 = AtomicU64::new(0);

/// What one supervision run observed.
#[derive(Debug)]
pub struct SupervisionOutcome {
    /// Per-trial wedge-to-banner distributions, in milliseconds.
    pub detect_trials: Vec<Distribution>,
    /// Per-trial death-to-recovered-text distributions, in milliseconds.
    pub rehydrate_trials: Vec<Distribution>,
    /// Median across trials of the wedge-to-banner p99, in milliseconds.
    pub gated_wedge_detect_p99_ms: f64,
    /// Median across trials of the death-to-recovered-text p99, in
    /// milliseconds.
    pub gated_restart_rehydrate_p99_ms: f64,
}

/// Runs both boundaries for `protocol.trials` trials against `view`.
///
/// # Errors
///
/// Returns [`BenchError::Desync`] if a boundary is never reached inside its
/// bound, [`BenchError::Session`] if a spawn or a write fails, and
/// [`BenchError::NoTrials`] if no trial was asked for.
pub fn run(
    view: ViewSpec<'_>,
    protocol: &Protocol,
    settle_deadline: Duration,
) -> Result<SupervisionOutcome, BenchError> {
    let settle = SettleBound {
        quiet: SETTLE_QUIET,
        deadline: settle_deadline,
    };
    let mut detect_trials = Vec::with_capacity(protocol.trials);
    let mut rehydrate_trials = Vec::with_capacity(protocol.trials);
    for _ in 0..protocol.trials {
        let mut detect = Vec::with_capacity(SAMPLES);
        let mut rehydrate = Vec::with_capacity(SAMPLES);
        for _ in 0..SAMPLES {
            detect.push(detect_sample(&own_state_home(view.0), settle)?);
            rehydrate.push(rehydrate_sample(&own_state_home(view.0), settle)?);
        }
        detect_trials.push(Distribution::from_samples(&detect, 0)?);
        rehydrate_trials.push(Distribution::from_samples(&rehydrate, 0)?);
    }
    let detect_p99: Vec<f64> = detect_trials.iter().map(Distribution::p99).collect();
    let rehydrate_p99: Vec<f64> = rehydrate_trials.iter().map(Distribution::p99).collect();
    Ok(SupervisionOutcome {
        gated_wedge_detect_p99_ms: median_of_trials(&detect_p99)?,
        gated_restart_rehydrate_p99_ms: median_of_trials(&rehydrate_p99)?,
        detect_trials,
        rehydrate_trials,
    })
}

/// One wedge sample: a settled session, the engine held in a synchronous
/// Lua loop, and the wait for the banner that says so.
fn detect_sample(view: &SpawnSpec, settle: SettleBound) -> Result<f64, BenchError> {
    let mut session = spawn_settled(view, settle)?;
    let pid = engine_pid(&mut session)?;
    let start = Instant::now();
    session.send(submitted(&wedge_command(WEDGE_SELF_BOUND)).as_bytes())?;
    let detected = wait_for(
        &mut session,
        Present(WedgeKind::ReadSide.notice()),
        DETECT_TIMEOUT,
    );
    // the engine outlives the editor here: it is inside a loop that reaches
    // no event loop iteration, so nothing view can send ends it and killing
    // the pty child would leave it spinning a core for the rest of its
    // self-bound. Ordered after the session's own teardown because view
    // would otherwise notice the death and spawn a replacement to reap.
    drop(session);
    let _ = kill_out_of_band(pid);
    detected.map(|at| elapsed_ms(start, at))
}

/// One restart sample: an unsaved line the swap holds, taken off the
/// screen, then the engine killed out of band and the wait for that line to
/// be painted again by its replacement.
fn rehydrate_sample(view: &SpawnSpec, settle: SettleBound) -> Result<f64, BenchError> {
    let mut session = spawn_settled(view, settle)?;
    let pid = engine_pid(&mut session)?;
    let marker = format!("{PID_MARKER}{pid}");
    // the swap file is written on a timer nvim owns (an idle span, a typed
    // character count), and this sample kills the engine seconds after one
    // programmatic edit: without asking for the flush, the replacement would
    // recover a swap that never held the line, which reads as a rehydration
    // that never happened rather than as the setup fault it is
    session.send(submitted(":preserve").as_bytes())?;
    // an empty buffer in front of the modified one, never a write: the line
    // has to leave the screen without leaving the swap, so that the screen
    // holding it again cannot be the frame that was already there
    session.send(submitted(":enew").as_bytes())?;
    wait_for_absence(&mut session, Absent(&marker), STEP_TIMEOUT)?;
    let start = Instant::now();
    kill_out_of_band(pid).map_err(BenchError::Session)?;
    let recovered = wait_for(&mut session, Present(&marker), REHYDRATE_TIMEOUT);
    session.shutdown();
    recovered.map(|at| elapsed_ms(start, at))
}

/// A spawned session that has settled, ready for a sample to fire.
fn spawn_settled(view: &SpawnSpec, settle: SettleBound) -> Result<BenchSession, BenchError> {
    let mut session = BenchSession::spawn(view)?;
    if !session.settle(settle) {
        return Err(BenchError::Desync {
            context: format!(
                "the session never settled within {:?}; screen:\n{}",
                settle.deadline,
                session.screen_text()
            ),
        });
    }
    Ok(session)
}

/// `base` with a state directory of this sample's own.
///
/// Every sample here ends with an engine that was killed rather than asked
/// to leave, so every sample leaves a swap file behind. Sharing one state
/// directory across samples would hand each spawn its predecessor's
/// leftovers to recover -- observed as `E309: Unable to read block 1` on
/// the second sample of a run, with the recovery prompt on screen where the
/// buffer should have been. A directory per sample makes the swap this row
/// cares about the only one any spawn can find.
fn own_state_home(base: &SpawnSpec) -> SpawnSpec {
    let serial = SAMPLE_SERIAL.fetch_add(1, Ordering::Relaxed);
    let mut spec = base.clone();
    for (key, value) in &mut spec.env {
        if key == STATE_HOME_VAR {
            let mut own = value.clone();
            own.push(OsString::from(format!(".{serial}")));
            *value = own;
        }
    }
    spec
}

/// Asks the engine to write its own pid into the top of the buffer and
/// reads it back off the screen.
///
/// The pid comes from the engine rather than from the platform's process
/// table: view's child is spawned with no process group of its own, and
/// walking a parent-to-child relation is a different mechanism on each of
/// the three platforms this row has to run on. Asking the process that
/// knows costs one round trip and works everywhere.
fn engine_pid(session: &mut BenchSession) -> Result<u32, BenchError> {
    session.send(submitted(&pid_command()).as_bytes())?;
    let deadline = Instant::now() + STEP_TIMEOUT;
    loop {
        if let Some(pid) = session.with_screen(|screen| {
            (0..screen.size().0)
                .find_map(|row| parse_pid(&crate::boundaries::row_text(screen, row)))
        }) {
            return Ok(pid);
        }
        if Instant::now() >= deadline {
            return Err(BenchError::Desync {
                context: format!(
                    "the engine never painted {PID_MARKER} within {STEP_TIMEOUT:?}; screen:\n{}",
                    session.screen_text()
                ),
            });
        }
        std::thread::sleep(POLL_INTERVAL);
    }
}

/// The `:` command that plants [`PID_MARKER`] and the engine's pid into the
/// buffer, on [`PID_LINE`] under blank lines above it.
///
/// A buffer line, not a printed message: a message is routed by kind and
/// may be shown for a bounded time, while a line is on screen for as long
/// as the buffer is, which is what the rehydration boundary needs from it.
/// No `<` anywhere in it, for the reason [`wedge_command`] documents.
fn pid_command() -> String {
    let above = PID_LINE - 1;
    format!(
        ":lua local l={{}} for i=1,{above} do l[i]='' end l[{PID_LINE}]='{PID_MARKER}'..\
         vim.uv.os_getpid() vim.api.nvim_buf_set_lines(0, 0, 0, false, l)"
    )
}

/// The pid on a screen row carrying [`PID_MARKER`], if that row has one.
///
/// Digits are required, not assumed. The command palette paints the command
/// being typed, marker included, before the engine has run any of it, so a
/// match on the marker alone would read the echo of the request as its
/// answer.
fn parse_pid(row: &str) -> Option<u32> {
    let tail = row.split(PID_MARKER).nth(1)?;
    let digits: String = tail.chars().take_while(char::is_ascii_digit).collect();
    digits.parse().ok()
}

/// `text` as a terminal types it: the command, then the carriage return
/// that submits it.
///
/// The engine's own `<CR>` notation is not usable here. These bytes go to a
/// terminal, where view reads them as keys, so the submit has to be the
/// real control character rather than the four letters naming it.
fn submitted(text: &str) -> String {
    format!("{text}\r")
}

/// What a wait is waiting for: the screen holding a needle, or the screen
/// having stopped holding it.
///
/// Two types rather than a bool, because a wait that polls for the wrong
/// polarity returns immediately and records a boundary that never happened.
#[derive(Clone, Copy)]
struct Present<'a>(&'a str);

/// See [`Present`].
#[derive(Clone, Copy)]
struct Absent<'a>(&'a str);

/// Polls until the screen holds `needle`, returning when it first did.
fn wait_for(
    session: &mut BenchSession,
    needle: Present<'_>,
    timeout: Duration,
) -> Result<Instant, BenchError> {
    poll_until(session, needle.0, true, timeout)
}

/// Polls until the screen no longer holds `needle`.
fn wait_for_absence(
    session: &mut BenchSession,
    needle: Absent<'_>,
    timeout: Duration,
) -> Result<Instant, BenchError> {
    poll_until(session, needle.0, false, timeout)
}

fn poll_until(
    session: &mut BenchSession,
    needle: &str,
    want: bool,
    timeout: Duration,
) -> Result<Instant, BenchError> {
    let deadline = Instant::now() + timeout;
    loop {
        if session.with_screen(|screen| screen_holds(screen, needle)) == want {
            return Ok(Instant::now());
        }
        if Instant::now() >= deadline {
            let state = if want { "reached" } else { "left" };
            return Err(BenchError::Desync {
                context: format!(
                    "the screen never {state} {needle:?} within {timeout:?}; screen:\n{}",
                    session.screen_text()
                ),
            });
        }
        std::thread::sleep(POLL_INTERVAL);
    }
}

fn elapsed_ms(start: Instant, at: Instant) -> f64 {
    at.saturating_duration_since(start).as_secs_f64() * 1000.0
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use std::path::PathBuf;

    use view_oracle::hang::DETECTION_BOUND;

    use super::*;

    /// The loop has to outlast the wait, or a sample would time a wedge the
    /// engine had already come out of.
    #[test]
    fn the_wedge_outlasts_the_wait_for_its_own_detection() {
        assert!(
            WEDGE_SELF_BOUND > DETECT_TIMEOUT,
            "a loop that ends first lets the engine answer again mid-sample"
        );
    }

    /// The wait has to outlast the ceiling production's own constants set,
    /// or the worst honest reading this row exists to record would be
    /// reported as a harness failure.
    #[test]
    fn the_wait_outlasts_the_heartbeats_own_ceiling() {
        assert!(DETECT_TIMEOUT > DETECTION_BOUND);
    }

    /// A sample recovers the swap it left itself and no other, so the one
    /// environment entry that decides where swap files land is the one
    /// entry a sample may rewrite.
    #[test]
    fn each_sample_gets_its_own_state_directory_and_nothing_else_moves() {
        let base = SpawnSpec {
            program: PathBuf::from("view"),
            args: vec![OsString::from("scratch.txt")],
            env: vec![
                (OsString::from("XDG_CONFIG_HOME"), OsString::from("/cfg")),
                (OsString::from(STATE_HOME_VAR), OsString::from("/state")),
                (OsString::from("TERM"), OsString::from("xterm-256color")),
            ],
            cwd: None,
        };
        let first = own_state_home(&base);
        let second = own_state_home(&base);
        assert_ne!(first.env[1].1, second.env[1].1);
        assert!(first.env[1]
            .1
            .to_string_lossy()
            .starts_with(&*base.env[1].1.to_string_lossy()));
        assert_eq!(first.env[0], base.env[0]);
        assert_eq!(first.env[2], base.env[2]);
        assert_eq!(first.args, base.args);
    }

    #[test]
    fn a_planted_pid_line_reads_back_as_its_number() {
        assert_eq!(parse_pid("VIEWENGINEPID=41234"), Some(41234));
        assert_eq!(parse_pid("  VIEWENGINEPID=7 trailing text"), Some(7));
    }

    /// The palette paints the request before the engine answers it, so the
    /// marker alone is not the answer.
    #[test]
    fn the_command_being_typed_is_not_read_as_the_answer() {
        assert_eq!(parse_pid(&pid_command()), None);
        assert_eq!(parse_pid("a row with no marker at all"), None);
    }

    /// The chunk is submitted as a real carriage return: the engine's key
    /// notation means nothing to a terminal.
    #[test]
    fn a_typed_command_ends_in_a_carriage_return_not_a_key_name() {
        let typed = submitted(&wedge_command(WEDGE_SELF_BOUND));
        assert!(typed.ends_with('\r'), "{typed}");
        assert!(!typed.contains("<CR>"), "{typed}");
    }
}
