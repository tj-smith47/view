//! End-to-end proof that the wired `view` binary starts nvim, paints typed
//! text into a real terminal, and exits with the right code on every exit
//! path: a clean `:q!`, a signal-killed engine, and an explicit `:cq` exit
//! code. All driven through an actual pty rather than an in-process mock.
//! These tests wait via fixed sleeps rather than a deterministic
//! "redraw settled" signal; they are smoke tests, not exhaustive protocol
//! coverage.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use portable_pty::{native_pty_system, CommandBuilder, PtySize};
use std::io::{Read, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::time::{Duration, Instant};

/// Locates the `view` binary next to this crate's own target directory,
/// building it first if it is not already there.
///
/// `view` is a bin-only crate with no library target, so Cargo's
/// `CARGO_BIN_EXE_<name>` mechanism is unavailable: Cargo only sets that
/// variable for binaries reachable via a package's own dependency graph, and
/// it refuses to add a lib-less crate as a dependency at all (confirmed by
/// attempting exactly that: `cargo add view -p view-oracle --dev` succeeds
/// but emits "ignoring invalid dependency `view` which is missing a lib
/// target", and `env!("CARGO_BIN_EXE_view")` then fails to compile). Falls
/// back to locating the workspace `target/<profile>/view` executable
/// directly, building it on demand so this test is self-sufficient under a
/// direct `cargo test -p view-oracle` as well as under `task test`.
fn view_bin_path() -> PathBuf {
    let profile_dir = if cfg!(debug_assertions) {
        "debug"
    } else {
        "release"
    };
    let mut path = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    path.pop(); // crates/
    path.pop(); // workspace root
    path.push("target");
    path.push(profile_dir);
    path.push("view");
    if !path.exists() {
        let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_string());
        let status = std::process::Command::new(cargo)
            .args(["build", "-p", "view"])
            .status()
            .expect("failed to invoke cargo build -p view");
        assert!(status.success(), "cargo build -p view failed");
    }
    path
}

/// Spawns a background thread that forwards every chunk read from `reader`
/// onto the returned channel, so the test thread can poll with a bounded
/// timeout instead of blocking on a single `read` call that may return only
/// part of the child's output.
fn spawn_reader(mut reader: Box<dyn Read + Send>) -> mpsc::Receiver<Vec<u8>> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut buf = [0_u8; 65536];
        loop {
            match reader.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if tx.send(buf[..n].to_vec()).is_err() {
                        break;
                    }
                }
            }
        }
    });
    rx
}

/// A `view` process running inside a real pty, with everything a test needs
/// to drive it and observe its screen: the child handle (for exit-status
/// assertions), a byte channel fed by a background reader thread, and a
/// `vt100` parser that turns those bytes into a queryable screen.
struct PtySession {
    child: Box<dyn portable_pty::Child>,
    rx: mpsc::Receiver<Vec<u8>>,
    writer: Box<dyn Write + Send>,
    parser: vt100::Parser,
    scratch: PathBuf,
    isolated_home: PathBuf,
}

