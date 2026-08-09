//! Black-box proof that `"+yy` inside a real `view` process lands on the
//! actual OS clipboard, read independently by a second, freshly spawned
//! process (`view --print-clipboard +`) rather than through any state the
//! session under test could itself have faked (its own shadow register,
//! in particular -- see `crate::clipboard`'s remote-paste contract in
//! `crates/view/src/clipboard.rs`).
//!
//! # Why this test threads its own `DISPLAY`, not the ambient one
//!
//! Every `view-oracle` pty spawn goes through `PtySession::spawn_configured`,
//! which strips every host environment variable `view_engine::env` does not
//! allowlist -- `DISPLAY` deliberately among them, so that no other test in
//! this tree can silently answer to (or write into) the operator's real
//! desktop clipboard. This test's whole subject is the opposite: it needs
//! the session under test to reach a real, independently-readable
//! clipboard. Rather than weakening that shared funnel for every other
//! test, this one explicitly threads a distinctly-named `VIEW_CLIPBOARD_TEST_DISPLAY`
//! variable (never `DISPLAY` itself) into both the spawned `view` process
//! and the independent reader process; the hermetic sweep only strips names
//! the host itself exports, so a name it never sees under `DISPLAY` -- set
//! explicitly here, by this test alone -- passes through untouched.
#![allow(clippy::unwrap_used, clippy::expect_used)]

#[cfg(unix)]
mod common;

#[cfg(unix)]
use std::time::{Duration, Instant};

/// `view --print-clipboard`'s own exit code for "no system clipboard
/// reachable on this host" (see `crates/view/src/main.rs`'s
/// `NO_DISPLAY_EXIT`): this test skips rather than fails when it sees this
/// code, the same "must not assume either way" idiom the worker's own unit
/// tests already use for a headless host.
#[cfg(unix)]
const NO_DISPLAY_EXIT: i32 = 3;

/// The env var this test reads its target display from, distinct from
/// `DISPLAY` itself so the hermetic sweep documented on
/// `PtySession::spawn_configured` never has a reason to touch it -- see the
/// module doc.
#[cfg(unix)]
const TEST_DISPLAY_VAR: &str = "VIEW_CLIPBOARD_TEST_DISPLAY";

#[cfg(unix)]
enum ClipboardProbe {
    Text(String),
    NoDisplay,
}

#[cfg(unix)]
fn read_system_clipboard(bin: &std::path::Path, register: char, display: &str) -> ClipboardProbe {
    let output = std::process::Command::new(bin)
        .args(["--print-clipboard", &register.to_string()])
        .env("DISPLAY", display)
        .output()
        .expect("failed to spawn view --print-clipboard");
    if output.status.code() == Some(NO_DISPLAY_EXIT) {
        return ClipboardProbe::NoDisplay;
    }
    ClipboardProbe::Text(String::from_utf8_lossy(&output.stdout).into_owned())
}

