//! The quiesce protocol both engine-attached drivers settle with: a
//! `SafeState` marker armed *ahead of* a script's own keys, whose `echom`
//! is nvim's own proof that the typeahead the marker was queued in front of
//! is empty again.
//!
//! Shared rather than implemented per driver because the two sides of a
//! differential run must settle by the identical rule: a side that decided
//! it was done by a different criterion would read its state at a different
//! point in the same script, and the resulting disagreement would be
//! reported as a divergence in view's own pipeline when it is really a
//! disagreement between two harness clocks. The one thing each driver keeps
//! for itself is what it *does* with a drained event batch ([`Settling::apply_batch`]) --
//! view's `Model`/`Grid` on one side, `RefGrid` on the other -- which is
//! exactly the difference the oracle exists to compare.

use std::time::{Duration, Instant};

use view_core::events::UiEvent;
use view_engine::handle::EngineHandle;

use crate::OracleError;

/// Vimscript augroup name [`install_hooks`] registers once at spawn time
/// and every [`arm`] call replaces the contents of; kept as a named
/// constant so the setup command and the per-call marker command cannot
/// drift apart into two different group names.
const QUIESCE_AUGROUP: &str = "ViewOracleQuiesce";

/// Wraps `cmd` as a single `<Cmd>...<CR>` key-notation segment: an Ex
/// command executed via `nvim_input` without leaving the current mode or
/// waiting for a reply, the mechanism every command in this protocol rides
/// on (see [`settle`] for why this, and not `nvim_command`/`nvim_eval`, is
/// what proves ordering).
fn cmd_key(cmd: &str) -> String {
    format!("<Cmd>{cmd}<CR>")
}

/// Builds the `<Cmd>`-wrapped arm command for marker prefix `marker`: it
/// replaces [`QUIESCE_AUGROUP`]'s contents with a hook that `echom`s the
/// marker, the mode it fired in (char-code encoded, see
/// [`decode_mode_payload`]), and a `:` terminator, every time nvim reaches
/// an idle point.
///
/// `SafeState` is the trigger because it is nvim's own "nothing is pending,
/// going to wait for the user to type a character" signal: per `:help
/// SafeState` it does not fire while there is typeahead, an operator is
/// pending, a register was entered with `"`, a mapping is executing, or
/// Insert/Command-line completion is active. Armed ahead of a script that
/// [`arm_and_input`] then puts into that same typeahead in one step, it
/// cannot fire until the script's last key has been consumed.
///
/// `++once`, so the hook publishes exactly one echo and unregisters
/// itself. A hook left armed would re-fire on every subsequent idle point,
/// and this protocol's own fast probe polls hard enough to manufacture
/// those: each `nvim_get_mode` wakes nvim, which re-enters the safe state
/// and echoes again, a message per poll that resets the silence window for
/// as long as anyone is waiting on it.
///
/// `mode(1)` is captured inside the hook body, at fire time, rather than by
/// the arm command: the arm runs before the script does, so its own mode
/// says nothing about where the script ended up, whereas the fire-time mode
/// is exactly the value [`settle`]'s integrity check holds the fast probe
/// to.
fn arm_command(marker: &str) -> String {
    cmd_key(&format!(
        "autocmd! {QUIESCE_AUGROUP} SafeState * ++once \
         echom '{marker}' . join(str2list(mode(1)), '-') . ':'"
    ))
}

/// Decodes the `-`-joined `str2list` character codes the quiesce marker's
/// `echom` publishes back into the `mode(1)` string the hook captured.
/// Char-code encoding rather than the raw mode string because message
/// rendering is not transparent for every mode name: visual-block is the
/// control character `CTRL-V`, which an `echom` would render as a caret
/// sequence and so never compare equal to the fast probe's raw string. A
/// payload that does not decode is returned as a labeled literal rather
/// than dropped: it can then never equal a real mode name, so a garbled
/// marker fails the round-trip check loudly instead of settling.
fn decode_mode_payload(payload: &str) -> String {
    let decoded: Option<String> = payload
        .split('-')
        .map(|tok| tok.parse::<u32>().ok().and_then(char::from_u32))
        .collect();
    decoded.unwrap_or_else(|| format!("<undecodable marker payload {payload:?}>"))
}