impl PtySession {
    /// Blocks (up to `timeout`) until the screen contains `needle`,
    /// returning whether it appeared.
    ///
    /// Checks the already-processed screen state before blocking on the
    /// channel: a prior call (or the startup drain) may already have
    /// processed the chunk that satisfies this condition, and this
    /// function would otherwise wait for a *new* chunk that never comes
    /// once the screen has settled, timing out despite the condition
    /// already being true.
    fn wait_for(&mut self, needle: &str, timeout: Duration) -> bool {
        if self.parser.screen().contents().contains(needle) {
            return true;
        }
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            match self.rx.recv_timeout(Duration::from_millis(200)) {
                Ok(chunk) => {
                    self.parser.process(&chunk);
                    if self.parser.screen().contents().contains(needle) {
                        return true;
                    }
                }
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => break,
            }
        }
        false
    }

    /// Writes `bytes` to the pty as if a user typed them, e.g. `Esc` plus a
    /// command-line invocation.
    fn send(&mut self, bytes: &[u8]) {
        self.writer.write_all(bytes).unwrap();
        self.writer.flush().unwrap();
    }

    /// Blocks (up to `timeout`) until the cell at `(row, col)` holds exactly
    /// `expected`, returning whether it did. Unlike [`Self::wait_for`]
    /// (whole-screen substring search), this pins content to a specific
    /// cell, for assertions where position is the point (e.g. the cmdline's
    /// `:` prefix belonging to the bottom row specifically, not appearing
    /// anywhere on screen by coincidence).
    ///
    /// Checks the already-processed screen state before blocking, for the
    /// same reason [`Self::wait_for`] does.
    fn wait_for_cell(&mut self, row: u16, col: u16, expected: &str, timeout: Duration) -> bool {
        if self
            .parser
            .screen()
            .cell(row, col)
            .is_some_and(|c| c.contents() == expected)
        {
            return true;
        }
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            match self.rx.recv_timeout(Duration::from_millis(200)) {
                Ok(chunk) => {
                    self.parser.process(&chunk);
                    if self
                        .parser
                        .screen()
                        .cell(row, col)
                        .is_some_and(|c| c.contents() == expected)
                    {
                        return true;
                    }
                }
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => break,
            }
        }
        false
    }

    /// The OS pid of the `view` process itself (not its embedded nvim
    /// child).
    fn view_pid(&self) -> u32 {
        self.child
            .process_id()
            .expect("view child exposes a pid on this platform")
    }
}

impl Drop for PtySession {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.scratch);
        let _ = std::fs::remove_dir_all(&self.isolated_home);
    }
}

static NEXT_SESSION_ID: AtomicU64 = AtomicU64::new(0);

/// Opens a pty sized 24x80 (nonzero: a 0x0 winsize makes nvim's UI attach
/// fail during startup), spawns `view` against a scratch file with the
/// host's real nvim config isolated out of the way, and waits for the first
/// non-blank redraw so callers start from a settled screen.
///
/// Scratch paths are disambiguated by an atomic counter, not just the test
/// process's pid: multiple tests in this file spawn a session concurrently
/// within the same test binary process, so pid alone would collide.
fn spawn_view_pty() -> PtySession {
    let pty = native_pty_system();
    let pair = pty
        .openpty(PtySize {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        })
        .unwrap();

    let pid = std::process::id();
    let session_id = NEXT_SESSION_ID.fetch_add(1, Ordering::Relaxed);
    let scratch = std::env::temp_dir().join(format!("view-oracle-smoke-{pid}-{session_id}.txt"));
    let isolated_home = std::env::temp_dir().join(format!("view-oracle-home-{pid}-{session_id}"));
    std::fs::create_dir_all(&isolated_home).unwrap();

    let mut cmd = CommandBuilder::new(view_bin_path());
    cmd.arg(&scratch);
    // isolate from any real nvim user config on the host running this test:
    // a dashboard plugin or custom keymap on a bare "i" would make the
    // typed-text assertion below nondeterministic
    for var in [
        "XDG_CONFIG_HOME",
        "XDG_DATA_HOME",
        "XDG_STATE_HOME",
        "XDG_CACHE_HOME",
    ] {
        cmd.env(var, isolated_home.join(var.to_lowercase()));
    }

    let child = pair.slave.spawn_command(cmd).unwrap();
    let reader = pair.master.try_clone_reader().unwrap();
    let writer = pair.master.take_writer().unwrap();
    // the slave fd must not outlive the child's own copy, or the master
    // never sees EOF once the child exits
    drop(pair.slave);

    let rx = spawn_reader(reader);
    let mut parser = vt100::Parser::new(24, 80, 0);

    // wait for nvim's startup redraw before driving input, same as a human
    // would
    let startup_deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < startup_deadline {
        match rx.recv_timeout(Duration::from_millis(200)) {
            Ok(chunk) => {
                parser.process(&chunk);
                if !parser.screen().contents().trim().is_empty() {
                    break;
                }
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break,
        }
    }

    PtySession {
        child,
        rx,
        writer,
        parser,
        scratch,
        isolated_home,
    }
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

    session.send(b"ihello from view");
    assert!(
        session.wait_for("hello from view", Duration::from_secs(5)),
        "screen never showed typed text; last screen:\n{}",
        session.parser.screen().contents()
    );

    session.send(b"\x1b:q!\r");
    let _ = session.child.wait();
}

#[test]
fn view_paints_wide_character_without_corrupting_neighbor_cell() {
    let mut session = spawn_view_pty();

    // "you", a double-width CJK character, immediately followed by an
    // ASCII neighbor: if the grid's wide-cell handling is off by one, this
    // is what would either overwrite or get overwritten by the adjacent
    // narrow cell.
    session.send("i你X".as_bytes());
    assert!(
        session.wait_for("你X", Duration::from_secs(5)),
        "screen never showed the CJK character next to its neighbor; last screen:\n{}",
        session.parser.screen().contents()
    );

    let screen = session.parser.screen();
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

    session.send(b"\x1b:q!\r");
    let _ = session.child.wait();
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
        .child
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
        session.parser.screen().contents()
    );
}

