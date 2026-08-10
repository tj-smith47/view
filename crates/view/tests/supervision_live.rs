//! Live-nvim wire facts behind the busy modal's `Interrupt` choice.
//!
//! The choice sends one keystroke and claims it reaches an engine that has
//! stopped answering. Whether it does is a property of the pinned engine and
//! not of view: nvim notices an interrupt only where its own break check
//! runs, and the two shapes of synchronous work an engine can be stuck in
//! differ on exactly that point. Nothing headless can answer it, and
//! `INTERRUPT_NOTATION`'s doc comment states the answer as fact, so the
//! answer is pinned here against a real engine rather than assumed.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use std::time::{Duration, Instant};

use view_core::model::Model;
use view_core::msg::{Effect, Key, Msg, RpcCall};
use view_core::native::supervision::{WedgeKind, ENGINE_BUSY_MODAL_THRESHOLD, INTERRUPT_NOTATION};
use view_core::update::update;

/// How long the engine is told to stay busy for. Far longer than any budget
/// below, so a probe that answers cannot be the busy work finishing on its
/// own. Never actually waited out: the engine is killed with the session.
const BUSY_SECS: u64 = 60;

/// How long an answer that proves the interrupt landed is waited for. An
/// interrupted engine answers in about a millisecond; this is slack for a
/// loaded box, still an order of magnitude short of `BUSY_SECS`.
const INTERRUPTED: Duration = Duration::from_secs(5);

/// How long the engine is given to get inside the loop it was told to run.
/// A budget, not a wait: the wait below ends the moment the engine actually
/// stops answering, so a fast host spends milliseconds here and a loaded one
/// spends what it needs.
const ENTERS_LOOP: Duration = Duration::from_secs(30);

/// A live engine with a UI attached and nothing in its config, put to work on
/// `busy` and blocked before this returns.
///
/// The readiness condition is the engine's own silence, never a sleep: a
/// fixed pause races the engine on any loaded host, and losing that race
/// makes every assertion below vacuous in the passing direction -- an engine
/// still reading the command line answers nothing either.
fn busy_engine(label: &str, busy: &str) -> view_engine::process::Engine {
    let dir = common::fixture(&format!("supervision-live-{label}"), "");
    let engine = common::spawn_with_drained_pump(common::isolated_reading(&dir.join("init.lua")));
    engine.handle.ui_attach(80, 24).unwrap();
    // typed rather than requested: `nvim_command` is a blocking request, and
    // a caller waiting on the reply to work that never returns could not go
    // on to interrupt it
    engine.handle.input(busy).unwrap();

    let deadline = Instant::now() + ENTERS_LOOP;
    // an engine that has not yet entered the loop answers this promptly;
    // one that has answers it not at all, which is the whole signal
    while engine.handle.get_mode().is_ok() {
        assert!(
            Instant::now() < deadline,
            "{label}: the engine kept answering for {ENTERS_LOOP:?} after being              told to run a {BUSY_SECS}s loop, so it never entered it"
        );
    }
    engine
}

/// A Vimscript `while`, whose break check pumps the event loop and so sees
/// input that arrived over RPC.
fn vimscript_loop() -> String {
    format!(
        ":let g:t=reltime() | while {BUSY_SECS} - reltimefloat(reltime(g:t)) > 0 | endwhile<CR>"
    )
}

/// A Lua `while`, which pumps nothing.
fn lua_loop() -> String {
    format!(
        ":lua local t=vim.uv.now() while {} - (vim.uv.now()-t) > 0 do end<CR>",
        BUSY_SECS * 1000
    )
}

/// The interrupt reaching the wedge class it can reach, with a control that
/// rules out the loop simply having finished: the identical session left
/// uninterrupted still owes its answer when the budget runs out.
#[test]
fn the_interrupt_notation_aborts_an_engine_stuck_in_a_vimscript_loop() {
    let interrupted = busy_engine("vim-interrupted", &vimscript_loop());
    interrupted.handle.input(INTERRUPT_NOTATION).unwrap();
    let start = Instant::now();
    let answered = interrupted.handle.eval_str("1");
    assert!(
        answered.is_ok(),
        "the interrupt did not abort a Vimscript loop: {answered:?}"
    );
    assert!(
        start.elapsed() < INTERRUPTED,
        "the engine answered only after {:?}, which is not an abort",
        start.elapsed()
    );

    let control = busy_engine("vim-control", &vimscript_loop());
    let answered = control.handle.eval_str("1");
    assert!(
        answered.is_err(),
        "the same loop answered with no interrupt sent, so the test above proves \
         nothing about the interrupt: {answered:?}"
    );
}

