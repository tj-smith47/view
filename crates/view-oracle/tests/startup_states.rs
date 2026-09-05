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

/// Writes a `view.toml` under `home` with each named `[native]` feature
/// switched off, `[engine] single_grid` as asked, and every other setting
/// left at its default.
///
/// The permutations this pin walks are the ones a noice / nvim-notify user
/// reaches: what a session leaves with nvim decides where a startup prompt
/// or error is *drawn*, and the hold has to cover every way that lands --
/// including `single_grid = true`, where nvim places no message grid and
/// composites its message area into the last row of grid 1.
fn plant_native_off(home: &std::path::Path, off: &[&str], single_grid: bool) {
    let dir = common::xdg_home(home, "XDG_CONFIG_HOME").join("view");
    std::fs::create_dir_all(&dir).expect("the isolated config home must be creatable");
    let mut text = String::from("[native]\n");
    for feature in off {
        text.push_str(feature);
        text.push_str(" = false\n");
    }
    if single_grid {
        text.push_str("\n[engine]\nsingle_grid = true\n");
    }
    std::fs::write(dir.join("view.toml"), text).expect("the isolated view.toml must be writable");
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

/// The same pin across the permutations that decide which surfaces nvim
/// keeps drawing into the grid, and so where a startup prompt or a startup
/// error appears -- and never whether the pre-`VimEnter` screen is shown.
/// The last leg is the narrowest one: with `single_grid = true` and both
/// message surfaces off nvim externalizes nothing, so its message area is
/// the last row of grid 1 and the half-built buffer sits in the rows above
/// it.
///
/// Every leg is a session the fixture really does flush a half-built screen
/// to: the config's mid-source flush is gated on the attach being a remote
/// one, which every permutation is, so no leg can pass by drawing nothing.
/// The legs run to completion and report together, because "which
/// permutations regress" is the question and a walk that stops at the first
/// answers it for one.
///
/// Disconfirm: keying `Model::withholds_grid` on the cmdline and the
/// message surfaces again -- or forcing it to `false` -- puts
/// `PREVIMENTERBUFFER` into all four streams while the defaults leg above
/// stays clean.
#[test]
fn the_hold_covers_every_native_permutation() {
    let mut regressed = Vec::new();
    for (off, single_grid) in [
        (&["notifications"][..], false),
        (&["palette"][..], false),
        (&["notifications", "palette"][..], false),
        (&["notifications", "palette"][..], true),
    ] {
        let paths = common::ScratchPaths::new("startup-states-permutation");
        common::plant_nvim_config(&paths.isolated_home, "startup-states");
        plant_native_off(&paths.isolated_home, off, single_grid);

        let mut under_test = view_session(&paths.isolated_home);
        let stream = until_the_split(&mut under_test, "view");

        if wrote(&stream, PRE_VIM_ENTER) {
            regressed.push(format!("{off:?} single_grid = {single_grid}"));
        }
    }
    assert!(
        regressed.is_empty(),
        "view wrote the pre-VimEnter screen with these [native] switches off: {}",
        regressed.join("; ")
    );
}

/// The prompt the hold must not hide, typed through the binary: a startup
/// `vim.fn.input()` on the two attach sets that leave it to nvim's own
/// message area -- the multigrid both-off set, where nvim places a message
/// grid for it, and `single_grid = true`, where nvim composites it into
/// grid 1 -- with `cmdheight` at 2 so the prompt sits a row above a blank
/// last row on both. Each leg waits for the prompt on view's screen,
/// answers it, and reaches the post-`VimEnter` layout; nvim under the same
/// config is the reference that the prompt is what a TUI shows.
///
/// What tells a release on the prompt from one on the hold's 1 s cap is
/// view's own record: the cap logs `grid hold expired before UIEnter` and
/// the prompt path logs nothing, so the assertion reads the session's
/// `VIEW_LOG` rather than a wall clock a loaded host can stretch past the
/// cap. The time from view's first byte to the prompt is measured and
/// printed alongside.
///
/// Disconfirm: making `GridRegistry::message_area_has_text` answer `false`
/// leaves the prompt hidden until the cap, and both legs fail on the
/// expiry line.
#[test]
fn a_startup_prompt_reaches_the_terminal_before_the_holds_cap() {
    let nvim_paths = common::ScratchPaths::new("startup-prompt-nvim");
    common::plant_nvim_config(&nvim_paths.isolated_home, "startup-prompt");
    let mut reference = nvim_session(&nvim_paths.isolated_home);
    assert!(
        reference.wait_for(PROMPT, view_test_support::host_deadline(BUDGET)),
        "nvim never showed the startup prompt, so it is not the reference \
         this pin claims; screen:\n{}",
        reference.screen()
    );
    reference
        .send(b"\r")
        .expect("the reference pty accepts the answer");
    let _ = until_the_split(&mut reference, "nvim");

    let mut regressed = Vec::new();
    for single_grid in [false, true] {
        let paths = common::ScratchPaths::new("startup-prompt-view");
        common::plant_nvim_config(&paths.isolated_home, "startup-prompt");
        plant_native_off(
            &paths.isolated_home,
            &["notifications", "palette"],
            single_grid,
        );
        let view_log = paths.isolated_home.join("view.log");
        let mut cmd = portable_pty::CommandBuilder::new(common::view_bin_path());
        common::isolate_xdg_first_launch(&mut cmd, &paths.isolated_home);
        cmd.env("VIEW_LOG", &view_log);
        let spawned = std::time::Instant::now();
        let mut under_test = recording(cmd);

        let first_byte = until_the_first_chunk(&mut under_test);
        let first_byte_after_spawn = spawned.elapsed();
        assert!(
            under_test.wait_for(PROMPT, view_test_support::host_deadline(BUDGET)),
            "view never showed the startup prompt with single_grid = \
             {single_grid}; screen:\n{}",
            under_test.screen()
        );
        let prompt_after_first_byte = first_byte.elapsed();
        println!(
            "single_grid = {single_grid}: first byte {first_byte_after_spawn:?} after the spawn, \
             prompt {prompt_after_first_byte:?} after the first byte"
        );
        under_test
            .send(b"\r")
            .expect("the pty under test accepts the answer");
        let _ = until_the_split(&mut under_test, "view");

        let log = std::fs::read_to_string(&view_log).unwrap_or_default();
        if log.contains(HOLD_EXPIRED) {
            regressed.push(format!(
                "single_grid = {single_grid}: the prompt reached the terminal \
                 only on the cap, {prompt_after_first_byte:?} after the first byte"
            ));
        }
    }
    assert!(
        regressed.is_empty(),
        "view held the startup prompt until the hold's cap: {}",
        regressed.join("; ")
    );
}

/// The buffer shape nearest to a prompt's, held: one line of text with
/// nothing drawn beneath it (`cmdheight` and `laststatus` at 0, blank
/// end-of-buffer fill) under the single-grid attach, where nvim composites
/// its message area into the same grid the buffer is on. The row reads as
/// a prompt would -- text on the cursor's row, blank rows below -- and only
/// the cursor's own cell says otherwise: a buffer's sits on its first
/// character while sourcing, a prompt's on the blank cell after the text.
///
/// The home is warmed by a first session before the one measured: a cold
/// state directory makes view box its theme-cache notice over the top rows
/// at this width, and the one row the fixture draws is the first, so a
/// frame let through would be covered on the terminal and never reach the
/// stream. The warmed session paints no notice and the row is bare.
///
/// Disconfirm: reading the cursor's row and the rows below it instead of
/// the cursor's cell puts `ONLYLINE` into view's stream before the layout
/// while nvim's stays clean.
#[test]
fn a_one_line_buffer_over_blank_rows_reaches_neither_editors_terminal() {
    let view_paths = common::ScratchPaths::new("startup-eob-view");
    let nvim_paths = common::ScratchPaths::new("startup-eob-nvim");
    common::plant_nvim_config(&view_paths.isolated_home, "startup-eob");
    common::plant_nvim_config(&nvim_paths.isolated_home, "startup-eob");
    plant_native_off(
        &view_paths.isolated_home,
        &["notifications", "palette"],
        true,
    );
    warm_the_home(&view_paths.isolated_home);

    let mut under_test = view_session(&view_paths.isolated_home);
    let mut reference = nvim_session(&nvim_paths.isolated_home);

    let view_stream = until_the_split(&mut under_test, "view");
    let nvim_stream = until_the_split(&mut reference, "nvim");

    assert!(
        !wrote(&nvim_stream, ONLY_LINE),
        "nvim drew the one-line buffer before VimEnter, so it is not the \
         reference this pin claims"
    );
    assert!(
        !wrote(&view_stream, ONLY_LINE),
        "view wrote the one-line buffer nvim never showed: a screen with the \
         cursor on its first character was read as a prompt"
    );
}

/// Runs one session under `home` to completion of its startup and out
/// again, so the theme cache and the first-run record exist for the next.
///
/// The cache is written when the highlight probe confirms, which is
/// ordered after the layout and not before it, so the session waits on the
/// file rather than on the screen.
fn warm_the_home(home: &std::path::Path) {
    let mut session = view_session(home);
    let _ = until_the_split(&mut session, "view (warming)");
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
        std::thread::sleep(Duration::from_millis(25));
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

/// The one line the startup-eob fixture's buffer holds.
const ONLY_LINE: &str = "ONLYLINE";

/// The prompt the startup-prompt fixture draws.
const PROMPT: &str = "PROMPTHERE:";

/// The `VIEW_LOG` line the runtime writes when the hold ends on its cap
/// rather than on a signal.
const HOLD_EXPIRED: &str = "grid hold expired before UIEnter";

/// Blocks until the session has written anything at all, and answers the
/// instant it did: the first byte view's terminal saw, which is the origin
/// the prompt's arrival is measured from.
///
/// The predicate is evaluated once against the screen as it stands before
/// any wait, then once per chunk absorbed; the first byte is the second
/// evaluation.
fn until_the_first_chunk(session: &mut PtySession) -> std::time::Instant {
    let mut evaluations = 0;
    assert!(
        session.wait_for_screen(view_test_support::host_deadline(BUDGET), |_| {
            evaluations += 1;
            evaluations > 1
        }),
        "view wrote nothing at all; screen:\n{}",
        session.screen()
    );
    std::time::Instant::now()
}

/// A replacement engine runs the same startup, so it owes the same hold:
/// with `[supervision] auto_restart` on, an engine killed mid-session is
/// respawned, sources the same config, and must not paint the buffer it
/// holds before its own `VimEnter`.
///
/// Disconfirm: dropping `Model::rearm_startup_hold` from
/// `recovery::restart_engine` puts `PREVIMENTERBUFFER` into the stream on
/// the restart while the first start stays clean.
#[test]
#[cfg(target_os = "linux")]
fn a_restarted_engine_holds_its_own_pre_vim_enter_screen() {
    let paths = common::ScratchPaths::new("startup-states-restart");
    common::plant_nvim_config(&paths.isolated_home, "startup-states");

    let mut under_test = view_session(&paths.isolated_home);
    let first = until_the_split(&mut under_test, "view");
    assert!(
        !wrote(&first, PRE_VIM_ENTER),
        "the first start already failed the pin this restart extends"
    );
    let session_pid = under_test.pid().expect("the session under test has a pid");
    let engine = engine_child_of(session_pid).expect("view spawned an nvim child");

    // from the kill onward, never the whole stream: the layout the first
    // start left is still on the screen, so a screen-side wait for it would
    // be answered by the dead engine's frame and this would assert against
    // a restart that never happened
    let mark = first.len();
    kill(engine);
    // whichever lands first, so a replacement that paints the held screen
    // fails on that rather than on the wait: without the re-arm it writes
    // `PREVIMENTERBUFFER` and never gets as far as rewriting the layout
    let after = until_either(&mut under_test, mark, [PRE_VIM_ENTER, SPLIT]);
    assert!(
        !wrote(&after, PRE_VIM_ENTER),
        "the replacement engine painted the screen its VimEnter had not \
         reached yet"
    );
    assert!(
        wrote(&after, SPLIT),
        "no replacement engine drew its own screen, so this asserted nothing \
         about a restart"
    );
}

/// Waits until any of `needles` is written past `mark` in the recorded
/// stream, and answers everything written from `mark` on.
#[cfg(target_os = "linux")]
fn until_either(session: &mut PtySession, mark: usize, needles: [&str; 2]) -> Vec<u8> {
    let deadline = std::time::Instant::now() + view_test_support::host_deadline(BUDGET);
    loop {
        let written = session.raw_output()[mark..].to_vec();
        if needles.iter().any(|needle| wrote(&written, needle)) {
            return written;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "nothing the restart could have written arrived; screen:\n{}",
            session.screen()
        );
        // drains for the interval; the predicate is never the answer here
        let _ = session.wait_for_screen(Duration::from_millis(50), |_| false);
    }
}

/// The pid of the `nvim` the session under test spawned.
#[cfg(target_os = "linux")]
fn engine_child_of(pid: u32) -> Option<u32> {
    view_test_support::child_pids(pid)
        .into_iter()
        .find(|child| {
            std::fs::read_to_string(format!("/proc/{child}/comm"))
                .is_ok_and(|comm| comm.trim() == "nvim")
        })
}

/// Ends `pid` the way a wedged engine ends: only a pid this test read off
/// the session it spawned, never a name.
#[cfg(target_os = "linux")]
fn kill(pid: u32) {
    let _ = nix::sys::signal::kill(
        nix::unistd::Pid::from_raw(i32::try_from(pid).expect("a pid read from /proc fits an i32")),
        nix::sys::signal::Signal::SIGKILL,
    );
}
