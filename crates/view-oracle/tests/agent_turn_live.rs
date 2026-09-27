//! The agent panel through a real session under tiles, read off the
//! terminal.
//!
//! The pill's word is derived in `view-core` from the panel's turn flag and
//! pinned there, and none of that says what a person sees once the turn
//! ends: the row carrying the word has to leave the screen, and a row that
//! leaves while nvim's resize reply is still in flight can leave its old
//! cells standing. The ways into the panel are here too, since each is a
//! claim about which keys reach it and when. This drives the binary a user
//! runs against the stub agent and reads every cell.
#![cfg(unix)]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use view_oracle::PtySession;

// wide enough that the panel's title keeps its whole focus hint beside
// the stub's long label and the gapped look's gutter
const COLS: u16 = 132;
const ROWS: u16 = 30;
const BUDGET: Duration = Duration::from_secs(30);
/// How long the pill's row may stand once the review has risen: the turn
/// ends in the stub's next message, a frame or two later, and a pill that
/// lingers is one a person reads as a turn still running.
const PILL_AFTER_REVIEW: Duration = Duration::from_millis(250);

/// The file the stub's `propose` diffs, seeded with the text its edit
/// expects to find.
const DIFF_FILE: &str = "view-ai-stub-diff.txt";
const DIFF_SEED: &str = "alpha\nbeta\ngamma\n";

/// The stub agent binary, built once per test process the way
/// [`common::view_bin_path`] builds view: a stale fixture is a pass or a
/// failure of code that is no longer in the tree.
fn stub_agent_bin() -> PathBuf {
    static BUILT: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();
    BUILT
        .get_or_init(|| common::built_bin("view-ai", "view-ai-stub-agent", &["test-support"]))
        .clone()
}

fn row_text(screen: &vt100::Screen, row: u16) -> String {
    (0..COLS)
        .filter_map(|col| screen.cell(row, col).map(vt100::Cell::contents))
        .collect()
}

