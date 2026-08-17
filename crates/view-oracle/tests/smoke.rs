//! End-to-end proof that the wired `view` binary starts nvim, paints typed
//! text into a real terminal, and exits with the right code on every exit
//! path: a clean `:q!`, a signal-killed engine, and an explicit `:cq` exit
//! code. All driven through an actual pty rather than an in-process mock.
//! These tests wait via fixed sleeps rather than a deterministic
//! "redraw settled" signal; they are smoke tests, not exhaustive protocol
//! coverage.
//!
//! Drives the pty through `view_oracle::PtySession` (promoted from what
//! used to be this file's own private scaffolding): [`ViewPtySession`] is a
//! thin wrapper adding only the `view`-binary-specific concerns the lib
//! type deliberately does not know about (an isolated scratch file and
//! `XDG_*_HOME`, and reading the scratch file back as an echo-immune
//! oracle), `Deref`/`DerefMut` to the promoted type for everything else
//! (`send`, `wait_for`, `wait_for_cell`, `wait_for_exit`, `screen`,
//! `screen_raw`, `pid`, `wait`).
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
// Every test in this file drives the wired `view` binary inside a real pty;
// view's Windows terminal runtime is a tier-2 surface validated on winserver
// rather than in CI, so the whole suite is gated off the Windows build. There
// are no pure-logic tests here to keep running on Windows.
#![cfg(unix)]

mod common;

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{RwLock, RwLockReadGuard, RwLockWriteGuard};
use std::time::{Duration, Instant};
use view_oracle::{PtySession, QueryPolicy};
use view_surface::SHELL_PLACEHOLDER;

// The pty-isolation lock. A timing-bound test measures an absolute wall-clock
// bound (startup+save) that is only meaningful with the host to itself: a
// parallel run on few cores (e.g. `taskpolicy -b` confining the suite to 2
// efficiency cores) inflates every session by the scheduling tax and
// false-trips a bound that is really about the 50ms probe deadline, not
// contention. Every spawn takes the read side, so ordinary sessions still run
// in parallel with each other; a timing-bound test takes the write side for a
// contention-free measurement window.
static PTY_ISOLATION: RwLock<()> = RwLock::new(());

// The read side of `PTY_ISOLATION`, held for a whole session lifetime.
// `unwrap_or_else(into_inner)` recovers a poisoned lock: the guarded value is
// `()` with no invariant to protect, so a panicking test must not cascade into
// every later session failing to acquire the lock.
fn shared_isolation() -> Option<RwLockReadGuard<'static, ()>> {
    Some(
        PTY_ISOLATION
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner()),
    )
}

// The write side of `PTY_ISOLATION`: acquire it BEFORE starting a timing clock
// (the acquisition can block waiting for in-flight sessions to drain, which is
// not part of the startup latency under test), then spawn via
// `spawn_view_pty_raw_isolated`. While the guard is held, no other spawn helper
// can start a session, so the measurement runs on an uncontended host.
fn pty_isolation_exclusive() -> RwLockWriteGuard<'static, ()> {
    PTY_ISOLATION
        .write()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Wraps the promoted `view_oracle::PtySession` with the `view`-binary
/// concerns that promotion deliberately left behind: an isolated scratch
/// file and `XDG_*_HOME` (the lib type's `spawn`/`spawn_configured` have no
/// env/cwd-free way to express those), plus reading the scratch file back
/// as an echo-immune oracle. `Deref`/`DerefMut` forward everything else
/// (`send`, `wait_for`, `wait_for_cell`, `wait_for_exit`, `screen`,
/// `screen_raw`, `pid`, `wait`) straight to the promoted type.
struct ViewPtySession {
    session: PtySession,
    paths: common::ScratchPaths,
    // The isolation read lock, held for this session's whole lifetime so a
    // timing-bound test's exclusive window (`pty_isolation_exclusive`) cannot
    // begin mid-session. `None` when the spawning test already holds the
    // exclusive write guard, since taking read on that same thread deadlocks.
    _isolation: Option<RwLockReadGuard<'static, ()>>,
}

impl std::ops::Deref for ViewPtySession {
    type Target = PtySession;
    fn deref(&self) -> &Self::Target {
        &self.session
    }
}

impl std::ops::DerefMut for ViewPtySession {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.session
    }
}

impl ViewPtySession {
    /// The OS pid of the `view` process itself (not its embedded nvim
    /// child). Only the signal-death test needs it, and that test reads
    /// the process tree through `/proc`, so this follows it in being
    /// Linux-only rather than reading as dead code everywhere else.
    #[cfg(target_os = "linux")]
    fn view_pid(&self) -> u32 {
        self.session
            .pid()
            .expect("view child exposes a pid on this platform")
    }

    /// Reads the scratch file `view` was launched against back off disk,
    /// after a `:wq` (or equivalent) has saved it.
    ///
    /// An echo-immune oracle: the pty master stream can show text that was
    /// never actually processed by nvim at all, since a real terminal's
    /// canonical-mode line discipline echoes back whatever the test itself
    /// wrote to the master, independent of whether the child process (or
    /// nvim inside it) ever read those bytes. Reading the saved file's real
    /// contents instead proves the text reached nvim's buffer through
    /// `nvim_input`, not just the terminal's own echo.
    fn read_saved_file(&self) -> String {
        std::fs::read_to_string(&self.paths.scratch)
            .expect("saved file should exist and be readable")
    }
}

/// Opens a pty sized 24x80 (nonzero: a 0x0 winsize makes nvim's UI attach
/// fail during startup), spawns `view` against a scratch file with the
/// host's real nvim config isolated out of the way, and waits for the first
/// non-blank redraw so callers start from a settled screen.
fn spawn_view_pty() -> ViewPtySession {
    let mut session = spawn_view_pty_raw();
    // waits specifically for a `~` (nvim's own empty-buffer-line marker,
    // painted the moment a fresh unnamed buffer's grid content actually
    // streams in), not merely "the screen is non-blank": since startup's
    // placeholder shell (`view_surface::LayerKind::Shell`, a themed
    // statusline bar plus a static "waiting for nvim" indicator) now paints
    // real, non-blank text of its own well before the engine attaches, a
    // bare blank-vs-non-blank check would return as soon as that
    // placeholder appears rather than once nvim is actually ready
    let _ = session.wait_for("~", Duration::from_secs(5));
    session
}

/// Like [`spawn_view_pty`], but returns as soon as the child is spawned
/// instead of waiting for the first redraw. Only tests that need to drive
/// input inside that startup window itself (the capability-probe residue
/// regression test) should call this directly; every other test wants
/// [`spawn_view_pty`]'s settled screen, same as a human would get.
fn spawn_view_pty_raw() -> ViewPtySession {
    spawn_view_pty_raw_with_args(&[])
}

/// Like [`spawn_view_pty_raw`], but with `extra_args` inserted before the
/// scratch-file positional argument (e.g. `--nvim-bin <wrapper>`, for tests
/// that need to control how slowly the embedded engine starts).
fn spawn_view_pty_raw_with_args(extra_args: &[&std::ffi::OsStr]) -> ViewPtySession {
    build_view_pty(extra_args, shared_isolation(), QueryPolicy::AnswerDa1)
}

/// Builds the raw `view` pty session with a given isolation guard and query
/// policy.
///
/// The guard is the read side of `PTY_ISOLATION` for the common case, or
/// `None` when the spawning test already holds the exclusive write guard
/// (taking read on that same thread deadlocks). The policy decides whether
/// this pty answers the child's DA1 fence like a real terminal or stays
/// mute, which is the difference between measuring startup on a normal
/// terminal and driving the probe's unanswered-deadline path.
fn build_view_pty(
    extra_args: &[&std::ffi::OsStr],
    isolation: Option<RwLockReadGuard<'static, ()>>,
    policy: QueryPolicy,
) -> ViewPtySession {
    build_view_pty_with_content(extra_args, None, isolation, policy)
}

/// Like [`build_view_pty`], but seeds the scratch file with `content` before
/// spawning, for tests whose falsifiable claim (e.g. "line 42 is on screen
/// for `+42`") only means something against a file with real content in it.
fn build_view_pty_with_content(
    extra_args: &[&std::ffi::OsStr],
    content: Option<&str>,
    isolation: Option<RwLockReadGuard<'static, ()>>,
    policy: QueryPolicy,
) -> ViewPtySession {
    let paths = common::ScratchPaths::new("smoke");
    if let Some(content) = content {
        std::fs::write(&paths.scratch, content).expect("scratch fixture must be writable");
    }
    spawn_view_pty_at(paths, extra_args, isolation, policy)
}

/// Like [`build_view_pty_with_content`], but against scratch paths the caller
/// already holds.
///
/// For a test that has to plant something under those paths before the
/// session starts -- a config the engine will source, a swap file it will
/// find -- and so cannot let the helper create them on its way past.
fn spawn_view_pty_at(
    paths: common::ScratchPaths,
    extra_args: &[&std::ffi::OsStr],
    isolation: Option<RwLockReadGuard<'static, ()>>,
    policy: QueryPolicy,
) -> ViewPtySession {
    let mut cmd = portable_pty::CommandBuilder::new(common::view_bin_path());
    for arg in extra_args {
        cmd.arg(arg);
    }
    cmd.arg(&paths.scratch);
    common::isolate_xdg_native_off(&mut cmd, &paths.isolated_home);

    let session = PtySession::spawn_configured_with(cmd, 80, 24, policy).unwrap();

    ViewPtySession {
        session,
        paths,
        _isolation: isolation,
    }
}

/// Like [`spawn_view_pty`], but isolates only the `XDG_*_HOME` env vars
/// (`common::isolate_xdg_first_launch`) instead of also writing a
/// `view.toml` that disables every registered feature
/// (`common::isolate_xdg_native_off`, what every other helper in this file
/// uses).
///
/// `NativeConfig::load`'s documented contract is that an absent config file
/// means every feature defaults on; `isolate_xdg_native_off`'s
/// deliberately-written all-off `view.toml` (a baseline predating any
/// native feature, kept as the default so unrelated smoke tests are not
/// coupled to a feature's rendering) would falsify a test whose subject is
/// specifically a default-on feature. Follows the same precedent as
/// `statusline_macro_recording.rs`'s `statusline_session`.
fn spawn_view_pty_first_launch() -> ViewPtySession {
    let paths = common::ScratchPaths::new("smoke");
    let mut cmd = portable_pty::CommandBuilder::new(common::view_bin_path());
    cmd.arg(&paths.scratch);
    common::isolate_xdg_first_launch(&mut cmd, &paths.isolated_home);

    let isolation = shared_isolation();
    let session = PtySession::spawn_configured_with(cmd, 80, 24, QueryPolicy::AnswerDa1).unwrap();

    let mut session = ViewPtySession {
        session,
        paths,
        _isolation: isolation,
    };
    let _ = session.wait_for("~", Duration::from_secs(5));
    session
}

/// Like [`spawn_view_pty_raw`] but takes NO isolation read lock, for a timing
/// test that already holds the exclusive write guard from
/// `pty_isolation_exclusive`; taking read on that same thread deadlocks.
fn spawn_view_pty_raw_isolated(policy: QueryPolicy) -> ViewPtySession {
    build_view_pty(&[], None, policy)
}

/// Like [`spawn_view_pty_raw`], but `preload` is already sitting in the
/// pty's input queue when `view` is spawned, so the child's very first
/// terminal read finds it there.
///
/// The distinction matters to exactly one property: input that arrived with
/// nothing following it. Sending after the spawn would add a second write,
/// and a second write is a fresh readiness edge -- enough on its own to
/// release a loop stalled on the first.
fn spawn_view_pty_preloaded(
    preload: &[u8],
    isolation: Option<RwLockReadGuard<'static, ()>>,
    policy: QueryPolicy,
) -> ViewPtySession {
    let paths = common::ScratchPaths::new("smoke");
    let mut cmd = portable_pty::CommandBuilder::new(common::view_bin_path());
    cmd.arg(&paths.scratch);
    common::isolate_xdg_native_off(&mut cmd, &paths.isolated_home);

    ViewPtySession {
        session: PtySession::spawn_preloaded(cmd, 80, 24, policy, preload).unwrap(),
        paths,
        _isolation: isolation,
    }
}

/// Like [`spawn_view_pty_raw_isolated`] but with `extra_args` inserted
/// before the scratch-file positional argument (e.g. `--nvim-bin
/// <wrapper>`), for a timing test that both needs to control the embedded
/// engine's startup and needs the exclusive isolation window.
fn spawn_view_pty_raw_isolated_with_args(
    extra_args: &[&std::ffi::OsStr],
    policy: QueryPolicy,
) -> ViewPtySession {
    build_view_pty(extra_args, None, policy)
}

