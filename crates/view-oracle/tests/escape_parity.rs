//! Falsifiable proof that an escape run reaching `view` from a terminal is
//! read the way the pinned engine reads the same bytes from a terminal of
//! its own: two `ESC`s in one read are two Escape keys, a run that stops
//! short is given up on at `ttimeoutlen`, and a key code behind a doubled
//! `ESC` is still that key code.
//!
//! Each run is written at two ptys -- one hosting the wired `view` binary,
//! one hosting the pinned `nvim` directly -- with the same normal-mode
//! mapping installed in both, and the two screens must agree. Nothing here
//! is transcribed from either decoder: what a run means is whichever
//! mapping the editor runs, so a reading `view` invents fails here whatever
//! its own table says.
//!
//! The bare-nvim side is the terminal case for the same reason the `view`
//! side is: an `--embed` child never decodes a terminal byte at all, and it
//! is that decode -- nvim's own tty input layer, `handle_forced_escape` and
//! the `ttimeout` wait behind it -- that this is parity with.
//!
//! Both sessions run over [`view_oracle::pty::QueryPolicy::AnswerDa1`], so
//! neither editor pushes the kitty keyboard protocol and the bytes below
//! are what a real terminal would then send. A terminal that speaks the
//! protocol spells the Escape key `CSI 27 u` and sends none of these runs;
//! that crossing is pinned in `view-tui/src/keys.rs`.
#![cfg(unix)]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use std::time::Duration;

use view_oracle::PtySession;

const COLS: u16 = 80;
const ROWS: u16 = 24;

/// Budget for anything either session has to do: a cold `view` spawn plus
/// an nvim spawn on a loaded box is the slow part, and every wait here is
/// satisfied in milliseconds by a healthy session -- the flush that leg two
/// waits on is `ttimeoutlen` behind the write, which is 50 ms of it.
const BUDGET: Duration = Duration::from_secs(20);

/// The bytes a terminal writes, the notation a user's mapping is written
/// in, and the text that mapping writes into line 1.
///
/// Each marker is a word nothing else on either screen spells, so a screen
/// carrying it can only be the mapping having run.
///
/// The first three runs are the three readings a folded `ESC` gets wrong.
/// Two `ESC`s in one read are one Escape key to a parser that treats the
/// second as the first one's payload, so a mapping written against the pair
/// never fires. A run that stops after the introducer is a key code no
/// further byte will ever complete, so a parser with no wait of its own
/// holds it -- and the `[` a user mapped never arrives. And a key code
/// behind a doubled `ESC` loses its own introducer to that fold, arriving
/// as the literal characters of the sequence.
///
/// The fourth run is the same bytes as the second with the chord itself
/// mapped, and it is the only one of the four that can tell the two
/// readings of them apart: unmapped, nvim degrades `<M-[>` to `<Esc>`
/// followed by `[`, so a decoder producing either reading fires the `[`
/// mapping and leg two passes for both. Mapped, only the chord fires it.
const RUNS: [(&str, &[u8], &str); 4] = [
    ("<Esc><Esc>", b"\x1b\x1b", "REACHEDDOUBLEESCAPE"),
    ("[", b"\x1b[", "REACHEDBAREBRACKET"),
    ("<Up>", b"\x1b\x1b[A", "REACHEDUPBEHINDESCAPE"),
    ("<M-[>", b"\x1b[", "REACHEDMETABRACKET"),
];

/// A `view` session with every native feature off, so the screen it paints
/// is nvim's own content and the two sides are comparable row for row.
fn view_session(paths: &common::ScratchPaths, view_log: &std::path::Path) -> PtySession {
    let mut cmd = portable_pty::CommandBuilder::new(common::view_bin_path());
    cmd.arg(&paths.scratch);
    common::isolate_xdg_native_off(&mut cmd, &paths.isolated_home);
    cmd.env("VIEW_LOG", view_log);
    let mut session = PtySession::spawn_configured(cmd, COLS, ROWS)
        .expect("PtySession::spawn_configured against target/debug/view");
    assert!(
        session.wait_for("~", BUDGET),
        "view never painted nvim's real content; screen:\n{}",
        session.screen()
    );
    session
}

