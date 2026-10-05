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

/// Asks nvim for the current buffer's tail and line, and waits for the
/// answer `want` on screen. Sent straight behind the key that opened it,
/// so the question is typed into whatever that open left. No `<Esc>` leads
/// the command: an `<Esc>` behind `<CR>` is read as the `Alt` prefix of `:`.
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
        current_is(&mut session, &format!("{}@1", order[1])),
        "the second result, {}, is not the current buffer; screen:\n{}",
        order[1],
        session.screen()
    );
}

/// Opens the selected row in a new tab and asks, in the same keystrokes,
/// which buffer the `tabs`th tab shows.
fn buffer_in_new_tab(session: &mut PtySession, tabs: u64) -> Option<u64> {
    session
        .send(b"\x14:echo 'pk=' . tabpagenr('$') . '/' . bufnr('%') . '.'\r")
        .unwrap();
    let want = format!("pk={tabs}/");
    if !session.wait_for(&want, budget()) {
        return None;
    }
    let screen = session.screen();
    let at = screen.find(&want)? + want.len();
    screen[at..].split('.').next()?.parse().ok()
}

#[test]
fn each_of_two_unnamed_buffers_opens_the_one_chosen() {
    let (_paths, _tree, mut session) = spawn_in("picker-keys-buffers", &[]);
    session.send(b":call setline(1, 'pkone')\r").unwrap();
    session.send(b":enew\r").unwrap();
    session.send(b":call setline(1, 'pktwo')\r").unwrap();
    assert!(
        session.wait_for("pktwo", budget()),
        "the second unnamed buffer never showed; screen:\n{}",
        session.screen()
    );
    let mut opened = Vec::new();
    for (downs, tabs) in [(0, 2), (1, 3)] {
        let before = session.screen().matches("[No Name]").count();
        session.send(b"\x1b:View picker buffers\r").unwrap();
        assert!(
            session.wait_for_screen(budget(), |screen| {
                screen.contents().matches("[No Name]").count() >= before + 2
            }),
            "the picker never listed both unnamed buffers; screen:\n{}",
            session.screen()
        );
        for _ in 0..downs {
            session.send(b"\x1b[B").unwrap();
        }
        let buffer = buffer_in_new_tab(&mut session, tabs);
        assert!(
            buffer.is_some(),
            "row {downs} never opened in a new tab; screen:\n{}",
            session.screen()
        );
        opened.extend(buffer);
    }
    assert_ne!(opened[0], opened[1], "both rows opened one buffer");
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
        current_is(&mut session, "pkgrep.txt@3"),
        "the match did not open on its line; screen:\n{}",
        session.screen()
    );
}

const OLD: &str = "pkold.txt";
const NEW: &str = "pknew.txt";

/// A session editing [`OLD`], with [`NEW`] beside it to be chosen.
fn editing_old(label: &str) -> (common::ScratchPaths, Tree, PtySession) {
    let (paths, tree, mut session) =
        spawn_in(label, &[(OLD, "o1\no2\no3\n"), (NEW, "n1\nn2\nn3\n")]);
    session.send(format!(":edit {OLD}\r").as_bytes()).unwrap();
    assert!(
        session.wait_for("o3", budget()),
        "{OLD} never opened; screen:\n{}",
        session.screen()
    );
    (paths, tree, session)
}

/// Asks nvim for both files' lines and waits for the answer `want`.
fn lines_are(session: &mut PtySession, want: &str) -> bool {
    session
        .send(
            format!(
                ":echo 'ls=' . join(getbufline('{OLD}', 1, '$'), ',') . '|' \
                 . join(getbufline('{NEW}', 1, '$'), ',')\r"
            )
            .as_bytes(),
        )
        .unwrap();
    session.wait_for(&format!("ls={want}"), budget())
}

/// `dd` typed in the same keystrokes as the `<CR>` choosing a file deletes
/// the chosen file's first line and leaves the file open before it alone.
#[test]
fn a_key_typed_right_after_choosing_from_the_picker_acts_in_that_file() {
    let (_paths, _tree, mut session) = editing_old("picker-keys-typed-ahead");
    let shown = session.screen().matches(OLD).count();
    session.send(b"\x1b:View picker files\r").unwrap();
    session.send(b"pknew").unwrap();
    // the unfiltered list goes once the query's answer replaces it
    assert!(
        session.wait_for_screen(budget(), |screen| {
            let text = screen.contents();
            text.contains(NEW) && text.matches(OLD).count() == shown
        }),
        "the picker never listed {NEW}; screen:\n{}",
        session.screen()
    );
    session.send(b"\rdd").unwrap();
    assert!(
        lines_are(&mut session, "o1,o2,o3|n2,n3"),
        "the keys behind the open acted elsewhere; screen:\n{}",
        session.screen()
    );
}

/// A file deleted after the picker listed it says so on screen when
/// chosen, and the file open before it stays.
#[test]
fn a_file_deleted_after_it_was_listed_says_so_when_chosen() {
    let (_paths, tree, mut session) = editing_old("picker-keys-deleted");
    let shown = session.screen().matches(OLD).count();
    session.send(b"\x1b:View picker files\r").unwrap();
    session.send(b"pknew").unwrap();
    assert!(
        session.wait_for_screen(budget(), |screen| {
            let text = screen.contents();
            text.contains(NEW) && text.matches(OLD).count() == shown
        }),
        "the picker never listed {NEW}; screen:\n{}",
        session.screen()
    );
    std::fs::remove_file(tree.0.join(NEW)).unwrap();
    session.send(b"\r").unwrap();
    assert!(
        // the toast wraps the full path, so its last word is the one read
        session.wait_for("exists", budget()),
        "choosing a deleted file said nothing; screen:\n{}",
        session.screen()
    );
    assert!(
        current_is(&mut session, &format!("{OLD}@1")),
        "the deleted file's open moved; screen:\n{}",
        session.screen()
    );
}

/// The same keystrokes from the file tree.
#[test]
fn a_key_typed_right_after_choosing_from_the_tree_acts_in_that_file() {
    let (_paths, _tree, mut session) = editing_old("tree-typed-ahead");
    session.send(b"\x1b:View tree\r").unwrap();
    assert!(
        session.wait_for(NEW, budget()),
        "the tree never listed {NEW}; screen:\n{}",
        session.screen()
    );
    // the tree lists the two files by name, the chosen one second
    session.send(b"j\rdd").unwrap();
    assert!(
        lines_are(&mut session, "o1,o2,o3|n2,n3"),
        "the keys behind the open acted elsewhere; screen:\n{}",
        session.screen()
    );
}
