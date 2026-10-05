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
    spawn_with(label, files, &[])
}

/// [`spawn_in`] with `env` set on the process.
fn spawn_with(
    label: &str,
    files: &[(&str, &str)],
    env: &[(&str, &str)],
) -> (common::ScratchPaths, Tree, PtySession) {
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
    for (key, value) in env {
        cmd.env(key, value);
    }
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

/// The setting that docks the file tree as a window beside the file.
const TREE_WINDOWED: (&str, &str) = ("VIEW_UI_SURFACES_TREE_PLACEMENT", "windowed");
/// The setting that docks the agent panel as a window beside the file.
const AGENT_WINDOWED: (&str, &str) = ("VIEW_UI_SURFACES_AGENT_PLACEMENT", "windowed");
/// The command line that opens the files picker from wherever the cursor is.
const PICKER: &[u8] = b"\x1b:View picker files\r";

/// A session editing [`OLD`], with [`NEW`] beside it to be chosen.
fn editing_old(label: &str) -> (common::ScratchPaths, Tree, PtySession) {
    editing_old_with(label, &[])
}

/// [`editing_old`] with `env` set on the process.
fn editing_old_with(label: &str, env: &[(&str, &str)]) -> (common::ScratchPaths, Tree, PtySession) {
    let (paths, tree, mut session) =
        spawn_with(label, &[(OLD, "o1\no2\no3\n"), (NEW, "n1\nn2\nn3\n")], env);
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
    typed_ahead_from_the_picker(&mut session, PICKER);
}

/// The same keystrokes from a picker opened by its key typed in the docked
/// tree, so the cursor is in the tree's window when the file is chosen.
#[test]
fn a_key_typed_right_after_choosing_from_the_picker_over_a_docked_tree_acts_in_that_file() {
    let (_paths, _tree, mut session) = tree_open("picker-over-docked-tree", &[TREE_WINDOWED]);
    // no config sets a leader here, so it is nvim's own backslash
    typed_ahead_from_the_picker(&mut session, b"\\ff");
}

/// The same keystrokes from a picker opened as the docked agent panel is
/// entered, so the cursor is in the panel's window when the file is
/// chosen. The panel types every key it is sent, so the picker's own key
/// cannot be typed there.
#[test]
fn a_key_typed_right_after_choosing_from_the_picker_over_a_docked_agent_acts_in_that_file() {
    let (_paths, _tree, mut session) = agent_docked("picker-over-docked-agent");
    typed_ahead_from_the_picker(&mut session, b"\x1b:View ai open | View picker files\r");
}

/// A session editing [`OLD`] with the docked agent panel opened, trusted
/// and left, so the next `:View ai open` enters its window.
fn agent_docked(label: &str) -> (common::ScratchPaths, Tree, PtySession) {
    agent_docked_with(label, &[])
}

/// [`agent_docked`] with `env` also set on the process.
fn agent_docked_with(
    label: &str,
    env: &[(&str, &str)],
) -> (common::ScratchPaths, Tree, PtySession) {
    // an agent no build provides, so a stray prompt launches nothing
    let mut all = vec![AGENT_WINDOWED, ("VIEW_AI_AGENT", "none")];
    all.extend_from_slice(env);
    let (paths, tree, mut session) = editing_old_with(label, &all);
    session.send(b"\x1b:View ai open\r").unwrap();
    assert!(
        session.wait_for("Trust ", budget()),
        "the agent panel raised no trust prompt; screen:\n{}",
        session.screen()
    );
    // the ruler reads the panel's empty buffer while the cursor is in its
    // window, and the file's first line once it is back
    session.send(b"y").unwrap();
    assert!(
        session.wait_for("0,0-1", budget()),
        "the trusted panel never took the cursor; screen:\n{}",
        session.screen()
    );
    session.send(b"\x1b").unwrap();
    assert!(
        session.wait_for_screen(budget(), |screen| {
            let text = screen.contents();
            !text.contains("0,0-1") && text.contains(OLD)
        }),
        "the panel kept the cursor; screen:\n{}",
        session.screen()
    );
    (paths, tree, session)
}

/// Chooses [`NEW`] in the files picker `open` opens, with `dd` in the same
/// keystrokes as the `<CR>`, and checks the `dd` deleted the chosen file's
/// first line and left the file open before it alone.
fn typed_ahead_from_the_picker(session: &mut PtySession, open: &[u8]) {
    query_new(session, open);
    session.send(b"\rdd").unwrap();
    assert!(
        lines_are(session, "o1,o2,o3|n2,n3"),
        "the keys behind the open acted elsewhere; screen:\n{}",
        session.screen()
    );
}

/// Opens the files picker with `open` and, in the same write, types a
/// query only [`NEW`] matches, and waits for the picker to list it alone.
fn query_new(session: &mut PtySession, open: &[u8]) {
    session.send(&[open, b"pknew"].concat()).unwrap();
    // the unfiltered list goes once the query's answer replaces it, and a
    // name outside the picker's frame (a sidebar, the status line) has no
    // border to its left
    let listed = |text: &str, name: &str| {
        text.lines().any(|row| {
            row.find('│')
                .is_some_and(|bar| row.match_indices(name).any(|(at, _)| bar < at))
        })
    };
    assert!(
        session.wait_for_screen(budget(), |screen| {
            let text = screen.contents();
            listed(&text, NEW) && !listed(&text, OLD)
        }),
        "the picker never listed {NEW} alone; screen:\n{}",
        session.screen()
    );
}

/// The id of the autocmd that reports each `:` line, read through a
/// message carrying `tag` so an earlier answer on screen is never read.
/// The marker is joined in nvim, so the typed line never carries it.
fn line_report_hook(session: &mut PtySession, tag: &str) -> String {
    session
        .send(
            format!(
                ":echo 'hook' . '{tag}=' . luaeval(\"vim.api.nvim_get_autocmds(\
                 {{ group = 'view_line_ran' }})[1].id\") . '='\r"
            )
            .as_bytes(),
        )
        .unwrap();
    let marker = format!("hook{tag}=");
    assert!(
        session.wait_for_screen(budget(), |screen| {
            let text = screen.contents();
            text.split(&marker)
                .nth(1)
                .is_some_and(|rest| rest.contains('='))
        }),
        "the report hook was never read; screen:\n{}",
        session.screen()
    );
    let text = session.screen();
    let rest = text.split(&marker).nth(1).unwrap_or_default();
    rest.split('=').next().unwrap_or_default().to_string()
}

/// Keys typed in the same write as a view command that refuses act in the
/// file once the line has run, released by its report and never by the
/// bound, which registers the report again, and the refusal is kept in
/// nvim's messages.
#[test]
fn a_key_typed_behind_a_refused_view_command_acts_in_the_file() {
    let (_paths, _tree, mut session) =
        editing_old_with("picker-keys-refused-command", &[("VIEW_AI_AGENT", "none")]);
    let hook = line_report_hook(&mut session, "before");
    session
        .send(b"\x1b:View dvr export $VIEW_KEYS_NEVER_SET/x\rdd")
        .unwrap();
    assert!(
        session.wait_for_screen(budget(), |screen| {
            let text = screen.contents();
            !text.contains("o1") && text.contains("o2")
        }),
        "the keys behind the refused command never deleted a line; screen:\n{}",
        session.screen()
    );
    assert_eq!(
        line_report_hook(&mut session, "after"),
        hook,
        "the keys behind the refused command waited for the bound"
    );
    assert!(
        lines_are(&mut session, "o2,o3|"),
        "the keys behind the refused command acted elsewhere; screen:\n{}",
        session.screen()
    );
    session
        .send(b":echo 'said=' . (execute('messages') =~# 'NEVER_SET is not set')\r")
        .unwrap();
    assert!(
        session.wait_for("said=1", budget()),
        "the refusal said nothing; screen:\n{}",
        session.screen()
    );
}

/// A query typed in the same write as a line that sleeps before it opens
/// the picker reaches the picker.
#[test]
fn a_query_typed_behind_a_line_that_sleeps_before_the_picker_reaches_it() {
    let (_paths, _tree, mut session) =
        editing_old_with("picker-keys-sleep-line", &[("VIEW_AI_AGENT", "none")]);
    query_new(&mut session, b"\x1b:sleep 10m | View picker files\r");
}

/// A file deleted after the picker listed it says so on screen when
/// chosen, and the file open before it stays.
#[test]
fn a_file_deleted_after_it_was_listed_says_so_when_chosen() {
    let (_paths, tree, mut session) = editing_old("picker-keys-deleted");
    query_new(&mut session, PICKER);
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

/// A session editing [`OLD`] with the file tree open on it, placed as
/// `env` says.
fn tree_open(label: &str, env: &[(&str, &str)]) -> (common::ScratchPaths, Tree, PtySession) {
    let (paths, tree, mut session) = editing_old_with(label, env);
    session.send(b"\x1b:View tree\r").unwrap();
    assert!(
        session.wait_for(NEW, budget()),
        "the tree never listed {NEW}; screen:\n{}",
        session.screen()
    );
    (paths, tree, session)
}

/// A file chosen in the tree drawn over the docked agent panel, entered as
/// the tree opens, takes the place of the file open before it and leaves
/// the panel's window alone.
#[test]
fn a_file_chosen_from_the_tree_over_a_docked_agent_opens_beside_the_panel() {
    let (_paths, _tree, mut session) = agent_docked("tree-over-docked-agent");
    session.send(b"\x1b:View ai open | View tree\r").unwrap();
    assert!(
        session.wait_for(NEW, budget()),
        "the tree never listed {NEW}; screen:\n{}",
        session.screen()
    );
    session
        .send(
            format!("j\r:echo 'wins=' . bufwinnr('{OLD}') . '/' . (bufwinnr('{NEW}') > 0)\r")
                .as_bytes(),
        )
        .unwrap();
    assert!(
        session.wait_for("wins=-1/1", budget()),
        "{NEW} did not replace {OLD} in its window; screen:\n{}",
        session.screen()
    );
}

/// A file chosen in the docked tree entered from the docked agent panel
/// opens in the window the panel was entered from, and the keys typed in
/// the same write act there.
#[test]
fn a_file_chosen_from_a_docked_tree_entered_from_the_docked_agent_opens_in_the_file_window() {
    let (_paths, _tree, mut session) =
        agent_docked_with("docked-tree-from-docked-agent", &[TREE_WINDOWED]);
    session.send(b"\x1b:View ai open | View tree\r").unwrap();
    assert!(
        session.wait_for(NEW, budget()),
        "the tree never listed {NEW}; screen:\n{}",
        session.screen()
    );
    session.send(b"j\rdd").unwrap();
    assert!(
        lines_are(&mut session, "o1,o2,o3|n2,n3"),
        "the keys behind the open acted elsewhere; screen:\n{}",
        session.screen()
    );
    session
        .send(
            format!(":echo 'wins=' . bufwinnr('{OLD}') . '/' . (bufwinnr('{NEW}') > 0)\r")
                .as_bytes(),
        )
        .unwrap();
    assert!(
        session.wait_for("wins=-1/1", budget()),
        "{NEW} did not replace {OLD} in its window; screen:\n{}",
        session.screen()
    );
}

/// With a `nofile` `winfixwidth` window docked at the far left, standing
/// in for a plugin's own file tree, a file chosen in the docked tree
/// entered from the docked agent panel opens in the file window, the keys
/// typed in the same write act there, and the plugin's window keeps its
/// buffer.
#[test]
fn a_file_chosen_from_a_docked_tree_skips_a_plugin_sidebar_before_the_file_window() {
    let (_paths, _tree, mut session) =
        agent_docked_with("docked-tree-beside-plugin-tree", &[TREE_WINDOWED]);
    session
        .send(
            b"\x1b:topleft 12vnew | setlocal buftype=nofile winfixwidth \
              | file plugintree | wincmd p\r\
              :echo 'plugin=' . bufwinnr('plugintree') . '/' . bufname('%')\r",
        )
        .unwrap();
    assert!(
        session.wait_for(&format!("plugin=1/{OLD}"), budget()),
        "the stand-in sidebar never docked left of {OLD}; screen:\n{}",
        session.screen()
    );
    session.send(b":View ai open | View tree\r").unwrap();
    assert!(
        session.wait_for(NEW, budget()),
        "the tree never listed {NEW}; screen:\n{}",
        session.screen()
    );
    session.send(b"j\rdd").unwrap();
    assert!(
        lines_are(&mut session, "o1,o2,o3|n2,n3"),
        "the keys behind the open acted elsewhere; screen:\n{}",
        session.screen()
    );
    session
        .send(
            format!(
                ":echo 'wins=' . bufwinnr('{OLD}') . '/' . (bufwinnr('{NEW}') > 0) \
                 . '/' . (bufwinnr('plugintree') > 0)\r"
            )
            .as_bytes(),
        )
        .unwrap();
    assert!(
        session.wait_for("wins=-1/1/1", budget()),
        "{NEW} did not replace {OLD} in its window; screen:\n{}",
        session.screen()
    );
}

/// The same keystrokes from the file tree, placed as `env` says.
fn typed_ahead_from_the_tree(label: &str, env: &[(&str, &str)]) {
    let (_paths, _tree, mut session) = tree_open(label, env);
    // the tree lists the two files by name, the chosen one second
    session.send(b"j\rdd").unwrap();
    assert!(
        lines_are(&mut session, "o1,o2,o3|n2,n3"),
        "the keys behind the open acted elsewhere; screen:\n{}",
        session.screen()
    );
}

#[test]
fn a_key_typed_right_after_choosing_from_the_tree_acts_in_that_file() {
    typed_ahead_from_the_tree("tree-typed-ahead", &[]);
}

#[test]
fn a_key_typed_right_after_choosing_from_a_docked_tree_acts_in_that_file() {
    typed_ahead_from_the_tree("tree-docked-typed-ahead", &[TREE_WINDOWED]);
}

/// Keys typed behind a file a docked tree can no longer open act in the
/// window the tree was opened from, and the tree raises no prompt.
#[test]
fn a_key_typed_behind_a_refused_open_from_a_docked_tree_acts_in_the_file() {
    let (_paths, tree, mut session) = tree_open("tree-docked-refused", &[TREE_WINDOWED]);
    std::fs::remove_file(tree.0.join(NEW)).unwrap();
    session.send(b"j\rdd").unwrap();
    assert!(
        session.wait_for("exists", budget()),
        "choosing a deleted file said nothing; screen:\n{}",
        session.screen()
    );
    assert!(
        lines_are(&mut session, "o2,o3|"),
        "the keys behind the refused open acted elsewhere; screen:\n{}",
        session.screen()
    );
    assert!(
        !session.screen().contains("Delete "),
        "the tree took the keys; screen:\n{}",
        session.screen()
    );
}