/// The pinned `nvim` in a pty of its own, started on the same scratch file
/// with none of the host's configuration.
fn nvim_session(paths: &common::ScratchPaths) -> PtySession {
    let cfg = view_engine::EngineConfig::isolated();
    let mut cmd = portable_pty::CommandBuilder::new(&cfg.nvim_bin);
    for arg in &cfg.extra_args {
        cmd.arg(arg);
    }
    cmd.arg(&paths.scratch);
    common::isolate_xdg_native_off(&mut cmd, &paths.isolated_home);
    let mut session = PtySession::spawn_configured(cmd, COLS, ROWS)
        .expect("PtySession::spawn_configured against the pinned nvim");
    assert!(
        session.wait_for("~", BUDGET),
        "nvim never painted its own content; screen:\n{}",
        session.screen()
    );
    session
}

/// Which of the markers the session is showing.
///
/// The comparison the two sides can hold each other to: view frames nvim's
/// content in chrome of its own, so the rendered rows are not comparable
/// cell for cell, while the buffer text under them is exactly what the two
/// editors must agree on. A marker on screen is one editor having run one
/// mapping, and the set names which.
fn markers_shown(session: &mut PtySession) -> Vec<&'static str> {
    let screen = session.screen();
    RUNS.iter()
        .filter(|(_, _, marker)| screen.contains(marker))
        .map(|(_, _, marker)| *marker)
        .collect()
}

/// Installs one normal-mode mapping.
///
/// No wait follows it: the pty carries one ordered byte stream, so the
/// `<CR>` that ends this command line is consumed before whatever is
/// written next, and the mapping is in place by the time the run is read.
fn install_mapping(session: &mut PtySession, notation: &str, marker: &str) {
    // the marker is written as two concatenated halves so the command line
    // itself never spells it: nvim leaves the typed command on screen, and a
    // screen carrying the marker has to be the buffer rather than the echo
    // of the mapping that would write it
    let (head, tail) = marker.split_at(marker.len() / 2);
    let command = format!(":nnoremap {notation} :call setline(1,'{head}'.'{tail}')<CR>\r");
    session.send(command.as_bytes()).unwrap();
}

/// Takes the mapping back out and empties the line it wrote, so the next
/// run is read against a screen that carries no earlier answer and a
/// keyboard that holds no earlier prefix.
fn reset(session: &mut PtySession, notation: &str) {
    let command = format!(":nunmap {notation}\r:call setline(1,'')\r");
    session.send(command.as_bytes()).unwrap();
}

/// How long a `SIGSTOP` may take to leave the target off the CPU. Scaled,
/// because it is a scheduling delay and nothing else.
const STOP_TAKES_EFFECT: Duration = Duration::from_secs(2);

/// Sends one signal, through the tool every unix host has.
fn signal(pid: u32, name: &str) {
    let status = std::process::Command::new("kill")
        .arg(format!("-{name}"))
        .arg(pid.to_string())
        .status()
        .expect("kill");
    assert!(status.success(), "kill -{name} {pid} failed");
}

/// Whether the process is in the stopped state `ps` reports as `T`.
fn stopped(pid: u32) -> bool {
    let out = std::process::Command::new("ps")
        .args(["-o", "state=", "-p", &pid.to_string()])
        .output()
        .expect("ps");
    String::from_utf8_lossy(&out.stdout)
        .trim_start()
        .starts_with('T')
}