/// Polls Linux's `/proc/<pid>/task/<pid>/children` for a direct child of
/// `parent_pid` whose `/proc/<pid>/comm` equals `comm`, or `None` once
/// `timeout` elapses.
///
/// `view` never exposes its embedded nvim's pid on its own API surface (by
/// design: only `view-engine` speaks to the child at all), so a black-box
/// pty test has no way to find it except by walking procfs from the
/// outside. Linux-only: this file's only caller is gated the same way,
/// since the other tests here don't need to reach into the process tree.
#[cfg(target_os = "linux")]
fn wait_for_child_pid(parent_pid: u32, comm: &str, timeout: Duration) -> Option<u32> {
    let deadline = Instant::now() + timeout;
    let children_path = format!("/proc/{parent_pid}/task/{parent_pid}/children");
    while Instant::now() < deadline {
        if let Ok(contents) = std::fs::read_to_string(&children_path) {
            for tok in contents.split_whitespace() {
                if let Ok(candidate) = tok.parse::<u32>() {
                    let comm_path = format!("/proc/{candidate}/comm");
                    if std::fs::read_to_string(&comm_path)
                        .map(|c| c.trim() == comm)
                        .unwrap_or(false)
                    {
                        return Some(candidate);
                    }
                }
            }
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    None
}

#[test]
fn view_paints_typed_text_in_a_pty() {
    let mut session = spawn_view_pty();

    session.send(b"ihello from view").unwrap();
    session.send(b"\x1b:wq\r").unwrap();
    let exit = session.wait().expect("view never exited after :wq");
    assert!(exit.success(), "view did not exit cleanly after :wq");

    // an echo-immune oracle: a screen-content assertion here would also
    // pass if the canonical-mode pty echoed these bytes back without nvim
    // ever processing them, since the vt100 parser can't tell the
    // difference between the terminal's own echo and a real redraw. The
    // saved file's contents can only be right if nvim_input actually
    // reached nvim's buffer.
    let saved = session.read_saved_file();
    assert!(
        saved.contains("hello from view"),
        "saved file did not contain the typed text; contents:\n{saved:?}"
    );
}

#[test]
fn view_starts_and_takes_input_under_a_pty_that_never_answers_capability_queries() {
    // `QueryPolicy::Silent` is what makes this test's name true: every other
    // test in this file gets a pty that answers the DA1 fence like a real
    // terminal, which lets the probe finish early and never reaches the
    // deadline path at all. Opting out of that reply here pins the property
    // the deadline path exists to protect: the startup probe (raw-mode-only,
    // pre-alt-screen) must never leave the terminal unresponsive or swallow
    // the first real keystroke once the alternate screen and nvim take over,
    // even though every one of its queries goes unanswered and it has to run
    // its full deadline out before giving up.
    //
    // Typing is sent with zero delay, straight after the pty is opened
    // (`spawn_view_pty_raw`, skipping `spawn_view_pty`'s own "wait for the
    // first redraw" step): that is the actual race the startup probe's
    // residue handling has to survive. Asserting only after a redraw has
    // already happened would let the race close before this test ever gets
    // to drive it, which is exactly how the original version of this test
    // stayed green even while the probe was silently discarding this input.
    //
    // The oracle is the saved file's real contents, not the pty's screen:
    // see `view_paints_typed_text_in_a_pty`'s comment for why a
    // screen-content assertion here would be vacuous.
    // An absolute wall-clock bound is only meaningful with exclusive CPU: take
    // the isolation window BEFORE the clock (its acquisition can block waiting
    // for in-flight sessions to drain, which is not the startup latency under
    // test) so a parallel run on few cores cannot inflate this measurement past
    // a bound that is really about the 50ms probe deadline, not host contention.
    let _exclusive = pty_isolation_exclusive();
    let start = Instant::now();
    let mut session = spawn_view_pty_raw_isolated(QueryPolicy::Silent);

    session.send(b"ibasic tier still works").unwrap();
    // A fixed sleep, not a screen-content wait, bridges to the save: the
    // canonical-mode echo of the bytes just sent would satisfy a
    // "screen is non-blank" check almost instantly, well before raw mode is
    // actually entered, making that signal itself part of the race rather
    // than a fix for it. This file's own module doc already documents fixed
    // sleeps over a deterministic "settled" signal as this suite's
    // tradeoff.
    std::thread::sleep(Duration::from_millis(500));
    session.send(b"\x1b:wq\r").unwrap();

    let exit = session.wait().expect("view never exited after :wq");
    assert!(exit.success(), "view did not exit cleanly after :wq");
    assert!(
        start.elapsed() < Duration::from_secs(3),
        "startup+save took {:?}, far longer than the probe's bounded \
         deadline plus ordinary nvim attach/save time should allow",
        start.elapsed()
    );

    let saved = session.read_saved_file();
    assert!(
        saved.contains("basic tier still works"),
        "saved file did not contain text typed immediately at spawn -- \
         startup-probe residue was silently dropped; contents:\n{saved:?}"
    );
}

#[test]
fn view_survives_a_promptly_replying_terminal_bursting_past_the_probes_chunk_size() {
    // Models a terminal that answers DA1 quickly, with the reply and a fast
    // typed/pasted burst landing together on the same fd inside the probe
    // window: exactly the shape `StdinReplySource::next_chunk`'s raw-fd
    // read exists to survive. `std::io::stdin()`'s shared, process-global
    // buffered handle can pull an entire multi-hundred-byte burst out of
    // the kernel in one syscall to satisfy a 256-byte-sized read, then
    // strand everything beyond those 256 bytes in its own userspace buffer
    // once `detect()`'s scan loop breaks on seeing the DA1 reply -- bytes a
    // raw fd read leaves sitting in the kernel's tty queue instead, still
    // visible to the real input path once `Term::init` hands off. The
    // burst below (276 bytes: a 6-byte DA1 reply, `i`, 260 `A`s, then a
    // distinct end marker) exceeds the 256-byte chunk size, so surviving it
    // intact proves the tail isn't getting orphaned in a buffered handle.
    //
    // `QueryPolicy::Silent`: this test writes the DA1 reply itself, glued to
    // the burst so both land in one read. A pty that also answered would get
    // its reply in first, on its own, letting the probe break out before the
    // burst ever arrived -- the co-arrival this test is built to exercise
    // would simply stop happening, and it would still pass.
    // An absolute wall-clock bound is only meaningful with exclusive CPU: take
    // the isolation window BEFORE the clock (its acquisition can block waiting
    // for in-flight sessions to drain, which is not the startup latency under
    // test) so a parallel run on few cores cannot inflate this measurement past
    // a bound that is really about the 50ms probe deadline, not host contention.
    let _exclusive = pty_isolation_exclusive();
    let start = Instant::now();
    let mut session = spawn_view_pty_raw_isolated(QueryPolicy::Silent);

    let da1_reply = b"\x1b[?62c";
    let payload = "A".repeat(260);
    let end_marker = "ENDMARKER";
    let mut burst = Vec::new();
    burst.extend_from_slice(da1_reply);
    burst.push(b'i');
    burst.extend_from_slice(payload.as_bytes());
    burst.extend_from_slice(end_marker.as_bytes());
    assert!(
        burst.len() > 256,
        "test burst must exceed the probe's 256-byte chunk size to actually \
         exercise the multi-chunk path, got {} bytes",
        burst.len()
    );
    session.send(&burst).unwrap();

    // A fixed sleep, not a screen-content wait, bridges to the save: see
    // the deadline-path test above for why a screen-content signal would
    // itself become part of the race here.
    std::thread::sleep(Duration::from_millis(500));
    session.send(b"\x1b:wq\r").unwrap();

    let exit = session.wait().expect("view never exited after :wq");
    assert!(exit.success(), "view did not exit cleanly after :wq");
    assert!(
        start.elapsed() < Duration::from_secs(3),
        "startup+save took {:?}, far longer than the probe's bounded \
         deadline plus ordinary nvim attach/save time should allow",
        start.elapsed()
    );

    let saved = session.read_saved_file();
    let expected_tail = format!("{payload}{end_marker}");
    assert!(
        saved.contains(&expected_tail),
        "saved file did not contain the full burst payload including the \
         end marker -- the burst's tail was likely stranded in a buffered \
         stdin handle instead of staying visible in the kernel for the \
         real input path to pick up; contents:\n{saved:?}"
    );
}

#[test]
fn a_startup_burst_that_nothing_follows_still_reaches_nvim_and_quits() {
    // The burst test above proves the startup probe hands its residue on;
    // this one proves the residue is acted on with no later input to prompt
    // it. The two differ by exactly one thing: there, the `:wq` is a second,
    // later write, and a write to the pty is itself a readiness edge on the
    // terminal fd. Here the DA1 reply, the typed text and the quit are one
    // burst already queued before `view` execs, so nothing ever arrives
    // again.
    //
    // That is the shape a two-reader terminal strands. Anything that asks
    // crossterm for an event moves the kernel's queued bytes into
    // crossterm's own userspace buffer; the kernel queue then reads empty,
    // and a readiness gate that consults only the raw fd concludes there is
    // nothing to drain. A later keystroke hides it completely -- the next
    // byte re-arms the fd and the buffered tail flushes along with it -- so
    // only a burst nothing follows can catch it. Unfixed, the session sits
    // until something kills it and the file is never written at all.
    //
    // `QueryPolicy::Silent` for the same reason the burst test uses it: the
    // DA1 reply is part of the burst, and a pty answering on its own would
    // let the probe finish before the burst was ever queued.
    let isolation = shared_isolation();

    let payload = "A".repeat(260);
    let end_marker = "ENDMARKER";
    let mut burst = Vec::new();
    burst.extend_from_slice(b"\x1b[?62c");
    burst.push(b'i');
    burst.extend_from_slice(payload.as_bytes());
    burst.extend_from_slice(end_marker.as_bytes());
    burst.extend_from_slice(b"\x1b:wq\r");
    assert!(
        burst.len() > 256,
        "the burst must exceed the probe's 256-byte chunk size to leave \
         residue at all, got {} bytes",
        burst.len()
    );

    let mut session = spawn_view_pty_preloaded(&burst, isolation, QueryPolicy::Silent);

    let exit = session.wait_for_exit(Duration::from_secs(20)).expect(
        "view never exited: the queued `:wq` never reached nvim, so the burst \
         is stranded in a buffer the readiness gate cannot see and no further \
         byte will ever arrive to release it",
    );
    assert!(
        exit.success(),
        "view did not exit cleanly after the queued :wq"
    );

    let saved = session.read_saved_file();
    let expected_tail = format!("{payload}{end_marker}");
    assert!(
        saved.contains(&expected_tail),
        "saved file did not contain the burst payload typed before startup \
         finished; contents:\n{saved:?}"
    );
}

#[test]
fn view_paints_wide_character_without_corrupting_neighbor_cell() {
    let mut session = spawn_view_pty();

    // "you", a double-width CJK character, immediately followed by an
    // ASCII neighbor: if the grid's wide-cell handling is off by one, this
    // is what would either overwrite or get overwritten by the adjacent
    // narrow cell.
    session.send("i你X".as_bytes()).unwrap();
    assert!(
        session.wait_for("你X", Duration::from_secs(5)),
        "screen never showed the CJK character next to its neighbor; last screen:\n{}",
        session.screen()
    );

    let screen = session.screen_raw();
    let not_found_msg = format!(
        "CJK character not found in any screen cell; last screen:\n{}",
        screen.contents()
    );
    let (row, col) = (0..24)
        .flat_map(|r| (0..78).map(move |c| (r, c)))
        .find(|&(r, c)| {
            screen
                .cell(r, c)
                .is_some_and(|cell| cell.contents() == "你")
        })
        .expect(&not_found_msg);

    let wide = screen.cell(row, col).unwrap();
    assert!(
        wide.is_wide(),
        "cell ({row},{col}) holding the CJK character was not flagged wide by the terminal parser"
    );

    let continuation = screen
        .cell(row, col + 1)
        .expect("wide character's continuation cell missing");
    assert!(
        continuation.is_wide_continuation(),
        "cell ({row},{}) was not marked as the wide character's continuation",
        col + 1
    );

    let neighbor = screen
        .cell(row, col + 2)
        .expect("neighbor cell after the wide character missing");
    assert_eq!(
        neighbor.contents(),
        "X",
        "neighbor cell corrupted by the adjacent wide character"
    );

    session.send(b"\x1b:q!\r").unwrap();
    let _ = session.wait();
}

/// Supervision's headline promise, end to end: view's engine is killed the
/// way a crash kills it, and the editor is still there, with a live engine
/// under it, painting what that engine sends. Until automatic recovery existed this same kill ended the
/// session with 128+SIGKILL; the exit-status mapping that pinned is pinned
/// now by the engine that stops because it was told to
/// (`view_propagates_cquit_exit_code`), which is the only stop that still
/// ends a session.
#[cfg(target_os = "linux")]
#[test]
fn view_recovers_its_own_engine_after_a_signal_death() {
    let mut session = spawn_view_pty();

    let view_pid = session.view_pid();
    let killed = wait_for_child_pid(view_pid, "nvim", Duration::from_secs(5))
        .expect("view never spawned an nvim child within the timeout");

    let kill_status = std::process::Command::new("kill")
        .arg("-KILL")
        .arg(killed.to_string())
        .status()
        .unwrap();
    assert!(kill_status.success(), "kill -KILL {killed} failed");

    let killed_at = Instant::now();

    // a *different* child, not merely a child: the pid answering here is
    // the whole difference between a session that recovered and one still
    // holding the corpse of the engine it lost
    let deadline = Instant::now() + Duration::from_secs(30);
    let mut replacement = None;
    while Instant::now() < deadline && replacement.is_none() {
        replacement = wait_for_child_pid(view_pid, "nvim", Duration::from_millis(25))
            .filter(|pid| *pid != killed);
    }
    let Some(replacement) = replacement else {
        panic!(
            "view never replaced the engine it lost within 30s; screen:\n{}",
            session.screen()
        );
    };
    assert_ne!(replacement, killed);
    // an unattended recovery is decided by the supervision fold, which the
    // loop reaches on its own readout tick rather than on a message -- so
    // the pass that decides it must go straight back to the top instead of
    // through the wait, or the user watches a frozen editor for another
    // whole `READOUT_RESOLUTION` after the decision was already made. Timed
    // to the spawn, not to the first paint, so this measures the loop's own
    // turnaround and not nvim's startup.
    let took = killed_at.elapsed();
    assert!(
        took < view_core::native::supervision::READOUT_RESOLUTION,
        "the replacement engine took {took:?} to start: an unattended \
         recovery must not wait on the readout timer"
    );

    // and the session is the user's again, driven end to end: the keystrokes
    // reach the replacement's input path and its answer comes back through
    // the pump the restart re-attached and the paint loop that drains it.
    //
    // An answer this test asks for rather than nvim's own recovery report,
    // which used to serve here: the report is taken off the screen now
    // without anyone typing anything
    // (`a_recovered_engine_says_so_and_clears_nvims_report_with_no_keypress`),
    // so reading it would be a race against the redraw that retires it.
    session.send(b"\x1b:echo \"recovered\"\r").unwrap();
    assert!(
        session.wait_for("recovered", Duration::from_secs(15)),
        "the replacement engine never painted anything through view; screen:\n{}",
        session.screen()
    );
}

/// nvim's own wording for a swap file it replayed. The engine is pinned, so
/// this string is as fixed as any other screen text asserted here, and a
/// restart that recovered nothing prints something else.
#[cfg(target_os = "linux")]
const SWAP_REPLAYED: &str = "Recovery completed";

/// What a user sees after a crash they did not cause and did not answer: the
/// buffer text the swap held, a line saying the recovery happened, and no
/// engine report box left sitting over the top of the file.
///
/// The keypress is the whole subject. nvim's multi-line recovery report
/// covers the buffer until something redraws over it, and until this chain
/// existed the only thing that did was a `<C-l>` the user had to know to
/// type. So nothing at all is sent between the kill and the assertions
/// below: every reading here is of a screen the session painted on its own.
#[cfg(target_os = "linux")]
#[test]
fn a_recovered_engine_says_so_and_clears_nvims_report_with_no_keypress() {
    let mut session = build_view_pty_with_content(
        &[],
        Some("on disk\n"),
        shared_isolation(),
        QueryPolicy::AnswerDa1,
    );
    assert!(
        session.wait_for("on disk", Duration::from_secs(15)),
        "the session never painted the file it was given; screen:\n{}",
        session.screen()
    );

    // never written to the file, so reading it back after the recovery can
    // only have come through nvim's own swap file
    const UNSAVED: &str = "zzunsavedzz";
    session.send(format!("o{UNSAVED}\x1b").as_bytes()).unwrap();
    assert!(
        session.wait_for(UNSAVED, Duration::from_secs(15)),
        "the unsaved line never reached the buffer; screen:\n{}",
        session.screen()
    );
    flush_swap_to_disk(&mut session);

    let view_pid = session.view_pid();
    let killed = wait_for_child_pid(view_pid, "nvim", Duration::from_secs(5))
        .expect("view never spawned an nvim child within the timeout");
    let kill_status = std::process::Command::new("kill")
        .arg("-KILL")
        .arg(killed.to_string())
        .status()
        .unwrap();
    assert!(kill_status.success(), "kill -KILL {killed} failed");

    let deadline = Instant::now() + Duration::from_secs(30);
    let mut replacement = None;
    while Instant::now() < deadline && replacement.is_none() {
        replacement = wait_for_child_pid(view_pid, "nvim", Duration::from_millis(25))
            .filter(|pid| *pid != killed);
    }
    assert!(
        replacement.is_some(),
        "view never replaced the engine it lost within 30s; screen:\n{}",
        session.screen()
    );

    let notice = view_core::native::supervision::swap_recovery_notice(1)
        .expect("a recovery of one swap file is worth a notice");
    let settled = session.wait_for_screen(Duration::from_secs(30), |screen| {
        let text = screen.contents();
        text.contains(UNSAVED) && text.contains(&notice) && !text.contains(SWAP_REPLAYED)
    });
    assert!(
        settled,
        "a recovery nobody answered left the screen wrong -- expected the \
         swap text {UNSAVED:?} and the notice {notice:?} with no {SWAP_REPLAYED:?} \
         report over them; screen:\n{}",
        session.screen()
    );
    assert!(
        !std::fs::read_to_string(&session.paths.scratch)
            .unwrap()
            .contains(UNSAVED),
        "{UNSAVED} is in the file on disk, so its return proves a re-read and \
         not a swap recovery"
    );
}

/// nvim's own error for a recovery it was asked to perform with no swap file
/// left to read. Pinned engine, so the code is as fixed as any other screen
/// text asserted here.
#[cfg(target_os = "linux")]
const NO_SWAP_TO_RECOVER: &str = "E305";

/// The other half of the recovery chain, and the destructive one: a session
/// whose engine dies having written no swap file at all.
///
/// `-n` is nvim's own switch for that, and it is not exotic -- `set
/// noswapfile` in a user's config, or an unwritable `'directory'`, reach the
/// same place, and supervision restarts the first few deaths without asking,
/// so nobody opts into it. The restart still asks for a recovery, nvim
/// answers `E305`, and the buffer comes up **empty** where the file's
/// contents should be: one `:w` from truncating the file.
///
/// So the assertions are the inverse of the success path's. The error must
/// still be on screen -- view neither raised it nor can attribute it, so it
/// is not view's to redraw away -- and view must say, in its own words, what
/// state the buffer is in. Nothing is typed here either: this is what the
/// session paints on its own.
#[cfg(target_os = "linux")]
#[test]
fn a_recovery_with_no_swap_to_read_keeps_nvims_error_and_says_the_buffer_is_empty() {
    let mut session = build_view_pty_with_content(
        &[std::ffi::OsStr::new("-n")],
        Some("on disk\n"),
        shared_isolation(),
        QueryPolicy::AnswerDa1,
    );
    assert!(
        session.wait_for("on disk", Duration::from_secs(15)),
        "the session never painted the file it was given; screen:\n{}",
        session.screen()
    );

    let view_pid = session.view_pid();
    let killed = wait_for_child_pid(view_pid, "nvim", Duration::from_secs(5))
        .expect("view never spawned an nvim child within the timeout");
    let kill_status = std::process::Command::new("kill")
        .arg("-KILL")
        .arg(killed.to_string())
        .status()
        .unwrap();
    assert!(kill_status.success(), "kill -KILL {killed} failed");

    let deadline = Instant::now() + Duration::from_secs(30);
    let mut replacement = None;
    while Instant::now() < deadline && replacement.is_none() {
        replacement = wait_for_child_pid(view_pid, "nvim", Duration::from_millis(25))
            .filter(|pid| *pid != killed);
    }
    assert!(
        replacement.is_some(),
        "view never replaced the engine it lost within 30s; screen:\n{}",
        session.screen()
    );

    let notice = view_core::native::supervision::swap_recovery_failure_notice("", true);
    let named = notice
        .split_once(" --")
        .map(|(said, _)| said.to_string())
        .unwrap_or(notice);
    let settled = session.wait_for_screen(Duration::from_secs(30), |screen| {
        let text = screen.contents();
        text.contains(NO_SWAP_TO_RECOVER) && text.contains(&named)
    });
    assert!(
        settled,
        "a recovery that could not happen left the user an empty buffer with \
         no account of itself -- expected nvim's {NO_SWAP_TO_RECOVER:?} still \
         standing and view's own {named:?} beside it; screen:\n{}",
        session.screen()
    );
}

/// The ordinary launch the recovery chain must stay silent through, driven
/// by the config error that reaches it most easily.
///
/// `:preserve` under `set noswapfile` raises `E313`, and it is not a contrived
/// pairing: auto-save plugins call `:preserve` on a timer, and swapfiles are
/// commonly off. That error is in the same numeric neighbourhood as the ones
/// a failed recovery raises and it is still standing in `v:errmsg` long after
/// the config that raised it finished, so a reading that only pattern-matched
/// the error would tell a user with a perfectly intact buffer that their text
/// was gone. Nothing here is a recovery at all: first launch, no crash, no
/// restart, no `-r`.
/// The config is a real `init.vim` and not a `--cmd`, because the two are
/// not interchangeable here: a pre-vimrc command's error is overwritten by
/// the ordinary `E216` nvim's own bundled plugins raise while loading, so it
/// is gone from `v:errmsg` before anything could misread it. A config's error
/// is raised after those plugins and stands.
#[cfg(target_os = "linux")]
#[test]
fn an_ordinary_launch_whose_config_errors_is_never_told_a_recovery_failed() {
    let paths = common::ScratchPaths::new("smoke");
    std::fs::write(&paths.scratch, "on disk\n").expect("scratch fixture must be writable");
    let config_dir = common::xdg_home(&paths.isolated_home, "XDG_CONFIG_HOME").join("nvim");
    std::fs::create_dir_all(&config_dir).expect("the isolated config dir must be creatable");
    std::fs::write(
        config_dir.join("init.vim"),
        "set noswapfile\nsilent! preserve\n",
    )
    .expect("the isolated config must be writable");

    let mut session = spawn_view_pty_at(paths, &[], shared_isolation(), QueryPolicy::AnswerDa1);

    // an absence is only worth reading against something that happened, and
    // both halves here are events rather than clocks. The screen is watched
    // from the spawn -- view's notices are transient toasts, so a reading
    // taken afterwards cannot tell a notice that was never raised from one
    // that has already expired, and this one would be raised before the
    // buffer paints at all. The watch then runs on until the session answers
    // a command sent after the attach, which nvim cannot answer ahead of the
    // recovery reading already queued in front of it, so the absence is
    // asserted against a reading that has been taken and folded
    let (painted, early) = watch_screen(&mut session, Duration::from_secs(30), FORBIDDEN, |text| {
        text.contains("on disk")
    });
    assert!(
        painted,
        "the session never painted the file it was given, so nothing here \
         watched a launch that got anywhere; screen:\n{}",
        session.screen()
    );
    const SETTLED: &str = "zzsettledzz";
    session.send(b"\x1b:echo 'zzsettled' . 'zz'\r").unwrap();
    let (answered, late) = watch_screen(&mut session, Duration::from_secs(30), FORBIDDEN, |text| {
        text.contains(SETTLED)
    });
    assert!(
        answered,
        "the session never answered a command, so nothing here established \
         that its recovery reading had been taken; screen:\n{}",
        session.screen()
    );
    assert!(
        !(early || late),
        "an ordinary launch was told something about a recovery; screen:\n{}",
        session.screen()
    );
}

/// The recovery lines an ordinary launch must never be shown: view's own
/// failure, damage and warning wordings all name the recovery, and the
/// success and warning wordings both name the work.
#[cfg(target_os = "linux")]
const FORBIDDEN: &[&str] = &[SAID_RECOVERY, RECOVERED_WORK];

/// What a user sees when the swap they were told is being replayed cannot be
/// read, and what they must never see: that their work came back.
///
/// Nothing here is exotic. A machine that dies mid-flush leaves a swap whose
/// blocks were never written, and the ordinary way a user meets it is the
/// next launch over that file -- no restart, no `-r`, just nvim finding a
/// swap and view answering the prompt on their behalf. The prompt is answered
/// identically whether the swap can be read or not, so a session that counted
/// the answered prompt would announce unsaved work that is gone, and redraw
/// away the engine's own account of where it went.
#[cfg(target_os = "linux")]
#[test]
fn a_swap_that_cannot_be_read_is_reported_as_a_failure_and_never_as_work_returned() {
    let paths = common::ScratchPaths::new("smoke");
    std::fs::write(&paths.scratch, "on disk\n").expect("scratch fixture must be writable");
    let swaps = paths.isolated_home.join("swap");
    plant_unreadable_swap(&paths.scratch, &swaps, &paths.isolated_home);

    let directory = std::ffi::OsString::from(format!("set directory={}", swaps.display()));
    let mut session = spawn_view_pty_at(
        paths,
        &[std::ffi::OsStr::new("--cmd"), &directory],
        shared_isolation(),
        QueryPolicy::AnswerDa1,
    );

    let named = said_part(&view_core::native::supervision::swap_recovery_failure_notice("", false));
    let (settled, claimed) = watch_screen(
        &mut session,
        Duration::from_secs(45),
        &[RECOVERED_WORK],
        |text| text.contains(UNREADABLE_SWAP) && text.contains(&named),
    );
    // the lie before the silence: a session that says nothing has at least
    // left the user looking at the engine's own error, and a session that
    // says the work came back has sent them away from it
    assert!(
        !claimed,
        "view told the user their unsaved work came back from a swap it could \
         not read; screen:\n{}",
        session.screen()
    );
    assert!(
        settled,
        "a recovery that could not read its swap left the user no account of \
         itself -- expected nvim's {UNREADABLE_SWAP:?} still standing and \
         view's own {named:?} beside it; screen:\n{}",
        session.screen()
    );
}

/// The same unreadable swap reached through the other path: view's own
/// restart, which carries `-r` and never meets a prompt at all.
///
/// Worth pinning apart from the launch above because nothing about the
/// reading looks the same on the two: one answers a prompt nvim raised, the
/// other replays a file nvim was told to replay and announces nothing. The
/// swap is corrupted between the death and the replacement, which is where a
/// half-written swap really comes from -- a `--nvim-bin` wrapper is the only
/// thing that runs in that gap.
#[cfg(target_os = "linux")]
#[test]
fn a_restart_that_cannot_read_the_swap_says_so_instead_of_going_silent() {
    let paths = common::ScratchPaths::new("smoke");
    std::fs::write(&paths.scratch, "on disk\n").expect("scratch fixture must be writable");
    let swaps = paths.isolated_home.join("swap");
    std::fs::create_dir_all(&swaps).expect("the swap dir must be creatable");
    let wrapper = write_swap_corrupting_nvim_wrapper(&swaps);

    let directory = std::ffi::OsString::from(format!("set directory={}", swaps.display()));
    let mut session = spawn_view_pty_at(
        paths,
        &[
            std::ffi::OsStr::new("--nvim-bin"),
            wrapper.path().as_os_str(),
            std::ffi::OsStr::new("--cmd"),
            &directory,
        ],
        shared_isolation(),
        QueryPolicy::AnswerDa1,
    );
    assert!(
        session.wait_for("on disk", Duration::from_secs(15)),
        "the session never painted the file it was given; screen:\n{}",
        session.screen()
    );

    const UNSAVED: &str = "zzunsavedzz";
    session.send(format!("o{UNSAVED}\x1b").as_bytes()).unwrap();
    assert!(
        session.wait_for(UNSAVED, Duration::from_secs(15)),
        "the unsaved line never reached the buffer; screen:\n{}",
        session.screen()
    );
    flush_swap_to_disk(&mut session);

    let view_pid = session.view_pid();
    let killed = wait_for_child_pid(view_pid, "nvim", Duration::from_secs(5))
        .expect("view never spawned an nvim child within the timeout");
    let kill_status = std::process::Command::new("kill")
        .arg("-KILL")
        .arg(killed.to_string())
        .status()
        .unwrap();
    assert!(kill_status.success(), "kill -KILL {killed} failed");

    let named = said_part(&view_core::native::supervision::swap_recovery_failure_notice("", false));
    let (settled, claimed) = watch_screen(
        &mut session,
        Duration::from_secs(45),
        &[RECOVERED_WORK],
        |text| text.contains(UNREADABLE_SWAP) && text.contains(&named),
    );
    assert!(
        !claimed,
        "view told the user their unsaved work came back from a swap it could \
         not read; screen:\n{}",
        session.screen()
    );
    assert!(
        settled,
        "a restart whose recovery could not read the swap said nothing at all, \
         leaving the work gone with no account of it -- expected nvim's \
         {UNREADABLE_SWAP:?} and view's own {named:?}; screen:\n{}",
        session.screen()
    );
}

/// What a user sees when the recovery ran, could not read all of it, and
/// left the buffer holding the rest.
///
/// The outcome between the two the other tests here drive: the work did not
/// come back whole, and it did not fail to come back either. nvim marks what
/// it could not read with `???` and says so in `E312`, so a session that
/// worded this as a failure would send a user looking for text that is on
/// their screen, and one that worded it as a success would leave them
/// editing a file with holes in it and no reason to look.
#[cfg(target_os = "linux")]
#[test]
fn a_recovery_that_came_back_with_holes_is_not_worded_as_one_that_failed() {
    let paths = common::ScratchPaths::new("smoke");
    std::fs::write(&paths.scratch, "on disk\n").expect("scratch fixture must be writable");
    let swaps = paths.isolated_home.join("swap");
    plant_swap_truncated_to(
        &paths.scratch,
        &swaps,
        &paths.isolated_home,
        SWAP_HEADER_BYTES + SWAP_BLOCK_BYTES,
    );

    let directory = std::ffi::OsString::from(format!("set directory={}", swaps.display()));
    let mut session = spawn_view_pty_at(
        paths,
        &[std::ffi::OsStr::new("--cmd"), &directory],
        shared_isolation(),
        QueryPolicy::AnswerDa1,
    );

    let named = said_part(&view_core::native::supervision::swap_recovery_damage_notice(""));
    let failed =
        said_part(&view_core::native::supervision::swap_recovery_failure_notice("", false));
    let (settled, claimed) = watch_screen(
        &mut session,
        Duration::from_secs(45),
        &[failed.as_str()],
        |text| text.contains(DAMAGED_SWAP) && text.contains(&named),
    );
    assert!(
        !claimed,
        "a recovery that came back with the work in it was reported as a \
         failure, sending the user looking for text they are looking at; \
         screen:\n{}",
        session.screen()
    );
    assert!(
        settled,
        "a recovery that came back with holes in it said nothing about them \
         -- expected nvim's {DAMAGED_SWAP:?} still standing and view's own \
         {named:?} beside it; screen:\n{}",
        session.screen()
    );
}

/// The warning class, through view's own restart: the work comes back and
/// the file it belongs to moved while the session was gone.
///
/// Not exotic, and not the same moment as the crash: a branch switch, a
/// formatter, or any second writer between the death and the restart leaves
/// the file on disk newer than the swap, and view's restarts are not all
/// immediate -- a remote reconnect spends its whole backoff first, and the
/// modal path waits on the user. nvim recovers the work anyway and warns
/// with `E308`, and the warning is the part the user acts on: the `:w` that
/// follows overwrites whatever moved the file.
///
/// So this pins three things a `-r` recovery must do that it did not do
/// until the window opened before nvim's replay: the work is back, view's
/// line says the work is back, and the engine's warning is still standing on
/// screen rather than redrawn away by a session announcing an unqualified
/// success.
#[cfg(target_os = "linux")]
#[test]
fn a_restart_whose_file_moved_under_the_swap_recovers_the_work_and_keeps_the_warning() {
    let paths = common::ScratchPaths::new("smoke");
    std::fs::write(&paths.scratch, "on disk\n").expect("scratch fixture must be writable");
    let swaps = paths.isolated_home.join("swap");
    std::fs::create_dir_all(&swaps).expect("the swap dir must be creatable");
    let wrapper = write_file_moving_nvim_wrapper(&paths.scratch);

    let directory = std::ffi::OsString::from(format!("set directory={}", swaps.display()));
    let mut session = spawn_view_pty_at(
        paths,
        &[
            std::ffi::OsStr::new("--nvim-bin"),
            wrapper.path().as_os_str(),
            std::ffi::OsStr::new("--cmd"),
            &directory,
        ],
        shared_isolation(),
        QueryPolicy::AnswerDa1,
    );
    assert!(
        session.wait_for("on disk", Duration::from_secs(15)),
        "the session never painted the file it was given; screen:\n{}",
        session.screen()
    );

    const UNSAVED: &str = "zzunsavedzz";
    session.send(format!("o{UNSAVED}\x1b").as_bytes()).unwrap();
    assert!(
        session.wait_for(UNSAVED, Duration::from_secs(15)),
        "the unsaved line never reached the buffer; screen:\n{}",
        session.screen()
    );
    flush_swap_to_disk(&mut session);

    let view_pid = session.view_pid();
    let killed = wait_for_child_pid(view_pid, "nvim", Duration::from_secs(5))
        .expect("view never spawned an nvim child within the timeout");
    let kill_status = std::process::Command::new("kill")
        .arg("-KILL")
        .arg(killed.to_string())
        .status()
        .unwrap();
    assert!(kill_status.success(), "kill -KILL {killed} failed");

    let named = said_part(&view_core::native::supervision::swap_recovery_warning_notice(1, ""));
    let failed =
        said_part(&view_core::native::supervision::swap_recovery_failure_notice("", false));
    let (settled, claimed) = watch_screen(
        &mut session,
        Duration::from_secs(45),
        &[failed.as_str()],
        |text| text.contains(&named) && text.contains(CHANGED_UNDER_SWAP),
    );
    assert!(
        !claimed,
        "a recovery that brought the work back was reported as a failure; \
         screen:\n{}",
        session.screen()
    );
    assert!(
        settled,
        "a restart whose file moved under its swap never said so -- expected \
         view's own {named:?} and the engine's {CHANGED_UNDER_SWAP:?} \
         standing together; screen:\n{}",
        session.screen()
    );

    // and the work really is under that report: the redraw is the user's to
    // ask for here, which is the whole difference from the unqualified
    // success -- a session that took the report down on its own would have
    // taken the engine's warning with it
    session.send(b"\x1b\x0c").unwrap();
    assert!(
        session.wait_for(UNSAVED, Duration::from_secs(15)),
        "the recovery view announced brought nothing back; screen:\n{}",
        session.screen()
    );
}

/// The silent failure: a recovery that cannot make a swap file of its own,
/// which nvim refuses to start and never says a word about on screen.
///
/// `'directory'` refusing a new file is not exotic -- a read-only mount, a
/// full or quota-bound filesystem, a swap directory owned by another user --
/// and the restart that meets it is unattended, so nobody opted into it. The
/// recovery does not happen: the buffer comes up **empty** where the file's
/// contents should be, exactly as the no-swap case does, and one `:w`
/// truncates the file on disk.
///
/// What makes this one worse than every other failure here is that nvim
/// paints nothing. The other failures leave their own error on screen and
/// view's line is the framing beside it; here view's line is the **only**
/// account there is, and a reading that dropped this error left the user an
/// empty buffer, a swap banner naming a recovery that did not happen, and
/// nothing else.
#[cfg(target_os = "linux")]
#[test]
fn a_restart_that_cannot_make_a_swap_of_its_own_says_the_recovery_failed() {
    let paths = common::ScratchPaths::new("smoke");
    std::fs::write(&paths.scratch, "on disk\n").expect("scratch fixture must be writable");
    let swaps = paths.isolated_home.join("swap");
    std::fs::create_dir_all(&swaps).expect("the swap dir must be creatable");
    assert!(
        UncreatableDir::refuses_a_new_file(&swaps),
        "neither `chattr +i` nor mode 0500 makes {} refuse a new file on \
         this host, so nothing here would drive the recovery this test is \
         about -- a fixture this host cannot express, not a recovery that \
         regressed",
        swaps.display()
    );
    let wrapper = write_swap_dir_locking_nvim_wrapper(&swaps);

    let directory = std::ffi::OsString::from(format!("set directory={}", swaps.display()));
    let mut session = spawn_view_pty_at(
        paths,
        &[
            std::ffi::OsStr::new("--nvim-bin"),
            wrapper.path().as_os_str(),
            std::ffi::OsStr::new("--cmd"),
            &directory,
        ],
        shared_isolation(),
        QueryPolicy::AnswerDa1,
    );
    // declared after the session so it drops before it: whatever the wrapper
    // does to that directory is undone even if an assertion below unwinds
    // first, and undone while the session's scratch tree can still be
    // removed -- an immutable directory defeats the removal silently, and
    // an unlock arriving after it has nothing left to retry
    let _unlock_at_exit = UncreatableDir::new(&swaps);
    assert!(
        session.wait_for("on disk", Duration::from_secs(15)),
        "the session never painted the file it was given; screen:\n{}",
        session.screen()
    );

    const UNSAVED: &str = "zzunsavedzz";
    session.send(format!("o{UNSAVED}\x1b").as_bytes()).unwrap();
    assert!(
        session.wait_for(UNSAVED, Duration::from_secs(15)),
        "the unsaved line never reached the buffer; screen:\n{}",
        session.screen()
    );
    flush_swap_to_disk(&mut session);

    let view_pid = session.view_pid();
    let killed = wait_for_child_pid(view_pid, "nvim", Duration::from_secs(5))
        .expect("view never spawned an nvim child within the timeout");
    let kill_status = std::process::Command::new("kill")
        .arg("-KILL")
        .arg(killed.to_string())
        .status()
        .unwrap();
    assert!(kill_status.success(), "kill -KILL {killed} failed");

    let named = said_part(&view_core::native::supervision::swap_recovery_failure_notice("", true));
    let (settled, claimed) = watch_screen(
        &mut session,
        Duration::from_secs(45),
        &[RECOVERED_WORK],
        |text| text.contains(&named) && text.contains(NO_SWAP_TO_MAKE),
    );
    assert!(
        !claimed,
        "view told the user their unsaved work came back from a recovery \
         that never ran; screen:\n{}",
        session.screen()
    );
    assert!(
        settled,
        "a recovery that could not make a swap file left the user an empty \
         buffer with no account of it anywhere -- expected view's own \
         {named:?} carrying the engine's {NO_SWAP_TO_MAKE:?}, which nothing \
         else on this screen says; screen:\n{}",
        session.screen()
    );
}

/// nvim's own error for a swap file it cannot create, which it raises for
/// the file it was told to recover and then gives up on the recovery.
///
/// The same code an ordinary session raises for its own swap file, which is
/// why the reading that admits it is keyed on the file the message names
/// rather than on the code (see `SWAP_RECOVERY_PROBE`).
#[cfg(target_os = "linux")]
const NO_SWAP_TO_MAKE: &str = "E303";

/// A swap directory that refuses new files, and puts itself back however it
/// was refused.
///
/// Two mechanisms because two callers: an unprivileged user is refused by
/// mode bits, and root is not -- root goes through `chattr +i`, which the
/// kernel enforces against every user. Which one applies is decided by
/// trying, never by asking who is running, so a host that answers neither
/// way is a loud failure rather than a test that quietly proves nothing.
#[cfg(target_os = "linux")]
struct UncreatableDir {
    path: PathBuf,
}

#[cfg(target_os = "linux")]
impl UncreatableDir {
    /// Adopts `path` for cleanup, without touching it: the wrapper is what
    /// locks it, and this is what guarantees the unlock.
    fn new(path: &std::path::Path) -> Self {
        Self {
            path: path.to_path_buf(),
        }
    }

    /// Whether this host can make `path` refuse a new file at all, leaving
    /// it creatable again either way.
    fn refuses_a_new_file(path: &std::path::Path) -> bool {
        lock_directory(path);
        let refused = std::fs::write(path.join("zz-precondition"), "x").is_err();
        unlock_directory(path);
        let _ = std::fs::remove_file(path.join("zz-precondition"));
        refused
    }
}

#[cfg(target_os = "linux")]
impl Drop for UncreatableDir {
    fn drop(&mut self) {
        unlock_directory(&self.path);
    }
}

/// Makes `path` refuse new files, by whichever of the two mechanisms this
/// user has. Written as free functions because the wrapper script performs
/// the same two steps in shell, and the pair has to stay readable as one
/// thing.
#[cfg(target_os = "linux")]
fn lock_directory(path: &std::path::Path) {
    let immutable = std::process::Command::new("chattr")
        .arg("+i")
        .arg(path)
        .status()
        .map(|status| status.success())
        .unwrap_or(false);
    if !immutable {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o500));
    }
}