/// `state()` flags that name a half-typed command nvim is holding until
/// another key arrives: a pending operator or a register prefix left dangling
/// by `"` (`o`), and an open Insert-mode completion (`a`). Both suppress
/// `SafeState`, and neither ends on its own -- which is why [`settle`] may
/// treat one as settled instead of waiting out a marker that can never
/// arrive.
///
/// `S` ("not triggering SafeState") is deliberately not among them even
/// though it is set in every one of these states: the pinned nvim also
/// reports a bare `S` for the whole time a command is *running* (live-checked
/// against a trailing `:sleep`, where mode and blocked flag are likewise
/// indistinguishable from an idle session), so settling on it would settle
/// mid-command -- the exact reading this protocol exists to make impossible.
const PENDING_INPUT_FLAGS: &str = "oa";

/// `state()` flags that mean nvim is still working, and so veto the above no
/// matter what else is set: mid-mapping/`feedkeys()`/`:normal` (`m`) means
/// keys are still queued to run, and mid-autocommand (`x`) means a cascade
/// the script started has not finished. Either can coexist with a pending
/// operator -- a script that stalls while one is half-typed shows both -- and
/// there the operator is a state being passed through, not the one the script
/// ended in.
const BUSY_FLAGS: &str = "mx";

/// Renders a fast-probe `(mode, blocking)` pair for
/// [`OracleError::QuiescePerturbed`]'s `observed` field, folding the
/// blocked flag into the one string the variant carries (a blocked
/// key-wait and plain normal mode both report mode `"n"`, so the flag is
/// the only thing distinguishing them in a report line).
fn describe_state(state: Option<&(String, bool)>) -> String {
    match state {
        Some((mode, true)) => format!("{mode} (blocked key-wait)"),
        Some((mode, false)) => mode.clone(),
        None => "<never probed>".to_string(),
    }
}

/// One driver's marker bookkeeping: the sequence counter that keeps a
/// stale echo from satisfying a later call, and the marker a script's own
/// [`arm_and_input`] typed ahead of it that no [`settle`] call has consumed
/// yet.
#[derive(Debug, Default)]
pub(crate) struct QuiesceMarkers {
    next_seq: u64,
    /// `None` means nothing has been typed since the last settle, which is
    /// what lets [`settle`] tell the two arming situations apart: a marker
    /// already riding in front of queued script keys (wait for it) versus
    /// no queued keys at all (arm one now, safe precisely because there is
    /// nothing pending that could swallow the arm keys).
    pending: Option<String>,
}

impl QuiesceMarkers {
    /// Allocates the next sequence-tagged marker prefix. A stale echo from
    /// an earlier call can never satisfy a later one: the sequence number
    /// is part of the text [`settle`] searches the message stream for.
    fn next(&mut self) -> String {
        self.next_seq += 1;
        format!("VIEW_ORACLE_QUIESCE:{}:", self.next_seq)
    }
}

/// What [`settle`] needs from a driver: its connection, its damage source,
/// its marker bookkeeping, and how it applies a drained batch.
///
/// A trait rather than a closure parameter so the whole protocol runs
/// through one `&mut` borrow of the driver: a closure capturing the
/// driver's applier fields while [`settle`] separately held its handle and
/// pump would force every implementor to destructure itself at the call
/// site.
pub(crate) trait Settling {
    /// The RPC connection the protocol types marker keys onto and reads
    /// the fast `nvim_get_mode` probe from.
    fn handle(&self) -> &EngineHandle;

    /// Drains whatever redraw traffic has arrived since the last call, in
    /// flush-bounded batches (see `DamagePump::take_damage`).
    fn take_damage(&self) -> Vec<UiEvent>;

    /// Applies one drained batch to this driver's own grid model. Called
    /// for every batch regardless of whether it precedes or follows the
    /// marker, and carrying every event the batch arrived with except the
    /// protocol's own marker echo (see [`take_marker_echo`]): a settle
    /// protocol must never cost the caller redraw content the script
    /// produced.
    fn apply_batch(&mut self, events: Vec<UiEvent>);

    /// This driver's marker bookkeeping.
    fn markers(&mut self) -> &mut QuiesceMarkers;
}

