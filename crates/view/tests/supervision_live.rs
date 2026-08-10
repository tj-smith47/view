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

use view_core::native::supervision::INTERRUPT_NOTATION;

/// How long the engine is told to stay busy for. Far longer than any budget
/// below, so a probe that answers cannot be the busy work finishing on its
/// own. Never actually waited out: the engine is killed with the session.
const BUSY_SECS: u64 = 60;

/// How long an answer that proves the interrupt landed is waited for. An
/// interrupted engine answers in about a millisecond; this is slack for a
/// loaded box, still an order of magnitude short of `BUSY_SECS`.
const INTERRUPTED: Duration = Duration::from_secs(5);

/// A live engine with a UI attached and nothing in its config, put to work on
/// `busy` and left there.
fn busy_engine(label: &str, busy: &str) -> view_engine::process::Engine {
    let dir = common::fixture(&format!("supervision-live-{label}"), "");
    let engine = common::spawn_with_drained_pump(common::isolated_reading(&dir.join("init.lua")));
    engine.handle.ui_attach(80, 24).unwrap();
    // typed rather than requested: `nvim_command` is a blocking request, and
    // a caller waiting on the reply to work that never returns could not go
    // on to interrupt it
    engine.handle.input(busy).unwrap();
    // long enough for the typed line to have been submitted and entered, so
    // what follows is measured against an engine that is already inside the
    // loop rather than one still reading the command
    std::thread::sleep(Duration::from_millis(800));
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
