//! A superseded claimant's float never reaches the terminal: with a surface
//! native, a window the plugin that used to render it opens is classified
//! at the placement event that would paint it, held off the screen while
//! its text is read, and routed into view's own chrome.
//!
//! Both sessions run under the same planted config, so nvim is the
//! reference rather than a second claim: the assertion is that the box the
//! config opens is on nvim's screen and never in view's stream.
#![cfg(unix)]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use std::time::Duration;

use view_oracle::PtySession;

const COLS: u16 = 80;
const ROWS: u16 = 24;

/// What one wait may take on an idle host, before the load this run started
/// under widens it.
const BUDGET: Duration = Duration::from_secs(20);

/// How many scroll keys the per-key assertion walks, matching the recorder
/// timeline in the report.
///
/// What is asserted per key is that a frame landed and that it carried no
/// float text -- not a write count. The pty reader's chunk boundaries are
/// its own read boundaries, not the child's writes, so a count taken here
/// would be a property of the host's scheduling; the write counts in the
/// report come from a recorder that times the reads against the keys.
const KEYS: usize = 6;

/// The float's own first line. On the terminal it is proof of a frame
/// carrying a window view was supposed to be holding back.
const FLOAT_TEXT: &str = "CLAIMANTFLOATTEXT";

/// The line the fixture's animation timer writes on the step that finds
/// the window gone: view closed it, so the take is complete and the
/// float's whole life is behind the stream this reads. The line it
/// replaces (`CLAIMANTFLOATREADY`, on screen from `VimEnter` until then)
/// is what a failure's screen dump carries instead.
const TAKEN: &str = "CLAIMANTFLOATTAKEN";

/// A session recording every byte from its own spawn, under the planted
/// config.
fn recording(cmd: portable_pty::CommandBuilder) -> PtySession {
    let mut session = PtySession::spawn_configured(cmd, COLS, ROWS)
        .expect("PtySession::spawn_configured against the editor under test");
    session.record_raw_output();
    session
}

/// The `view` under test, on its `[native]` defaults.
fn view_session(home: &std::path::Path) -> PtySession {
    let mut cmd = portable_pty::CommandBuilder::new(common::view_bin_path());
    common::isolate_xdg_first_launch(&mut cmd, home);
    recording(cmd)
}

/// The pinned `nvim` under the same config, drawing its own screen.
fn nvim_session(home: &std::path::Path) -> PtySession {
    recording(common::reference_nvim(home))
}

/// The `view.toml` this pin is stated against: the message surface native,
/// which is what makes the plugin that used to draw it a superseded
/// claimant.
fn plant_notifications_on(home: &std::path::Path) {
    let dir = common::xdg_home(home, "XDG_CONFIG_HOME").join("view");
    std::fs::create_dir_all(&dir).expect("the isolated config home must be creatable");
    std::fs::write(dir.join("view.toml"), "[native]\nnotifications = true\n")
        .expect("the isolated view.toml must be writable");
}

/// Whether `needle` was ever written to the terminal.
fn wrote(stream: &[u8], needle: &str) -> bool {
    stream
        .windows(needle.len())
        .any(|window| window == needle.as_bytes())
}

/// Runs one session under `home` to its layout and out again, so the theme
/// cache exists for the next.
///
/// A cold state directory makes view box its first-launch notice over the
/// top rows at this width, which are the rows the float claims: a frame
/// that did reach the terminal would be covered there and read as a frame
/// never drawn. The warmed session paints no notice, so the corner is bare.
fn warm_the_home(home: &std::path::Path) {
    let mut session = view_session(home);
    assert!(
        session.wait_for(TAKEN, view_test_support::host_deadline(BUDGET)),
        "the warming session never reached the fixture's layout; screen:\n{}",
        session.screen()
    );
    let cache = common::xdg_home(home, "XDG_STATE_HOME").join("view");
    let written = || {
        std::fs::read_dir(&cache).is_ok_and(|entries| {
            entries
                .flatten()
                .any(|entry| entry.file_name().to_string_lossy().starts_with("theme-"))
        })
    };
    let deadline = std::time::Instant::now() + view_test_support::host_deadline(BUDGET);
    while !written() && std::time::Instant::now() < deadline {
        std::thread::sleep(view_test_support::host_deadline(Duration::from_millis(25)));
    }
    assert!(
        written(),
        "the warming session never wrote a theme cache under {}",
        cache.display()
    );
    session
        .send(b"\x1b:qa!\r")
        .expect("the warming pty accepts the quit");
    let _ = session.wait_for_exit(view_test_support::host_deadline(BUDGET));
}

/// The pin: the box a superseded claimant opens is on the reference's
/// screen and in no frame view ever wrote, and its text is in view's
/// notification history instead.
///
/// The stream is read at the moment the fixture's own timer reports the
/// window closed, which is after every step of its animation: a frame
/// carrying any of them would already be in it.
///
/// Disconfirm: dropping the withheld filter from the pane walk, or the
/// classification from `update::ui_event`'s `WinFloatPos` arm, puts
/// `CLAIMANTFLOATTEXT` into view's stream while nvim's screen is unchanged.
#[test]
fn a_superseded_claimants_float_reaches_the_history_and_never_the_terminal() {
    let view_paths = common::ScratchPaths::new("claimant-float-view");
    let nvim_paths = common::ScratchPaths::new("claimant-float-nvim");
    common::plant_nvim_config(&view_paths.isolated_home, "claimant-float");
    common::plant_nvim_config(&nvim_paths.isolated_home, "claimant-float");
    plant_notifications_on(&view_paths.isolated_home);
    warm_the_home(&view_paths.isolated_home);

    let mut reference = nvim_session(&nvim_paths.isolated_home);
    assert!(
        reference.wait_for(FLOAT_TEXT, view_test_support::host_deadline(BUDGET)),
        "nvim never drew the stand-in's float, so it is not the reference \
         this pin claims; screen:\n{}",
        reference.screen()
    );

    let mut under_test = view_session(&view_paths.isolated_home);
    assert!(
        under_test.wait_for(TAKEN, view_test_support::host_deadline(BUDGET)),
        "the fixture's window was never closed, so view left a claimant's \
         float standing; screen:\n{}",
        under_test.screen()
    );
    let stream = under_test.raw_output().to_vec();
    assert!(
        !wrote(&stream, FLOAT_TEXT),
        "view painted a superseded claimant's float; screen:\n{}",
        under_test.screen()
    );

    for key in 1..=KEYS {
        let before = under_test.raw_output().len();
        under_test
            .send(b"\x1b[B")
            .expect("the pty under test accepts a scroll key");
        let deadline = std::time::Instant::now() + view_test_support::host_deadline(BUDGET);
        while under_test.raw_output().len() == before && std::time::Instant::now() < deadline {
            std::thread::sleep(view_test_support::host_deadline(Duration::from_millis(5)));
        }
        let written = under_test.raw_output()[before..].to_vec();
        assert!(
            !written.is_empty(),
            "key {key} drew no frame at all, so the assertion below is vacuous"
        );
        assert!(
            !wrote(&written, FLOAT_TEXT),
            "key {key} carried a superseded claimant's float onto the screen; \
             screen:\n{}",
            under_test.screen()
        );
    }

    // read after the stream is taken, since the history view puts the text
    // on the terminal itself -- which is the point of it
    under_test
        .send(b"\\fm")
        .expect("the pty under test accepts the history key");
    assert!(
        under_test.wait_for(FLOAT_TEXT, view_test_support::host_deadline(BUDGET)),
        "nothing is discarded: the plugin's own words owe the notification \
         history; screen:\n{}",
        under_test.screen()
    );
}
