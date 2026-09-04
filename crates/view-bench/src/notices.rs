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

/// How long an `<Esc>` needs to itself before the keys behind it are sent:
/// long enough that the terminal delivers it in a read of its own.
///
/// The seam is the measured editor's own input decoder, not the engine's:
/// an `<Esc>` and the byte behind it arriving in ONE read are decoded as
/// that key Alt-modified (`view-tui/src/keys.rs`, `alt_key`), so the
/// `<Esc>` never becomes a mode change and the ex-command behind it types
/// itself into the buffer. Two writes a scheduling quantum apart cannot
/// land in one read, which is all this has to buy; it is not a wait on any
/// configured timeout, so no fixture can shorten what it is worth.
///
/// `CTRL-\ CTRL-N` -- the mode change with no `ESC` prefix at all -- is
/// the sequence with no seam to settle, and both of its bytes now reach
/// the engine under the names nvim gives them (`view-tui/src/keys.rs`,
/// `plain_key`). This settle is what the `<Esc>`-led form every scenario
/// here types still needs.
const MODE_SETTLE: Duration = Duration::from_millis(150);

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
/// Read off the program under measurement, not off which side of a pair
/// the spec occupies: a bare editor can hold the measured side, and a
/// takedown keyed on the side would type view's own command at an editor
/// that has never heard of it.
///
/// [`SpawnSpec::measured_program`] rather than the program spawned, and
/// never the arguments: a spawn wrapped in a shell shim runs `sh`, so the
/// spawned program answers no for a view session, while an argument scan
/// answers yes for any spawn that merely names the binary -- including a
/// bare editor handed view's own scratch file.
#[must_use]
pub fn runs_view(spec: &SpawnSpec) -> bool {
    spec.measured_program
        .as_ref()
        .unwrap_or(&spec.program)
        .file_stem()
        .is_some_and(|stem| stem == std::ffi::OsStr::new(VIEW_PROGRAM))
}

/// Clears view's notice stack off `session`: waits for view's own bootstrap
/// to answer, waits out the toasts that retire on a timer, then walks the
/// message history taking down the notices that never do.
///
/// `repaint_quiet` is the caller's own quiet span for one round trip to
/// nvim and back, floored at [`REPAINT_QUIET`]; a caller measuring over an
/// injected-latency transport passes the widened span it already uses for
/// its startup settle. The wait for view's own bootstrap backs off from
/// that span, since every attempt made before the bootstrap answers leaves
/// an error in the engine's message stream for the walk below to clear.
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
    // ex-command below is typed at the command line rather than into the
    // buffer
    leave_mode(session)?;
    // A quiet screen is not a started view. `:View` is created by the
    // registration view runs at `VimEnter`, and nvim reaches `VimEnter`
    // only after sourcing a config that can spend seconds in Lua without
    // painting anything -- so a settle is satisfied by the file nvim
    // already drew, and the command sent on the strength of it comes back
    // `E492`. The overlay arriving is the one observable that says the
    // registration has run; ask for it until it does.
    let mut attempt_quiet = repaint_quiet;
    while !open_history(session, attempt_quiet, until)? {
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
        leave_mode(session)?;
        // every attempt that lands before the registration exists leaves
        // one more `E492` in the engine's own message stream, which view
        // surfaces as one more notice for the walk below to take down;
        // backing off keeps a cold start's wait a handful of attempts
        // rather than one per repaint window
        attempt_quiet = backed_off(attempt_quiet, repaint_quiet);
    }
    leave_mode(session)?;
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
    leave_mode(session)?;
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

/// The quiet span the next bootstrap attempt waits for: the one just
/// spent, doubled, and held at the span a draining toast stack needs.
///
/// Held no lower than `floor`, the caller's own span for one round trip:
/// on a latency-injected transport that span is the larger of the two, and
/// a ceiling applied over it would ask for less quiet than the transport
/// needs -- which is satisfied by the static screen standing before the
/// reply lands, the class of bug [`REPAINT_QUIET`] documents.
fn backed_off(attempt_quiet: Duration, floor: Duration) -> Duration {
    attempt_quiet.saturating_mul(2).min(CLEAR_QUIET.max(floor))
}

