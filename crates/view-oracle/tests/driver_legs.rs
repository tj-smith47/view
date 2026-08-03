//! Self-tests proving the four-leg oracle contract each of the three
//! headless drivers satisfies: Msg-level injection (`Session`) and
//! pty-level injection (`PtySession`) for leg (a), deterministic `Surface`
//! capture for leg (b) (covered by `view-oracle`'s own `src/lib.rs` unit
//! test `session_fed_a_scripted_redraw_and_flush_yields_the_known_screen_text`,
//! not duplicated here), a harness-owned clock for leg (c)
//! (`pump_until_flush`'s `deadline` parameter, proven live by the
//! `EngineSession` test below actually terminating), and engine
//! state-parity probes for leg (d) (`eval_str`, proven by the same test:
//! the decoded screen and nvim's own buffer state must agree).
#![allow(clippy::unwrap_used, clippy::expect_used)]

// Only the pty-level integration leg (gated below) drives the `view` binary
// through this scratch/XDG-isolation helper; the Session and EngineSession
// legs do not, so on Windows, where that leg is gated off, it is unused.
#[cfg(unix)]
mod common;

use std::time::{Duration, Instant};
use view_core::msg::{Key, Msg};
use view_oracle::{EngineSession, Session};
// The pty-level leg drives the real `view` binary under a terminal, a tier-2
// Windows surface validated on winserver rather than in CI; gated off Windows.
#[cfg(unix)]
use view_oracle::PtySession;

/// leg (a) (Msg-level injection) + leg (b) (deterministic capture), driven
/// through the pure `Session`: a scripted `Redraw` batch ending in `Flush`
/// yields exactly the known screen text.
#[test]
fn session_msg_level_injection_yields_the_expected_screen_text() {
    let mut session = Session::new(5, 1);

    session.feed(Msg::Redraw(vec![
        view_core::events::UiEvent::GridResize {
            grid: 1,
            width: 5,
            height: 1,
        },
        view_core::events::UiEvent::GridLine {
            grid: 1,
            row: 0,
            col_start: 0,
            cells: vec![view_core::events::GridCell {
                text: "o".to_string(),
                hl_id: 0,
                repeat: 1,
            }],
        },
        view_core::events::UiEvent::Flush,
    ]));

    assert_eq!(session.screen_text(), "o    ");
}

/// Covers the harness-owned clock (`pump_until_flush`'s `deadline` parameter
/// is the only timing in this test, proven by the call actually returning)
/// and the engine state-parity probe (`eval_str` and the decoded screen must
/// agree on what got typed). Spawns a real embedded nvim: no terminal, no
/// pty, the "truth path" tier.
#[test]
fn engine_session_input_and_pump_until_flush_agree_with_eval_str_probe() {
    let mut session = EngineSession::spawn(40, 6).expect("EngineSession::spawn against real nvim");

    session
        .input("ihello<Esc>")
        .expect("input() against a freshly attached engine");

    // pump_until_flush answers the next Flush of any origin -- an attach-time
    // one, or one nvim emits partway through the typed keys -- so the leg is
    // proven by pumping until the decoded screen carries the text, under this
    // test's own deadline, rather than by trusting a single flush to be the
    // one that finished the script
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut flushed = false;
    while Instant::now() < deadline && !session.screen_text().contains("hello") {
        flushed |= session.pump_until_flush(Duration::from_millis(250));
    }
    assert!(
        flushed,
        "pump_until_flush never observed a Flush within the deadline; screen:\n{}",
        session.screen_text()
    );

    assert!(
        session.screen_text().contains("hello"),
        "decoded screen never showed the typed text; screen:\n{}",
        session.screen_text()
    );

    let buffer_line = session
        .eval_str("getline(1)")
        .expect("eval_str against a live engine");
    assert_eq!(
        buffer_line, "hello",
        "engine's own buffer state disagrees with the decoded screen"
    );
}

/// The deadline leg must return `false` rather than hang when the engine is
/// quiescent: with no input queued there is no Flush coming, so this returns
/// only because the deadline expired. Deleting the deadline check would turn
/// this test into a hang, caught by the test runner's own timeout rather
/// than silently passing.
#[test]
fn pump_until_flush_returns_false_at_the_deadline_when_no_flush_arrives() {
    let mut session = EngineSession::spawn(40, 6).expect("EngineSession::spawn against real nvim");

    // drain every startup flush first so the timed pump below observes a
    // genuinely quiescent engine, not attach-time redraw traffic
    while session.pump_until_flush(Duration::from_millis(500)) {}

    let flushed = session.pump_until_flush(Duration::from_millis(300));
    assert!(
        !flushed,
        "pump_until_flush reported a Flush from a quiescent engine with no input queued"
    );
}

/// leg (a) (pty-level injection) at the integration tier: a real `view`
/// process inside a real pty shows a typed character on screen. Full stack,
/// full fidelity, the slowest and least isolated of the three legs by
/// design.
#[cfg(unix)]
#[test]
fn pty_session_against_the_view_binary_shows_a_typed_character_on_screen() {
    let paths = common::ScratchPaths::new("driver-legs");

    let mut cmd = portable_pty::CommandBuilder::new(common::view_bin_path());
    cmd.arg(&paths.scratch);
    common::isolate_xdg(&mut cmd, &paths.isolated_home);

    let mut session = PtySession::spawn_configured(cmd, 80, 24)
        .expect("PtySession::spawn_configured against target/debug/view");

    assert!(
        session.wait_for("~", Duration::from_secs(5)),
        "view never painted its startup shell; screen:\n{}",
        session.screen()
    );

    session.send(b"iZ").unwrap();
    assert!(
        session.wait_for("Z", Duration::from_secs(5)),
        "typed character never appeared on screen; screen:\n{}",
        session.screen()
    );

    session.send(b"\x1b:q!\r").unwrap();
    let _ = session.wait_for_exit(Duration::from_secs(5));
}

/// A sanity check on `Session::feed`'s total-and-lossy contract for a
/// `Msg::Key`: the pure Msg-level driver has no engine to route the
/// resulting `RpcCall::Input` effect to, and must not panic trying.
#[test]
fn session_feed_is_total_over_a_key_msg_with_no_engine_attached() {
    // Session::new only pre-fills the terminal size (Model::with_term_size);
    // the engine grid itself starts 0x0 until a real GridResize arrives, so
    // an untouched session's screen is empty, not term-size-shaped blanks
    let mut session = Session::new(10, 3);
    session.feed(Msg::Key(Key {
        notation: "x".to_string(),
    }));
    assert_eq!(
        session.screen_text(),
        "",
        "no panic and no change expected: a pure Session has no engine for a key to reach"
    );
}
