//! Falsifiable proof that the statusline's mode segment reflects a real
//! nvim state change reaching the terminal, in the wired `view` binary over
//! a real pty.
//!
//! Starting macro recording (`qq`) is answered by nvim's own
//! `msg_showmode`, live-captured in `docs/statusline-wire-capture.md` as
//! `['msg_showmode', [[[15, 'recording @q', 11]]]]`. Nothing else on an
//! idle empty-buffer screen spells that phrase, so a screen showing it after
//! exactly this keystroke can only be the
//! `msg_showmode -> SegmentUpdate::Mode -> Model::statusline -> Statusline
//! layer` path having actually run end to end -- not a coincidence of typed
//! text, and not a unit test's fixture standing in for the wire shape.
#![cfg(unix)]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use std::time::Duration;

use view_oracle::PtySession;

const COLS: u16 = 80;
const ROWS: u16 = 24;

/// Budget for anything the session has to do: a cold `view` spawn plus an
/// nvim spawn on a loaded box is the slow part, and every wait here is
/// satisfied in milliseconds by a healthy session.
const BUDGET: Duration = Duration::from_secs(10);

/// A `view` session with every native feature at its default -- statusline
/// included -- paused once nvim's real content, not the pre-attach shell
/// placeholder, is on screen.
///
/// `common::isolate_xdg_first_launch` rather than
/// `common::isolate_xdg_native_off`:
/// the latter writes a `view.toml` disabling every registered feature,
/// including `statusline`, which is exactly the row this test exists to
/// exercise. An absent config file is the documented "full experience"
/// default (`NativeConfig::load`), so a first launch is what leaves the
/// statusline on without this test hand-writing a config of its own.
fn statusline_session(paths: &common::ScratchPaths) -> PtySession {
    let mut cmd = portable_pty::CommandBuilder::new(common::view_bin_path());
    cmd.arg(&paths.scratch);
    common::isolate_xdg_first_launch(&mut cmd, &paths.isolated_home);

    let mut session = PtySession::spawn_configured(cmd, COLS, ROWS)
        .expect("PtySession::spawn_configured against target/debug/view");

    assert!(
        session.wait_for("~", BUDGET),
        "view never painted nvim's real content; screen:\n{}",
        session.screen()
    );
    session
}

/// The whole path, end to end, in the binary a user runs: recording a macro
/// mid-session shows a recording indicator on the statusline without a
/// restart, and the indicator clears once the recording stops.
///
/// Both halves are asserted on purpose. The appearance alone cannot say the
/// segment tracks live state rather than latching the first thing it saw;
/// the disappearance alone cannot say a real update ever reached the
/// screen at all.
#[test]
fn starting_a_macro_recording_shows_on_the_statusline() {
    let paths = common::ScratchPaths::new("statusline-macro");
    let mut session = statusline_session(&paths);

    assert!(
        !session.screen().contains("recording @q"),
        "the session already showed a recording indicator before qq was \
         sent, so nothing below could distinguish a real update from a \
         stale one"
    );

    session.send(b"qq").unwrap();

    assert!(
        session.wait_for("recording @q", BUDGET),
        "the statusline never showed the macro-recording indicator after \
         qq; screen:\n{}",
        session.screen()
    );

    // `q` a second time stops the recording, which nvim answers by clearing
    // msg_showmode's content back to empty -- the segment must disappear
    // with it, or the statusline is latching text rather than rendering the
    // engine's live state.
    session.send(b"q").unwrap();
    assert!(
        session.wait_for_screen(BUDGET, |screen| !screen.contents().contains("recording @q")),
        "the recording indicator never cleared once the macro stopped; \
         screen:\n{}",
        session.screen()
    );

    session.send(b"\x1b:qa!\r").unwrap();
    let _ = session.wait_for_exit(BUDGET);
}
