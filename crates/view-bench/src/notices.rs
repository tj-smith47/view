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

use std::time::Duration;

use view_core::native::palette::MESSAGE_HISTORY_TITLE;
use view_core::native::toast::{DEFAULT_CAPACITY, TRANSIENT_TOAST_TIMEOUT};

use crate::sampling::Side;
use crate::session::{BenchSession, SettleBound};
use crate::BenchError;

/// The quiet span a screen must hold before a cell may call it clear of
/// notices: past the idle timeout a transient toast expires on, so the
/// settle outlasts a toast that took the top slot at the instant the quiet
/// clock last restarted.
pub const CLEAR_QUIET: Duration =
    TRANSIENT_TOAST_TIMEOUT.saturating_add(Duration::from_millis(500));

/// The quiet span a repaint gets to reach the screen: the message history
/// arriving, or the notices it took down leaving. Both are one round trip
/// to nvim and back with nothing timed behind them.
const REPAINT_QUIET: Duration = Duration::from_millis(500);

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

/// The ex-command that opens the message history.
#[must_use]
pub fn history_command() -> String {
    format!(":View {HISTORY_FEATURE} {HISTORY_VERB}\r")
}

/// Clears view's notice stack off `session`: waits out the toasts that
/// retire on a timer, then walks the message history taking down the
/// notices that never do.
///
/// `Side::Nvim` answers immediately: a bare editor draws no notice over its
/// buffer, and the overlay keys below would be a delete and a motion in its
/// buffer instead.
///
/// # Errors
///
/// Returns [`BenchError::Desync`] when the message history never reaches the
/// screen -- which is also the guard that keeps the walk's keys off the
/// buffer -- or when the screen never goes quiet within `deadline`.
pub fn take_down(
    session: &mut BenchSession,
    side: Side,
    deadline: Duration,
) -> Result<(), BenchError> {
    if side == Side::Nvim {
        return Ok(());
    }
    // with no modal open this is the way out of nvim's own standing wire
    // errors, and it leaves whatever mode the session was in so the
    // ex-command below is typed at the command line rather than into a
    // buffer
    session.send(b"\x1b")?;
    // the transients go first and they go by waiting: `d` retracts a family
    // and a toast on a timer carries none, so a stack still draining is a
    // stack that will paint a box over the overlay this is about to read
    if !session.settle(SettleBound {
        quiet: CLEAR_QUIET,
        deadline,
    }) {
        return Err(BenchError::Desync {
            context: format!(
                "the screen never held still for {CLEAR_QUIET:?} within {deadline:?}, so the \
                 toasts on a timer were still draining; screen:\n{}",
                session.screen_text()
            ),
        });
    }
    session.send(history_command().as_bytes())?;
    let opened = session.settle(SettleBound {
        quiet: REPAINT_QUIET,
        deadline,
    }) && session.screen_text().contains(MESSAGE_HISTORY_TITLE);
    if !opened {
        return Err(BenchError::Desync {
            context: format!(
                "the message history never reached the screen within {deadline:?}, so the keys \
                 that take a standing notice down would have edited the buffer instead; \
                 screen:\n{}",
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
        quiet: REPAINT_QUIET,
        deadline,
    }) {
        return Err(BenchError::Desync {
            context: format!(
                "the screen never held still for {REPAINT_QUIET:?} within {deadline:?} after the \
                 notice takedown; screen:\n{}",
                session.screen_text()
            ),
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use view_core::native::mappings;

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

    /// The constant this module exists for. A quiet span at or under the
    /// idle timeout is satisfied by the very interval a promoted toast is
    /// counting down through, which is how a two-second settle came to
    /// return with a notice standing over the cells about to be sampled.
    #[test]
    fn the_clear_quiet_outlasts_the_idle_timeout_a_transient_toast_expires_on() {
        assert!(
            CLEAR_QUIET > TRANSIENT_TOAST_TIMEOUT,
            "{CLEAR_QUIET:?} does not outlast {TRANSIENT_TOAST_TIMEOUT:?}"
        );
    }

    /// The walk covers the history ring, so no standing notice can sit
    /// past the last entry it reaches.
    #[test]
    fn the_walk_reaches_every_entry_the_history_ring_can_hold() {
        let keys = WALK_STEP.repeat(DEFAULT_CAPACITY);
        assert_eq!(
            keys.iter().filter(|byte| **byte == b'd').count(),
            DEFAULT_CAPACITY
        );
        assert_eq!(
            keys.iter().filter(|byte| **byte == b'j').count(),
            DEFAULT_CAPACITY
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
             which is what keeps a corner box from hiding it",
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