/// Pins `timeoutlen`/`updatetime` far outside any run's real duration (a
/// mapping timeout or a `CursorHold` firing mid-script would inject
/// nondeterministic redraw noise into the quiesce silence window) and
/// creates the empty [`QUIESCE_AUGROUP`] every [`arm`] call replaces the
/// contents of. Sent via `nvim_input` (fire-and-forget, no reply awaited)
/// rather than a synchronous `nvim_command` request: everything this
/// protocol depends on is typed through the same typeahead queue real test
/// input rides, and mixing in a request/reply round-trip here would be
/// exactly the ordering hazard [`settle`]'s doc comment explains why to
/// avoid.
///
/// # Errors
///
/// Returns [`OracleError::Engine`] if the setup commands cannot be written
/// to the connection.
pub(crate) fn install_hooks(handle: &EngineHandle) -> Result<(), OracleError> {
    let setup = format!(
        "{}{}{}",
        cmd_key("set timeoutlen=86400000 updatetime=86400000"),
        cmd_key(&format!("augroup {QUIESCE_AUGROUP}")),
        cmd_key("autocmd!"),
    ) + &cmd_key("augroup END");
    handle.input(&setup)?;
    Ok(())
}

/// Types the next marker's arm command and `notation` as one `nvim_input`
/// payload, in that order, and records the marker for the next [`settle`]
/// call to wait on.
///
/// One call, not an arm call followed by an input call: `nvim_input`
/// appends to nvim's input buffer, so two calls leave a window in which
/// nvim can consume the arm command, find nothing else pending, and fire
/// the `SafeState` hook before the script's first key has even arrived -- a
/// marker that proves nothing. Fused into one payload, the arm command is
/// unambiguously ahead of every script key in the same typeahead FIFO: it
/// executes first, registering the hook while the script's keys are still
/// queued behind it (so `SafeState` is suppressed by nvim's own "there is
/// typeahead" rule -- see [`arm_command`]), and the hook can therefore only
/// fire once the last script key has been consumed.
///
/// That ordering is what makes this the alternative to arming *after* a
/// script: a marker typed behind a script that is still draining lands
/// wherever the script's own pending input leaves it -- consumed as an
/// operator's motion, as a `t`/`f` target, or as the register name after
/// `"` -- and no quiet-window heuristic can see the queued keys that would
/// eat it, because nvim neither redraws nor changes mode while it drains
/// typeahead.
///
/// # Contract
///
/// Call only on a session that has already settled (the caller's previous
/// [`settle`] returned `true`). Typed into a session that is *itself*
/// sitting in a pending key-wait, the arm command becomes that wait's
/// continuation exactly as any other key would. Violating this is loud, not
/// silent: the marker either never fires (settle deadline) or fires in a
/// state the fast probe contradicts ([`OracleError::QuiescePerturbed`]).
///
/// # Errors
///
/// Returns [`OracleError::Engine`] if the connection's writer thread has
/// already exited.
pub(crate) fn arm_and_input<S: Settling>(
    session: &mut S,
    notation: &str,
) -> Result<(), OracleError> {
    let marker = session.markers().next();
    let payload = format!("{}{notation}", arm_command(&marker));
    session.handle().feed_keys(&payload)?;
    session.markers().pending = Some(marker);
    Ok(())
}

/// Types a fresh marker's arm command on its own, for the
/// nothing-was-typed case [`settle`] documents. Fresh rather than re-armed:
/// [`arm_command`] replaces the augroup's contents, so an earlier call's
/// `++once` hook cannot fire late and satisfy this one under the wrong
/// sequence number.
fn arm<S: Settling>(session: &mut S) -> Result<String, OracleError> {
    let marker = session.markers().next();
    session.handle().input(&arm_command(&marker))?;
    Ok(marker)
}