/// Writes a run with the reading process stopped, so every byte of it is
/// in the terminal's own buffer before that process reads again.
///
/// Which read a run arrives in is the kernel's choice, not the writer's: a
/// reader awake at the first `ESC` can take it alone, and a pair split that
/// way decodes correctly even through a parser that folds it -- so a leg
/// written as a plain `send` passes against the very defect it exists for,
/// at whatever rate the host happens to schedule. Both sides are written
/// this way because the claim is parity over one read, and the engine's own
/// reading of a run it receives whole is what view is being held to.
fn write_as_one_read(session: &mut PtySession, bytes: &[u8]) {
    let pid = session.pid().expect("a live session has a pid");
    signal(pid, "STOP");
    // `kill` returns before the target has left the CPU
    let deadline = std::time::Instant::now() + view_test_support::host_deadline(STOP_TAKES_EFFECT);
    while !stopped(pid) {
        assert!(
            std::time::Instant::now() < deadline,
            "pid {pid} never stopped, so the run below would be read in \
             whatever splits the host chose"
        );
        std::thread::sleep(view_test_support::host_deadline(Duration::from_millis(1)));
    }
    session.send(bytes).unwrap();
    signal(pid, "CONT");
}

/// Types every run at both sessions and holds the two screens to each
/// other, naming which pass failed.
fn each_run_agrees(under_test: &mut PtySession, reference: &mut PtySession, pass: &str) {
    for (notation, bytes, marker) in RUNS {
        install_mapping(under_test, notation, marker);
        install_mapping(reference, notation, marker);

        write_as_one_read(under_test, bytes);
        write_as_one_read(reference, bytes);

        let reached = |screen: &vt100::Screen| screen.contents().contains(marker);
        let seen_by_nvim = reference.wait_for_screen(BUDGET, reached);
        let seen_by_view = under_test.wait_for_screen(BUDGET, reached);
        let by_nvim = markers_shown(reference);
        let by_view = markers_shown(under_test);

        assert!(
            seen_by_nvim,
            "{pass}: the pinned engine did not run its own {notation} mapping for the \
             run {bytes:?}, so this leg proves nothing about view; screen:\n{}",
            reference.screen()
        );
        assert_eq!(
            by_view,
            by_nvim,
            "{pass}, run {bytes:?}: nvim read it as the keys a user wrote {notation} \
             against and view read it as something else (view saw the marker: \
             {seen_by_view}); view's screen:\n{}",
            under_test.screen()
        );
        assert_eq!(
            by_nvim,
            vec![marker],
            "{pass}, run {bytes:?}: both sides agree on a screen that is not the \
             mapping's own marker, so neither ran it"
        );

        reset(under_test, notation);
        reset(reference, notation);
    }
}

/// The whole point: the same bytes, the same mapping, the same screen.
///
/// Both sides are asserted against the marker as well as against each
/// other. Two sessions agreeing on a line that says nothing would be a pass
/// for a `view` that read the run as some key nvim ignores.
///
/// Every run is typed twice because `view` reads a byte through one of two
/// decoders depending on when it lands: the startup guard's own reader
/// while it still owns the terminal, and the steady-state reader
/// afterwards. The guard reads a byte at a time, so a run split across its
/// reads is the one case an escape fold cannot reach -- and the pass that
/// matters for the fold is the second. The first is ordered by nothing and
/// takes whichever window it lands in; the second is ordered against the
/// guard by the session's own `VIEW_LOG` line, which `view` writes only
/// once the handle whose constructor computed that window's deadline
/// exists. Waiting [`common::PROBE_HARD_CAP`] from a line that can only
/// have been written after the deadline was set puts the second pass past
/// it, with no inference about which of two processes reached a statement
/// first.
#[test]
fn every_escape_run_fires_the_mapping_nvims_own_reading_of_it_fires() {
    let view_paths = common::ScratchPaths::new("escape-parity-view");
    let nvim_paths = common::ScratchPaths::new("escape-parity-nvim");
    let view_log = view_paths.isolated_home.join("view.log");
    let mut under_test = view_session(&view_paths, &view_log);
    let mut reference = nvim_session(&nvim_paths);

    each_run_agrees(&mut under_test, &mut reference, "guard window");

    let armed = common::wait_for_log_line(&view_log, "input guard");
    std::thread::sleep(common::PROBE_HARD_CAP.saturating_sub(armed.elapsed()));
    each_run_agrees(&mut under_test, &mut reference, "past the guard");

    for session in [&mut under_test, &mut reference] {
        session.send(b"\x1b:qa!\r").unwrap();
        let _ = session.wait_for_exit(BUDGET);
    }
}
