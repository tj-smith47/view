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
    let mut out = Vec::new();
    let walker = ignore::WalkBuilder::new(root)
        .sort_by_file_name(std::ffi::OsStr::cmp)
        .build();
    for entry in walker {
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

    // proves the `cancel` check actually stops the walk early rather than
    // merely existing unused: disabling it (e.g. removing the `if
    // cancel.load(..) { break; }` line) makes `scan` run to completion
    // regardless, and this test then deterministically fails, since
    // `entries.len()` would equal `total` instead of falling short of it.
    #[test]
    fn a_cancelled_scan_stops_short_of_the_full_tree() {
        let root = scratch("cancel");
        let dirs = 200;
        let files_per_dir = 100;
        for d in 0..dirs {
            let dir = root.join(format!("d{d}"));
            std::fs::create_dir_all(&dir).expect("mkdir");
            for f in 0..files_per_dir {
                std::fs::write(dir.join(format!("f{f}.txt")), "").expect("write file");
            }
        }
        let total = dirs * (files_per_dir + 1);

        let cancel = std::sync::Arc::new(AtomicBool::new(false));
        let scan_root = root.clone();
        let scan_cancel = std::sync::Arc::clone(&cancel);
        let handle = std::thread::spawn(move || scan(&scan_root, &scan_cancel));
        // no sleep: flipping the flag immediately, with no coordination
        // beyond the spawn itself, is what makes this deterministic --
        // whichever of the walk or this store runs first, the walk's very
        // next per-entry check sees `true` and stops there
        cancel.store(true, Ordering::Release);
        let entries = handle.join().expect("scan thread joins");

        assert!(
            entries.len() < total,
            "a cancelled scan must stop short of the full tree ({} of {total} entries)",
            entries.len()
        );

        let _ = std::fs::remove_dir_all(&root);
    }
}
