//! Live check that the picker's keys open what a person chose: the real
//! binary in a pty, a query typed, `<Down>` and `<CR>` sent as the bytes a
//! terminal sends, and nvim's current buffer read back afterwards.
//!
//! Unix only, like every other leg that spawns the binary in a pty.
#![cfg(unix)]
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::path::PathBuf;
use std::time::Duration;

use view_oracle::{PtySession, QueryPolicy};

const COLS: u16 = 100;
const ROWS: u16 = 30;

fn budget() -> Duration {
    view_test_support::host_deadline(Duration::from_secs(20))
}

/// A directory of its own for the picker to search, removed on drop.
struct Tree(PathBuf);

impl Drop for Tree {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Spawns `view` with its working directory at a fresh tree holding
/// `files`, each written with its contents.
fn spawn_in(label: &str, files: &[(&str, &str)]) -> (common::ScratchPaths, Tree, PtySession) {
    let paths = common::ScratchPaths::new(label);
    let tree = Tree(
        paths
            .isolated_home
            .with_file_name(format!("{label}-tree-{}", std::process::id())),
    );
    std::fs::create_dir_all(&tree.0).unwrap();
    for (name, text) in files {
        std::fs::write(tree.0.join(name), text).unwrap();
    }
    let mut cmd = portable_pty::CommandBuilder::new(common::view_bin_path());
    cmd.cwd(&tree.0);
    common::isolate_xdg_first_launch(&mut cmd, &paths.isolated_home);
    let mut session = PtySession::spawn_configured_with(cmd, COLS, ROWS, QueryPolicy::AnswerDa1)
        .expect("PtySession::spawn_configured_with against target/debug/view");
    assert!(
        session.wait_for("~", budget()),
        "view never painted its startup shell; screen:\n{}",
        session.screen()
    );
    (paths, tree, session)
}

/// The names in `names` in the order the screen lists them, top first.
fn listed_order<'a>(screen: &str, names: &[&'a str]) -> Vec<&'a str> {
    let mut found: Vec<(usize, &str)> = names
        .iter()
        .filter_map(|name| screen.find(name).map(|at| (at, *name)))
        .collect();
    found.sort_unstable();
    found.into_iter().map(|(_, name)| name).collect()
}

/// Waits for the picker to close and the statusline to name `tail`. An
/// `:echo` typed straight behind `<CR>` left nothing on screen in four of
/// thirteen runs while the right file opened, so the query waits for the
/// open to show first.
fn opened(session: &mut PtySession, tail: &str) -> bool {
    let statusline = format!("{tail} text");
    session.wait_for_screen(budget(), |screen| {
        let text = screen.contents();
        !text.contains('╭') && text.contains(&statusline)
    })
}

/// Asks nvim for the current buffer's tail and line, and waits for the
/// answer `want` on screen. No `<Esc>` leads the command: the picker is
/// closed, and an `<Esc>` close behind `<CR>` is read as the `Alt` prefix
/// of `:`.
fn current_is(session: &mut PtySession, want: &str) -> bool {
    session
        .send(b":echo 'at=' . expand('%:t') . '@' . line('.')\r")
        .unwrap();
    session.wait_for(&format!("at={want}"), budget())
}

#[test]
fn down_then_enter_opens_the_second_result() {
    let names = ["pkalpha_one.txt", "pkalpha_two.txt"];
    let (_paths, _tree, mut session) = spawn_in(
        "picker-keys-files",
        &[
            (names[0], "first body\n"),
            (names[1], "second body\n"),
            ("unmatched.txt", "third body\n"),
        ],
    );
    session.send(b"\x1b:View picker files\r").unwrap();
    session.send(b"pkalpha").unwrap();
    // the unfiltered list stays on screen until the query's own answer
    // replaces it, in an order of its own, so the order is read once the
    // file the query leaves out has gone
    assert!(
        session.wait_for_screen(budget(), |screen| {
            let text = screen.contents();
            names.iter().all(|name| text.contains(name)) && !text.contains("unmatched.txt")
        }),
        "the picker never listed the two matching files alone; screen:\n{}",
        session.screen()
    );
    let order = listed_order(&session.screen(), &names);
    assert_eq!(order.len(), 2, "{order:?}");

    session.send(b"\x1b[B").unwrap();
    session.send(b"\r").unwrap();
    assert!(
        opened(&mut session, order[1]),
        "{} never opened; screen:\n{}",
        order[1],
        session.screen()
    );
    assert!(
        current_is(&mut session, &format!("{}@1", order[1])),
        "the second result, {}, is not the current buffer; screen:\n{}",
        order[1],
        session.screen()
    );
}

#[test]
fn enter_on_a_grep_match_lands_on_its_line() {
    let (_paths, _tree, mut session) = spawn_in(
        "picker-keys-grep",
        &[("pkgrep.txt", "one\ntwo\nthe pkneedle line\nfour\n")],
    );
    session.send(b"\x1b:View picker grep\r").unwrap();
    session.send(b"pkneedle").unwrap();
    assert!(
        session.wait_for("pkgrep.txt:3:", budget()),
        "the grep picker never listed the match; screen:\n{}",
        session.screen()
    );
    session.send(b"\r").unwrap();
    assert!(
        opened(&mut session, "pkgrep.txt"),
        "pkgrep.txt never opened; screen:\n{}",
        session.screen()
    );
    assert!(
        current_is(&mut session, "pkgrep.txt@3"),
        "the match did not open on its line; screen:\n{}",
        session.screen()
    );
}
