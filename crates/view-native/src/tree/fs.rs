//! The tree sidebar's filesystem scan: an `ignore`-walked listing of `root`,
//! sorted and flattened into `view_core::native::tree::TreeEntry`'s
//! depth-first shape.
//!
//! Dotfiles are listed, and entries an `.ignore` file names are skipped,
//! as are entries a `.gitignore` names inside a git repository. A `.git`
//! folder lists what sits directly inside it and nothing deeper. A
//! symbolic link is listed as one entry carrying its target, a link to a
//! folder sorting with the folders, and nothing beneath it is listed.
//! Within each directory, folders come first and then files, each group
//! ordered by name without regard to case.
//!
//! The picker's `Files` source (`view_native::picker::sources`) walks with
//! `ignore::WalkBuilder`'s defaults, which skip dotfiles, and leaves the
//! order to its fuzzy matcher. The two differ in those two respects.

use std::cmp::Ordering as Order;
use std::ffi::OsStr;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};

use view_core::native::tree::TreeEntry;

/// Walks `root` and returns every entry beneath it (never `root` itself),
/// depth-first: a directory's entry is always immediately followed by every
/// one of its descendants before the next sibling appears, matching
/// [`TreeEntry`]'s own documented invariant. A malformed entry (a permission
/// error, a broken symlink `ignore` could not stat) is skipped rather than
/// aborting the whole walk, the same degrade `picker::sources::spawn_file_scan`
/// uses.
///
/// `cancel` is checked ahead of every entry the walk visits, on the same
/// per-entry grain `picker::sources::spawn_file_scan` checks its own
/// `cancel` at: a caller flips it and this returns whatever it has
/// collected so far rather than finishing the walk. This is the executor's
/// only way to stop a scan of a huge tree once it is already running --
/// unlike the picker's worker, this runs to completion in one blocking call
/// with no generation check of its own along the way, so without this flag
/// closing the sidebar mid-scan would leave the walk running unobserved for
/// as long as it takes.
#[must_use]
pub fn scan(root: &Path, cancel: &AtomicBool) -> Vec<TreeEntry> {
    scan_paced(root, cancel, || {})
}

/// [`scan`] with a hook run ahead of every entry, immediately before the
/// `cancel` check. `scan` supplies an empty closure, which monomorphises
/// away and leaves the per-entry cost exactly the one atomic load it
/// already paid; a cancellation test supplies a latch, so it can hold the
/// walk between two entries while it flips the flag. A test without that
/// hold can only flip the flag and hope the walk has not already run out
/// of tree, which is a race the walk wins whenever the test thread loses
/// the CPU for as long as the walk takes, which a 20,100-entry tree on a
/// loaded macOS host left it ample room to do.
fn scan_paced(root: &Path, cancel: &AtomicBool, pace: impl Fn()) -> Vec<TreeEntry> {
    let mut out = Vec::new();
    let real_root = std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    let walker = ignore::WalkBuilder::new(root)
        .hidden(false)
        .filter_entry(|entry| {
            // the depth guard keeps a root that is itself a `.git` folder
            // listing its whole tree
            entry.depth() < 3
                || entry
                    .path()
                    .parent()
                    .and_then(Path::parent)
                    .and_then(Path::file_name)
                    != Some(OsStr::new(".git"))
        })
        .build();
    for entry in walker {
        pace();
        if cancel.load(Ordering::Acquire) {
            break;
        }
        let Ok(entry) = entry else { continue };
        // depth 0 is root itself (ignore::WalkBuilder always yields it
        // first); everything this scan reports is relative to it, so it
        // carries nothing a tree row could show
        let depth = entry.depth();
        if depth == 0 {
            continue;
        }
        let link = entry.path_is_symlink();
        // `ignore` does not follow links, so a link's own type is a link
        // and its target's type is read here
        let is_dir = if link {
            entry.path().is_dir()
        } else {
            entry.file_type().is_some_and(|ft| ft.is_dir())
        };
        let Ok(rel) = entry.path().strip_prefix(root) else {
            continue;
        };
        #[allow(clippy::cast_possible_truncation)]
        let depth = (depth - 1) as u16;
        let mut listed = TreeEntry::new(rel.to_path_buf(), is_dir, depth);
        if link {
            listed = listed.with_link(link_target(entry.path(), &real_root));
        }
        out.push(listed);
    }
    out.sort_by(tree_order);
    out
}

