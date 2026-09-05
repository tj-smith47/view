//! The start the user sees: one screen, not two. view's first content frame
//! must be the frame nvim's own TUI first draws, and every screen nvim
//! never showed must never reach the terminal.
//!
//! nvim `--embed` sources nothing until a UI attaches, and what it draws
//! while sourcing depends on who attached: view externalizes the cmdline,
//! the messages and the popupmenu, which a config can read and answer --
//! the user's does, through noice's notification about exactly that, which
//! nvim-notify animates and the animation redraws. A TUI attach sets none
//! of those, gets no notification, and draws nothing before `VimEnter`. The
//! fixture config here is that mechanism reduced to its two moving parts.
//!
//! Both sessions run under the same planted config, so nvim is the
//! reference rather than a second claim: the assertion is that the buffer
//! the config leaves behind on `VimEnter` never reaches either terminal.
#![cfg(unix)]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use std::time::Duration;

use view_oracle::PtySession;

const COLS: u16 = 80;
const ROWS: u16 = 24;

/// What one wait may take on an idle host, before the load this run started
/// under widens it. A cold `view` spawn is the slow part; a healthy session
/// reaches its first frame in a fraction of this.
const BUDGET: Duration = Duration::from_secs(20);

/// The startup buffer's text, which the config replaces on `VimEnter`. On
/// the terminal it is proof of a frame drawn before the config finished
/// opening its windows -- the screen nvim's own TUI never writes.
const PRE_VIM_ENTER: &str = "PREVIMENTERBUFFER";

/// The scratch buffer the config's `VimEnter` autocommand opens in a
/// vsplit, and the window beside it: together, the first screen either
/// editor is allowed to show.
const SPLIT: &str = "POSTVIMENTERSPLIT";
const BESIDE_THE_SPLIT: &str = "POSTVIMENTERLEFT";

/// A session recording every byte from its own spawn, under the planted
/// config.
///
/// Recording is armed before anything drains, which is what makes the
/// stream complete: `PtySession` records from the next drain onward, and
/// the first frame is written long before the first `wait_for`.
fn recording(cmd: portable_pty::CommandBuilder) -> PtySession {
    let mut session = PtySession::spawn_configured(cmd, COLS, ROWS)
        .expect("PtySession::spawn_configured against the editor under test");
    session.record_raw_output();
    session
}

/// The `view` under test, on its `[native]` defaults -- the surfaces the
/// externalized attach takes are the whole reason this startup differs from
/// a TUI's.
fn view_session(home: &std::path::Path) -> PtySession {
    let mut cmd = portable_pty::CommandBuilder::new(common::view_bin_path());
    common::isolate_xdg_first_launch(&mut cmd, home);
    recording(cmd)
}

/// The pinned `nvim` under the same config, drawing its own screen: the
/// reference the bar is stated against.
fn nvim_session(home: &std::path::Path) -> PtySession {
    let cfg = view_engine::EngineConfig::default();
    let mut cmd = portable_pty::CommandBuilder::new(&cfg.nvim_bin);
    // `-n` alone, never `--clean`: the planted config is the subject, and
    // `EngineConfig::isolated`'s argument list would skip it
    cmd.arg("-n");
    common::isolate_xdg_first_launch(&mut cmd, home);
    recording(cmd)
}

/// Waits for the whole post-`VimEnter` layout -- both windows -- and
/// answers everything the session wrote up to that point.
///
/// Read off the parsed screen rather than the stream, because a frame is
/// written as damage: the scratch buffer's text replaces the startup
/// buffer's in the same column, and the columns those two share are not
/// re-sent. The screen is where "this layout is showing" is a fact; the
/// stream below is only asked what was never sent at all.
fn until_the_split(session: &mut PtySession, who: &str) -> Vec<u8> {
    for needle in [SPLIT, BESIDE_THE_SPLIT] {
        assert!(
            session.wait_for(needle, view_test_support::host_deadline(BUDGET)),
            "{who} never showed {needle}, so this config did not open the \
             layout the pin is about; screen:\n{}",
            session.screen()
        );
    }
    session.raw_output().to_vec()
}

/// Whether `needle` was ever written to the terminal.
fn wrote(stream: &[u8], needle: &str) -> bool {
    stream
        .windows(needle.len())
        .any(|window| window == needle.as_bytes())
}

/// The pin: the buffer a startup holds before its config has opened its
/// windows never reaches the terminal, at either editor.
///
/// The count of screens is what this reads, in the one form that cannot
/// race a sampler: a screen never written is a screen never counted, so the
/// stream carrying the post-`VimEnter` layout and not the buffer before it
/// is a start of exactly two states -- the blank one and the finished one.
///
/// Disconfirm: dropping the `withholds_grid` arm from
/// `view_core::update::ui_event`'s `Flush`, or answering `UIEnter` with a
/// notification instead of the blocking request, puts `PREVIMENTERBUFFER`
/// back into view's stream while nvim's stays clean.
#[test]
fn the_buffer_before_vim_enter_reaches_neither_editors_terminal() {
    let view_paths = common::ScratchPaths::new("startup-states-view");
    let nvim_paths = common::ScratchPaths::new("startup-states-nvim");
    common::plant_nvim_config(&view_paths.isolated_home, "startup-states");
    common::plant_nvim_config(&nvim_paths.isolated_home, "startup-states");

    let mut under_test = view_session(&view_paths.isolated_home);
    let mut reference = nvim_session(&nvim_paths.isolated_home);

    let view_stream = until_the_split(&mut under_test, "view");
    let nvim_stream = until_the_split(&mut reference, "nvim");

    assert!(
        !wrote(&nvim_stream, PRE_VIM_ENTER),
        "nvim drew the pre-VimEnter buffer, so it is not the reference this \
         pin claims: the fixture's redraw is reaching a TUI attach"
    );
    assert!(
        !wrote(&view_stream, PRE_VIM_ENTER),
        "view wrote a screen nvim never showed -- the buffer its config \
         replaced on VimEnter"
    );
}
