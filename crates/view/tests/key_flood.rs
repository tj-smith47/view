//! What a paste that arrives as discrete keystrokes costs the runtime loop.
//!
//! `tmux paste-buffer` without `-p` hands the terminal a pasted body one
//! key at a time instead of the single `nvim_paste` a bracketed paste
//! turns into, so the loop holds thousands of already-decoded keys at
//! once. A loop that renders and writes a frame between two keys it
//! already holds pays for a frame no terminal can draw before the next one
//! replaces it, and the paste burns a core for as long as it lasts. The
//! bound below is on the cost of the whole flood, so the frame a drained
//! batch owes is still paid and the frame between two queued keys is not.
//!
//! Linux only, for the instrument rather than for the subject: the cost is
//! a process's own user and system time, and `/proc/<pid>/stat` is where a
//! test can read another process's while it is still running.
#![cfg(target_os = "linux")]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::time::{Duration, Instant};

use view_oracle::PtySession;
use view_test_support::ScratchDir;

const COLS: u16 = 200;
const ROWS: u16 = 50;

/// One pasted line, without its newline.
const LINE: &str = "abcdefghijklmnopqrstuvwxyz0123456789ABCDEFGHIJKLMNOPQRSTUVW";

/// Lines in the pasted body. With `LINE`'s own length plus a newline each,
/// this is the 3000-key flood the paste delivers.
const PAYLOAD_LINES: usize = 50;

/// The line the opened file already holds, so the session can be waited on
/// before anything is typed at it.
const SEED: &str = "KEYFLOODSEED";

/// What the screen says once the engine has reported insert mode, which
/// is also what tells view a printable key is a glyph worth predicting.
const INSERT: &str = "INSERT";

/// How much of the pasted body one write puts in the terminal, which is
/// the order a terminal multiplexer's own paste writes in. Small enough
/// that the descriptor never holds the whole body at once and large enough
/// that a read carries more keys than the loop can answer one at a time.
const CHUNK: usize = 512;

/// Lines of text the opened file already holds under the seed, enough to
/// fill the window at [`ROWS`] and leave a screen's worth below it.
const FILLER_LINES: usize = 200;

/// How much of a core the whole flood may cost view's own process.
///
/// Read off both sides of the defect rather than picked. A loop painting
/// between two keys it already holds answers each of them at the engine's
/// own pace, so the engine sends a redraw batch per key as well, and this
/// flood cost a debug build a shade under twelve seconds of a core; one
/// painting per drained batch cost it sixty milliseconds and drew ten
/// batches. The bar sits an order of magnitude above the second and well
/// under the first, and [`view_test_support::host_deadline`] widens it for
/// the host's load.
const CPU_BUDGET: Duration = Duration::from_millis(600);

/// How long the flood may take from the first key to the written file.
///
/// Wide of the flood this measures and narrow of the one the defect
/// produced: the same body cost a debug build a fifth of a second painting
/// per batch and most of ten seconds painting per key.
const RUN_BUDGET: Duration = Duration::from_secs(8);

/// How long the session may take to reach the opened file's own text.
const STARTUP_BUDGET: Duration = Duration::from_secs(20);

/// The user and system time `pid` has charged so far.
///
/// `/proc/<pid>/stat`'s `utime` and `stime`, which the kernel reports in
/// `USER_HZ` units -- fixed at 100 on Linux for this file whatever the
/// kernel's own tick rate is. The fields are counted from after the
/// command name, which is parenthesised and may itself contain spaces and
/// parens, so the split is on the last `)` rather than on whitespace.
fn cpu_time(pid: u32) -> Option<Duration> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let (_, after_comm) = stat.rsplit_once(')')?;
    let fields: Vec<&str> = after_comm.split_whitespace().collect();
    // utime is field 14 and stime field 15 of the whole line; the first
    // field after the command name is field 3, so they sit at 11 and 12
    let ticks: u64 = fields.get(11)?.parse::<u64>().ok()? + fields.get(12)?.parse::<u64>().ok()?;
    Some(Duration::from_secs_f64(ticks as f64 / 100.0))
}

