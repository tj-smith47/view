//! The tree sidebar's git-status refresh: shells out to
//! `git status --porcelain=v2` on `root`, off the loop, and collapses each
//! reported entry down to the single [`GitMark`] a tree row can carry.
//!
//! Shelling out rather than depending on a git library: `git`'s own
//! porcelain v2 output is a stable, documented wire format this module owns
//! the parsing of end to end, while a library dependency would need to
//! track upstream's own status-computation semantics release over release.
//! `git` absent from `PATH`, or `root` not
//! sitting inside a repository, both degrade to an empty result rather than
//! an error -- the tree renders undecorated either way, never blocked on
//! git being installed.
//!
//! # `XY` code collapsing
//!
//! `--porcelain=v2` reports a staged (`X`) and unstaged (`Y`) status
//! character per changed entry. This module shows the change closest to
//! what the editor displays: the unstaged character when it names one,
//! falling back to the staged character otherwise. `A`/`D`/`R`/`C` map onto
//! their like-named [`GitMark`]; `M` and `T` (a type change, e.g. a file
//! becoming a symlink) both read as [`GitMark::Modified`] -- a tree row has
//! no glyph of its own for "type changed", and folding it into "modified"
//! still tells a user the entry needs a look rather than hiding it. Type-2
//! (renamed/copied) lines carry the same `XY` shape and reuse the same
//! mapping; type-`u` (unmerged) lines always map to
//! [`GitMark::Conflicted`] regardless of their own `XY`, since porcelain v2
//! reserves that line type for the conflict states themselves.

use std::io::Read;
use std::path::Path;
use std::process::{Child, Command};
use std::time::{Duration, Instant};

use view_core::native::tree::GitEntry;
use view_core::native::views::GitMark;

/// How long a `git status` child is given before this module gives up on it
/// and reports a timeout rather than blocking forever. Matches
/// `view-oracle::compat::PROBE_TIMEOUT`, the repo's existing precedent for
/// bounding a child process -- reimplemented locally rather than imported,
/// since `view-native` cannot depend on `view-oracle` (dependency direction:
/// `core <- surface <- {native, ai}`). A wedged `git` (a stale index lock, a
/// network-backed credential helper hanging, ...) must still surface as a
/// bounded failure: `TreeState::apply_git` is the only clearer of
/// `git_refresh_in_flight`, so a `status` call that never returns would
/// otherwise freeze the sidebar's git decorations for the rest of the
/// session.
const GIT_STATUS_TIMEOUT: Duration = Duration::from_secs(10);

/// Refreshes `root`'s git status. An empty result is not an error: it is
/// what a clean tree, a tree outside any repository, and a tree with `git`
/// absent from `PATH` all report identically -- see this module's own doc.
/// A wedged `git` also degrades to empty here; a caller that must tell that
/// case apart from the others wants [`status_bounded`] instead.
#[must_use]
pub fn status(root: &Path) -> Vec<GitEntry> {
    status_bounded(root).0
}

/// [`status`], plus a second element that is `true` when the `git` child
/// was killed for outliving [`GIT_STATUS_TIMEOUT`] rather than exiting on
/// its own -- the one case among this module's empty-result degrades that a
/// caller may want to surface to the user instead of rendering identically
/// to "nothing to decorate".
#[must_use]
pub fn status_bounded(root: &Path) -> (Vec<GitEntry>, bool) {
    run_git_status(root, Path::new("git"), GIT_STATUS_TIMEOUT)
}

/// `status`'s implementation, taking the program to run so a test can prove
/// both spawn-side degrades -- a `git` that is not installed, and one that
/// never exits -- without mutating the test process's own environment,
/// which every other test in the binary shares.
///
/// The program itself rather than a `PATH` override, because a `PATH`
/// override does not mean the same thing everywhere: Unix resolves the
/// child's program through the `PATH` the child is given, while Windows
/// falls back to the parent process's own, so an emptied `PATH` there
/// still finds the real `git` and proves nothing. A name nothing resolves
/// fails to spawn identically on every platform. `timeout` is injected
/// (rather than always [`GIT_STATUS_TIMEOUT`]) so a test can prove the
/// bound itself without waiting out the real production deadline.
fn run_git_status(root: &Path, program: &Path, timeout: Duration) -> (Vec<GitEntry>, bool) {
    let mut cmd = Command::new(program);
    cmd.current_dir(root)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .args([
            "-c",
            "core.quotePath=false",
            "status",
            "--porcelain=v2",
            "--untracked-files=all",
            // scopes the report to `root` itself: without this, a `root` that
            // is not a repository's own top level (a subdirectory tree, or --
            // the case this module's own tests caught -- a scratch directory
            // that resolves upward to an unrelated enclosing repository) gets
            // back the WHOLE repository's status, with paths climbing back out
            // of `root` via `../` that can never match a `TreeEntry` this tree
            // ever lists
            "--",
            ".",
        ]);
    let Ok(child) = cmd.spawn() else {
        return (Vec::new(), false);
    };
    let Some(output) = wait_with_timeout(child, timeout) else {
        return (Vec::new(), true);
    };
    if !output.status.success() {
        return (Vec::new(), false);
    }
    let Ok(text) = String::from_utf8(output.stdout) else {
        return (Vec::new(), false);
    };
    (parse_porcelain_v2(&text), false)
}