/// The wedge class the interrupt cannot reach, and the reason the modal
/// offers `Restart` beside it rather than presenting an interrupt as the
/// answer. The liveness probe goes unanswered here too, which is what makes
/// this shape detectable at all.
#[test]
fn a_synchronous_lua_loop_answers_neither_the_liveness_probe_nor_the_interrupt() {
    let engine = busy_engine("lua-interrupted", &lua_loop());

    let probed = engine.handle.get_mode();
    assert!(
        probed.is_err(),
        "the liveness probe answered during a synchronous Lua loop, so this wedge \
         would never be detected: {probed:?}"
    );

    engine.handle.input(INTERRUPT_NOTATION).unwrap();
    let answered = engine.handle.eval_str("1");
    assert!(
        answered.is_err(),
        "the interrupt reached a synchronous Lua loop, so INTERRUPT_NOTATION's doc \
         comment understates what it can do: {answered:?}"
    );
}

/// The modal is raised by view noticing something, not by the user asking
/// for it, and the thing it notices is very often a long operation that
/// finishes. Anything typed while it is up must land in the buffer exactly
/// as it would have with no modal on screen -- proved here against a real
/// engine and a real buffer rather than against the effect alone, since an
/// effect that never reaches nvim is the failure this is guarding.
///
/// `i` leads the sequence deliberately: it is the reflex keystroke of a
/// user waiting out a slow operation, and a modal that bound it would
/// answer that reflex by aborting the operation being waited for.
#[test]
fn every_key_typed_at_the_modal_still_lands_in_the_buffer() {
    let dir = common::fixture("supervision-live-passthrough", "");
    let engine = common::spawn_with_drained_pump(common::isolated_reading(&dir.join("init.lua")));
    engine.handle.ui_attach(80, 24).unwrap();

    let mut model = Model::with_term_size(80, 24);
    let _ = update(
        &mut model,
        Msg::EngineLiveness {
            wedge: Some(WedgeKind::ReadSide),
            observed_for: ENGINE_BUSY_MODAL_THRESHOLD,
        },
    );
    assert!(
        !model.overlays().is_empty(),
        "a wedge past the threshold must have opened the modal"
    );

    // typed with the modal up, one keypress at a time, exactly as the input
    // reader delivers them
    for notation in ["i", "x", "y", "z"] {
        let effects = update(
            &mut model,
            Msg::Key(Key {
                notation: notation.to_string(),
            }),
        );
        let [Effect::Rpc(RpcCall::Input { notation: sent })] = &effects[..] else {
            panic!("{notation:?} was swallowed by the modal: {effects:?}");
        };
        assert_eq!(sent, notation);
        engine.handle.input(sent).unwrap();
    }

    // asserted here rather than only at the end: this blocking round-trip is
    // also the barrier the interrupt below needs, since `nvim_input` queues
    // and an interrupt flushes whatever typeahead is still unread
    assert_eq!(
        engine.handle.eval_str("getline(1)").unwrap(),
        "xyz",
        "keystrokes typed at the modal never reached the buffer"
    );

    // the interrupt choice, picked by the very key it sends: the wire input
    // is what the engine would have received with no modal on screen, and
    // the modal stays up because the interrupt may not have landed
    let effects = update(
        &mut model,
        Msg::Key(Key {
            notation: INTERRUPT_NOTATION.to_string(),
        }),
    );
    let [Effect::Rpc(RpcCall::Input { notation: sent })] = &effects[..] else {
        panic!("the interrupt choice sent something else: {effects:?}");
    };
    assert_eq!(sent, INTERRUPT_NOTATION);
    engine.handle.input(sent).unwrap();
    assert!(
        !model.overlays().is_empty(),
        "an interrupt that may not land must leave the modal up"
    );

    // <Esc> is the modal's own Dismiss key, and it both closes the modal and
    // goes on to nvim: answering an annunciator the user never asked for may
    // not cost them a keystroke, so this <Esc> is also the one that leaves
    // insert mode
    let effects = update(
        &mut model,
        Msg::Key(Key {
            notation: "<Esc>".into(),
        }),
    );
    let [Effect::Rpc(RpcCall::Input { notation: sent })] = &effects[..] else {
        panic!("Dismiss must still deliver its <Esc> to the engine: {effects:?}");
    };
    engine.handle.input(sent).unwrap();
    assert!(model.overlays().is_empty(), "Dismiss must close the modal");

    // and with it gone the identical key routes to the engine the same way,
    // which is the whole claim: the modal changed what was on screen and
    // nothing about what a key does
    let effects = update(
        &mut model,
        Msg::Key(Key {
            notation: "<Esc>".into(),
        }),
    );
    let [Effect::Rpc(RpcCall::Input { notation: sent })] = &effects[..] else {
        panic!("a key with no overlay open must reach the engine: {effects:?}");
    };
    engine.handle.input(sent).unwrap();

    assert_eq!(
        engine.handle.eval_str("getline(1)").unwrap(),
        "xyz",
        "answering the modal cost the buffer the keystrokes it was typed"
    );
    assert_eq!(
        engine.handle.eval_str("mode()").unwrap(),
        "n",
        "the dismissal's <Esc> never reached the engine, so insert mode survived it"
    );
}