/// Reads nvim's own `state()` and reports whether the session is parked in a
/// half-typed command it will hold until another key arrives (see
/// [`PENDING_INPUT_FLAGS`] and [`BUSY_FLAGS`] for the exact reading).
///
/// The counterpart to the fast probe's blocked flag, for the states that
/// flag cannot see. `nvim_get_mode` reports `blocking = true` only for a wait
/// it entered from a *complete* command (`f`'s character argument, a
/// hit-enter prompt); a register prefix left dangling by `"` reports plain
/// unblocked normal mode while `SafeState` stays suppressed by that same
/// prefix -- so the marker can never arrive, the fast probe sees nothing to
/// wait on, and the round would die on the deadline with the session
/// perfectly healthy underneath.
///
/// An `nvim_eval` and not a fast probe, so this is only sound where
/// [`settle`] calls it: on a session the fast probe just reported unblocked,
/// which is exactly where a deferred request is answered. Asked of a blocked
/// session it would wait for the key that never comes.
///
/// # Errors
///
/// Returns [`OracleError::Engine`] if the evaluation fails or times out --
/// never folded into a `false`, which would be indistinguishable from a
/// healthy session that simply is not parked.
fn parked_awaiting_input<S: Settling>(session: &mut S) -> Result<bool, OracleError> {
    Ok(reads_as_parked(&session.handle().eval_str("state()")?))
}

/// Reads one `state()` result: parked if it names a pending-input reason and
/// nothing that is still running. Split from [`parked_awaiting_input`] so the
/// reading itself is testable without a live session -- the flag set is the
/// part that decides whether a round settles or dies on the deadline.
fn reads_as_parked(flags: &str) -> bool {
    !flags.chars().any(|flag| BUSY_FLAGS.contains(flag))
        && flags.chars().any(|flag| PENDING_INPUT_FLAGS.contains(flag))
}