/// Runs `child` to completion, polling rather than blocking, killing (and
/// reaping, so it cannot become a zombie) it if `timeout` elapses first.
/// Mirrors `view-oracle::compat::wait_with_timeout`'s own shape -- see that
/// function's doc for why `stdout`/`stderr` are drained on background
/// threads from the moment `child` is handed in, rather than read
/// synchronously after it exits.
fn wait_with_timeout(mut child: Child, timeout: Duration) -> Option<std::process::Output> {
    let stdout_reader = child.stdout.take().map(spawn_pipe_drain);
    let stderr_reader = child.stderr.take().map(spawn_pipe_drain);

    let deadline = Instant::now() + timeout;
    let status = loop {
        if let Ok(Some(status)) = child.try_wait() {
            break Some(status);
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            break None;
        }
        std::thread::sleep(Duration::from_millis(20));
    };

    // joined only after the child has already exited or been killed, so
    // each reader thread is already at (or immediately reaches) EOF rather
    // than blocking this call past its own deadline
    let stdout = stdout_reader
        .and_then(|h| h.join().ok())
        .unwrap_or_default();
    let stderr = stderr_reader
        .and_then(|h| h.join().ok())
        .unwrap_or_default();

    status.map(|status| std::process::Output {
        status,
        stdout,
        stderr,
    })
}

/// Spawns a background thread that reads `pipe` to EOF and returns its full
/// contents, the concurrent counterpart [`wait_with_timeout`]'s own doc
/// comment explains the need for.
fn spawn_pipe_drain(mut pipe: impl Read + Send + 'static) -> std::thread::JoinHandle<Vec<u8>> {
    std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = pipe.read_to_end(&mut buf);
        buf
    })
}

fn parse_porcelain_v2(text: &str) -> Vec<GitEntry> {
    text.lines().filter_map(parse_line).collect()
}

fn parse_line(line: &str) -> Option<GitEntry> {
    match line.split(' ').next()? {
        "1" => parse_ordinary(line),
        "2" => parse_rename_or_copy(line),
        "u" => parse_unmerged(line),
        "?" => parse_untracked(line),
        // "!" (ignored) never appears without --ignored, which this module
        // does not pass; any other line shape is not a decoration this
        // tree renders
        _ => None,
    }
}

/// `1 <XY> <sub> <mH> <mI> <mW> <hH> <hI> <path>` -- 9 space-separated
/// fields, the last of which is the path itself (which may contain spaces,
/// hence `splitn`).
fn parse_ordinary(line: &str) -> Option<GitEntry> {
    let mut parts = line.splitn(9, ' ');
    parts.next()?; // "1"
    let xy = parts.next()?;
    for _ in 0..6 {
        parts.next()?; // sub, mH, mI, mW, hH, hI
    }
    let path = parts.next()?;
    Some(GitEntry::new(path.into(), mark_from_xy(xy)?))
}

/// `2 <XY> <sub> <mH> <mI> <mW> <hH> <hI> <X><score> <path>\t<origPath>` --
/// 10 space-separated fields, the last carrying the current path and the
/// rename/copy source separated by a tab.
fn parse_rename_or_copy(line: &str) -> Option<GitEntry> {
    let mut parts = line.splitn(10, ' ');
    parts.next()?; // "2"
    let xy = parts.next()?;
    for _ in 0..7 {
        parts.next()?; // sub, mH, mI, mW, hH, hI, X-score
    }
    let rest = parts.next()?;
    let path = rest.split('\t').next()?;
    Some(GitEntry::new(path.into(), mark_from_xy(xy)?))
}