#[test]
fn view_shows_an_echoed_message() {
    let mut session = spawn_view_pty();

    session.send(b"\x1b:echo \"hi\"\r");
    assert!(
        session.wait_for("hi", Duration::from_secs(5)),
        "screen never showed the echoed message text; last screen:\n{}",
        session.parser.screen().contents()
    );

    session.send(b"\x1b:q!\r");
    let _ = session.child.wait();
}

#[test]
fn view_shows_the_cmdline_prefix_on_the_bottom_row_while_typing_a_command() {
    let mut session = spawn_view_pty();

    session.send(b"\x1b:");
    assert!(
        session.wait_for_cell(23, 0, ":", Duration::from_secs(5)),
        "cmdline row never showed its \":\" prefix; last screen:\n{}",
        session.parser.screen().contents()
    );

    session.send(b"\x1b:q!\r");
    let _ = session.child.wait();
}

#[test]
fn view_shows_the_prompt_label_on_the_bottom_row_during_call_input() {
    let mut session = spawn_view_pty();

    session.send(b"\x1b:call input('name: ')\r");
    assert!(
        session.wait_for("name: ", Duration::from_secs(5)),
        "prompt label never appeared on the bottom row; last screen:\n{}",
        session.parser.screen().contents()
    );

    session.send(b"X");
    assert!(
        session.wait_for("name: X", Duration::from_secs(5)),
        "typed character never landed after the prompt label; last screen:\n{}",
        session.parser.screen().contents()
    );

    // <CR> submits the input() prompt itself before quitting, or the
    // pending prompt would swallow the following :q!
    session.send(b"\r\x1b:q!\r");
    let _ = session.child.wait();
}

#[test]
fn view_propagates_cquit_exit_code() {
    let mut session = spawn_view_pty();

    session.send(b"\x1b:cq 5\r");

    let exit = session
        .child
        .wait()
        .expect("view process never exited after :cq 5");
    assert_eq!(
        exit.exit_code(),
        5,
        "view did not propagate :cq's exit code; screen:\n{}",
        session.parser.screen().contents()
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
    session.send(b"\x1b[200~alpha\nbeta\x1b[201~");
    assert!(
        session.wait_for("alpha", Duration::from_secs(5)),
        "first pasted line never landed; last screen:\n{}",
        session.parser.screen().contents()
    );
    assert!(
        session.wait_for_cell(1, 0, "b", Duration::from_secs(5)),
        "second pasted line never landed on its own row (both lines collapsed onto \
         one, or newline-splitting broke); last screen:\n{}",
        session.parser.screen().contents()
    );

    // nvim_paste's contract: the whole multi-line paste is one undo unit.
    // A single "u" must remove both lines at once: row 1 reverting all the
    // way back to nvim's "~" empty-line marker (not just an empty line)
    // proves the buffer returned to its pre-paste single-empty-line state,
    // not merely that the second pasted line alone was undone.
    session.send(b"u");
    assert!(
        session.wait_for_cell(1, 0, "~", Duration::from_secs(5)),
        "a single undo did not remove the whole two-line paste as one unit; last screen:\n{}",
        session.parser.screen().contents()
    );

    session.send(b"\x1b:q!\r");
    let _ = session.child.wait();
}