#[cfg(unix)]
#[test]
fn a_system_clipboard_yank_is_independently_visible_to_a_fresh_process() {
    // no `VIEW_CLIPBOARD_TEST_DISPLAY` means no display was set up for this
    // run (the common case: most hosts, including CI, run headless) --
    // skip cleanly rather than assume a display exists or fabricate one
    let Ok(display) = std::env::var(TEST_DISPLAY_VAR) else {
        eprintln!(
            "skipping a_system_clipboard_yank_is_independently_visible_to_a_fresh_process: \
             {TEST_DISPLAY_VAR} not set, no display configured for this run"
        );
        return;
    };

    let bin = common::view_bin_path();
    let paths = common::ScratchPaths::new("clipboard-roundtrip");

    let mut cmd = portable_pty::CommandBuilder::new(&bin);
    cmd.arg(&paths.scratch);
    // every native feature off: a first-launch takeover notice would
    // otherwise cover the top rows this test types into, and the yank
    // this test is proving has nothing to do with any native surface
    common::isolate_xdg_native_off(&mut cmd, &paths.isolated_home);
    // explicit, distinctly-sourced override -- see the module doc for why
    // this is not the ambient `DISPLAY` the hermetic sweep would strip
    cmd.env("DISPLAY", &display);

    let mut session = view_oracle::PtySession::spawn_configured(cmd, 80, 24)
        .expect("PtySession::spawn_configured against target/debug/view");

    assert!(
        session.wait_for("~", Duration::from_secs(5)),
        "view never painted its startup shell; screen:\n{}",
        session.screen()
    );

    session.send(b"iview-clipboard-oracle-marker\x1b").unwrap();
    assert!(
        session.wait_for("view-clipboard-oracle-marker", Duration::from_secs(5)),
        "typed marker never appeared on screen; screen:\n{}",
        session.screen()
    );
    session.send(b"0\"+yy").unwrap();

    // `"+yy` yanks a whole line, which nvim's `g:clipboard` contract marks
    // linewise; the system clipboard's raw bytes must carry the trailing
    // `\n` that convention signals (`view --print-clipboard` never adds one
    // itself -- it's a bare `print!("{text}")` of exactly what `arboard`
    // returned), so this compares the full string rather than `.trim()`ed
    // content. A `.trim()`-based comparison would pass identically whether
    // or not the trailing newline made it onto the real clipboard, which is
    // exactly the byte this leg exists to prove.
    const EXPECTED_LINEWISE_TEXT: &str = "view-clipboard-oracle-marker\n";

    // a single-line yank prints no cmdline message under nvim's default
    // 'report', so there is no on-screen signal this leg can wait on
    // instead; this polls the independent read process (never the session
    // under test) until the write it triggered lands, bounded so a genuine
    // regression fails with a named timeout instead of hanging
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut last_seen = String::new();
    let mut confirmed = false;
    'poll: while Instant::now() < deadline {
        match read_system_clipboard(&bin, '+', &display) {
            ClipboardProbe::Text(text) => {
                last_seen = text;
                if last_seen == EXPECTED_LINEWISE_TEXT {
                    confirmed = true;
                    break 'poll;
                }
            }
            ClipboardProbe::NoDisplay => {
                session.send(b"\x1b:q!\r").unwrap();
                let _ = session.wait_for_exit(Duration::from_secs(5));
                eprintln!(
                    "skipping a_system_clipboard_yank_is_independently_visible_to_a_fresh_process: \
                     no system clipboard reachable on this host"
                );
                return;
            }
        }
        std::thread::sleep(Duration::from_millis(100));
    }

    // durability, not just a momentary echo: arboard's own X11 backend
    // grants a dropped `Clipboard` handle up to a 100ms grace window (a
    // synchronous clipboard-manager handoff attempt) before it tears the
    // whole connection down, so a worker that opened-and-dropped a fresh
    // `Clipboard` per job could still win the first read inside that
    // window and this test would falsely confirm it. Sleeping well past
    // that window and reading again is what actually distinguishes "wrote
    // through a connection the worker holds open" from "wrote, then tore
    // the connection down a moment later" -- the defect this test exists
    // to catch.
    std::thread::sleep(Duration::from_millis(400));
    let durable = matches!(
        read_system_clipboard(&bin, '+', &display),
        ClipboardProbe::Text(text) if text == EXPECTED_LINEWISE_TEXT
    );

    session.send(b"\x1b:q!\r").unwrap();
    let _ = session.wait_for_exit(Duration::from_secs(5));

    assert!(
        confirmed,
        "a fresh `view --print-clipboard +` process never independently observed the yanked \
         text, WITH its linewise trailing newline, on the system clipboard; expected \
         {EXPECTED_LINEWISE_TEXT:?}, last read: {last_seen:?}"
    );
    assert!(
        durable,
        "the yanked text was visible to an independent reader immediately after the yank but \
         not 400ms later; the clipboard worker is not holding its connection open (see this \
         test's module doc)"
    );
}
