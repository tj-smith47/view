//! Taking view's own standing notices off the screen before a cell samples
//! it.
//!
//! view draws its message stack over the buffer, so a notice still standing
//! when sampling starts occupies the cells the row reads and the row times a
//! character that can never paint there. nvim puts its messages on the
//! command line below the buffer, so the bare side of a pair has nothing
//! standing over its own text and nothing to take down.
//!
//! Two lifetimes have to end, and only one of them ends by waiting. A
//! transient toast retires on its own idle timer, but a toast promoted into
//! the top slot is armed for its whole timeout at the moment the toast above
//! it left -- and that departure is the repaint a quiet clock restarts on --
//! so a settle whose quiet span is shorter than the timeout returns with the
//! promoted toast still up, however long the session has been idle. The
//! notices view raised about a condition it went and observed never retire
//! at all: they carry a family, `<Esc>` deliberately leaves them alone, and
//! `d` in the message history is their one way down.

use std::time::{Duration, Instant};

use view_core::native::palette::MESSAGE_HISTORY_TITLE;
use view_core::native::toast::{DEFAULT_CAPACITY, TRANSIENT_TOAST_TIMEOUT};

use crate::session::{BenchSession, SettleBound, SpawnSpec};
use crate::BenchError;

/// The quiet span a screen must hold before a cell may call it clear of
/// notices: past the idle timeout a transient toast expires on, so the
/// settle outlasts a toast that took the top slot at the instant the quiet
/// clock last restarted.
pub const CLEAR_QUIET: Duration =
    TRANSIENT_TOAST_TIMEOUT.saturating_add(Duration::from_millis(500));

/// The floor on the quiet span a repaint gets to reach the screen: the
/// message history arriving, or the notices it took down leaving. Both are
/// one round trip to nvim and back with nothing timed behind them, so a
/// local attach finishes well inside this.
///
/// It is a floor and not the span itself because a caller reaching its
/// measured side over an injected-latency transport pays that latency on
/// every one of those trips, and a sub-second span is then satisfied by the
/// static screen standing before the reply lands -- the same class of bug
/// [`crate::scenarios::echo::DEFAULT_STARTUP_QUIET`] documents for the
/// startup settle. Such a caller passes its own widened span to
/// [`take_down`], which takes whichever is larger.
pub const REPAINT_QUIET: Duration = Duration::from_millis(500);

/// The `:View` form that opens the message history, in the two words
/// [`view_core::native::mappings::default_maps`] spells it with.
const HISTORY_FEATURE: &str = "notifications";
const HISTORY_VERB: &str = "history";

/// One pass over one history entry: take the standing notice it belongs to
/// down, then select the next one.
///
/// `d` on an entry whose line carries no family takes nothing down, and `j`
/// past the last entry moves nothing, so a walk longer than the history is
/// keystrokes and no other effect.
const WALK_STEP: &[u8] = b"dj";

/// nvim's own answer to an ex-command it does not know, which is what view's
/// command is until view has registered it.
const UNKNOWN_COMMAND: &str = "Not an editor command";

/// The file stem every build of view is spawned under -- the release
/// binary, the tap and no-speculate variants beside it, and the one a
/// remote row reaches its engine from.
const VIEW_PROGRAM: &str = "view";

/// The ex-command that opens the message history.
#[must_use]
pub fn history_command() -> String {
    format!(":View {HISTORY_FEATURE} {HISTORY_VERB}\r")
}

/// Whether `spec` runs view rather than a bare editor.
///
/// Read off the program a session was spawned with, not off which side of a
/// pair it measures: the null-pair calibration the gate opens with drives
/// the echo scenario with a bare editor in the measured side's place, and a
/// takedown that keyed on the side would have typed view's own command at
/// an editor that has never heard of it.
#[must_use]
pub fn runs_view(spec: &SpawnSpec) -> bool {
    spec.program
        .file_stem()
        .is_some_and(|stem| stem.eq_ignore_ascii_case(VIEW_PROGRAM))
}