/// Whether the written file carries every line of the pasted body.
///
/// The lines are compared trimmed: what this asserts is that no key was
/// dropped on the way to the buffer, and an indent nvim's own options put
/// in front of one is not a dropped key.
fn payload_lines_written(path: &std::path::Path) -> usize {
    std::fs::read_to_string(path).map_or(0, |text| {
        text.lines().filter(|line| line.trim() == LINE).count()
    })
}

#[test]
fn a_paste_arriving_as_three_thousand_keys_costs_the_batch_and_not_the_keys() {
    let scratch = ScratchDir::new("key-flood").unwrap();
    let file = scratch.path().join("flood.txt");
    // a window full of text, because what a paint costs is what it has to
    // draw: over an almost empty grid a frame per key and a frame per
    // batch cost the same, and the file a person pastes into is not that
    let mut seeded = format!("{SEED}\n");
    for row in 0..FILLER_LINES {
        seeded.push_str(&format!("{row:04} {}\n", LINE.repeat(3)));
    }
    std::fs::write(&file, seeded).unwrap();

    let mut cmd = portable_pty::CommandBuilder::new(env!("CARGO_BIN_EXE_view"));
    cmd.arg(&file);
    // this session's own standard-path roots, so its engine leaves its
    // data and state here rather than in the hermetic home every other
    // spawn in the suite shares -- a `.local/share` left there refuses the
    // next spawn whatever test it belongs to
    for var in [
        "XDG_CONFIG_HOME",
        "XDG_DATA_HOME",
        "XDG_STATE_HOME",
        "XDG_CACHE_HOME",
    ] {
        cmd.env(var, scratch.path().join(var.to_ascii_lowercase()));
    }
    let mut session = PtySession::spawn_configured(cmd, COLS, ROWS)
        .expect("a pty session for the view binary under test");
    assert!(
        session.wait_for(SEED, view_test_support::host_deadline(STARTUP_BUDGET)),
        "the session never reached the opened file's own text; screen:\n{}",
        session.screen()
    );
    let pid = session.pid().expect("the pty child's pid");

    // insert mode first and observed on the screen, which is what a person
    // pasting has done: the engine's mode change is what tells view a
    // printable key is a glyph it can predict, so a body typed ahead of it
    // reaches the buffer without a prediction and without the frame each
    // one owes
    session.send(b"i").expect("the pty accepts the insert key");
    assert!(
        session.wait_for(INSERT, view_test_support::host_deadline(STARTUP_BUDGET)),
        "the session never reported insert mode; screen:\n{}",
        session.screen()
    );

    let before = cpu_time(pid).expect("view's own cpu time before the flood");
    let started = Instant::now();
    // the body goes down in terminal-sized chunks, which is what an
    // unbracketed paste is: every byte a discrete keystroke, no paste
    // marker around any of them, and as many per read of the descriptor as
    // the terminal had ready. A test writing one key per call paces the
    // flood at its own speed instead, and the loop never holds two keys at
    // once -- which is the whole state being measured
    let mut body = String::new();
    for _ in 0..PAYLOAD_LINES {
        body.push_str(LINE);
        body.push('\r');
    }
    for chunk in body.as_bytes().chunks(CHUNK) {
        session.send(chunk).expect("the pty accepts a pasted chunk");
    }
    session
        .send(b"\x1b:w\r")
        .expect("the pty accepts the write that ends the flood");

    let deadline = started + view_test_support::host_deadline(RUN_BUDGET);
    while payload_lines_written(&file) < PAYLOAD_LINES && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(25));
    }
    let elapsed = started.elapsed();
    let after = cpu_time(pid).expect("view's own cpu time after the flood");
    let spent = after.saturating_sub(before);

    let written = payload_lines_written(&file);
    assert_eq!(
        written,
        PAYLOAD_LINES,
        "only {written} of {PAYLOAD_LINES} pasted lines reached the buffer in {elapsed:?}; \
         screen:\n{}",
        session.screen()
    );
    let run_bar = view_test_support::host_deadline(RUN_BUDGET);
    assert!(
        elapsed <= run_bar,
        "the flood took {elapsed:?}, past the {run_bar:?} bar"
    );
    let cpu_bar = view_test_support::host_deadline(CPU_BUDGET);
    assert!(
        spent <= cpu_bar,
        "the flood cost view {spent:?} of a core, past the {cpu_bar:?} bar -- a frame \
         rendered and written between two keys the loop already held"
    );
}