/// Undoes [`lock_directory`], both ways, whichever one took.
#[cfg(target_os = "linux")]
fn unlock_directory(path: &std::path::Path) {
    let _ = std::process::Command::new("chattr")
        .arg("-i")
        .arg(path)
        .status();
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755));
}

/// The real engine binary these wrappers stand in front of.
#[cfg(unix)]
fn real_nvim_path() -> String {
    String::from_utf8(
        std::process::Command::new("which")
            .arg("nvim")
            .output()
            .expect("which nvim failed")
            .stdout,
    )
    .expect("non-utf8 which output")
    .trim()
    .to_string()
}

/// A `--nvim-bin` wrapper script that takes itself, and anything it wrote
/// beside itself, away when the test that built it ends.
///
/// Each wrapper gets a directory of its own under the scratch root, named by
/// pid and a per-call counter: every test in this binary runs as a thread of
/// the same process, so a shared name lets one test's cleanup race another's
/// in-flight `view --nvim-bin <wrapper>` spawn -- the collision
/// `ScratchPaths` documents and avoids the same way. Removing the directory
/// rather than the file is what lets a script keep state between spawns (a
/// counter, say) without that state outliving the test either.
///
/// The removal is a `Drop` rather than a line at the end of each test
/// because a failing assertion unwinds past that line, and a wrapper left in
/// the shared scratch root outlives the run entirely: the root grew by one
/// file per wrapper-using test per run, without bound, and an immutable
/// directory among them can turn a later `rm -rf target` into a failure.
#[cfg(unix)]
struct WrapperScript {
    dir: PathBuf,
    path: PathBuf,
}