/// Waits until everything typed into `session` has been fully processed,
/// using nvim's own idle signal rather than any RPC-reply ordering. An
/// `nvim_eval`/`nvim_command` round-trip cannot stand in for it twice over:
/// it only proves the channel consumed prior messages, not that the
/// *processing* those messages queued (redraw bursts, autocmd cascades) has
/// finished, and a deferred request is free to be serviced *ahead* of typed
/// keys still sitting in the input buffer, since `nvim_input` queues into
/// nvim's typeahead and returns before any of it is processed.
///
/// Three settle paths, because a script is free to end with nvim waiting for
/// more input, and that wait is a real final state the session must preserve
/// for a parity snapshot rather than disturb:
///
/// - **Blocked waits**, seen by the fast `nvim_get_mode` probe (answered even
///   while nvim is blocked -- see `EngineHandle::get_mode`) as `blocking =
///   true` (a hit-enter prompt, a pending `t`/`f`/`r` character argument) or
///   as a pending operator (mode `no*`): `SafeState` cannot fire in any of
///   them (`:help SafeState` excludes a pending operator, and a blocked wait
///   defers everything non-fast), so no marker can ever arrive and the
///   silence window alone is the settle signal. That is sound because these
///   states cannot coexist with queued input: nvim only waits for a character
///   once every already-queued key has been consumed, so reaching one (and
///   holding it stable through the window below) means the typed script has
///   been fully processed.
/// - **Parked half-typed commands**, which look like an ordinary unblocked
///   mode to the fast probe and are named only by nvim's `state()`: a
///   register prefix left dangling by `"`, an open Insert-mode completion.
///   `SafeState` is suppressed for the same reason the marker would be waited
///   on forever, so once the silence window has closed with no echo,
///   [`parked_awaiting_input`] asks nvim directly whether that is what it is
///   doing, and a yes settles the round on the same footing as a blocked
///   wait.
/// - **Every other state**: the `SafeState` marker protocol.
///   [`arm_and_input`] has already typed a sequence-tagged `++once` hook
///   ahead of the script's own keys, and nvim refuses to fire `SafeState`
///   while there is typeahead, so the hook's `echom` is proof the script's
///   last key has been consumed -- proof this loop only reads, never types,
///   so nothing of this protocol's can land where the script's own pending
///   input could eat it. The echo publishes the `mode(1)` the hook fired
///   in, char-code encoded so control-char modes like visual-block survive
///   message rendering, and terminator-delimited so a payload cut short at
///   a message-chunk boundary can never decode as a shorter mode that
///   prefixes the real one.
///   When nothing has been typed since the last settle -- a startup drain,
///   or a repeat call -- there is no marker riding in front of anything, so
///   one is armed here instead, after a full quiet window. That is safe for
///   exactly the reason arming behind a script was not: there are no queued
///   script keys for the arm keys to land behind, and typing the arm is
///   itself what makes nvim leave and re-enter the safe state the hook
///   waits on.
///
/// A settled marker-path result requires all of: the echo arrived, the fast
/// probe reports exactly the state the echo published (mode *and* blocked
/// flag) at the moment the window closes, and it never left that state in
/// between. A violation fails loudly as [`OracleError::QuiescePerturbed`]
/// rather than settling: a session that moved after proving itself idle
/// still had input pending, so it no longer holds the state the script
/// alone produced.
///
/// In every path the silence window is the backstop against late async
/// bursts (a deferred `timer_start` mapping firing after nvim went idle):
/// any drained event -- and any observed mode/blocked transition, which can
/// occur without a redraw of its own -- resets the window, so quiescence is
/// never declared while a burst is still in flight. The whole wait is
/// bounded by `deadline`; returns `Ok(false)` if it elapses first, whether
/// or not the marker was ever seen.
///
/// # Errors
///
/// - [`OracleError::Engine`] if the fast state probe, the parked-state
///   probe, or the marker's arm `nvim_input` call fails at the RPC layer --
///   surfaced as the RPC error it is, not folded into the deadline's
///   `Ok(false)`, so a broken connection is never misreported as a timeout.
/// - [`OracleError::QuiescePerturbed`] if the marker round-trip failed an
///   integrity check above.
pub(crate) fn settle<S: Settling>(
    session: &mut S,
    silence: Duration,
    deadline: Duration,
) -> Result<bool, OracleError> {
    let start = Instant::now();
    let mut marker = session.markers().pending.take();
    // the mode(1) the hook itself published when it fired
    let mut fired_mode: Option<String> = None;
    let mut quiet_since = Instant::now();
    let mut last_state: Option<(String, bool)> = None;
    let mut last_park_probe: Option<Instant> = None;
    loop {
        let mut events = session.take_damage();
        if !events.is_empty() {
            if fired_mode.is_none() {
                if let Some(prefix) = &marker {
                    fired_mode = take_marker_echo(&mut events, prefix);
                }
            }
            session.apply_batch(events);
            quiet_since = Instant::now();
        }

        let state = session.handle().get_mode()?;
        if last_state.as_ref() != Some(&state) {
            quiet_since = Instant::now();
            last_state = Some(state);
        }
        let awaiting_more_input = last_state
            .as_ref()
            .is_some_and(|(mode, blocking)| *blocking || mode.starts_with("no"));
        let window_elapsed = quiet_since.elapsed() >= silence;

        if let Some(published) = &fired_mode {
            // nvim proved its typeahead empty when the hook fired, so
            // anything that moves the session afterwards was input this
            // protocol cannot account for -- a comparison against the
            // resulting state would be measuring that input, not the
            // script's
            if last_state.as_ref() != Some(&(published.clone(), false)) {
                return Err(OracleError::QuiescePerturbed {
                    armed: published.clone(),
                    observed: describe_state(last_state.as_ref()),
                });
            }
            if window_elapsed {
                return Ok(true);
            }
        } else if awaiting_more_input {
            if window_elapsed {
                return Ok(true);
            }
        } else if window_elapsed {
            if marker.is_none() {
                marker = Some(arm(session)?);
            } else if last_park_probe.is_none_or(|at| at.elapsed() >= silence) {
                // rate-limited to the silence window: each probe is an RPC
                // round-trip that wakes nvim, and its answer cannot change
                // without a state change that resets that window anyway
                last_park_probe = Some(Instant::now());
                if parked_awaiting_input(session)? {
                    return Ok(true);
                }
            }
        }

        if start.elapsed() >= deadline {
            return Ok(false);
        }
        std::thread::sleep(Duration::from_millis(1));
    }
}