/// Clears view's notice stack off `session`: waits for view's own bootstrap
/// to answer, waits out the toasts that retire on a timer, then walks the
/// message history taking down the notices that never do.
///
/// `repaint_quiet` is the caller's own quiet span for one round trip to
/// nvim and back, floored at [`REPAINT_QUIET`]; a caller measuring over an
/// injected-latency transport passes the widened span it already uses for
/// its startup settle.
///
/// A session that is not running view answers immediately: a bare editor
/// draws no notice over its buffer, and the overlay keys below would be a
/// delete and a motion in its buffer instead.
///
/// # Errors
///
/// Returns [`BenchError::Desync`] when view never answers the command that
/// opens the message history -- which is also the guard that keeps the
/// walk's keys off the buffer -- or when the screen never goes quiet.
/// `deadline` bounds the whole takedown rather than each settle inside it,
/// so a screen that never holds still costs the caller what it asked for
/// and not a multiple of it.
pub fn take_down(
    session: &mut BenchSession,
    spec: &SpawnSpec,
    repaint_quiet: Duration,
    deadline: Duration,
) -> Result<(), BenchError> {
    if !runs_view(spec) {
        return Ok(());
    }
    let until = Instant::now() + deadline;
    let repaint_quiet = repaint_quiet.max(REPAINT_QUIET);
    // with no modal open this is the way out of nvim's own standing wire
    // errors, and it leaves whatever mode the session was in so the
    // ex-command below is typed at the command line rather than into a
    // buffer
    session.send(b"\x1b")?;
    // A quiet screen is not a started view. `:View` is created by the
    // registration view runs at `VimEnter`, and nvim reaches `VimEnter`
    // only after sourcing a config that can spend seconds in Lua without
    // painting anything -- so a settle is satisfied by the file nvim
    // already drew, and the command sent on the strength of it comes back
    // `E492`. The overlay arriving is the one observable that says the
    // registration has run; ask for it until it does.
    while !open_history(session, repaint_quiet, until)? {
        if Instant::now() >= until {
            return Err(BenchError::Desync {
                context: format!(
                    "view never answered {} within {deadline:?}{}; screen:\n{}",
                    history_command().trim_end(),
                    unknown_command_note(&session.screen_text()),
                    session.screen_text()
                ),
            });
        }
        // the command line, and any error nvim put on it, go back down
        // before the next attempt types over them
        session.send(b"\x1b")?;
    }
    session.send(b"\x1b")?;
    // the transients go next and they go by waiting: `d` retracts a family
    // and a toast on a timer carries none, so a stack still draining is a
    // stack that will paint a box over the overlay below. Run after the
    // wait above rather than before it, since the notices a startup raises
    // do not exist until the startup that raises them has run.
    if !session.settle(SettleBound {
        quiet: CLEAR_QUIET,
        deadline: remaining(until),
    }) {
        return Err(BenchError::Desync {
            context: format!(
                "the screen never held still for {CLEAR_QUIET:?} within {deadline:?}, so the \
                 toasts on a timer were still draining; screen:\n{}",
                session.screen_text()
            ),
        });
    }
    if !open_history(session, repaint_quiet, until)? {
        return Err(BenchError::Desync {
            context: format!(
                "the message history never reached the screen within {deadline:?} on a session \
                 that had already answered for it, so the keys that take a standing notice down \
                 would have edited the buffer instead; screen:\n{}",
                session.screen_text()
            ),
        });
    }
    // the whole ring rather than a count of what is standing: the harness
    // cannot see how many notices carry a family, and the steps past the
    // last entry cost nothing
    session.send(&WALK_STEP.repeat(DEFAULT_CAPACITY))?;
    session.send(b"\x1b")?;
    if !session.settle(SettleBound {
        quiet: repaint_quiet,
        deadline: remaining(until),
    }) {
        return Err(BenchError::Desync {
            context: format!(
                "the screen never held still for {repaint_quiet:?} within {deadline:?} after the \
                 notice takedown; screen:\n{}",
                session.screen_text()
            ),
        });
    }
    Ok(())
}