/// Where the link at `path` points: relative to `real_root` when it
/// resolves beneath it, the resolved path when it resolves elsewhere, and
/// the link's own text when it resolves nowhere.
fn link_target(path: &Path, real_root: &Path) -> String {
    match std::fs::canonicalize(path) {
        Ok(real) => real
            .strip_prefix(real_root)
            .unwrap_or(&real)
            .to_string_lossy()
            .into_owned(),
        Err(_) => std::fs::read_link(path)
            .map(|target| target.to_string_lossy().into_owned())
            .unwrap_or_default(),
    }
}

/// The order of the depth-first listing, compared one path component at a
/// time: a directory sorts ahead of everything beneath it, and each
/// sibling group puts folders before files, each ordered by name without
/// regard to case and then by its bytes. Every component but the last is a
/// folder.
fn tree_order(a: &TreeEntry, b: &TreeEntry) -> Order {
    let mut left = a.path.components();
    let mut right = b.path.components();
    loop {
        let (x, y) = match (left.next(), right.next()) {
            (None, None) => return Order::Equal,
            (None, Some(_)) => return Order::Less,
            (Some(_), None) => return Order::Greater,
            (Some(x), Some(y)) => (x, y),
        };
        if x == y {
            continue;
        }
        let x_file = left.as_path().as_os_str().is_empty() && !a.is_dir;
        let y_file = right.as_path().as_os_str().is_empty() && !b.is_dir;
        return x_file
            .cmp(&y_file)
            .then_with(|| {
                let (x, y) = (
                    x.as_os_str().to_string_lossy(),
                    y.as_os_str().to_string_lossy(),
                );
                x.chars()
                    .flat_map(char::to_lowercase)
                    .cmp(y.chars().flat_map(char::to_lowercase))
            })
            .then_with(|| x.as_os_str().cmp(y.as_os_str()));
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    fn scratch(nonce: &str) -> std::path::PathBuf {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/tmp")
            .join(format!("tree-fs-scan-{}-{nonce}", std::process::id()));
        std::fs::create_dir_all(&root).expect("create scratch root");
        root
    }

    #[test]
    fn a_scan_lists_files_and_directories_depth_first_and_omits_the_root() {
        let root = scratch("basic");
        std::fs::create_dir_all(root.join("src")).expect("mkdir src");
        std::fs::write(root.join("src/main.rs"), "").expect("write main.rs");
        std::fs::write(root.join("Cargo.toml"), "").expect("write Cargo.toml");

        let entries = scan(&root, &AtomicBool::new(false));
        assert!(
            entries.iter().all(|e| e.path != std::path::Path::new("")),
            "the root itself must never appear as an entry"
        );

        let src_idx = entries
            .iter()
            .position(|e| e.path == std::path::Path::new("src"))
            .expect("src listed");
        assert!(entries[src_idx].is_dir);
        assert_eq!(entries[src_idx].depth, 0);

        let main_idx = entries
            .iter()
            .position(|e| e.path == std::path::Path::new("src/main.rs"))
            .expect("src/main.rs listed");
        assert!(
            main_idx > src_idx,
            "a directory's own entry must precede its descendants"
        );
        assert_eq!(entries[main_idx].depth, 1);
        // every entry between src and its one child must itself be a
        // descendant of src -- this is the invariant TreeState relies on to
        // fold a collapsed ancestor's state into its whole subtree in one
        // linear pass, not an ancestor-chain walk per row
        for e in &entries[src_idx + 1..=main_idx] {
            assert!(
                e.path.starts_with("src"),
                "{:?} sits between src and its child but is not beneath it",
                e.path
            );
        }

        let cargo_idx = entries
            .iter()
            .position(|e| e.path == std::path::Path::new("Cargo.toml"))
            .expect("Cargo.toml listed");
        assert!(!entries[cargo_idx].is_dir);
        assert_eq!(entries[cargo_idx].depth, 0);

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_gitignored_file_is_not_listed() {
        let root = scratch("gitignore");
        std::fs::write(root.join(".gitignore"), "ignored.txt\n").expect("write .gitignore");
        std::fs::write(root.join("ignored.txt"), "").expect("write ignored.txt");
        std::fs::write(root.join("kept.txt"), "").expect("write kept.txt");

        let entries = scan(&root, &AtomicBool::new(false));
        assert!(
            !entries
                .iter()
                .any(|e| e.path == std::path::Path::new("ignored.txt")),
            "a .gitignore'd file must not appear in the scan"
        );
        assert!(entries
            .iter()
            .any(|e| e.path == std::path::Path::new("kept.txt")));

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_tree_scan_puts_folders_before_files_ignoring_case() {
        let root = scratch("order");
        for dir in ["b_dir", "A_dir", "b_dir/z_sub"] {
            std::fs::create_dir_all(root.join(dir)).expect("mkdir");
        }
        for file in ["a.txt", "C.txt", "b_dir/y.rs", "B.txt"] {
            std::fs::write(root.join(file), "").expect("write");
        }

        let paths: Vec<String> = scan(&root, &AtomicBool::new(false))
            .into_iter()
            .map(|e| {
                let parts: Vec<_> = e.path.iter().map(|p| p.to_string_lossy()).collect();
                parts.join("/")
            })
            .collect();
        assert_eq!(
            paths,
            [
                "A_dir",
                "b_dir",
                "b_dir/z_sub",
                "b_dir/y.rs",
                "a.txt",
                "B.txt",
                "C.txt"
            ]
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_tree_scan_lists_dotfiles_and_one_level_of_git_but_not_ignored_ones() {
        let root = scratch("dotfiles");
        std::fs::create_dir_all(root.join(".git/objects/ab")).expect("mkdir .git");
        std::fs::write(root.join(".git/HEAD"), "").expect("write HEAD");
        std::fs::write(root.join(".git/objects/ab/cd"), "").expect("write an object");
        std::fs::write(root.join(".gitignore"), ".secret\n").expect("write .gitignore");
        std::fs::write(root.join(".secret"), "").expect("write .secret");
        std::fs::write(root.join(".env.example"), "").expect("write .env.example");

        let entries = scan(&root, &AtomicBool::new(false));
        let listed = |name: &str| entries.iter().any(|e| e.path == Path::new(name));
        assert!(listed(".env.example"), "{entries:?}");
        assert!(listed(".gitignore"), "{entries:?}");
        assert!(
            !listed(".secret"),
            "a gitignored dotfile stays out: {entries:?}"
        );
        assert!(listed(".git"), "{entries:?}");
        let git = entries
            .iter()
            .find(|e| e.path == Path::new(".git"))
            .expect(".git is listed");
        assert!(git.is_dir);
        assert!(listed(".git/HEAD"), "{entries:?}");
        assert!(listed(".git/objects"), "{entries:?}");
        assert!(
            !entries
                .iter()
                .any(|e| e.path.starts_with(".git/objects/ab")),
            "nothing below .git's own entries is listed: {entries:?}"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_tree_scan_lists_one_level_of_a_nested_git_folder() {
        let root = scratch("nested-git");
        std::fs::create_dir_all(root.join("sub/.git/refs/heads")).expect("mkdir");
        std::fs::write(root.join("sub/.git/HEAD"), "").expect("write HEAD");
        std::fs::write(root.join("sub/.git/refs/heads/main"), "").expect("write ref");

        let entries = scan(&root, &AtomicBool::new(false));
        let listed = |name: &str| entries.iter().any(|e| e.path == Path::new(name));
        assert!(listed("sub/.git"), "{entries:?}");
        assert!(listed("sub/.git/HEAD"), "{entries:?}");
        assert!(listed("sub/.git/refs"), "{entries:?}");
        assert!(!listed("sub/.git/refs/heads"), "{entries:?}");

        let _ = std::fs::remove_dir_all(&root);
    }

    #[cfg(unix)]
    #[test]
    fn a_tree_scan_sorts_a_linked_folder_with_the_folders_and_lists_nothing_inside() {
        let root = scratch("symlink");
        std::fs::create_dir_all(root.join("real/inner")).expect("mkdir");
        std::fs::write(root.join("real/inner/x.rs"), "").expect("write");
        std::fs::write(root.join("a.txt"), "").expect("write");
        std::os::unix::fs::symlink(root.join("real"), root.join("linked")).expect("symlink dir");
        std::os::unix::fs::symlink(root.join("a.txt"), root.join("b.txt")).expect("symlink file");

        let entries = scan(&root, &AtomicBool::new(false));
        let top: Vec<_> = entries
            .iter()
            .filter(|e| e.depth == 0)
            .map(|e| e.path.to_string_lossy().into_owned())
            .collect();
        assert_eq!(top, ["linked", "real", "a.txt", "b.txt"], "{entries:?}");
        let linked = entries
            .iter()
            .find(|e| e.path == Path::new("linked"))
            .expect("the link is listed");
        assert!(linked.is_dir);
        assert_eq!(linked.link.as_deref(), Some("real"));
        assert!(
            !entries
                .iter()
                .any(|e| e.path.starts_with("linked") && e.path != Path::new("linked")),
            "nothing beneath a linked folder is listed: {entries:?}"
        );
        let file = entries
            .iter()
            .find(|e| e.path == Path::new("b.txt"))
            .expect("the file link is listed");
        assert!(!file.is_dir);
        assert_eq!(file.link.as_deref(), Some("a.txt"));

        let _ = std::fs::remove_dir_all(&root);
    }

    /// Which consult parks the walk on its [`view_test_support::ScanGate`].
    /// `ignore` yields the root itself first (`scan` skips it) and then the
    /// files under it, so parking on the fourth leaves two entries already
    /// collected behind the gate and five ahead of it.
    const GATE_PARKS_AT: usize = 4;

    /// Files under the cancel fixture: comfortably more than
    /// [`GATE_PARKS_AT`] consults, so a walker that ignored its cancel flag
    /// has several entries left to consult the gate about, and small enough
    /// that writing them costs nothing worth measuring.
    const CANCEL_TREE_FILES: u32 = 8;

    /// Proves the `cancel` check stops the walk early rather than merely
    /// existing unused, and proves it per entry: the walk is held on a gate
    /// with entries still ahead of it, the flag is flipped while it is
    /// held, and the walk is then released and joined. Removing the `if
    /// cancel.load(..) { break; }` line makes this fail by name, because
    /// the released walk goes on to consult the gate for every remaining
    /// entry instead of ending there.
    ///
    /// The gate is what makes that a fact rather than a race. Flipping the
    /// flag right after the spawn and asserting the walk stopped short is
    /// only correct while the test thread wins a footrace against the whole
    /// walk -- and on a loaded 3-core macOS runner it does not, which is
    /// how the picker's twin of this test read a correct cancellation as a
    /// missing one.
    #[test]
    fn a_cancelled_scan_stops_short_of_the_full_tree() {
        let root = scratch("cancel");
        for f in 0..CANCEL_TREE_FILES {
            std::fs::write(root.join(format!("f{f}.txt")), "").expect("write file");
        }

        let (gate, pace) = view_test_support::ScanGate::new(GATE_PARKS_AT);
        let cancel = std::sync::Arc::new(AtomicBool::new(false));
        let scan_root = root.clone();
        let scan_cancel = std::sync::Arc::clone(&cancel);
        let handle = std::thread::spawn(move || scan_paced(&scan_root, &scan_cancel, || pace()));

        gate.wait_until_parked();
        cancel.store(true, Ordering::Release);
        gate.release();
        let entries = handle.join().expect("scan thread joins");

        assert!(
            !entries.is_empty(),
            "the gate must park the walk with entries already collected, or \
             no scan in flight is under test"
        );
        assert_eq!(
            gate.steps_after_release(),
            0,
            "expected the walk to end at the cancel check it was held on, \
             but it went on to take further entries"
        );

        let _ = std::fs::remove_dir_all(&root);
    }
}