/// `u <XY> <sub> <m1> <m2> <m3> <mW> <h1> <h2> <h3> <path>` -- 11
/// space-separated fields. Always [`GitMark::Conflicted`]: this line type
/// exists precisely for the unmerged states, regardless of its own `XY`.
fn parse_unmerged(line: &str) -> Option<GitEntry> {
    let mut parts = line.splitn(11, ' ');
    parts.next()?; // "u"
    parts.next()?; // XY, unused: every unmerged line is a conflict
    for _ in 0..8 {
        parts.next()?; // sub, m1, m2, m3, mW, h1, h2, h3
    }
    let path = parts.next()?;
    Some(GitEntry::new(path.into(), GitMark::Conflicted))
}

/// `? <path>` -- the path is everything after the first space.
fn parse_untracked(line: &str) -> Option<GitEntry> {
    let mut parts = line.splitn(2, ' ');
    parts.next()?; // "?"
    let path = parts.next()?;
    Some(GitEntry::new(path.into(), GitMark::Untracked))
}

/// Collapses a two-character `XY` porcelain code to the single character
/// this module maps to a [`GitMark`] -- see the module doc for why the
/// unstaged half wins when both are present.
fn mark_from_xy(xy: &str) -> Option<GitMark> {
    let mut chars = xy.chars();
    let x = chars.next()?;
    let y = chars.next()?;
    let code = if y == '.' { x } else { y };
    match code {
        'A' => Some(GitMark::Added),
        'D' => Some(GitMark::Deleted),
        'R' => Some(GitMark::Renamed),
        'C' => Some(GitMark::Copied),
        'M' | 'T' => Some(GitMark::Modified),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    fn scratch(nonce: &str) -> std::path::PathBuf {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/tmp")
            .join(format!("tree-git-status-{}-{nonce}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("create scratch root");
        root
    }

    fn git(root: &Path, args: &[&str]) {
        let status = std::process::Command::new("git")
            .current_dir(root)
            .args(args)
            .status()
            .expect("git is on PATH for this test's own setup");
        assert!(status.success(), "git {args:?} failed in {root:?}");
    }

    fn init_repo(root: &Path) {
        git(root, &["init", "-q"]);
        git(root, &["config", "user.email", "test@example.com"]);
        git(root, &["config", "user.name", "test"]);
    }

    #[test]
    fn a_clean_repo_reports_no_decorations() {
        let root = scratch("clean");
        init_repo(&root);
        std::fs::write(root.join("a.txt"), "one\n").expect("write a.txt");
        git(&root, &["add", "a.txt"]);
        git(&root, &["commit", "-q", "-m", "init"]);

        assert!(
            status(&root).is_empty(),
            "a clean tree must report no decorations"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn modified_added_and_untracked_entries_map_to_the_right_mark() {
        let root = scratch("mixed");
        init_repo(&root);
        std::fs::write(root.join("tracked.txt"), "one\n").expect("write tracked.txt");
        git(&root, &["add", "tracked.txt"]);
        git(&root, &["commit", "-q", "-m", "init"]);

        std::fs::write(root.join("tracked.txt"), "two\n").expect("modify tracked.txt");
        std::fs::write(root.join("staged.txt"), "new\n").expect("write staged.txt");
        git(&root, &["add", "staged.txt"]);
        std::fs::write(root.join("untracked.txt"), "new\n").expect("write untracked.txt");

        let entries = status(&root);
        let find = |name: &str| {
            entries
                .iter()
                .find(|e| e.path == std::path::Path::new(name))
                .unwrap_or_else(|| panic!("{name} missing from status: {entries:?}"))
        };
        assert_eq!(find("tracked.txt").mark, GitMark::Modified);
        assert_eq!(find("staged.txt").mark, GitMark::Added);
        assert_eq!(find("untracked.txt").mark, GitMark::Untracked);

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_directory_outside_any_repository_reports_no_decorations_not_an_error() {
        let root = scratch("no-repo");
        std::fs::write(root.join("a.txt"), "one\n").expect("write a.txt");

        assert!(
            status(&root).is_empty(),
            "a directory with no git repository must degrade to no \
             decorations rather than erroring"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    /// The falsifiable check the tree's git-decoration support exists to
    /// satisfy: with no `git` to run, the tree still lists files (proven
    /// separately by `tree::fs::scan`, which never shells out to git at
    /// all) with no decorations, rather than erroring or blocking. The
    /// repository is real and dirty, so anything short of a failed spawn
    /// would report a decoration here. See `run_git_status`'s doc for why
    /// the absence is a program nothing resolves rather than an emptied
    /// `PATH`.
    #[test]
    fn an_unrunnable_git_reports_no_decorations_not_an_error() {
        let root = scratch("no-git-on-path");
        init_repo(&root);
        std::fs::write(root.join("a.txt"), "one\n").expect("write a.txt");
        git(&root, &["add", "a.txt"]);
        git(&root, &["commit", "-q", "-m", "init"]);
        std::fs::write(root.join("a.txt"), "two\n").expect("modify a.txt");

        let (entries, timed_out) = run_git_status(
            &root,
            Path::new("view-git-that-is-not-installed"),
            GIT_STATUS_TIMEOUT,
        );
        assert!(
            entries.is_empty(),
            "with no git to run the tree must report no decorations, not \
             an error: {entries:?}"
        );
        assert!(
            !timed_out,
            "a git that never started is a spawn failure, not a timeout"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    /// The falsifiable check the deadline exists to satisfy: a `git` that
    /// never exits must not hang this module forever, or the sidebar's git
    /// decorations freeze for the rest of the session (`apply_git` is the
    /// only clearer of `git_refresh_in_flight`). A fake `git` -- a script
    /// that sleeps well past a short injected timeout, handed to the status
    /// call outright as the program to run -- proves the call returns
    /// `(empty, true)` in bounded wall-clock time rather than blocking for
    /// the sleep's full duration.
    // The fake `git` this test spawns is a `#!/bin/sh` script and the
    // permission bits it sets are POSIX-only; a wedged-child bound is not a
    // platform-specific property, but this particular way of simulating one
    // is, so this test is unix-only rather than the fix itself.
    #[cfg(unix)]
    #[test]
    fn a_wedged_git_is_killed_at_its_deadline_not_awaited_forever() {
        let root = scratch("wedged-git");
        std::fs::create_dir_all(&root).expect("create root");
        let bin_dir = scratch("wedged-git-bin");
        let fake_git = bin_dir.join("git");
        // `exec sleep 5`, not a bare `sleep 5`: without `exec`, `sh` forks a
        // child to run `sleep` and stays around as its parent, so killing
        // the pid this test's `Command` actually holds (`sh`'s) leaves the
        // grandchild `sleep` running and still holding the stdout pipe
        // open, which would then block `spawn_pipe_drain`'s read until the
        // full 5s sleep elapses regardless of the kill. `exec` replaces
        // `sh`'s own process image with `sleep`'s, so the pid this test
        // kills and the pid actually sleeping are one and the same.
        std::fs::write(&fake_git, "#!/bin/sh\nexec sleep 5\n").expect("write fake git");
        let mut perms = std::fs::metadata(&fake_git)
            .expect("stat fake git")
            .permissions();
        std::os::unix::fs::PermissionsExt::set_mode(&mut perms, 0o755);
        std::fs::set_permissions(&fake_git, perms).expect("chmod fake git");

        // named outright rather than shadowed onto a `PATH`: the
        // environment stays untouched, so the script's own `sleep` and the
        // `/bin/sh` its shebang names resolve exactly as they always would
        let started = std::time::Instant::now();
        let (entries, timed_out) = run_git_status(&root, &fake_git, Duration::from_millis(200));
        let elapsed = started.elapsed();

        assert!(entries.is_empty(), "a killed child reports no decorations");
        assert!(timed_out, "outliving the deadline must report timed_out");
        assert!(
            elapsed < Duration::from_secs(2),
            "the wedged child must be killed near its 200ms deadline, not \
             awaited for its full 5s sleep: took {elapsed:?}"
        );

        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&bin_dir);
    }

    #[test]
    fn parses_a_rename() {
        let line = "2 R. N... 100644 100644 100644 aaaa bbbb R100 new.txt\told.txt";
        let entry = parse_line(line).expect("rename line parses");
        assert_eq!(entry.path, std::path::Path::new("new.txt"));
        assert_eq!(entry.mark, GitMark::Renamed);
    }

    #[test]
    fn parses_an_unmerged_conflict() {
        let line = "u UU N... 100644 100644 100644 100644 aaaa bbbb cccc conflict.txt";
        let entry = parse_line(line).expect("unmerged line parses");
        assert_eq!(entry.path, std::path::Path::new("conflict.txt"));
        assert_eq!(entry.mark, GitMark::Conflicted);
    }

    #[test]
    fn an_ignored_line_and_a_header_line_are_not_decorations() {
        assert!(parse_line("! ignored.txt").is_none());
        assert!(parse_line("# branch.oid deadbeef").is_none());
    }
}