/// Finds `prefix`'s marker echo in one drained batch, removes that message
/// from the batch, and decodes the mode it published; `None` (batch
/// untouched) if this batch does not carry it.
///
/// Removed rather than passed through to the applier because the echo is
/// this protocol's own message, not the script's: left in the stream, one
/// side of a differential run paints it (view's `Surface` renders a message
/// layer) while the other discards it by construction, and the rows it
/// covers would drop out of every comparison as masked chrome -- the
/// protocol would be buying its settle proof with the coverage the run
/// exists to produce. Every other event, marker batch or not, still reaches
/// the applier.
///
/// Each message's chunks are reassembled before searching, and the payload
/// must reach its `:` terminator: a payload cut at a chunk or truncation
/// boundary could otherwise decode as a shorter mode that is a prefix of
/// the real one ("n" out of "no") and falsely pass the round-trip check.
fn take_marker_echo(events: &mut Vec<UiEvent>, prefix: &str) -> Option<String> {
    let found = events.iter().enumerate().find_map(|(index, ev)| {
        let UiEvent::MsgShow { content, .. } = ev else {
            return None;
        };
        let full: String = content.iter().map(|(_, text)| text.as_str()).collect();
        let (_, rest) = full.split_once(prefix)?;
        let mode = match rest.split_once(':') {
            Some((payload, _)) => decode_mode_payload(payload),
            None => format!("<unterminated marker payload {rest:?}>"),
        };
        Some((index, mode))
    });
    let (index, mode) = found?;
    events.remove(index);
    Some(mode)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    fn msg(text: &str) -> UiEvent {
        UiEvent::MsgShow {
            kind: String::new(),
            content: vec![(0, text.to_string())],
            replace_last: false,
        }
    }

    #[test]
    fn marker_echo_decodes_the_published_mode_and_leaves_the_batch() {
        let mut events = vec![
            UiEvent::MsgClear,
            msg("VIEW_ORACLE_QUIESCE:3:110-111:"),
            UiEvent::Flush,
        ];
        assert_eq!(
            take_marker_echo(&mut events, "VIEW_ORACLE_QUIESCE:3:"),
            Some("no".to_string())
        );
        assert_eq!(
            events,
            vec![UiEvent::MsgClear, UiEvent::Flush],
            "only the protocol's own echo may be withheld from the applier"
        );
    }

    #[test]
    fn a_different_sequence_number_is_not_this_calls_marker() {
        let mut events = vec![msg("VIEW_ORACLE_QUIESCE:2:110:")];
        assert_eq!(
            take_marker_echo(&mut events, "VIEW_ORACLE_QUIESCE:3:"),
            None
        );
        assert_eq!(events.len(), 1, "a foreign message must reach the applier");
    }

    #[test]
    fn an_unterminated_payload_never_decodes_as_a_real_mode() {
        let mut events = vec![msg("VIEW_ORACLE_QUIESCE:1:110-11")];
        let decoded = take_marker_echo(&mut events, "VIEW_ORACLE_QUIESCE:1:").unwrap();
        assert!(
            decoded.starts_with("<unterminated"),
            "expected a labeled literal, got {decoded:?}"
        );
    }

    #[test]
    fn arm_command_carries_the_marker_and_captures_the_fire_time_mode() {
        let arm = arm_command("VIEW_ORACLE_QUIESCE:7:");
        assert!(arm.starts_with("<Cmd>") && arm.ends_with("<CR>"), "{arm}");
        assert!(arm.contains("SafeState * ++once"), "{arm}");
        assert!(arm.contains("str2list(mode(1))"), "{arm}");
        assert!(arm.contains("VIEW_ORACLE_QUIESCE:7:"), "{arm}");
    }

    #[test]
    fn a_half_typed_command_reads_as_parked() {
        // the pinned nvim's own answers: `"0` and a pending operator both
        // report `oS`, an open Insert-mode completion reports `aS`, and the
        // fast probe calls every one of them unblocked
        assert!(reads_as_parked("oS"));
        assert!(reads_as_parked("aS"));
        assert!(reads_as_parked("oSc"), "a live callback flag is not a veto");
    }

    #[test]
    fn a_still_running_script_never_reads_as_parked() {
        assert!(!reads_as_parked(""), "an idle session is not parked");
        assert!(
            !reads_as_parked("S"),
            "a bare S is what a running :sleep reports"
        );
        assert!(!reads_as_parked("c"), "a callback alone is not a key-wait");
        assert!(
            !reads_as_parked("s"),
            "a scrolled message is not a key-wait"
        );
        assert!(!reads_as_parked("mS"), "queued keys are still to come");
        assert!(
            !reads_as_parked("moS"),
            "a busy flag vetoes a pending-input flag seen in passing"
        );
        assert!(
            !reads_as_parked("xoS"),
            "an autocmd cascade is still running"
        );
    }

    #[test]
    fn each_marker_gets_its_own_sequence_number() {
        let mut markers = QuiesceMarkers::default();
        assert_ne!(markers.next(), markers.next());
    }
}
