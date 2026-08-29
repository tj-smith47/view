//! The tree sidebar's filesystem scan: an `ignore`-walked listing of `root`,
//! sorted and flattened into `view_core::native::tree::TreeEntry`'s
//! depth-first shape.
//!
//! Shares `ignore::WalkBuilder`'s defaults with the picker's `Files` source
//! (`view_native::picker::sources`): hidden files and `.gitignore`/`.ignore`
//! entries are skipped, and a symlinked subtree is listed as one entry,
//! never descended into. Sorted by file name within each directory (the
//! picker's walk is not, since it only ever feeds a fuzzy matcher that
//! re-orders everything anyway) so the sidebar's listing is stable across
//! repeated scans of an unchanged tree.

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
/// the CPU for as long as the walk takes -- 20,100 entries in ~83 ms on a
/// loaded macOS host.
fn scan_paced(root: &Path, cancel: &AtomicBool, pace: impl Fn()) -> Vec<TreeEntry> {
    let mut out = Vec::new();
    let walker = ignore::WalkBuilder::new(root)
        .sort_by_file_name(std::ffi::OsStr::cmp)
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
        let is_dir = entry.file_type().is_some_and(|ft| ft.is_dir());
        let Ok(rel) = entry.path().strip_prefix(root) else {
            continue;
        };
        #[allow(clippy::cast_possible_truncation)]
        let depth = (depth - 1) as u16;
        out.push(TreeEntry::new(rel.to_path_buf(), is_dir, depth));
    }
    out
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

    /// Which consult parks the walk on its [`view_test_support::ScanGate`].
    /// `ignore` yields the root itself first (`scan` skips it) and then the
    /// files under it in name order, so parking on the fourth leaves two
    /// entries already collected behind the gate and five ahead of it.
    const GATE_PARKS_AT: usize = 4;

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
        for f in 0..8 {
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