fn every_row(screen: &vt100::Screen) -> String {
    (0..ROWS)
        .map(|row| row_text(screen, row))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Starts view under tiles in a workspace holding the diff seed, with the
/// stub agent configured to hold its `propose-when-released` turn until
/// `resume` exists. `args` go to view ahead of the file.
fn launch(paths: &common::ScratchPaths, work: &Path, resume: &Path, args: &[&str]) -> PtySession {
    std::fs::create_dir_all(work).unwrap();
    std::fs::write(work.join(DIFF_FILE), DIFF_SEED).unwrap();
    let mut cmd = portable_pty::CommandBuilder::new(common::view_bin_path());
    cmd.cwd(work);
    cmd.args(args);
    cmd.arg(DIFF_FILE);
    common::isolate_xdg_first_launch(&mut cmd, &paths.isolated_home);
    let config = common::xdg_home(&paths.isolated_home, "XDG_CONFIG_HOME")
        .join("view")
        .join("view.toml");
    std::fs::write(
        &config,
        format!(
            "[ui]\npanes = \"tiles\"\n\n[ai]\nagent = [{:?}, {:?}]\n",
            stub_agent_bin().to_string_lossy(),
            resume.to_string_lossy()
        ),
    )
    .unwrap();
    let mut session = PtySession::spawn_configured(cmd, COLS, ROWS)
        .expect("PtySession::spawn_configured against target/debug/view");
    assert!(
        session.wait_for("alpha", BUDGET),
        "view never painted the seeded file; screen:\n{}",
        session.screen()
    );
    session
}

/// Opens the panel and answers the trust prompt a fresh workspace raises.
fn open_and_trust(session: &mut PtySession) {
    session.send(b"\x1b:View ai open\r").unwrap();
    answer_trust(session);
}

fn answer_trust(session: &mut PtySession) {
    assert!(
        session.wait_for("Trust ", BUDGET),
        "opening the panel in a fresh workspace raised no trust prompt; screen:\n{}",
        session.screen()
    );
    session.send(b"y").unwrap();
    assert!(
        session.wait_for_screen(BUDGET, |screen| !every_row(screen).contains("Trust ")),
        "the trust answer never took the prompt down; screen:\n{}",
        session.screen()
    );
}

/// Whether the pill has left the top row. Under tiles that row is the
/// lattice's top gap, which an idle session leaves blank.
fn pill_row_gone(screen: &vt100::Screen) -> bool {
    row_text(screen, 0).trim().is_empty()
}

/// Every frame from the prompt to the end of the turn is read. The word
/// has to show while the turn is held, and the first frame on which the
/// pill's row has left the screen carries no cell of it: a row that leaves
/// while its old cells still stand is the defect this reads for. The row
/// leaves within [`PILL_AFTER_REVIEW`] of the review rising.
#[test]
fn the_agent_word_is_gone_on_the_frame_its_row_leaves() {
    let paths = common::ScratchPaths::new("agent-turn");
    let work = paths.isolated_home.join("work");
    let resume = paths.isolated_home.join("resume");
    let mut session = launch(&paths, &work, &resume, &[]);
    open_and_trust(&mut session);

    session.send(b"propose-when-released\r").unwrap();
    let mut saw_running = false;
    let mut left: Option<String> = None;
    let mut review_at: Option<(Instant, String)> = None;
    let mut pill_after_review = Duration::ZERO;
    let settled = session.wait_for_screen(BUDGET, |screen| {
        if !saw_running {
            if every_row(screen).contains("running") && !pill_row_gone(screen) {
                saw_running = true;
                std::fs::write(&resume, b"").unwrap();
            }
            return false;
        }
        if pill_row_gone(screen) {
            left = Some(every_row(screen));
            if let Some((at, _)) = &review_at {
                pill_after_review = at.elapsed();
            }
            return true;
        }
        if review_at.is_none() && every_row(screen).contains("Review") {
            review_at = Some((Instant::now(), every_row(screen)));
        }
        false
    });
    // the review rises with the edit, and the stub ends the turn in the
    // message after it, so the two may take one frame each
    if let Some((_, frame)) = review_at {
        assert!(
            pill_after_review <= PILL_AFTER_REVIEW,
            "the pill's row stood {pill_after_review:?} after the review rose, past \
             {PILL_AFTER_REVIEW:?}; the first frame with the review:\n{frame}"
        );
    }
    assert!(
        saw_running,
        "a held turn never put the agent word on the pill's row; screen:\n{}",
        session.screen()
    );
    let left = left.unwrap_or_else(|| {
        panic!(
            "the pill's row never left the screen after the turn was released \
             (settled: {settled}); screen:\n{}",
            session.screen()
        )
    });
    assert!(
        !left.contains("running"),
        "the frame on which the pill's row left still carries the agent word:\n{left}"
    );
    assert!(
        session.wait_for("Review", BUDGET),
        "the released turn never raised its review; screen:\n{}",
        session.screen()
    );
    // a cell the row's departure left unpainted comes back with the next
    // frame that repaints around it, so the word is watched for a while
    // after it went
    assert!(
        !session.wait_for_screen(STAYS_GONE, |screen| every_row(screen).contains("running")),
        "the agent word came back after the turn ended; screen:\n{}",
        session.screen()
    );
}

/// How long the screen is watched after the word first leaves it.
const STAYS_GONE: Duration = Duration::from_secs(2);

/// `view -c 'View ai open'` runs the command during nvim's startup, before
/// `VimEnter`, and the panel it opens asks its trust question like any
/// other way in.
#[test]
fn a_view_command_given_at_launch_opens_the_panel() {
    let paths = common::ScratchPaths::new("agent-launch-cmd");
    let work = paths.isolated_home.join("work");
    let resume = paths.isolated_home.join("resume");
    let mut session = launch(&paths, &work, &resume, &["-c", "View ai open"]);
    answer_trust(&mut session);
    assert!(
        session.wait_for(": Esc returns", BUDGET),
        "the panel `-c 'View ai open'` asked for never took the keyboard; screen:\n{}",
        session.screen()
    );
}

/// Keys written in the same burst as `<leader>ai` are typed into the panel
/// it enters, and the buffer is left as it was.
#[test]
fn keys_typed_behind_the_leader_key_that_opens_the_panel_reach_it() {
    let paths = common::ScratchPaths::new("agent-leader-typeahead");
    let work = paths.isolated_home.join("work");
    let resume = paths.isolated_home.join("resume");
    let mut session = launch(&paths, &work, &resume, &[]);
    open_and_trust(&mut session);
    assert!(
        session.wait_for(": Esc returns", BUDGET),
        "the trusted panel never took the keyboard; screen:\n{}",
        session.screen()
    );
    session.send(b"\x1b").unwrap();
    assert!(
        session.wait_for_screen(BUDGET, |screen| !every_row(screen)
            .contains(": Esc returns")),
        "Escape never left the composer; screen:\n{}",
        session.screen()
    );

    // no config sets a leader here, so it is nvim's own backslash
    session.send(b"\\aihello").unwrap();
    assert!(
        session.wait_for("> hello", BUDGET),
        "the keys behind <leader>ai never reached the composer; screen:\n{}",
        session.screen()
    );

    let after = work.join("after.txt");
    session.send(b"\x1b").unwrap();
    assert!(
        session.wait_for_screen(BUDGET, |screen| !every_row(screen)
            .contains(": Esc returns")),
        "Escape never left the composer; screen:\n{}",
        session.screen()
    );
    session
        .send(format!("\x1b:silent write {}\r", after.display()).as_bytes())
        .unwrap();
    let written = view_test_support::host_deadline(BUDGET);
    let started = std::time::Instant::now();
    while !after.exists() && started.elapsed() < written {
        std::thread::sleep(view_test_support::host_deadline(Duration::from_millis(50)));
    }
    assert_eq!(
        std::fs::read_to_string(&after).unwrap_or_default(),
        DIFF_SEED,
        "the buffer changed under keys that were typed into the panel; screen:\n{}",
        session.screen()
    );
}

/// Alt with a key the user's own config maps runs that mapping from inside
/// the panel: the panel lets go of the keyboard and nvim gets the Meta key
/// whole, so the mapping fires and the composer types nothing.
#[test]
fn a_user_meta_mapping_fires_from_the_panel() {
    let paths = common::ScratchPaths::new("agent-meta-map");
    let work = paths.isolated_home.join("work");
    let resume = paths.isolated_home.join("resume");
    let fired = work.join("fired.txt");
    let map = format!(
        "nnoremap <M-y> <Cmd>call writefile(['fired'], '{}')<CR>",
        fired.display()
    );
    let mut session = launch(&paths, &work, &resume, &["-c", &map]);
    open_and_trust(&mut session);
    assert!(
        session.wait_for(": Esc returns", BUDGET),
        "the trusted panel never took the keyboard; screen:\n{}",
        session.screen()
    );
    session.send(b"\x1by").unwrap();
    let deadline = view_test_support::host_deadline(BUDGET);
    let started = std::time::Instant::now();
    while !fired.exists() && started.elapsed() < deadline {
        std::thread::sleep(view_test_support::host_deadline(Duration::from_millis(50)));
    }
    assert_eq!(
        std::fs::read_to_string(&fired).unwrap_or_default(),
        "fired\n",
        "Alt+y in the panel never ran the user's <M-y> mapping; screen:\n{}",
        session.screen()
    );
    assert!(
        !every_row(session.screen_raw()).contains("> y"),
        "the composer typed the y; screen:\n{}",
        session.screen()
    );
}

/// Keys written in the same burst as the `:View` command that opens the
/// panel are typed into the panel. nvim runs the command before it reads
/// the keys behind it, so a panel that took focus only once the command's
/// notice came back would leave them to nvim as normal-mode input, and
/// `hello` would edit the buffer.
#[test]
fn keys_typed_behind_the_command_that_opens_the_panel_reach_it() {
    let paths = common::ScratchPaths::new("agent-typeahead");
    let work = paths.isolated_home.join("work");
    let resume = paths.isolated_home.join("resume");
    let mut session = launch(&paths, &work, &resume, &[]);
    open_and_trust(&mut session);
    assert!(
        session.wait_for(": Esc returns", BUDGET),
        "the trusted panel never took the keyboard; screen:\n{}",
        session.screen()
    );
    // out of the composer, so the burst below starts from normal mode in
    // the buffer with the panel still framed
    session.send(b"\x1b").unwrap();
    assert!(
        session.wait_for_screen(BUDGET, |screen| !every_row(screen)
            .contains(": Esc returns")),
        "Escape never left the composer; screen:\n{}",
        session.screen()
    );

    session.send(b"\x1b:View ai open\rhello").unwrap();
    assert!(
        session.wait_for("> hello", BUDGET),
        "the keys behind `:View ai open` never reached the composer; screen:\n{}",
        session.screen()
    );

    let after = work.join("after.txt");
    session.send(b"\x1b").unwrap();
    assert!(
        session.wait_for_screen(BUDGET, |screen| !every_row(screen)
            .contains(": Esc returns")),
        "Escape never left the composer; screen:\n{}",
        session.screen()
    );
    session
        .send(format!("\x1b:silent write {}\r", after.display()).as_bytes())
        .unwrap();
    let written = view_test_support::host_deadline(BUDGET);
    let started = std::time::Instant::now();
    while !after.exists() && started.elapsed() < written {
        std::thread::sleep(view_test_support::host_deadline(Duration::from_millis(50)));
    }
    assert_eq!(
        std::fs::read_to_string(&after).unwrap_or_default(),
        DIFF_SEED,
        "the buffer changed under keys that were typed into the panel; screen:\n{}",
        session.screen()
    );
}
