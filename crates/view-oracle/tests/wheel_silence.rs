//! The lag a user reported while flinging the wheel over a tree that had
//! already reached its end, held to what the pinned nvim does with the same
//! reports at the same pty: nothing on the wire.
//!
//! nvim flushes a redraw batch for input it did not act on, and view painted
//! one terminal write per batch -- a cursor move to where the cursor already
//! was, a show of an already shown cursor, an SGR reset undoing nothing.
//! Over ssh each of those is a packet the far terminal parses and repaints
//! for. The screen cannot show the difference, so the pin reads the bytes:
//! a burst of wheel reports that changes nothing must cost the terminal
//! exactly what it costs nvim, which is zero.
//!
//! Both sessions run with every native feature off, so view's screen is
//! nvim's own content and no chrome of view's has news of its own to write.
#![cfg(unix)]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use std::time::{Duration, Instant};

use view_oracle::PtySession;

const COLS: u16 = 80;
const ROWS: u16 = 24;

/// What one wait may take on an idle host, before the load this run started
/// under widens it. A cold `view` spawn plus an nvim spawn is the slow part;
/// a healthy session satisfies every wait here in milliseconds.
const BUDGET: Duration = Duration::from_secs(20);

/// How long a burst is given to produce whatever it is going to produce,
/// measured from the write that dispatched it rather than from the start of
/// the wait, so a descheduled test thread moves both ends of the window
/// together.
const SILENCE: Duration = Duration::from_secs(2);

/// How often a session's recording is re-read while a window is open.
const POLL: Duration = Duration::from_millis(25);

/// One SGR wheel-down report at row 3, column 10: button 65, the `M` press
/// form a terminal sends for a wheel notch.
const WHEEL_DOWN: &[u8] = b"\x1b[<65;10;3M";

/// Reports per burst. More than the buffer has lines to give, so every one
/// of them past the first asks for a scroll the window cannot perform.
const BURST: usize = 20;

/// The fixture's first line, which is how a session says it has opened the
/// file, and the line the liveness marker later overwrites.
const FIRST_LINE: &str = "wheel silence fixture";

/// What the liveness marker writes into line 1 once the measurement is over.
const MARKER: &str = "STILLALIVE";

/// A five-line file, which at [`ROWS`] rows is a buffer whose every line is
/// already on screen: a wheel-down over it is an event nvim reads, acts on
/// as far as it can, and has nothing to show for.
fn fixture(paths: &common::ScratchPaths) {
    let mut text = String::from(FIRST_LINE);
    for line in 2..=5 {
        text.push_str(&format!("\nline {line}"));
    }
    text.push('\n');
    std::fs::write(&paths.scratch, text).unwrap();
}

/// The `view` under test on the fixture, with every native feature off.
fn view_session(paths: &common::ScratchPaths) -> PtySession {
    let mut cmd = portable_pty::CommandBuilder::new(common::view_bin_path());
    cmd.arg(&paths.scratch);
    common::isolate_xdg_native_off(&mut cmd, &paths.isolated_home);
    let session = PtySession::spawn_configured(cmd, COLS, ROWS)
        .expect("PtySession::spawn_configured against target/debug/view");
    ready(session)
}

/// The pinned `nvim` on the same fixture, with none of the host's
/// configuration.
fn nvim_session(paths: &common::ScratchPaths) -> PtySession {
    let cfg = view_engine::EngineConfig::isolated();
    let mut cmd = portable_pty::CommandBuilder::new(&cfg.nvim_bin);
    for arg in &cfg.extra_args {
        cmd.arg(arg);
    }
    cmd.arg(&paths.scratch);
    common::isolate_xdg_native_off(&mut cmd, &paths.isolated_home);
    let session = PtySession::spawn_configured(cmd, COLS, ROWS)
        .expect("PtySession::spawn_configured against the pinned nvim");
    ready(session)
}

/// Waits for the session to have the fixture on screen and turns the mouse
/// on, then starts recording the bytes it writes.
///
/// Recording starts after the setup rather than before it: what the startup
/// and the `:set` cost is not what this measures, and the recorder captures
/// from the next drain onward either way.
fn ready(mut session: PtySession) -> PtySession {
    assert!(
        session.wait_for(FIRST_LINE, view_test_support::host_deadline(BUDGET)),
        "the session never opened the fixture; screen:\n{}",
        session.screen()
    );
    session.send(b"\x1b:set mouse=a\r").unwrap();
    session.record_raw_output();
    session
}

