//! An agent turn through a real session under tiles, read off the terminal.
//!
//! The pill's word is derived in `view-core` from the panel's turn flag and
//! pinned there, and none of that says what a person sees once the turn
//! ends: the row carrying the word has to leave the screen, and a row that
//! leaves while nvim's resize reply is still in flight can leave its old
//! cells standing. This drives the binary a user runs against the stub
//! agent and reads every cell.
#![cfg(unix)]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use std::path::{Path, PathBuf};
use std::time::Duration;

use view_oracle::PtySession;

const COLS: u16 = 120;
const ROWS: u16 = 30;
const BUDGET: Duration = Duration::from_secs(30);

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
        .get_or_init(|| {
            let profile = if cfg!(debug_assertions) {
                "debug"
            } else {
                "release"
            };
            let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_string());
            let status = std::process::Command::new(cargo)
                .args([
                    "build",
                    "-p",
                    "view-ai",
                    "--features",
                    "test-support",
                    "--bin",
                    "view-ai-stub-agent",
                ])
                .status()
                .expect("failed to invoke cargo build for the stub agent");
            assert!(status.success(), "building view-ai-stub-agent failed");
            view_oracle::target_root()
                .join(profile)
                .join("view-ai-stub-agent")
        })
        .clone()
}

fn every_row(screen: &vt100::Screen) -> String {
    (0..ROWS)
        .map(|row| {
            (0..COLS)
                .filter_map(|col| screen.cell(row, col).map(vt100::Cell::contents))
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Starts view under tiles in a workspace holding the diff seed, with the
/// stub agent configured to hold its `propose-when-released` turn until
/// `resume` exists.
fn launch(paths: &common::ScratchPaths, work: &Path, resume: &Path) -> PtySession {
    std::fs::create_dir_all(work).unwrap();
    std::fs::write(work.join(DIFF_FILE), DIFF_SEED).unwrap();
    let mut cmd = portable_pty::CommandBuilder::new(common::view_bin_path());
    cmd.cwd(work);
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

/// The word shows while the turn is held, and no cell carries it once the
/// reply that ends the turn has been folded, which the review it carries
/// being on screen marks.
#[test]
fn the_agent_word_leaves_the_row_the_frame_after_turn_ended() {
    let paths = common::ScratchPaths::new("agent-turn");
    let work = paths.isolated_home.join("work");
    let resume = paths.isolated_home.join("resume");
    let mut session = launch(&paths, &work, &resume);

    session.send(b"\x1b:View ai open\r").unwrap();
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

    session.send(b"propose-when-released\r").unwrap();
    assert!(
        session.wait_for_screen(BUDGET, |screen| every_row(screen).contains("running")),
        "a held turn never put the agent word on screen; screen:\n{}",
        session.screen()
    );

    std::fs::write(&resume, b"").unwrap();
    assert!(
        session.wait_for("Review", BUDGET),
        "the released turn never raised its review; screen:\n{}",
        session.screen()
    );
    let ended = |screen: &vt100::Screen| !every_row(screen).contains("running");
    assert!(
        session.wait_for_screen(BUDGET, ended),
        "the agent word stayed on screen after the turn ended; screen:\n{}",
        session.screen()
    );
    // a cell the row's departure left unpainted comes back with the next
    // frame that repaints around it, so the word is watched for a while
    // after it first went
    assert!(
        !session.wait_for_screen(STAYS_GONE, |screen| every_row(screen).contains("running")),
        "the agent word came back after the turn ended; screen:\n{}",
        session.screen()
    );
}

/// How long the screen is watched after the word first leaves it.
const STAYS_GONE: Duration = Duration::from_secs(2);