/// Asks for the message history and answers whether it reached the screen.
///
/// # Errors
///
/// Returns [`BenchError`] only when the write to the pty fails.
fn open_history(
    session: &mut BenchSession,
    quiet: Duration,
    until: Instant,
) -> Result<bool, BenchError> {
    session.send(history_command().as_bytes())?;
    Ok(session.settle(SettleBound {
        quiet,
        deadline: remaining(until),
    }) && session.screen_text().contains(MESSAGE_HISTORY_TITLE))
}

/// The half of a refusal that separates a view which never finished starting
/// from one whose overlay is on screen under something else, since the two
/// are the same silence to the caller.
fn unknown_command_note(screen: &str) -> &'static str {
    if screen.contains(UNKNOWN_COMMAND) {
        " -- nvim answered that no such command exists, so view's own \
         registration had not run yet"
    } else {
        ""
    }
}

/// What is left of the takedown's own deadline, floored at zero so a settle
/// that has already run out refuses at once rather than waiting again.
fn remaining(until: Instant) -> Duration {
    until.saturating_duration_since(Instant::now())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use view_core::native::{mappings, toast};

    /// The command is typed at a live editor, so the two words in it have to
    /// be the two words this build answers -- a renamed verb would leave the
    /// harness typing a command nvim reports as unknown, and the walk that
    /// follows would land in the buffer.
    #[test]
    fn the_history_command_names_a_form_this_build_answers() {
        assert!(
            mappings::default_maps()
                .iter()
                .any(|spec| spec.feature == HISTORY_FEATURE && spec.verb == HISTORY_VERB),
            "no mapping row opens the message history as :View {HISTORY_FEATURE} {HISTORY_VERB}"
        );
        assert_eq!(history_command(), ":View notifications history\r");
    }

    fn spec_running(program: &str) -> SpawnSpec {
        SpawnSpec {
            program: std::path::PathBuf::from(program),
            args: Vec::new(),
            env: Vec::new(),
            cwd: None,
        }
    }

    /// The takedown's whole subject is view's own notices, and the gate
    /// opens with a null-pair calibration that drives the measured side of
    /// the echo scenario with a bare editor -- so "which side" is not the
    /// question, "which program" is. Every build of view the matrix spawns
    /// answers yes; the engine it spawns answers no.
    #[test]
    fn only_a_session_running_view_has_a_notice_stack_to_take_down() {
        for program in [
            "target/release/view",
            "target/taps/release/view",
            "target/nospec/release/view",
            "/usr/local/bin/view.exe",
        ] {
            assert!(runs_view(&spec_running(program)), "{program}");
        }
        for program in ["/usr/bin/nvim", "target/engine/bin/nvim", "nvim"] {
            assert!(!runs_view(&spec_running(program)), "{program}");
        }
    }

    /// The two silences a refusal has to tell apart: a view that has not
    /// registered its command yet, and a session that will never have one.
    #[test]
    fn a_refusal_says_when_nvim_called_the_command_unknown() {
        assert!(
            unknown_command_note("E492: Not an editor command: View x").contains("registration")
        );
        assert!(unknown_command_note("~\n~\nscratch.txt").is_empty());
    }

    /// The constant this module exists for, read against the timeout the
    /// toast module actually schedules a transient on rather than against
    /// the constant this file already imports: a route change that stops
    /// expiring transients on `TRANSIENT_TOAST_TIMEOUT` leaves the drain
    /// settle satisfied inside the interval it exists to outwait, and trips
    /// here instead of in a battery.
    #[test]
    fn the_clear_quiet_outlasts_the_timeout_a_transient_toast_is_scheduled_on() {
        let scheduled = toast::timeout_for(toast::Route::Transient)
            .expect("a transient toast expires on its own idle timer");
        assert!(
            CLEAR_QUIET > scheduled,
            "{CLEAR_QUIET:?} does not outlast the scheduled {scheduled:?}"
        );
    }

    /// What a scenario source does about the notices view draws over the
    /// buffer: either it takes them down, or it says why nothing it reads
    /// can be under one.
    const TAKES_THEM_DOWN: &str = "";
    const SCENARIO_TREATMENT: &[(&str, &str)] = &[
        (
            "ai_session.rs",
            "drives a session the taps preamble already spawned and cleared, and adds no \
             spawn of its own",
        ),
        ("clock.rs", "a clock the scenarios read, not a session"),
        ("echo.rs", TAKES_THEM_DOWN),
        (
            "echo_control.rs",
            "the echo row against nvim's own out-of-process TUI, driven through `echo::run`",
        ),
        (
            "echo_speculated.rs",
            "the echo row's predicted-glyph boundary, driven through `echo::run_observed`",
        ),
        (
            "echo_speculated_rtt.rs",
            "arms the latency relay and builds the spec the speculated row is driven with; \
             it opens no session",
        ),
        (
            "first_paint.rs",
            "the boundary is the cold startup itself, so there is no settled screen to \
             clear before it; the marker is planted on more lines than the grid has rows, \
             which is what keeps a corner box from hiding it and is pinned by \
             `the_first_paint_marker_outfills_the_grid_it_is_read_on`",
        ),
        ("flood.rs", TAKES_THEM_DOWN),
        (
            "memory.rs",
            "the sample is the measured process's resident size, never a cell",
        ),
        ("mod.rs", "the module list"),
        ("picker.rs", TAKES_THEM_DOWN),
        (
            "remote_memory.rs",
            "a pass-through to `memory::run` against a remote engine, so its sample is a \
             footprint too",
        ),
        ("scroll.rs", TAKES_THEM_DOWN),
        ("supervision.rs", TAKES_THEM_DOWN),
        (
            "taps/ai.rs",
            "opens the agent panel on a session `taps::prepare` cleared before handing it \
             over",
        ),
        ("taps/mod.rs", TAKES_THEM_DOWN),
    ];

    /// Walks the scenario sources rather than a list of the ones anybody
    /// remembered: a new row fails here until it either clears the stack or
    /// writes down why its samples cannot be under a notice.
    #[test]
    fn every_scenario_source_answers_for_the_notice_stack() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/scenarios");
        let mut sources: Vec<(String, String)> = Vec::new();
        let mut stack = vec![root.clone()];
        while let Some(dir) = stack.pop() {
            for entry in std::fs::read_dir(&dir).unwrap() {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    stack.push(path);
                    continue;
                }
                if path.extension().is_none_or(|ext| ext != "rs") {
                    continue;
                }
                let name = path
                    .strip_prefix(&root)
                    .unwrap()
                    .to_string_lossy()
                    .replace('\\', "/");
                sources.push((name, std::fs::read_to_string(&path).unwrap()));
            }
        }
        sources.sort_by(|a, b| a.0.cmp(&b.0));

        let listed: Vec<&str> = SCENARIO_TREATMENT.iter().map(|(name, _)| *name).collect();
        let found: Vec<&str> = sources.iter().map(|(name, _)| name.as_str()).collect();
        assert_eq!(
            found, listed,
            "a scenario source is missing from the notice-stack treatment table"
        );
        for (name, source) in &sources {
            let grounds = SCENARIO_TREATMENT
                .iter()
                .find(|(listed, _)| listed == name)
                .map(|(_, grounds)| *grounds)
                .unwrap();
            let clears = source.contains("notices::take_down");
            assert_eq!(
                clears,
                grounds == TAKES_THEM_DOWN,
                "{name}: the table and the source disagree about whether it clears the \
                 notice stack (grounds on file: {grounds:?})"
            );
        }
    }
}
