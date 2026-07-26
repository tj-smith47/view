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
#![allow(clippy::unwrap_used, clippy::expect_used)]
// Every test in this file drives the wired `view` binary inside a real pty;
// view's Windows terminal runtime is a tier-2 surface validated on winserver
// rather than in CI, so the whole suite is gated off the Windows build. There
// are no pure-logic tests here to keep running on Windows.
#![cfg(unix)]

mod common;

use std::path::PathBuf;
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
    let paths = common::ScratchPaths::new("smoke");

    let mut cmd = portable_pty::CommandBuilder::new(common::view_bin_path());
    for arg in extra_args {
        cmd.arg(arg);
    }
    cmd.arg(&paths.scratch);
    common::isolate_xdg(&mut cmd, &paths.isolated_home);

    let session = PtySession::spawn_configured_with(cmd, 80, 24, policy).unwrap();

    ViewPtySession {
        session,
        paths,
        _isolation: isolation,
    }
}

/// Like [`spawn_view_pty_raw`] but takes NO isolation read lock, for a timing
/// test that already holds the exclusive write guard from
/// `pty_isolation_exclusive`; taking read on that same thread deadlocks.
fn spawn_view_pty_raw_isolated(policy: QueryPolicy) -> ViewPtySession {
    build_view_pty(&[], None, policy)
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

#[cfg(target_os = "linux")]
#[test]
fn view_exits_nonzero_when_engine_dies_by_signal() {
    let mut session = spawn_view_pty();

    let view_pid = session.view_pid();
    let nvim_pid = wait_for_child_pid(view_pid, "nvim", Duration::from_secs(5))
        .expect("view never spawned an nvim child within the timeout");

    let kill_status = std::process::Command::new("kill")
        .arg("-KILL")
        .arg(nvim_pid.to_string())
        .status()
        .unwrap();
    assert!(kill_status.success(), "kill -KILL {nvim_pid} failed");

    let exit = session
        .wait()
        .expect("view process never exited after its embedded nvim was killed");
    // 128 + SIGKILL(9): the conventional signal-death exit code (see
    // exit_info_from_status in crates/view-engine/src/process.rs and the
    // EngineDown arm of update() in crates/view-core/src/update.rs),
    // stronger than a bare nonzero check since it also pins the exact
    // mapping formula
    assert_eq!(
        exit.exit_code(),
        137,
        "view did not map its engine's signal death to 128+signal; screen:\n{}",
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

#[test]
fn view_shows_the_cmdline_prefix_on_the_bottom_row_while_typing_a_command() {
    let mut session = spawn_view_pty();

    session.send(b"\x1b:").unwrap();
    assert!(
        session.wait_for_cell(23, 0, ":", Duration::from_secs(5)),
        "cmdline row never showed its \":\" prefix; last screen:\n{}",
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

/// Writes a shell script that sleeps `delay_ms` milliseconds, then `exec`s
/// the real `nvim` (resolved via `which`, matching this file's other
/// `nvim`-locating helpers) with every argument forwarded verbatim --
/// standing in for a slow-starting engine without patching nvim itself.
/// Marked executable directly (`portable_pty`/`Command` exec it, not a
/// shell), and disambiguated by pid the same way this file's scratch paths
/// are, since parallel tests in this binary could otherwise collide.
#[cfg(unix)]
fn write_delayed_nvim_wrapper(delay_ms: u64) -> PathBuf {
    let real_nvim = String::from_utf8(
        std::process::Command::new("which")
            .arg("nvim")
            .output()
            .expect("which nvim failed")
            .stdout,
    )
    .expect("non-utf8 which output")
    .trim()
    .to_string();

    let path = common::scratch_root().join(format!("delayed-nvim-{}.sh", std::process::id()));
    let script = format!(
        "#!/bin/sh\nsleep {}\nexec {real_nvim} \"$@\"\n",
        f64::from(u32::try_from(delay_ms).unwrap_or(u32::MAX)) / 1000.0
    );
    std::fs::write(&path, script).expect("failed to write delayed-nvim wrapper script");

    use std::os::unix::fs::PermissionsExt;
    let mut perms = std::fs::metadata(&path).unwrap().permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(&path, perms).unwrap();

    path
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

    let mut session =
        spawn_view_pty_raw_with_args(&[std::ffi::OsStr::new("--nvim-bin"), wrapper.as_os_str()]);

    assert_shell_frame_precedes_attach(&mut session);

    // typed immediately, well before the delayed engine has attached: this
    // is exactly the pre-attach window startup::drain_pre_attach buffers
    session.send(b"ihello world").unwrap();

    // the wrapper sleeps 500ms before nvim even starts; wait comfortably
    // past attach plus startup for the buffered keys to replay into the
    // real buffer. Text in the buffer is also the engine-attached half of
    // the ordering asserted above: buffer content can only be on screen
    // once the engine attached and painted, so the placeholder observed
    // without it genuinely preceded attach
    assert!(
        session.wait_for("hello world", Duration::from_secs(5)),
        "pre-attach keys never replayed into the buffer after attach, or \
         did not replay in order; last screen:\n{}",
        session.screen()
    );

    session.send(b"\x1b:wq\r").unwrap();
    let exit = session
        .wait()
        .expect("view never exited after :wq against the delayed engine");
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

    let _ = std::fs::remove_file(&wrapper);
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
    let mut session =
        spawn_view_pty_raw_with_args(&[std::ffi::OsStr::new("--nvim-bin"), wrapper.as_os_str()]);

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

    let _ = std::fs::remove_file(&wrapper);
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
    common::isolate_xdg(&mut cmd, &paths.isolated_home);
    cmd.env("VIEW_LOG", "/nonexistent-dir-xyz/log.txt");

    let session = PtySession::spawn_configured(cmd, 80, 24).unwrap();
    ViewPtySession {
        session,
        paths,
        _isolation: shared_isolation(),
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