#[cfg(unix)]
impl WrapperScript {
    /// Writes an executable `/bin/sh` wrapper whose body `script` builds,
    /// given the directory the wrapper may write its own state into.
    fn new(kind: &str, script: impl FnOnce(&std::path::Path) -> String) -> Self {
        static NEXT_WRAPPER_ID: AtomicU64 = AtomicU64::new(0);

        let id = NEXT_WRAPPER_ID.fetch_add(1, Ordering::Relaxed);
        let dir = common::scratch_root().join(format!("{kind}-{}-{id}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("failed to create the wrapper's own directory");
        let path = dir.join("nvim");
        std::fs::write(&path, script(&dir)).expect("failed to write the wrapper script");

        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(&path).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&path, perms).unwrap();

        Self { dir, path }
    }

    /// What to hand `--nvim-bin`.
    fn path(&self) -> &std::path::Path {
        &self.path
    }
}

#[cfg(unix)]
impl Drop for WrapperScript {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// A `--nvim-bin` wrapper that makes `swaps` refuse new files on its way to
/// a recovering engine, and leaves every other spawn alone.
///
/// The refusal has to arrive between the death and the replacement -- before
/// it, the session under test could never write the swap this recovery is
/// supposed to read. The `-r` the restart carries is what tells the two
/// spawns apart, and it is the engine's own flag rather than anything this
/// test invents.
#[cfg(target_os = "linux")]
fn write_swap_dir_locking_nvim_wrapper(swaps: &std::path::Path) -> WrapperScript {
    let real_nvim = real_nvim_path();
    WrapperScript::new("locking-nvim", |_| {
        format!(
            "#!/bin/sh\nfor arg in \"$@\"; do\n  if [ \"$arg\" = \"-r\" ]; then\n    \
             chattr +i {swaps} 2>/dev/null || chmod 0500 {swaps}\n    break\n  fi\ndone\n\
             exec {real_nvim} \"$@\"\n",
            swaps = swaps.display(),
        )
    })
}

/// nvim's own error for a swap file whose blocks it cannot read back. Pinned
/// engine, and pinned corruption: [`plant_unreadable_swap`] drops every block
/// but the header, which is what a process dying mid-flush leaves.
#[cfg(target_os = "linux")]
const UNREADABLE_SWAP: &str = "E309";

/// nvim's own error for a recovery that ran to the end over blocks it could
/// not all read, and marked what is missing in the buffer.
#[cfg(target_os = "linux")]
const DAMAGED_SWAP: &str = "E312";

/// nvim's own warning for a recovery whose file on disk no longer matches
/// the one the swap was written against.
#[cfg(target_os = "linux")]
const CHANGED_UNDER_SWAP: &str = "E308";

/// The success wording, named here so the failure tests can assert it never
/// appears -- the harm they guard is not silence, it is a session announcing
/// work that is gone.
#[cfg(target_os = "linux")]
const RECOVERED_WORK: &str = "unsaved changes recovered";

/// The part every failure, damage and warning wording shares, for a test
/// whose claim is that none of them was raised.
#[cfg(target_os = "linux")]
const SAID_RECOVERY: &str = "swap recovery";

/// What a notice says before it starts quoting the engine, so a test can
/// assert view's own wording without pinning an error text view did not
/// write.
#[cfg(target_os = "linux")]
fn said_part(notice: &str) -> String {
    notice
        .split_once(" --")
        .map_or_else(|| notice.to_string(), |(said, _)| said.to_string())
}

/// Samples the screen until `settled` holds or `timeout` elapses, answering
/// both whether it settled and whether any of `forbidden` was on screen at
/// any point on the way there.
///
/// Both readings have to come from the same sampling. view's notices expire
/// on their own after a few seconds, so a forbidden line looked for once at
/// the end is a line that had the whole window to appear and vanish
/// unobserved.
#[cfg(target_os = "linux")]
fn watch_screen(
    session: &mut ViewPtySession,
    timeout: Duration,
    forbidden: &[&str],
    settled: impl Fn(&str) -> bool,
) -> (bool, bool) {
    let deadline = Instant::now() + timeout;
    let mut seen = false;
    loop {
        let text = session.screen();
        seen |= forbidden.iter().any(|line| text.contains(line));
        if settled(&text) {
            return (true, seen);
        }
        if Instant::now() >= deadline {
            return (false, seen);
        }
        std::thread::sleep(Duration::from_millis(25));
    }
}

/// Puts the buffer's unsaved work in its swap file on disk, and waits for
/// the engine's own answer that the flush has run.
///
/// `:preserve` rather than a wait on nvim's idle flush: the swap is what a
/// recovery replays, and a test that raced the flush would fail for the
/// clock rather than for the recovery. The echo is the barrier -- nvim runs
/// one command line in order, so the swap is on disk by the time its answer
/// is readable, and killing on the send alone hands the replacement a
/// half-written swap (`E309`) instead.
///
/// The answer is spelled as a concatenation because the screen scan covers
/// the command line too: a barrier echoing its own literal is satisfied by
/// the keystrokes that type it rather than by the flush that follows, which
/// on a host loaded enough to widen that window is exactly the `E309` the
/// barrier exists to prevent.
#[cfg(target_os = "linux")]
fn flush_swap_to_disk(session: &mut ViewPtySession) {
    const PRESERVED: &str = "zzpreservedzz";
    const TYPED: &str = ":preserve | echo 'zzpreserv' . 'edzz'\r";
    assert!(
        !TYPED.contains(PRESERVED),
        "the barrier types the answer it waits for, so the keystrokes \
         satisfy it before the flush they were meant to follow"
    );
    session.send(TYPED.as_bytes()).unwrap();
    assert!(
        session.wait_for(PRESERVED, Duration::from_secs(15)),
        "the swap was never flushed; screen:\n{}",
        session.screen()
    );
}

/// Leaves a swap file under `swaps` that nvim will find for `scratch` and
/// then fail to read.
///
/// Written by a real engine rather than synthesized: a swap nvim does not
/// recognize as this file's is one it never offers to recover, so the fixture
/// has to be genuine and then lose its blocks. `:preserve` is what puts it on
/// disk, the kill is what leaves it there, and truncating to the header is
/// what a process dying mid-flush leaves behind.
#[cfg(target_os = "linux")]
fn plant_unreadable_swap(
    scratch: &std::path::Path,
    swaps: &std::path::Path,
    home: &std::path::Path,
) {
    plant_swap_truncated_to(scratch, swaps, home, SWAP_HEADER_BYTES);
}

/// [`plant_unreadable_swap`]'s general form: the same genuine swap, cut to
/// `bytes`.
///
/// How much of a swap survived decides what nvim does with it, and the two
/// answers are different outcomes rather than degrees of one: cut to the
/// header alone the recovery cannot start, cut one block further it runs and
/// comes back with holes. A fixture that could only produce the first can
/// only ever drive the failure wording.
#[cfg(target_os = "linux")]
fn plant_swap_truncated_to(
    scratch: &std::path::Path,
    swaps: &std::path::Path,
    home: &std::path::Path,
    bytes: u64,
) {
    std::fs::create_dir_all(swaps).expect("the swap dir must be creatable");
    let ready = swaps.join("planted");
    let mut command = std::process::Command::new("nvim");
    command
        .arg("--headless")
        .arg("--cmd")
        .arg(format!("set directory={}", swaps.display()))
        .arg("-c")
        .arg("normal Gozzplantedzz")
        .arg("-c")
        .arg("preserve")
        .arg("-c")
        .arg(format!(
            "call writefile(['planted'], '{}')",
            ready.display()
        ))
        .arg("-c")
        .arg("while 1 | sleep 100m | endwhile")
        .arg(scratch)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    for var in [
        "XDG_CONFIG_HOME",
        "XDG_DATA_HOME",
        "XDG_STATE_HOME",
        "XDG_CACHE_HOME",
    ] {
        command.env(var, common::xdg_home(home, var));
    }
    let mut child = command
        .spawn()
        .expect("failed to spawn the swap-planting engine");

    let deadline = Instant::now() + Duration::from_secs(30);
    while Instant::now() < deadline && !ready.exists() {
        std::thread::sleep(Duration::from_millis(25));
    }
    let planted = ready.exists();
    let _ = child.kill();
    let _ = child.wait();
    assert!(
        planted,
        "the swap-planting engine never flushed its swap file"
    );

    let swap = std::fs::read_dir(swaps)
        .expect("the swap dir must be readable")
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .find(|path| path.extension().is_some_and(|ext| ext == "swp"))
        .expect("the planting engine left no swap file behind");
    std::fs::OpenOptions::new()
        .write(true)
        .open(&swap)
        .expect("the planted swap must be writable")
        .set_len(bytes)
        .expect("the planted swap must be truncatable");
}

/// One swap block, which is what a swap file is left with when everything
/// after its header is lost. nvim reads the header to recognize the file and
/// offer the recovery, then fails on the first block it needs.
#[cfg(target_os = "linux")]
const SWAP_HEADER_BYTES: u64 = 4096;

/// One block past that header, which is where the recovery stops failing and
/// starts succeeding badly: nvim reads the pointer block it needs to begin,
/// runs out of file partway through, and finishes with the missing lines
/// marked in the buffer. Measured against the pinned engine on both the
/// prompt and the `-r` path.
#[cfg(target_os = "linux")]
const SWAP_BLOCK_BYTES: u64 = 4096;

/// A `--nvim-bin` wrapper that truncates any swap file under `swaps` on its
/// way to the real engine.
///
/// The corruption has to happen between one engine's death and its
/// replacement's spawn, and the wrapper is the only thing that runs there.
/// The first spawn of a session finds no swap and passes straight through.
#[cfg(target_os = "linux")]
fn write_swap_corrupting_nvim_wrapper(swaps: &std::path::Path) -> WrapperScript {
    let real_nvim = real_nvim_path();
    WrapperScript::new("corrupting-nvim", |_| {
        format!(
            "#!/bin/sh\nfor swap in {}/*.swp; do\n  [ -f \"$swap\" ] && truncate -s {SWAP_HEADER_BYTES} \"$swap\"\ndone\nexec {real_nvim} \"$@\"\n",
            swaps.display()
        )
    })
}

/// A `--nvim-bin` wrapper that rewrites `file` on its way to the real
/// engine, so every spawn finds it holding different content at a later
/// timestamp than the spawn before saw.
///
/// What a second writer does to a file while the session that was editing it
/// is dead, and the wrapper is the only thing that runs in that gap. Both
/// halves are needed: nvim compares the swap's record of the file against
/// what is on disk, and a rewrite alone can land inside the same second the
/// swap recorded. The counter the script keeps beside itself is what puts
/// the timestamps in order across successive spawns.
#[cfg(target_os = "linux")]
fn write_file_moving_nvim_wrapper(file: &std::path::Path) -> WrapperScript {
    let real_nvim = real_nvim_path();
    WrapperScript::new("moving-nvim", |dir| {
        format!(
            "#!/bin/sh\nn=$(cat {counter} 2>/dev/null || echo 0)\nn=$((n + 1))\necho \"$n\" > {counter}\n\
             printf 'on disk\\nrewritten %s\\n' \"$n\" > {file}\ntouch -d \"+$n hours\" {file}\nexec {real_nvim} \"$@\"\n",
            counter = dir.join("count").display(),
            file = file.display(),
        )
    })
}

/// The restart the other recovery test cannot reach: this one replaces an
/// engine whose reader thread is *still alive* at the moment of the swap.
///
/// A signal-killed engine is already reaped by the time its replacement is
/// asked for, so the reader has spoken its one terminal message and gone
/// before the swap -- the stop and the restart are ordered by the death
/// itself. A wedged engine is not: `Engine::restart` is what ends it, and
/// the reader it shared a connection with reports that ending *after* the
/// replacement is attached and running. That report names a connection this
/// session no longer has, and acting on it would tear down a healthy engine
/// (or, with unattended recovery on, restart one that never died). So the
/// assertion is a count, not a liveness check: exactly one replacement, and
/// it keeps running.
///
/// Slow by construction -- the modal offering `<F5>` is deliberately behind
/// `HEARTBEAT_WEDGE_THRESHOLD` + `ENGINE_BUSY_MODAL_THRESHOLD` (10s + 30s),
/// because a user must never be interrupted for a hiccup. There is no
/// shorter route to a restart of a live engine.
#[cfg(target_os = "linux")]
#[test]
fn a_restart_of_a_still_running_engine_leaves_exactly_one_replacement() {
    let mut session = spawn_view_pty();

    let view_pid = session.view_pid();
    let wedged = wait_for_child_pid(view_pid, "nvim", Duration::from_secs(5))
        .expect("view never spawned an nvim child within the timeout");

    // SIGSTOP rather than a busy loop inside nvim: both stop the engine
    // answering heartbeats with its reader thread intact, but a stopped
    // child costs the host nothing, and a suite that runs on a contended
    // box must not spend 40 seconds of a core spinning in `:lua`
    let stop_status = std::process::Command::new("kill")
        .arg("-STOP")
        .arg(wedged.to_string())
        .status()
        .unwrap();
    assert!(stop_status.success(), "kill -STOP {wedged} failed");

    assert!(
        session.wait_for("Restart", Duration::from_secs(90)),
        "the wedge modal never offered a restart; screen:\n{}",
        session.screen()
    );
    // `<F5>` as the terminal sends it
    session.send(b"\x1b[15~").unwrap();

    let deadline = Instant::now() + Duration::from_secs(30);
    let mut replacement = None;
    while Instant::now() < deadline && replacement.is_none() {
        replacement = wait_for_child_pid(view_pid, "nvim", Duration::from_millis(250))
            .filter(|pid| *pid != wedged);
    }
    let Some(replacement) = replacement else {
        panic!(
            "the restart never produced a different engine within 30s; screen:\n{}",
            session.screen()
        );
    };

    // the window in which the dead reader's report arrives: it is sent the
    // moment the old child is reaped, which is inside the restart above, so
    // anything it was going to do has happened well before this ends
    let watch_until = Instant::now() + Duration::from_secs(10);
    while Instant::now() < watch_until {
        if let Some(pid) = wait_for_child_pid(view_pid, "nvim", Duration::from_millis(500)) {
            assert_eq!(
                pid, replacement,
                "a third engine appeared: the replaced engine's terminal \
                 message was acted on against the session that replaced it"
            );
        }
    }

    // and the session is the user's again, not a screen with a modal still
    // on it over an engine nobody is driving
    session.send(b"\x1b:echo \"restarted\"\r").unwrap();
    assert!(
        session.wait_for("restarted", Duration::from_secs(15)),
        "the replacement engine never answered a command; screen:\n{}",
        session.screen()
    );
}

#[test]
fn view_shows_an_echoed_message() {
    let mut session = spawn_view_pty();

    session.send(b"\x1b:echo \"hi\"\r").unwrap();
    assert!(
        session.wait_for("hi", Duration::from_secs(5)),
        "screen never showed the echoed message text; last screen:\n{}",
        session.screen()
    );

    session.send(b"\x1b:q!\r").unwrap();
    let _ = session.wait();
}

// A transient toast (Route::Transient in view-core's toast.rs) carries a
// ScheduleToastExpiry effect that reaps it after TRANSIENT_TOAST_TIMEOUT
// with no further input at all. The paint loop is otherwise timer-free (it
// only repaints in response to a Msg), so this is the one falsifiable proof
// that the timer-thread design actually wakes an idle editor rather than
// only expiring toasts a later keystroke happens to paint over. EngineSession
// (the in-process oracle driver used elsewhere in this crate) drops
// Effect::ScheduleToastExpiry in apply_effects, since it only forwards
// Effect::Rpc -- only a real pty against the compiled binary, where
// view::runtime::Executor actually runs the effect, can exercise this.
#[test]
fn a_transient_toast_expires_on_its_own_after_the_idle_timeout() {
    let mut session = spawn_view_pty();

    session.send(b"\x1b:echo 'sometoken'\r").unwrap();
    assert!(
        session.wait_for("sometoken", Duration::from_secs(5)),
        "screen never showed the echoed transient message; last screen:\n{}",
        session.screen()
    );

    // No further input from here on: proves expiry is driven by the
    // ToastExpired timer, not by some other event's repaint incidentally
    // dropping the entry.
    let margin = view_core::native::toast::TRANSIENT_TOAST_TIMEOUT + Duration::from_secs(4);
    assert!(
        session.wait_for_screen(margin, |screen| !screen.contents().contains("sometoken")),
        "transient toast never expired while the editor sat idle; last screen:\n{}",
        session.screen()
    );

    session.send(b"\x1b:q!\r").unwrap();
    let _ = session.wait();
}

// The falsifiable counterpart to the transient-expiry test above: an emsg
// carries no ScheduleToastExpiry effect at all (route() sends persistent
// kinds to Route::Sticky, and timeout_for only returns Some for
// Route::Transient), so the same idle wait that reaps a transient toast
// must never touch it.
#[test]
fn a_persistent_emsg_survives_the_same_idle_wait_a_transient_toast_does_not() {
    let mut session = spawn_view_pty();

    session.send(b"\x1b:echoerr 'errtoken'\r").unwrap();
    assert!(
        session.wait_for("errtoken", Duration::from_secs(5)),
        "screen never showed the echoerr message; last screen:\n{}",
        session.screen()
    );

    std::thread::sleep(view_core::native::toast::TRANSIENT_TOAST_TIMEOUT + Duration::from_secs(4));
    let screen = session.screen();
    assert!(
        screen.contains("errtoken"),
        "persistent emsg vanished after an idle wait that should only reap transient toasts; \
         last screen:\n{screen}"
    );

    session.send(b"\x1b:q!\r").unwrap();
    let _ = session.wait();
}

// The choke-point-unification counterpart to the wire-sourced test above.
// `:View` with no args reaches `Msg::FeatureInvoke { feature: "", verb: "" }`
// (see native/mappings.rs's Lua-side registration: `opts.fargs[1] or ''`).
// A native notice must flow through the same classify/expire/history path a
// wire toast does, or it sits on screen forever on an idle editor -- this
// proves the live-process half of that: the same idle wait that reaps a
// wire-sourced transient toast also reaps a locally-synthesized one, driven
// by the same ScheduleToastExpiry/toast-timer machinery. The other half --
// that the entry also lands in ToastHistory -- is proven at the unit level
// in view-core's update.rs, where ToastHistory's contents are directly
// inspectable; this pty test stays scoped to the live-process expiry
// timing alone rather than also driving the message-history overlay.
#[test]
fn a_native_notice_expires_on_its_own_after_the_idle_timeout_same_as_a_wire_toast() {
    let mut session = spawn_view_pty();

    session.send(b"\x1b:View\r").unwrap();
    assert!(
        session.wait_for("needs a feature and a verb", Duration::from_secs(5)),
        "screen never showed the native invoke notice; last screen:\n{}",
        session.screen()
    );

    // No further input from here on: proves expiry is driven by the
    // ToastExpired timer, not by some other event's repaint incidentally
    // dropping the entry.
    let margin = view_core::native::toast::TRANSIENT_TOAST_TIMEOUT + Duration::from_secs(4);
    assert!(
        session.wait_for_screen(margin, |screen| {
            !screen.contents().contains("needs a feature and a verb")
        }),
        "native notice never expired while the editor sat idle; last screen:\n{}",
        session.screen()
    );

    session.send(b"\x1b:q!\r").unwrap();
    let _ = session.wait();
}

#[test]
fn view_shows_the_cmdline_prefix_inside_the_command_palette_while_typing_a_command() {
    // the command palette (native.palette) paints `:`'s typed line as a
    // centered floating box rather than the grid's bottom row, so this
    // checks for the palette's own header format -- its `PROMPT_MARK`
    // glyph, a space, then the query -- wherever on screen the box actually
    // lands, rather than a fixed-cell assertion at the old bottom row.
    //
    // `spawn_view_pty_first_launch`, not `spawn_view_pty`: every other
    // helper in this file isolates under a `view.toml` that disables every
    // registered feature, palette included, as this file's deliberate
    // pre-native-features baseline. A default-on feature's own rendering
    // test needs the config-absent default that baseline exists to opt
    // everything else out of.
    let mut session = spawn_view_pty_first_launch();

    session.send(b"\x1b:").unwrap();
    assert!(
        session.wait_for("> :", Duration::from_secs(5)),
        "the command palette never showed its \"> :\" query prefix; last screen:\n{}",
        session.screen()
    );

    session.send(b"\x1b:q!\r").unwrap();
    let _ = session.wait();
}

#[test]
fn view_shows_the_prompt_label_on_the_bottom_row_during_call_input() {
    let mut session = spawn_view_pty();

    session.send(b"\x1b:call input('name: ')\r").unwrap();
    assert!(
        session.wait_for("name: ", Duration::from_secs(5)),
        "prompt label never appeared on the bottom row; last screen:\n{}",
        session.screen()
    );

    session.send(b"X").unwrap();
    assert!(
        session.wait_for("name: X", Duration::from_secs(5)),
        "typed character never landed after the prompt label; last screen:\n{}",
        session.screen()
    );

    // <CR> submits the input() prompt itself before quitting, or the
    // pending prompt would swallow the following :q!
    session.send(b"\r\x1b:q!\r").unwrap();
    let _ = session.wait();
}

#[test]
fn view_propagates_cquit_exit_code() {
    let mut session = spawn_view_pty();

    session.send(b"\x1b:cq 5\r").unwrap();

    let exit = session
        .wait()
        .expect("view process never exited after :cq 5");
    assert_eq!(
        exit.exit_code(),
        5,
        "view did not propagate :cq's exit code; screen:\n{}",
        session.screen()
    );
}

#[test]
fn a_leading_plus_line_number_places_the_cursor_on_that_line() {
    // `+42` is `main.rs`'s CLI passthrough for nvim's own "go to line N on
    // startup" argument (see `a_leading_plus_line_number_reaches_the_engine_verbatim`
    // in `crates/view/src/main.rs`, which pins that it reaches the engine
    // byte-for-byte); this is that argument's falsifiable end-to-end claim
    // against a real spawned `view` -- the cursor is actually on line 42,
    // not merely that the flag was forwarded.
    let mut content = String::new();
    for n in 1..=60 {
        content.push_str(&format!("line {n}\n"));
    }
    let mut session = build_view_pty_with_content(
        &[std::ffi::OsStr::new("+42")],
        Some(&content),
        shared_isolation(),
        QueryPolicy::AnswerDa1,
    );
    // `~` (nvim's empty-buffer-line marker) never appears here: the seeded
    // 60-line fixture fills every row of the 24-row viewport, unlike the
    // blank scratch buffer `spawn_view_pty`'s own `~` wait assumes. `line
    // 42` is the one string guaranteed on screen once the buffer paints,
    // since `+42` scrolls the viewport to make the cursor's own line
    // visible.
    assert!(
        session.wait_for("line 42", Duration::from_secs(5)),
        "view never painted the seeded buffer around line 42; last screen:\n{}",
        session.screen()
    );

    // `line('.')` reads the cursor's real position out of nvim itself,
    // rather than trusting the screen's own row math (which a scrolled
    // viewport would make an off-by-N lie): the saved-file oracle this
    // file otherwise relies on can't answer a cursor-position question at
    // all, since a cursor position is not buffer text.
    session
        .send(b"\x1b:echo 'CURSORLINE=' . line('.')\r")
        .unwrap();
    assert!(
        session.wait_for("CURSORLINE=42", Duration::from_secs(5)),
        "+42 did not place the cursor on line 42; last screen:\n{}",
        session.screen()
    );

    session.send(b"\x1b:q!\r").unwrap();
    let _ = session.wait();
}

#[test]
fn minus_capital_o_opens_two_vertical_splits() {
    // `-O`'s two files land in `extra_args` directly (see
    // `vertical_split_flag_forwards_both_files` in `crates/view/src/main.rs`),
    // not through `build_view_pty`'s single scratch-file positional, so this
    // test builds its own two-file pty session instead of reusing the shared
    // helper.
    let a = common::ScratchPaths::new("smoke-split-a");
    let b = common::ScratchPaths::new("smoke-split-b");

    let mut cmd = portable_pty::CommandBuilder::new(common::view_bin_path());
    cmd.arg("-O");
    cmd.arg(&a.scratch);
    cmd.arg(&b.scratch);
    common::isolate_xdg_native_off(&mut cmd, &a.isolated_home);

    let isolation = shared_isolation();
    let session = PtySession::spawn_configured_with(cmd, 80, 24, QueryPolicy::AnswerDa1).unwrap();
    let mut session = ViewPtySession {
        session,
        paths: a,
        _isolation: isolation,
    };
    assert!(
        session.wait_for("~", Duration::from_secs(5)),
        "view never painted the two-file split"
    );

    // `winnr('$')` is nvim's own window count, the least ambiguous proof
    // that two windows actually opened (as opposed to two buffers stacked
    // in one window, which would still show two filenames somewhere on
    // screen). `│` (nvim's default vertical-split separator glyph) is the
    // most direct, literal evidence that the split is vertical rather than
    // horizontal (`-o`), which a window *count* alone cannot distinguish.
    session
        .send(b"\x1b:echo 'WINCOUNT=' . winnr('$')\r")
        .unwrap();
    assert!(
        session.wait_for("WINCOUNT=2", Duration::from_secs(5)),
        "-O did not open two windows; last screen:\n{}",
        session.screen()
    );
    assert!(
        session.screen().contains('\u{2502}'),
        "-O's windows are not separated by nvim's vertical-split glyph, so \
         the split is not actually vertical; last screen:\n{}",
        session.screen()
    );

    session.send(b"\x1b:qa!\r").unwrap();
    let _ = session.wait();
}

#[test]
fn piped_stdin_content_reaches_the_first_buffer_and_survives_wq() {
    // `ls | view -`'s defining property is that the child's fd 0 is a real
    // pipe, not a tty: `main::maybe_relay_stdin` only arms when
    // `std::io::stdin().is_terminal()` is false, and a pty's own fd 0 is
    // always a terminal. `view_oracle::PtySession` /
    // `portable_pty::SlavePty::spawn_command` always wires stdin to the
    // same pty slave as stdout/stderr (confirmed in `portable-pty-0.9.0`'s
    // own `unix.rs`, with no public hook to split them), so this test hand
    // -rolls a pty (for a real controlling terminal on stdout/stderr, which
    // crossterm's raw-mode and `/dev/tty` input still need) plus a plain
    // pipe (for stdin) instead of going through that shared harness.
    use std::io::{Read, Write};
    use std::os::unix::process::CommandExt;
    use std::process::Stdio;

    let paths = common::ScratchPaths::new("smoke-stdin-relay");
    let piped_content = "piped stdin content for view -";

    let winsize = nix::pty::Winsize {
        ws_row: 24,
        ws_col: 80,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    let pty = nix::pty::openpty(Some(&winsize), None).expect("openpty for the piped-stdin test");
    // Both ends close-on-exec, which `std::io::pipe` guarantees and
    // `nix::unistd::pipe` does not: an inherited write-end duplicate
    // survives this process's own fork+exec of `view` and again `view`'s
    // fork+exec of nvim, so nvim's read on its relayed stdin never reaches
    // EOF (the stdin-relay "deadlock" this test used to report was that
    // leaked fd, not a `view` or nvim bug). std's own pipe rather than
    // `nix::unistd::pipe2`, which nix gates off macOS entirely -- and a
    // `pipe` plus a separate `fcntl` would reopen the same leak for any
    // fork racing between the two calls. The read end still reaches the
    // child, since `Stdio::from` redirects it onto fd 0 with a `dup2`
    // during exec setup and a `dup2` result does not carry `FD_CLOEXEC`.
    let (stdin_read, mut stdin_write) =
        std::io::pipe().expect("cloexec pipe for the child's stdin");
    let stdout_fd = nix::unistd::dup(&pty.slave).expect("dup pty slave for stdout");
    let stderr_fd = nix::unistd::dup(&pty.slave).expect("dup pty slave for stderr");

    let mut cmd = std::process::Command::new(common::view_bin_path());
    cmd.arg("-").arg(&paths.scratch);
    view_oracle::make_hermetic(&mut cmd).expect("hermetic env for the piped-stdin child");
    for var in [
        "XDG_CONFIG_HOME",
        "XDG_DATA_HOME",
        "XDG_STATE_HOME",
        "XDG_CACHE_HOME",
    ] {
        cmd.env(var, common::xdg_home(&paths.isolated_home, var));
    }
    common::disable_native_features(&paths.isolated_home);

    cmd.stdin(Stdio::from(stdin_read));
    cmd.stdout(Stdio::from(stdout_fd));
    cmd.stderr(Stdio::from(stderr_fd));
    // A pty slave is only a controlling terminal once the child both starts
    // a new session (`setsid`) and claims the slave as that session's
    // controlling terminal (`TIOCSCTTY`); `std::process::Command::setsid`
    // is nightly-only (`#105376`), so both calls live in this one pre_exec
    // closure instead of the declarative `CommandExt::setsid`.
    //
    // SAFETY: runs after std's own stdio dup2 (confirmed against
    // `library/std/src/sys/process/unix/unix.rs`'s `do_exec` ordering) and
    // before execvp, so fd 1 already is the pty slave; both calls here are
    // async-signal-safe, and `TIOCSCTTY` on fd 1 is side-effect-free beyond
    // claiming the controlling terminal the preceding `setsid` just
    // detached the child from.
    #[allow(unsafe_code)]
    unsafe {
        cmd.pre_exec(|| {
            nix::unistd::setsid().map_err(std::io::Error::from)?;
            if nix::libc::ioctl(nix::libc::STDOUT_FILENO, nix::libc::TIOCSCTTY as _, 0) != 0 {
                Err(std::io::Error::last_os_error())
            } else {
                Ok(())
            }
        });
    }

    let mut child = cmd
        .spawn()
        .expect("spawn view against a piped stdin and a hand-rolled pty");
    // The child now owns its own dup'd copies of the slave; holding this
    // one open in the parent too would keep the pty from ever hanging up,
    // so the drain thread's read below would never see EOF once the child
    // exits.
    drop(pty.slave);

    stdin_write
        .write_all(piped_content.as_bytes())
        .expect("write piped content to the child's stdin");
    // Closes the write end, so nvim's `stdin_fd` startup read reaches EOF
    // and returns instead of blocking forever waiting for more.
    drop(stdin_write);

    let mut master = std::fs::File::from(pty.master);
    let mut drain = master
        .try_clone()
        .expect("clone pty master for the drain thread");
    // Captured (not just discarded) so a failure can report the last thing
    // the child actually painted -- this hand-rolled pty has no vt100
    // parser, so this is raw bytes rather than a rendered screen.
    let screen = std::sync::Arc::new(std::sync::Mutex::new(Vec::<u8>::new()));
    let screen_writer = std::sync::Arc::clone(&screen);
    // The same responder every `PtySession` runs, rather than a second
    // hand-rolled answer to "what does a real terminal reply": this pty is
    // hand-rolled only because `portable_pty` cannot split the child's stdin
    // from its stdout, not because it should behave any less like a
    // terminal. `AnswerFullTier` is what makes the capability assertion
    // below falsifiable -- a probe that never reached a tty resolves the
    // same all-false floor no matter what this pty would have answered.
    let mut responder = view_oracle::QueryResponder::for_policy(QueryPolicy::AnswerFullTier);
    let mut answer = master
        .try_clone()
        .expect("clone pty master for the capability responder");
    std::thread::spawn(move || {
        let mut sink = [0_u8; 4096];
        while let Ok(n) = drain.read(&mut sink) {
            if n == 0 {
                break;
            }
            let replies = responder.replies_for(&sink[..n]);
            if !replies.is_empty() {
                let _ = answer.write_all(&replies);
                let _ = answer.flush();
            }
            if let Ok(mut buf) = screen_writer.lock() {
                buf.extend_from_slice(&sink[..n]);
            }
        }
    });

    // Fixed sleep, not a screen-content wait: see the module doc and the
    // Silent-policy tests above for the same tradeoff this suite always
    // makes when there is no settled-redraw signal to wait on instead.
    std::thread::sleep(Duration::from_millis(800));
    // `nvim - <scratch>` (per `docs/stdin-relay-wire-capture.md`'s captured
    // `:help -`) opens the piped content into buffer 1 (current, unnamed)
    // and `<scratch>` as a separate buffer 2 -- it does not name the piped
    // buffer `<scratch>`. A bare `:wq` targets buffer 1, which has no file
    // name, so it fails with `E32: No file name` and never reaches `:quit`.
    // Writing buffer 1's content to the scratch path explicitly, then
    // force-quitting every window, is what the two-buffer startup shape
    // actually requires.
    let write_cmd = format!("\x1b:w {}\r:qa!\r", paths.scratch.display());
    master
        .write_all(write_cmd.as_bytes())
        .expect("write the explicit :w + :qa! sequence to the pty master");

    // `Child::wait` has no built-in timeout, and a genuinely deadlocked
    // stdin relay (the parent's own copy of the relay fd never closing, so
    // the engine never sees EOF) must surface as a named, bounded failure
    // rather than hang the whole suite -- a hung child was already observed
    // to survive external `task test` termination as an orphan.
    let deadline = std::time::Instant::now() + Duration::from_secs(15);
    let status = loop {
        if let Some(status) = child
            .try_wait()
            .expect("poll view for exit after :w + :qa!")
        {
            break status;
        }
        if std::time::Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            let dump = screen
                .lock()
                .map(|buf| String::from_utf8_lossy(&buf).into_owned());
            panic!(
                "view never exited within 15s of the stdin-relay :w + :qa! sequence \
                 (stdin relay deadlock); last pty bytes:\n{dump:?}"
            );
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    let dump = screen
        .lock()
        .map(|buf| String::from_utf8_lossy(&buf).into_owned());
    assert!(
        status.success(),
        "view did not exit cleanly after :w + :qa!; status={status:?}; last pty bytes:\n{dump:?}"
    );
    // The probe reads replies off descriptor 0, and this session started with
    // a pipe there. Resolving `sync` and `kitty_kbd` at all therefore proves
    // the session found its real terminal and put it on that descriptor:
    // without that, the probe has a pipe to read from, no reply can ever
    // arrive, and both flags fall to the same floor an unanswered terminal
    // produces. The keystrokes below it depend on the same fact, since
    // crossterm picks its input descriptor by asking whether fd 0 is a tty
    // and has no other hook to offer.
    let caps_line = dump.as_deref().unwrap_or_default();
    assert!(
        caps_line.contains("sync=true") && caps_line.contains("kitty_kbd=true"),
        "a piped-stdin session negotiated no capabilities with the terminal \
         answering every query, so its probe was reading the pipe rather than \
         the tty; last pty bytes:\n{dump:?}"
    );

    let saved =
        std::fs::read_to_string(&paths.scratch).expect("saved file should exist and be readable");
    assert!(
        saved.contains(piped_content),
        "saved file did not contain the piped stdin content; contents:\n{saved:?}"
    );
}

#[test]
fn view_pastes_a_two_line_bracketed_paste_as_one_undo_unit() {
    let mut session = spawn_view_pty();

    // a real terminal's bracketed-paste report (`ESC[200~ ... ESC[201~`
    // wrapping the pasted bytes), written straight to the pty master
    // instead of typed: this exercises the terminal input thread's
    // `Event::Paste` decode path end to end (crossterm -> Msg::Paste ->
    // RpcCall::Paste -> nvim_paste), never `nvim_input` keystroke replay.
    session.send(b"\x1b[200~alpha\nbeta\x1b[201~").unwrap();
    assert!(
        session.wait_for("alpha", Duration::from_secs(5)),
        "first pasted line never landed; last screen:\n{}",
        session.screen()
    );
    assert!(
        session.wait_for_cell(1, 0, "b", Duration::from_secs(5)),
        "second pasted line never landed on its own row (both lines collapsed onto \
         one, or newline-splitting broke); last screen:\n{}",
        session.screen()
    );

    // nvim_paste's contract: the whole multi-line paste is one undo unit.
    // A single "u" must remove both lines at once: row 1 reverting all the
    // way back to nvim's "~" empty-line marker (not just an empty line)
    // proves the buffer returned to its pre-paste single-empty-line state,
    // not merely that the second pasted line alone was undone.
    session.send(b"u").unwrap();
    assert!(
        session.wait_for_cell(1, 0, "~", Duration::from_secs(5)),
        "a single undo did not remove the whole two-line paste as one unit; last screen:\n{}",
        session.screen()
    );

    session.send(b"\x1b:q!\r").unwrap();
    let _ = session.wait();
}

#[test]
fn view_resizes_with_tabline_open_and_reaches_the_new_row_count() {
    let mut session = spawn_view_pty();

    // opens a second tab first so the tabline's chrome row is already
    // reserved (chrome_rows() > 0) going into the resize, exercising the
    // row-reservation round trip together with the grid resize itself
    // rather than a bare single-tab resize
    session.send(b":tabnew\r").unwrap();
    assert!(
        session.wait_for_cell(0, 0, " ", Duration::from_secs(5)),
        "tabline row never appeared after :tabnew; last screen:\n{}",
        session.screen()
    );

    // grows from the 24-row spawn size to 48 rows: SIGWINCH (delivered by
    // the kernel from the pty resize below) -> crossterm's Event::Resize
    // -> Msg::Resized -> Effect::Rpc(RpcCall::TryResize) ->
    // nvim_ui_try_resize -> nvim's own grid_resize redraw event -> repaint.
    //
    // A bare `resize()` here is not reliable: `TIOCSWINSZ`'s `SIGWINCH` has
    // been observed to go missing outright between the kernel and view's
    // signal-handling thread (confirmed by instrumenting view's own Msg
    // dispatch across repeated runs -- in the failing runs no resize event
    // reaches it at all within several seconds, with no other sign of the
    // process being stuck, so this is a lost notification, not a slow one).
    // `resize_until` retries the stimulus itself, not just the wait: on
    // each retry it forces a fresh, genuinely-different size before
    // re-requesting the target, so a dropped signal gets another chance at
    // delivery instead of the test just waiting longer on the one attempt
    // that already failed to arrive.
    //
    // Row 30 as the confirmation point: `PtySession::resize` flips the
    // *local* vt100 parser's own dimensions synchronously (so
    // `screen_raw().size()` reports (48, 80) immediately regardless of
    // whether nvim has caught up), so it says nothing about whether nvim's
    // own window has actually grown. Row 30 sits past the old window's
    // bottom edge (chrome_rows(1) + a statusline leaves about 22 reachable
    // buffer lines at 24 rows), so an empty buffer's own "~" past-EOF
    // marker can only appear there once nvim's window has genuinely grown
    // -- the same "~" convention spawn_view_pty already uses to mean "nvim
    // is actually ready". Typing the marker before that has landed races
    // it: nvim would still be operating its old 24-row window when it
    // processes the keystrokes, scrolls to keep the cursor visible there,
    // and does not necessarily revisit that scroll position once the
    // resize eventually does arrive.
    let resized = session
        .resize_until(80, 48, Duration::from_secs(5), |s| {
            s.wait_for_cell(30, 0, "~", Duration::from_millis(400))
        })
        .unwrap();
    assert!(
        resized,
        "resize never reached nvim's own window (no '~' appeared at row 30, \
         past the old 24-row window's bottom edge) even after repeated \
         resize retries; last screen:\n{}",
        session.screen()
    );

    let mut input = Vec::from(*b"i");
    for _ in 0..29 {
        input.extend_from_slice(b"\r");
    }
    input.extend_from_slice(b"MARKERX");
    session.send(&input).unwrap();

    assert!(
        session.wait_for_cell(30, 0, "M", Duration::from_secs(5)),
        "marker typed 29 lines below the tab's first line never landed on \
         row 30, even after the resize was already confirmed to have \
         reached nvim's window; last screen:\n{}",
        session.screen()
    );
    assert_eq!(
        session.screen_raw().size(),
        (48, 80),
        "pty/vt100 geometry itself never reached the resized dimensions"
    );

    // `:qa!` rather than `:q!`: `:tabnew` above left two tabs open, and
    // `:q!` alone only closes the current window/tab, not the editor
    session.send(b"\x1b:qa!\r").unwrap();
    let _ = session.wait();
}

#[test]
fn view_shrinks_and_writes_nothing_below_the_new_last_row() {
    // The direction that had no coverage anywhere, and the one where a
    // stale paint area is actually destructive: with the shadow still sized
    // to the old, larger terminal, every cell emitted past the new last row
    // is addressed to a row the terminal does not have and gets clamped
    // onto one it does, overwriting real content.
    let mut session = spawn_view_pty();

    // fills well past the shrunk window's bottom edge, so the rows that
    // disappear held real content rather than blank padding: a clamped
    // write is only observable when there is something for it to land on
    let mut input = Vec::from(*b"i");
    for line in 0..20 {
        input.extend_from_slice(format!("LINE{line:02}\r").as_bytes());
    }
    input.extend_from_slice(b"LINE20\x1b");
    session.send(&input).unwrap();
    assert!(
        session.wait_for("LINE00", Duration::from_secs(5)),
        "the buffer never painted before the shrink; last screen:\n{}",
        session.screen()
    );

    // 24 -> 12 rows. `PtySession::resize` flips the local vt100 parser's
    // own dimensions synchronously, so geometry alone says nothing about
    // whether nvim's window followed. The observable that does: at 24 rows
    // the whole buffer fits, LINE00 included; at 12 rows it cannot, and
    // nvim keeps the cursor (left on LINE20) visible, so a window that
    // genuinely shrank shows LINE20 and no longer shows LINE00.
    let shrunk = session
        .resize_until(80, 12, Duration::from_secs(5), |s| {
            s.wait_for("LINE20", Duration::from_millis(400)) && !s.screen().contains("LINE00")
        })
        .unwrap();
    assert!(
        shrunk,
        "shrink never reached nvim's own window even after repeated resize \
         retries; last screen:\n{}",
        session.screen()
    );

    // the property: nothing may be addressed past the terminal's new last
    // row. vt100 clamps a CUP beyond the screen, so an over-tall frame does
    // not show up as an out-of-range row -- it shows up as the content that
    // *should* be on the bottom rows having been overwritten by content
    // meant for rows that no longer exist. Typing a fresh marker after the
    // shrink and finding it intact is what proves the frame that painted it
    // was sized to the real terminal.
    session.send(b"GoSHRINKMARKER\x1b").unwrap();
    assert!(
        session.wait_for("SHRINKMARKER", Duration::from_secs(5)),
        "text typed after the shrink never painted intact, so a frame was \
         still being composed at the pre-shrink size; last screen:\n{}",
        session.screen()
    );
    assert_eq!(
        session.screen_raw().size(),
        (12, 80),
        "pty/vt100 geometry itself never reached the shrunk dimensions"
    );

    session.send(b"\x1b:qa!\r").unwrap();
    let _ = session.wait();
}

/// A `--nvim-bin` wrapper that sleeps `delay_ms` milliseconds before the
/// real engine, standing in for a slow-starting one without patching nvim
/// itself.
#[cfg(unix)]
fn write_delayed_nvim_wrapper(delay_ms: u64) -> WrapperScript {
    let real_nvim = real_nvim_path();
    WrapperScript::new("delayed-nvim", |_| {
        format!(
            "#!/bin/sh\nsleep {}\nexec {real_nvim} \"$@\"\n",
            f64::from(u32::try_from(delay_ms).unwrap_or(u32::MAX)) / 1000.0
        )
    })
}

/// nvim's own empty-buffer line marker, the first thing a fresh buffer's
/// grid content puts on screen. The startup shell paints no `~` of its own
/// (only a blank statusline bar and the placeholder label) and no scratch
/// path this file uses contains one, so its presence on screen means the
/// engine attached and its grid reached the terminal.
#[cfg(unix)]
const ENGINE_CONTENT_MARKER: char = '~';

/// Asserts the ordering the startup shell exists for: the placeholder frame
/// reaches the terminal while the engine's own content is not yet on
/// screen. Both halves are read from a single screen state, so what is
/// proven is the order of the two frames in the pty stream, not the wall
/// time either took to get there.
///
/// Deliberately not a latency bar. Measured shell-frame paint spans roughly
/// 50ms on Linux to 450ms on macOS on developer hardware, so any fixed
/// millisecond bar tight enough to be meaningful on one platform sits
/// inside the other's ordinary distribution; absolute first-paint budgets
/// are gated in the bench matrix, on a release build under a controlled
/// protocol, rather than by a debug binary on whatever host runs the tests.
/// The caller's delayed-engine wrapper is what makes the two frames
/// separately observable, by holding nvim back far longer than a pty read
/// takes to deliver the frame already written.
///
/// Proves only the first half of the ordering: the caller must go on to
/// establish that the engine really did attach afterwards (otherwise a
/// `view` that never starts an engine at all would satisfy this vacuously).
#[cfg(unix)]
fn assert_shell_frame_precedes_attach(session: &mut ViewPtySession) {
    let ordered = session.wait_for_screen(Duration::from_secs(15), |screen| {
        let text = screen.contents();
        text.contains(SHELL_PLACEHOLDER) && !text.contains(ENGINE_CONTENT_MARKER)
    });
    assert!(
        ordered,
        "never observed the startup shell frame ({SHELL_PLACEHOLDER:?}) on screen ahead of the \
         engine's own content ({ENGINE_CONTENT_MARKER:?}): either the placeholder never painted, \
         or engine content was already on screen by the time it did; last screen:\n{}",
        session.screen()
    );
}

/// The startup sequence's shell frame -- a themed statusline placeholder
/// plus a static "waiting for nvim" indicator, painted before the engine
/// even spawns -- must be visible well before a deliberately slow (500ms)
/// embedded engine ever attaches, and keys typed during that gap must
/// reach the real buffer, in order, once attach completes.
///
/// This is also the seam's liveness proof end to end: `view_vim_enter`'s
/// blocking `rpcrequest` only ever resolves if `update()`'s
/// `Msg::EngineRequest(EngineRequest::VimEnter)` arm replies via
/// `Effect::Reply` and the reply actually reaches nvim over the wire -- a
/// deadlock anywhere in that path would hang this test's `:wq` at the very
/// end (nvim can never fully start, let alone quit) rather than merely
/// fail an assertion.
#[cfg(unix)]
#[test]
fn shell_frame_paints_before_a_slow_engine_and_pre_attach_keys_replay_in_order() {
    let wrapper = write_delayed_nvim_wrapper(500);

    // This test's ordering proof and its 15s replay wait both depend on
    // the delayed-nvim wrapper's real wall-clock startup, not just its
    // artificial 500ms sleep: a real nvim process competing for scheduler
    // time against this binary's other real-nvim-spawning smoke tests can
    // itself clear several seconds with zero product-side delay (see the
    // `wait_for` call below). `pty_isolation_exclusive` -- the same
    // mechanism this file already uses for its tight-bound startup-latency
    // tests -- removes that sibling-session contention for those two
    // wall-clock-bound waits only, not the whole test: `std::sync::RwLock`
    // has no downgrade, so the guard is held as an `Option`, dropped, and
    // replaced with the ordinary shared guard once both waits are done,
    // rather than blocking every other pty test in the binary for the
    // untimed `:wq`/exit tail below too. That tail is bounded in its own
    // right (see `wait_for_exit` below): a wedged engine at exit must fail
    // this test loudly rather than hang the whole binary, and with it CI.
    let mut exclusive = Some(pty_isolation_exclusive());
    let mut session = spawn_view_pty_raw_isolated_with_args(
        &[
            std::ffi::OsStr::new("--nvim-bin"),
            wrapper.path().as_os_str(),
        ],
        QueryPolicy::AnswerDa1,
    );

    assert_shell_frame_precedes_attach(&mut session);

    // typed immediately, well before the delayed engine has attached: this
    // is exactly the pre-attach window startup::drain_pre_attach buffers
    session.send(b"ihello world").unwrap();

    // the wrapper sleeps 500ms before nvim even starts; wait comfortably
    // past attach plus startup for the buffered keys to replay into the
    // real buffer. Text in the buffer is also the engine-attached half of
    // the ordering asserted above: buffer content can only be on screen
    // once the engine attached and painted, so the placeholder observed
    // without it genuinely preceded attach. 15s matches this file's own
    // convention for every other post-delayed-attach content wait (see
    // `ENGINE_CONTENT_MARKER` below): a real nvim startup competing for
    // scheduler time under parallel test load can clear 5s on a loaded host
    // with no product-side delay at all
    assert!(
        session.wait_for("hello world", Duration::from_secs(15)),
        "pre-attach keys never replayed into the buffer after attach, or \
         did not replay in order; last screen:\n{}",
        session.screen()
    );

    // both wall-clock-bound waits above are done; downgrade to the shared
    // lock so the untimed tail below no longer blocks every other pty test
    // in the binary for its duration
    drop(exclusive.take());
    session._isolation = shared_isolation();

    session.send(b"\x1b:wq\r").unwrap();
    // `wait_for_exit`, not `wait`'s unbounded block: kills and reaps the
    // child on deadline, so a wedged engine here fails this assertion
    // promptly with no orphaned process, instead of hanging the whole test
    // binary (and CI) behind it.
    let exit = session
        .wait_for_exit(Duration::from_secs(15))
        .unwrap_or_else(|| {
            panic!(
                "view never exited within 15s of :wq against the delayed engine; screen:\n{}",
                session.screen()
            )
        });
    assert!(
        exit.success(),
        "view did not exit cleanly after :wq; screen:\n{}",
        session.screen()
    );

    let saved = session.read_saved_file();
    assert!(
        saved.contains("hello world"),
        "saved file did not contain the pre-attach-typed text; contents:\n{saved:?}"
    );
}

/// A flood of pre-attach keystrokes at (and somewhat past)
/// `KEY_RING_CAPACITY` (64), overlapping a real embedded engine's attach
/// window, must never freeze the session.
///
/// This is a supplementary, real-engine regression test; the deterministic
/// unit-level coverage for the hazard shape it guards lives in
/// `runtime::tests::re_enqueueing_replayed_keys_onto_a_full_bounded_channel_with_no_consumer_blocks_forever`
/// (`runtime.rs`) and `startup::tests::run_cutover_against_a_pre_filled_channel_replays_everything_without_blocking`
/// (`startup.rs`), for a reason worth recording here: the hazard this test
/// aims at requires at least one extra `Msg` to land in `msg_tx` in a
/// microsecond-scale gap during cutover -- both the ring and the channel
/// share the exact same 64 capacity, so replaying exactly 64 buffered keys
/// into an otherwise-empty channel fits without blocking regardless of
/// whether the code under test still writes into that channel at cutover.
/// Driving this pty with a 300ms-delayed engine and 150 keystrokes sent one
/// at a time over ~450ms (to straddle attach completion) reliably fills the
/// ring to its full 64-key capacity but has never observed a key landing in
/// that microsecond-scale gap, so this harness cannot reliably distinguish
/// a version that writes into the channel at cutover from one that does
/// not -- the live race window is real for a paste-sized burst, just not
/// reproducible through this harness's timing. Kept here anyway as
/// end-to-end coverage that a heavy, realistic flood against a real engine
/// still behaves correctly.
#[cfg(unix)]
#[test]
fn a_flood_of_more_than_64_pre_attach_keys_never_freezes_the_session() {
    let wrapper = write_delayed_nvim_wrapper(300);
    let mut session = spawn_view_pty_raw_with_args(&[
        std::ffi::OsStr::new("--nvim-bin"),
        wrapper.path().as_os_str(),
    ]);

    assert_shell_frame_precedes_attach(&mut session);

    // 150 keystrokes, one at a time, over ~450ms: comfortably past
    // KEY_RING_CAPACITY (64), and comfortably past the wrapper's 300ms
    // delay plus ordinary attach time, so typing is still in flight right
    // at the cutover instant
    for _ in 0..150 {
        session.send(b"x").unwrap();
        std::thread::sleep(Duration::from_millis(3));
    }
    // the engine-attached half of the ordering asserted above: the
    // placeholder was observed without this content, and now the content
    // is here, so the shell frame genuinely preceded attach rather than
    // standing in for an engine that never arrived
    assert!(
        session.wait_for(&ENGINE_CONTENT_MARKER.to_string(), Duration::from_secs(15)),
        "the engine never put its own content on screen, so the shell frame \
         preceded nothing; last screen:\n{}",
        session.screen()
    );

    session.send(b"\x1b:q!\r").unwrap();

    let exit = session.wait_for_exit(Duration::from_secs(15)).expect(
        "view appears wedged (never exited) after a >64-key pre-attach flood \
         spread across the engine's attach window -- see this test's doc \
         comment for the replay-deadlock hazard it guards against",
    );
    assert!(
        exit.success(),
        "view did not exit cleanly after a >64-key pre-attach flood"
    );
}

/// The deferred-scheduling half of the pre-attach seam: a key-ring
/// overflow before any `Executor` exists still owes its notice the same
/// idle expiry a wire-sourced toast gets, once one exists to run the
/// buffered effect against. A native notice must flow through the same
/// classify/expire/history path regardless of when in startup it was
/// created; a split here is invisible on an idle editor exactly the way a
/// missing choke point anywhere else is.
#[cfg(unix)]
#[test]
fn a_pre_attach_key_overflow_notice_expires_after_attach_the_same_idle_wait_a_wire_toast_does() {
    let wrapper = write_delayed_nvim_wrapper(800);
    let mut session = spawn_view_pty_raw_with_args(&[
        std::ffi::OsStr::new("--nvim-bin"),
        wrapper.path().as_os_str(),
    ]);

    assert_shell_frame_precedes_attach(&mut session);

    // over KEY_RING_CAPACITY (64), sent with no throttling, comfortably
    // inside the wrapper's 800ms delay: every one of these lands before
    // main.rs's pre-loop Executor is constructed, so the notice's
    // ScheduleToastExpiry effect can only reach DrainedInput::toast_effects,
    // never run immediately -- this is what exercises the deferred half of
    // the seam rather than the ordinary in-loop path the other toast-expiry
    // tests already cover
    for _ in 0..100 {
        session.send(b"x").unwrap();
    }

    assert!(
        session.wait_for("startup key buffer full", Duration::from_secs(2)),
        "the pre-attach overflow notice never reached the screen; last screen:\n{}",
        session.screen()
    );

    assert!(
        session.wait_for(&ENGINE_CONTENT_MARKER.to_string(), Duration::from_secs(15)),
        "the engine never attached after the pre-attach flood; last screen:\n{}",
        session.screen()
    );

    // no further input from here on: the notice's expiry must be driven by
    // the ToastExpired timer the replayed effect scheduled at attach time,
    // not by some other event's repaint incidentally dropping the entry
    let margin = view_core::native::toast::TRANSIENT_TOAST_TIMEOUT + Duration::from_secs(6);
    assert!(
        session.wait_for_screen(margin, |screen| {
            !screen.contents().contains("startup key buffer full")
        }),
        "the pre-attach overflow notice never expired after attach; last screen:\n{}",
        session.screen()
    );

    session.send(b"\x1b:q!\r").unwrap();
    let _ = session.wait();
}

/// Spawns `view` with `VIEW_LOG` pointed at a path whose parent directory
/// does not exist, reproducing the unwritable-path degrade `vlog::init`
/// documents: the open fails, one line goes to stderr, and the sink stays
/// `None` for the rest of the session rather than the session refusing to
/// start. Duplicates `spawn_view_pty_raw`'s setup instead of threading an
/// env override through it: this is the only test in the file that needs a
/// `VIEW_LOG` override, and adding a parameter for one caller would reshape
/// every other caller's signature for no benefit.
fn spawn_view_pty_with_unwritable_view_log() -> ViewPtySession {
    let paths = common::ScratchPaths::new("smoke-view-log");
    let mut cmd = portable_pty::CommandBuilder::new(common::view_bin_path());
    cmd.arg(&paths.scratch);
    common::isolate_xdg_native_off(&mut cmd, &paths.isolated_home);
    cmd.env("VIEW_LOG", "/nonexistent-dir-xyz/log.txt");

    let isolation = shared_isolation();
    let session = PtySession::spawn_configured(cmd, 80, 24).unwrap();
    ViewPtySession {
        session,
        paths,
        _isolation: isolation,
    }
}

/// Regression: an unwritable `VIEW_LOG` path must degrade to no diagnostic
/// logging, never to a broken or refused session. The oracle is the saved
/// file's real contents (echo-immune, per `view_paints_typed_text_in_a_pty`'s
/// comment), not the pty's screen -- proving the keystroke actually reached
/// nvim's buffer, not just that the terminal echoed it back.
#[test]
fn view_degrades_gracefully_when_view_log_path_is_unwritable() {
    let mut session = spawn_view_pty_with_unwritable_view_log();
    let _ = session.wait_for("~", Duration::from_secs(5));

    session.send(b"ihello from an unwritable VIEW_LOG").unwrap();
    session.send(b"\x1b:wq\r").unwrap();
    let exit = session
        .wait()
        .expect("view never exited after :wq with an unwritable VIEW_LOG path");
    assert!(
        exit.success(),
        "view did not exit cleanly when VIEW_LOG named an unopenable path"
    );

    let saved = session.read_saved_file();
    assert!(
        saved.contains("hello from an unwritable VIEW_LOG"),
        "saved file did not contain the typed text; an unwritable VIEW_LOG \
         path must never take the session down with it; contents:\n{saved:?}"
    );
}

/// The synchronized-output bracket `view` writes around a frame once it
/// believes the terminal supports mode 2026. Its presence is the only
/// external evidence of the derived tier: a private mode leaves no cell for
/// a screen assertion to read.
const SYNC_BRACKET_OPEN: &[u8] = b"\x1b[?2026h";

fn contains_subslice(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.windows(needle.len()).any(|w| w == needle)
}

/// Pins the chain a benchmark's stated tier depends on: the pty's answers
/// decide what the child's probe resolves, which decides how much work each
/// of its frames does.
///
/// Without this, a session could silently downgrade every child it hosts and
/// nothing would fail -- the screen looks identical either way, because the
/// difference is a mode the terminal applies and the parser discards. A row
/// promised at the full tier would then be timed against cheaper frames than
/// it names.
#[test]
fn the_terminals_answers_decide_which_tier_the_child_paints_at() {
    for (policy, want_sync) in [
        (QueryPolicy::AnswerDa1, false),
        (QueryPolicy::AnswerFullTier, true),
    ] {
        let mut session = build_view_pty(&[], shared_isolation(), policy);
        // before any drain: recording captures from the next drain onward,
        // and the probe traffic under test is the first thing the child writes
        session.record_raw_output();
        assert!(
            session.wait_for("~", Duration::from_secs(5)),
            "view never painted a buffer under {policy:?}"
        );

        session.send(b"itier probe").unwrap();
        session.send(b"\x1b:wq\r").unwrap();
        let exit = session.wait().expect("view never exited after :wq");
        assert!(exit.success(), "view did not exit cleanly under {policy:?}");

        assert_eq!(
            contains_subslice(session.raw_output(), SYNC_BRACKET_OPEN),
            want_sync,
            "under {policy:?} the synchronized-output bracket should{} have been \
             written; a child derives that capability only from the reply this \
             pty chose to send",
            if want_sync { "" } else { " not" }
        );
    }
}

/// The tier `view` derives, read from its own startup log line rather than
/// inferred from what it painted.
///
/// The two inputs are independent and neither is visible on screen: the
/// probe replies this pty chooses to send, and `COLORTERM` in the child's
/// environment. `Tier::Full` -- the tier the measurement protocol's budget
/// rows name -- needs both, so a bench that set only one would report a row
/// at a tier its child never reached.
fn derived_tier(policy: QueryPolicy, colorterm: Option<&str>) -> String {
    let paths = common::ScratchPaths::new("smoke-tier");
    let log_path = paths.isolated_home.join("view.log");

    let mut cmd = portable_pty::CommandBuilder::new(common::view_bin_path());
    cmd.arg(&paths.scratch);
    common::isolate_xdg_native_off(&mut cmd, &paths.isolated_home);
    cmd.env("VIEW_LOG", &log_path);
    // COLORTERM is on the hermetic allowlist, so a truecolor host shell
    // reaches the child unless its absence is forced here rather than
    // assumed.
    match colorterm {
        Some(value) => cmd.env("COLORTERM", value),
        None => cmd.env_remove("COLORTERM"),
    }

    let isolation = shared_isolation();
    let mut session = ViewPtySession {
        session: PtySession::spawn_configured_with(cmd, 80, 24, policy).unwrap(),
        paths,
        _isolation: isolation,
    };
    assert!(
        session.wait_for("~", Duration::from_secs(5)),
        "view never painted a buffer under {policy:?} / COLORTERM={colorterm:?}"
    );
    session.send(b"\x1b:q!\r").unwrap();
    let _ = session.wait();

    let log = std::fs::read_to_string(&log_path).unwrap();
    let tier = log
        .lines()
        .find_map(|line| line.split("caps tier=").nth(1))
        .and_then(|after| after.split_whitespace().next())
        .map(str::to_string);
    assert!(
        tier.is_some(),
        "no startup line in VIEW_LOG; the log was:\n{log}"
    );
    tier.unwrap_or_default()
}

/// Pins the condition the budget rows are stated at, against the child's own
/// report of it.
///
/// Both inputs are load-bearing and each fails silently on its own: a probe
/// reply this pty withholds and an environment variable it forgets produce
/// the same screen as the full tier, so nothing but this assertion stands
/// between a row that says `tier full` and a child that painted at `Basic`.
#[test]
fn the_bench_configuration_is_the_only_one_that_reaches_the_full_tier() {
    assert_eq!(
        derived_tier(QueryPolicy::AnswerDa1, None),
        "Basic",
        "a DA1-only terminal resolves no optional capability"
    );
    assert_eq!(
        derived_tier(QueryPolicy::AnswerFullTier, None),
        "Basic",
        "answering the whole probe batch is not enough on its own: COLORTERM \
         is the sole input to the truecolor bit that Full also requires"
    );
    assert_eq!(
        derived_tier(QueryPolicy::AnswerDa1, Some("truecolor")),
        "Standard",
        "COLORTERM alone reaches Standard, not Full"
    );
    assert_eq!(
        derived_tier(QueryPolicy::AnswerFullTier, Some("truecolor")),
        "Full",
        "the configuration `BenchSession::spawn` and the bench environment \
         set together is what the budget rows name"
    );
}