/// Leaves whatever mode the session is in, giving the `<Esc>` the read of
/// its own that [`MODE_SETTLE`] documents, so the keys behind it are read
/// as their own.
///
/// # Errors
///
/// Returns [`BenchError`] only when the write to the pty fails.
fn leave_mode(session: &mut BenchSession) -> Result<(), BenchError> {
    session.send(b"\x1b")?;
    std::thread::sleep(MODE_SETTLE);
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
            measured_program: None,
        }
    }

    /// The takedown's whole subject is view's own notices, so what it asks
    /// is which program a session measures. Every spec this crate builds
    /// for a view session is asked here through the builder that produces
    /// it, never through a path written out beside it: the shim answering
    /// for the shell it spawns rather than the binary it wraps is what
    /// silenced the takedown on every taps row once already, and a path
    /// literal cannot see that.
    #[cfg(unix)]
    #[test]
    fn every_view_spawn_this_crate_builds_answers_for_its_notices() {
        use crate::scenarios::{echo_speculated_rtt, taps};

        let scratch = view_test_support::ScratchDir::new("notices-builders").unwrap();
        let tap_path = scratch.path().join("tap.fifo");

        let shimmed = taps::shim_taps_spec(spec_running("target/taps/release/view"), &tap_path);
        assert_eq!(shimmed.program, std::path::PathBuf::from("sh"));
        assert!(
            runs_view(&shimmed),
            "the tap shim hides the measured program behind the shell it execs from"
        );
        assert!(
            !runs_view(&taps::shim_taps_spec(spec_running("nvim"), &tap_path)),
            "a bare engine under the same shim is still a bare engine"
        );

        // the tier spec is built by its own arming step, which needs a
        // Python interpreter for the relay: a host without one cannot run
        // the leg this row belongs to either, and failing here would
        // charge that absence to a predicate that has no opinion on it
        if let Some(reason) = echo_speculated_rtt::delay_relay_unavailable_reason() {
            eprintln!("the RTT tier spec is not built on this host: {reason}");
            return;
        }
        let rtt = echo_speculated_rtt::remote_rtt_view_spec(
            scratch.path().to_path_buf(),
            Vec::new(),
            std::path::Path::new("target/taps/release/view"),
            std::path::Path::new("nvim"),
            std::path::Path::new("scratch.txt"),
            &tap_path,
            0,
        )
        .expect("the committed delay relay and its stub client arm the tier spec");
        assert!(
            runs_view(&rtt),
            "an RTT tier measures the same editor through a relay"
        );
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

    /// The span a caller passes is the one its own transport needs for a
    /// round trip, and the tiered RTT leg passes spans larger than the
    /// ceiling the retry holds at: a backoff that applied that ceiling
    /// over the caller's span would ask a 300ms tier for less quiet than
    /// one trip takes, and the static screen standing before the reply
    /// lands would satisfy it.
    #[cfg(unix)]
    #[test]
    fn the_bootstrap_backoff_never_asks_for_less_quiet_than_the_caller_did() {
        use crate::scenarios::{echo::DEFAULT_STARTUP_QUIET, echo_speculated_rtt};

        for rtt_ms in echo_speculated_rtt::RTT_TIERS_MS {
            let floor = echo_speculated_rtt::widened_for_tier(DEFAULT_STARTUP_QUIET, rtt_ms)
                .max(REPAINT_QUIET);
            let mut quiet = floor;
            for attempt in 1..=8u32 {
                quiet = backed_off(quiet, floor);
                assert!(
                    quiet >= floor,
                    "attempt {attempt} at the {rtt_ms}ms tier waits {quiet:?}, under the \
                     {floor:?} the caller asked for"
                );
                assert!(
                    quiet <= CLEAR_QUIET.max(floor),
                    "attempt {attempt} at the {rtt_ms}ms tier waits {quiet:?}, past the span a \
                     draining toast stack needs"
                );
            }
        }
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

    /// Every `.rs` file under `root`, as its path relative to `root` and
    /// its text, sorted by path.
    fn rust_sources(root: &std::path::Path) -> Vec<(String, String)> {
        let mut sources: Vec<(String, String)> = Vec::new();
        let mut stack = vec![root.to_path_buf()];
        while let Some(dir) = stack.pop() {
            for entry in std::fs::read_dir(&dir).unwrap() {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    // a build directory left beside a crate holds
                    // generated code, which nobody writes a literal in
                    if path.file_name().is_some_and(|name| name == "target") {
                        continue;
                    }
                    stack.push(path);
                    continue;
                }
                if path.extension().is_none_or(|ext| ext != "rs") {
                    continue;
                }
                let name = path
                    .strip_prefix(root)
                    .unwrap()
                    .to_string_lossy()
                    .replace('\\', "/");
                sources.push((name, std::fs::read_to_string(&path).unwrap()));
            }
        }
        sources.sort_by(|a, b| a.0.cmp(&b.0));
        sources
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
    ///
    /// A census of what each source intends, which is all a source walk can
    /// see: that a call actually clears anything is a property of the spec
    /// handed to it, pinned by
    /// `every_view_spawn_this_crate_builds_answers_for_its_notices` and its
    /// mirror over the harness's own builders.
    #[test]
    fn every_scenario_source_is_classified_for_the_notice_stack() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/scenarios");
        let sources = rust_sources(&root);

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
                "{name}: the table and the source disagree about whether it asks for the \
                 notice stack to come down (grounds on file: {grounds:?})"
            );
        }
    }
    /// Every `SpawnSpec` literal in the tree, against the answer it writes
    /// for [`SpawnSpec::measured_program`] and why that answer is the
    /// right one.
    ///
    /// Rows are `(path under `crates/`, answer, grounds)`, in source order
    /// within a file.
    const SPAWN_SPEC_LITERALS: &[(&str, &str, &str)] = &[
        (
            "view-bench/src/notices.rs",
            "None",
            "a test spec whose program is the one it measures",
        ),
        (
            "view-bench/src/remote_ui.rs",
            "..nvim.clone()",
            "the pty-hosted client re-points the spawn it derives from at a socket; what runs \
             is unchanged",
        ),
        (
            "view-bench/src/remote_ui.rs",
            "None",
            "a test spec whose program is the one it measures",
        ),
        (
            "view-bench/src/remote_ui.rs",
            "Some(PathBuf::from(\"target/release/view\"))",
            "a test base shaped like a wrapped spawn, so a derivation that dropped the record \
             fails there",
        ),
        (
            "view-bench/src/scenarios/echo_speculated_rtt.rs",
            "None",
            "the tier's inner spawn names the binary it runs; the tap shim it is handed to \
             records that binary as it moves it into the shell's argv",
        ),
        (
            "view-bench/src/scenarios/flood.rs",
            "None",
            "a test spec naming a program that cannot be spawned at all",
        ),
        (
            "view-bench/src/scenarios/picker.rs",
            "..spec.clone()",
            "a corpus root moves where a spawn starts, not what it runs",
        ),
        (
            "view-bench/src/scenarios/picker.rs",
            "Some(PathBuf::from(\"target/release/view\"))",
            "a test base shaped like a wrapped spawn, so a derivation that dropped the record \
             fails there",
        ),
        (
            "view-bench/src/scenarios/remote_memory.rs",
            "None",
            "a test spec whose program is the one it measures",
        ),
        (
            "view-bench/src/scenarios/supervision.rs",
            "Some(PathBuf::from(\"view\"))",
            "a test base shaped like a wrapped spawn, so a derivation that dropped the record \
             fails there",
        ),
        (
            "view-bench/src/scenarios/taps/mod.rs",
            "Some(measured_program)",
            "the one wrapper in the tree: it spawns a shell and measures the binary that \
             shell execs",
        ),
        (
            "view-bench/src/scenarios/taps/mod.rs",
            "None",
            "the pty floor control spawns a shell that never becomes an editor, so there is \
             no view under it to measure",
        ),
        (
            "view-bench/tests/remote_ui.rs",
            "None",
            "a bare engine spawned as itself",
        ),
        (
            "view-harness/src/bin/bench/cell_world.rs",
            "None",
            "a matrix cell's view side spawns the binary it measures",
        ),
        (
            "view-harness/src/bin/bench/cell_world.rs",
            "None",
            "a matrix cell's baseline spawns the engine it measures",
        ),
        (
            "view-harness/src/bin/bench/remote_rows.rs",
            "None",
            "a remote row spawns view directly, reaching its engine over the transport",
        ),
        (
            "view-harness/src/bin/rtt_acceptance/run.rs",
            "None",
            "the tier's baseline spawns the engine it measures",
        ),
        (
            "view-harness/tests/user_fixture.rs",
            "None",
            "a side spec whose program the caller fills in with the binary it measures",
        ),
    ];

    /// The answer each `SpawnSpec` literal in `source` writes for
    /// `measured_program`, in source order: the field's own expression, or
    /// the struct-update tail the literal inherits it through.
    fn measured_program_answers(source: &str) -> Vec<String> {
        // spelled in two pieces so the walk does not find itself
        let needle = concat!("SpawnSpec", " {");
        let mut answers = Vec::new();
        let mut cursor = 0;
        while let Some(offset) = source[cursor..].find(needle) {
            let at = cursor + offset;
            cursor = at + needle.len();
            // a return type, the definition and an impl block are written
            // the same way a literal is, path qualifier included
            let mut head = source[..at].trim_end();
            while let Some(qualified) = head.strip_suffix("::") {
                head = qualified
                    .trim_end_matches(|c: char| c.is_alphanumeric() || c == '_')
                    .trim_end();
            }
            if ["->", "struct", "impl", "for", "let"]
                .iter()
                .any(|opener| head.ends_with(opener))
            {
                continue;
            }
            let open = cursor - 1;
            let mut depth = 0usize;
            let mut close = open;
            for (offset, ch) in source[open..].char_indices() {
                match ch {
                    '{' => depth += 1,
                    '}' => {
                        depth -= 1;
                        if depth == 0 {
                            close = open + offset;
                            break;
                        }
                    }
                    _ => {}
                }
            }
            let mut answer = String::from("(no answer)");
            for field in top_level_fields(&source[open + 1..close]) {
                if let Some(expression) = field.strip_prefix("measured_program:") {
                    answer = expression.trim().to_string();
                    break;
                }
                if field.starts_with("..") {
                    answer = field;
                }
            }
            answers.push(answer);
            cursor = close;
        }
        answers
    }

    /// The fields of one struct literal's body, whitespace collapsed, split
    /// on the commas that are not inside a nested expression.
    fn top_level_fields(body: &str) -> Vec<String> {
        let mut fields = Vec::new();
        let mut depth = 0i32;
        let mut current = String::new();
        for ch in body.chars() {
            match ch {
                '{' | '[' | '(' => depth += 1,
                '}' | ']' | ')' => depth -= 1,
                ',' if depth == 0 => {
                    fields.push(std::mem::take(&mut current));
                    continue;
                }
                _ => {}
            }
            current.push(ch);
        }
        fields.push(current);
        fields
            .iter()
            .map(|field| field.split_whitespace().collect::<Vec<_>>().join(" "))
            .filter(|field| !field.is_empty())
            .collect()
    }

    /// Every reassignment of a spawn's `program` in the tree, against the
    /// expression it assigns and why that spawn still measures what it
    /// says it does.
    ///
    /// A literal is not the only way to build a spawn whose program is not
    /// what it measures: a builder can take a finished spec and move its
    /// program into a wrapper's arguments, which reaches no literal at all.
    const PROGRAM_ASSIGNMENTS: &[(&str, &str, &str)] = &[
        (
            "view-harness/tests/user_fixture.rs",
            "PathBuf::from(\"nvim\")",
            "the side helper leaves the program empty; the baseline fills in the engine it \
             measures",
        ),
        (
            "view-harness/tests/user_fixture.rs",
            "view_bin",
            "the same helper, filled in with the binary the measured side runs",
        ),
    ];

    /// The expression each `program` reassignment in `source` writes, in
    /// source order.
    ///
    /// Statements, not physical lines: rustfmt breaks a long assignment
    /// after the `=`, or inside the call on its right, and a line-at-a-time
    /// scan then reads only the fragment that shares the needle's own line
    /// -- an empty expression for the first shape, a truncated one for the
    /// second, neither of which an author can write a census row against.
    fn program_assignments(source: &str) -> Vec<String> {
        // a statement ends at the first `;` the accumulation holds, which a
        // `;` inside a string literal would end early: adequate for the
        // sources this walks, where no program expression carries one
        let mut statements: Vec<String> = Vec::new();
        let mut current = String::new();
        for line in source.lines().map(str::trim) {
            if !current.is_empty() {
                current.push(' ');
            }
            current.push_str(line);
            if current.contains(';') {
                statements.push(std::mem::take(&mut current));
            }
        }
        statements.push(current);
        statements
            .iter()
            // spelled in two pieces so the walk does not find itself
            .filter_map(|line| line.split_once(concat!(".program", " =")))
            // a comparison reads the field, it does not move a program
            .filter(|(_, assigned)| !assigned.starts_with('='))
            .filter_map(|(_, assigned)| assigned.split(';').next())
            .map(|assigned| assigned.split_whitespace().collect::<Vec<_>>().join(" "))
            .collect()
    }

    /// rustfmt breaks a long assignment in two places, and the census read
    /// neither: after the `=` it recorded an empty expression, and inside the
    /// call on its right a truncated one.
    ///
    /// The needle is spelled in pieces for the same reason the walk spells
    /// its own that way -- a contiguous copy in this file would make the
    /// fixture a census row of its own.
    #[test]
    fn the_census_reads_an_assignment_rustfmt_wrapped() {
        let needle = concat!("spec.", "program", " =");
        let after_the_equals = format!("{needle}\n    PathBuf::from(\"wrapped\");\n");
        let inside_the_call = format!("{needle} PathBuf::from(\n    \"wrapped\",\n);\n");
        assert_eq!(
            program_assignments(&after_the_equals),
            vec![String::from("PathBuf::from(\"wrapped\")")],
            "a wrap after the `=` leaves the expression on a line of its own, and a per-line \
             scan reads an empty assignment nobody can write a census row against"
        );
        assert_eq!(
            program_assignments(&inside_the_call),
            vec![String::from("PathBuf::from( \"wrapped\", )")],
            "a wrap inside the call leaves the expression split across lines, and a per-line \
             scan reads only the opening of it"
        );
    }

    /// Compares one census against what the tree actually builds, per file
    /// so a mismatch names the file whose author has to write the answer
    /// down.
    fn compare_census(found: &[(String, String)], listed: &[(&str, &str, &str)], subject: &str) {
        for (name, _, grounds) in listed {
            assert!(
                !grounds.is_empty(),
                "{name}: a listed entry has no grounds for the answer it writes"
            );
        }
        let mut files: Vec<&str> = found
            .iter()
            .map(|(name, _)| name.as_str())
            .chain(listed.iter().map(|(name, _, _)| *name))
            .collect();
        files.sort_unstable();
        files.dedup();
        for name in files {
            let built: Vec<&str> = found
                .iter()
                .filter(|(file, _)| file == name)
                .map(|(_, answer)| answer.as_str())
                .collect();
            let census: Vec<&str> = listed
                .iter()
                .filter(|(file, _, _)| *file == name)
                .map(|(_, answer, _)| *answer)
                .collect();
            assert_eq!(
                built, census,
                "{name}: {subject} here and the census rows for this file \
                 disagree -- a spawn whose program is not the binary it measures has to record \
                 that binary, or nothing downstream recognises the session as view"
            );
        }
    }

    /// Walks what the tree builds rather than the builders anybody
    /// remembered: `measured_program` is an `Option`, so a spawn that moves
    /// its program into a wrapper's argv and answers `None` compiles,
    /// passes every builder pin, and leaves the takedown inert on whatever
    /// rows it serves. Both ways of building one -- a literal, and a
    /// finished spec whose program is reassigned -- fail here until their
    /// author writes the answer down.
    #[test]
    fn every_spawn_this_tree_builds_is_listed_against_what_it_measures() {
        let crates = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .to_path_buf();
        let mut literals: Vec<(String, String)> = Vec::new();
        let mut assignments: Vec<(String, String)> = Vec::new();
        for (name, source) in rust_sources(&crates) {
            for answer in measured_program_answers(&source) {
                literals.push((name.clone(), answer));
            }
            for assigned in program_assignments(&source) {
                assignments.push((name.clone(), assigned));
            }
        }
        compare_census(&literals, SPAWN_SPEC_LITERALS, "the SpawnSpec literals");
        compare_census(
            &assignments,
            PROGRAM_ASSIGNMENTS,
            "the reassignments of a spawn's program",
        );
    }
}