/// Blocks until the session has written nothing across two consecutive
/// reads, and answers how many bytes it has written in all.
fn settle(session: &mut PtySession) -> usize {
    let deadline = Instant::now() + view_test_support::host_deadline(BUDGET);
    let mut last = session.raw_output().len();
    let mut still = 0_u8;
    while Instant::now() < deadline {
        std::thread::sleep(POLL);
        let now = session.raw_output().len();
        if now == last {
            still += 1;
            if still == 2 {
                return now;
            }
        } else {
            still = 0;
        }
        last = now;
    }
    last
}

/// Sends one burst of [`BURST`] wheel-down reports as a single write, then
/// holds the window open from that write until [`SILENCE`] has passed,
/// draining throughout so every byte the session produces is recorded.
/// Answers how many bytes it has written in all.
fn burst_and_watch(session: &mut PtySession) -> usize {
    let reports = WHEEL_DOWN.repeat(BURST);
    let dispatched = Instant::now();
    session.send(&reports).unwrap();
    let window = view_test_support::host_deadline(SILENCE);
    while dispatched.elapsed() < window {
        std::thread::sleep(POLL);
        let _ = session.raw_output();
    }
    session.raw_output().len()
}

/// Overwrites line 1 and waits for it, which is what makes the silence above
/// a measurement rather than a stall: the pty carries one ordered byte
/// stream, so a marker on screen is the whole burst before it having been
/// read and acted on.
fn prove_alive(session: &mut PtySession, silent_at: usize) {
    let command = format!(":call setline(1,'{MARKER}')\r");
    session.send(command.as_bytes()).unwrap();
    assert!(
        session.wait_for(MARKER, view_test_support::host_deadline(BUDGET)),
        "the session never ran the marker command, so its silence was a stall \
         rather than a frame it declined to write; screen:\n{}",
        session.screen()
    );
    assert!(
        session.raw_output().len() > silent_at,
        "the marker reached the screen without the session writing a byte, so \
         this recording is not reading the session's output at all"
    );
}

/// The pin: a wheel burst that changes nothing on screen writes nothing to
/// the terminal, and nvim's own answer to the same reports is what says how
/// much "nothing" is.
///
/// The first burst is the settle point -- whatever either editor does the
/// first time the mouse is used, it has done by the time that one is over --
/// and the second is the measurement. Both sides are asserted, so the pin
/// says which parity it proves rather than resting on two sessions agreeing
/// about a stream neither produced.
///
/// Disconfirm: making the SGR trailer unconditional in
/// `view-tui/src/paint/emit.rs`, or re-stating the cursor position on every
/// frame, fails the `view` assertion with the byte count of the frames the
/// burst then produced.
#[test]
fn a_wheel_burst_that_scrolls_nothing_writes_nothing_at_either_editor() {
    let view_paths = common::ScratchPaths::new("wheel-silence-view");
    let nvim_paths = common::ScratchPaths::new("wheel-silence-nvim");
    fixture(&view_paths);
    fixture(&nvim_paths);

    let mut under_test = view_session(&view_paths);
    let mut reference = nvim_session(&nvim_paths);

    let _ = burst_and_watch(&mut under_test);
    let _ = burst_and_watch(&mut reference);
    let view_settled = settle(&mut under_test);
    let nvim_settled = settle(&mut reference);

    let view_after = burst_and_watch(&mut under_test);
    let nvim_after = burst_and_watch(&mut reference);

    prove_alive(&mut under_test, view_after);
    prove_alive(&mut reference, nvim_after);

    assert_eq!(
        nvim_after - nvim_settled,
        0,
        "the pinned engine wrote {} bytes for a wheel burst that changed \
         nothing, so this leg states no floor for view to be held to",
        nvim_after - nvim_settled
    );
    assert_eq!(
        view_after - view_settled,
        0,
        "view wrote {} bytes for a wheel burst that changed nothing, where \
         nvim wrote none: a redraw batch nvim flushed without acting on \
         became a terminal write",
        view_after - view_settled
    );

    for session in [&mut under_test, &mut reference] {
        session.send(b"\x1b:qa!\r").unwrap();
        let _ = session.wait_for_exit(BUDGET);
    }
}
