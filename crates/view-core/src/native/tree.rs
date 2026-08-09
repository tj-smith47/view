//! Pure state for the file tree sidebar overlay: the flattened entry list,
//! which directories are expanded, the cursor row, and the git decorations
//! overlaid on top. No filesystem or git access lives here --
//! `view-native`'s plain, synchronous `tree::fs::scan`/`tree::git::status`
//! functions do that work off the paint loop, in a thread the `view` bin
//! crate's executor owns; this module only tracks what was asked for and
//! what the last answer was.
//!
//! # Two independent generations
//!
//! A filesystem scan and a git-status refresh are timed independently (a
//! scan runs once when the tree opens or its root changes; a git refresh
//! runs on every bridge write/focus callback while the tree stays open), so
//! [`TreeState`] keeps two separate monotonic counters rather than one
//! shared one. Sharing would let a git refresh in flight when the tree
//! reopens land on the wrong generation and be wrongly accepted over (or
//! wrongly dropped in favor of) a scan it has nothing to do with. Both
//! counters draw from the same process-wide monotonic source
//! [`crate::native::picker::PickerState`] already established, for the same
//! collision hazard a per-instance counter would reopen.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use super::views::{GitMark, TreeRow, TreeView};

static NEXT_GENERATION: AtomicU64 = AtomicU64::new(1);

fn next_generation() -> u64 {
    NEXT_GENERATION.fetch_add(1, Ordering::Relaxed)
}

/// One filesystem entry from an `ignore`-walked scan: a path relative to the
/// tree's root, whether it is a directory, and its nesting depth (zero at
/// the root's direct children).
///
/// `view_native::tree::fs::scan` sorts its output depth-first, so a
/// directory's entry is always immediately followed by every one of its
/// descendants before the next sibling appears -- the exact invariant
/// [`TreeState::visible_indices`] depends on to fold an ancestor's collapsed
/// state into every descendant in one linear pass rather than an
/// ancestor-chain walk per row.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TreeEntry {
    pub path: PathBuf,
    pub is_dir: bool,
    pub depth: u16,
}

impl TreeEntry {
    /// One entry `view_native::tree::fs::scan` found at `path` (relative to
    /// the tree's root), `depth` levels beneath it. A plain constructor
    /// rather than a struct literal: `#[non_exhaustive]` closes literal
    /// construction to this crate, and `view-native`'s scan is the one
    /// caller outside it that ever builds one.
    #[must_use]
    pub fn new(path: PathBuf, is_dir: bool, depth: u16) -> Self {
        Self {
            path,
            is_dir,
            depth,
        }
    }
}

/// One `git status --porcelain=v2` row, already collapsed to a path
/// relative to the tree's root and the single decoration it maps to -- see
/// `view_native::tree::git`'s doc for exactly how a two-character `XY` code
/// collapses to one [`GitMark`].
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitEntry {
    pub path: PathBuf,
    pub mark: GitMark,
}

impl GitEntry {
    /// One row `view_native::tree::git::status` collapsed from a
    /// `git status --porcelain=v2` line. See [`TreeEntry::new`] for why
    /// this constructor exists rather than a struct literal.
    #[must_use]
    pub fn new(path: PathBuf, mark: GitMark) -> Self {
        Self { path, mark }
    }
}

/// The file tree sidebar's state: the root it was opened on, the last scan
/// and git reply accepted for it, which directories are expanded, and the
/// cursor row.
#[derive(Debug, Clone, PartialEq)]
pub struct TreeState {
    root: PathBuf,
    entries: Vec<TreeEntry>,
    expanded: HashSet<PathBuf>,
    selected: usize,
    generation: u64,
    git_generation: u64,
    git_status: HashMap<PathBuf, GitMark>,
    /// Whether a `git_generation` scan has been issued and not yet replied
    /// to. Every bridge write/focus callback while a tree is open asks for
    /// a refresh, so a slow `git status` on a large repo can still be
    /// running when the next callback fires; without this, each callback
    /// would spawn its own concurrent process for a reply the tree can
    /// only ever act on once anyway.
    git_refresh_in_flight: bool,
    /// Set when a refresh is requested while one is already in flight, so
    /// its reply re-arms exactly one more scan instead of the request being
    /// silently dropped. A single flag rather than a counter: repeated
    /// callbacks that arrive before the in-flight one answers all want the
    /// same thing, a status current as of "now", so collapsing them to one
    /// pending refresh loses nothing a second or third flag would have
    /// bought.
    git_refresh_pending: bool,
}

impl TreeState {
    /// A tree opened on `root`, with a scan generation already allocated --
    /// the caller issues `Effect::TreeScan` for [`TreeState::generation`]
    /// immediately after constructing this, the same "state exists before
    /// its first reply lands" shape
    /// [`crate::native::picker::PickerState::open`] uses.
    #[must_use]
    pub fn open(root: PathBuf) -> Self {
        Self {
            root,
            entries: Vec::new(),
            expanded: HashSet::new(),
            selected: 0,
            generation: next_generation(),
            git_generation: 0,
            git_status: HashMap::new(),
            git_refresh_in_flight: false,
            git_refresh_pending: false,
        }
    }

    /// The root this tree was opened on.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The generation the tree's most recent (or in-flight) filesystem scan
    /// was issued under.
    #[must_use]
    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// The generation the tree's most recent (or in-flight) git-status
    /// refresh was issued under, or `0` before any refresh has ever been
    /// requested.
    #[must_use]
    pub fn git_generation(&self) -> u64 {
        self.git_generation
    }

    /// Allocates and records a fresh scan generation, for a caller about to
    /// issue a new `Effect::TreeScan` (the tree's root changed, or a rename
    /// reply requires a rescan since `nvim_buf_set_name` does not itself
    /// trigger the bridge's ordinary file-change autocmds).
    #[must_use]
    pub fn request_rescan(&mut self) -> u64 {
        self.generation = next_generation();
        self.generation
    }

    /// Allocates and records a fresh git generation, for a caller about to
    /// issue a new `Effect::TreeGitScan` -- unless a refresh is already in
    /// flight, in which case this request is coalesced into it: the flight
    /// is marked pending and `None` is returned, so the caller issues no
    /// second scan process. [`TreeState::apply_git`] re-arms a pending
    /// request once the in-flight one answers, so nothing asked for while a
    /// scan was running is ever silently dropped -- only deduplicated.
    #[must_use]
    pub fn request_git_refresh(&mut self) -> Option<u64> {
        if self.git_refresh_in_flight {
            self.git_refresh_pending = true;
            return None;
        }
        self.git_refresh_in_flight = true;
        self.git_generation = next_generation();
        Some(self.git_generation)
    }

    /// Accepts a filesystem scan reply for `generation`, replacing the
    /// entry list. A reply for any other generation is stale and dropped --
    /// the tree was rescanned or closed and reopened before this one
    /// finished.
    pub fn apply_scan(&mut self, generation: u64, entries: Vec<TreeEntry>) {
        if generation != self.generation {
            return;
        }
        self.entries = entries;
        let visible = self.visible_indices().len();
        if self.selected >= visible {
            self.selected = visible.saturating_sub(1);
        }
    }

    /// Decorations only; an empty status (no git) is a valid state, not
    /// an error -- the tree renders undecorated. Returns `true` when a
    /// refresh was requested while this one was in flight and coalesced
    /// into it (see [`TreeState::request_git_refresh`]): the caller must
    /// issue one more `Effect::TreeGitScan` via a fresh
    /// `request_git_refresh` call to answer it, since the flag alone
    /// carries no generation or root to build that effect from.
    #[must_use]
    pub fn apply_git(&mut self, generation: u64, status: Vec<GitEntry>) -> bool {
        if generation != self.git_generation {
            return false;
        }
        self.git_status = status.into_iter().map(|e| (e.path, e.mark)).collect();
        self.git_refresh_in_flight = false;
        std::mem::take(&mut self.git_refresh_pending)
    }

    /// Toggles the directory at visible row `index` between expanded and
    /// collapsed. A leaf row, or an index past the last visible row, is
    /// left untouched -- there is nothing there to toggle.
    pub fn toggle_expand(&mut self, index: usize) {
        let visible = self.visible_indices();
        let Some(&entry_index) = visible.get(index) else {
            return;
        };
        let entry = &self.entries[entry_index];
        if !entry.is_dir {
            return;
        }
        let path = entry.path.clone();
        if !self.expanded.remove(&path) {
            self.expanded.insert(path);
        }
    }

    /// Moves the cursor row by `delta` visible rows, clamped to the
    /// currently visible range.
    pub fn move_selection(&mut self, delta: isize) {
        let len = self.visible_indices().len();
        if len == 0 {
            self.selected = 0;
            return;
        }
        let current = self.selected.min(len - 1) as isize;
        let next = (current + delta).clamp(0, len as isize - 1);
        self.selected = next as usize;
    }

    /// The entry under the cursor, or `None` for an empty tree.
    #[must_use]
    pub fn selected_entry(&self) -> Option<&TreeEntry> {
        let visible = self.visible_indices();
        let &i = visible.get(self.selected)?;
        self.entries.get(i)
    }

    /// The entry under the cursor's absolute path, or `None` for an empty
    /// tree.
    #[must_use]
    pub fn selected_path(&self) -> Option<PathBuf> {
        self.selected_entry()
            .map(|entry| self.root.join(&entry.path))
    }

    /// The indices into `entries` currently visible: every entry whose full
    /// ancestor chain is expanded.
    ///
    /// A single linear pass rather than a per-entry ancestor walk, relying
    /// on `entries` being depth-first ordered (see [`TreeEntry`]'s doc): a
    /// `hidden_below` depth threshold folds an ancestor's collapsed state
    /// into every descendant beneath it in O(n) rather than O(n * depth).
    /// That matters here specifically because this runs on every
    /// [`TreeState::view`] call, and `view` runs on every frame the tree
    /// overlay is on screen.
    fn visible_indices(&self) -> Vec<usize> {
        let mut out = Vec::with_capacity(self.entries.len());
        let mut hidden_below: Option<u16> = None;
        for (i, entry) in self.entries.iter().enumerate() {
            if let Some(threshold) = hidden_below {
                if entry.depth > threshold {
                    continue;
                }
                hidden_below = None;
            }
            out.push(i);
            if entry.is_dir && !self.expanded.contains(&entry.path) {
                hidden_below = Some(entry.depth);
            }
        }
        out
    }

    /// The tree's current paint frame: the visible rows, top to bottom,
    /// each carrying whatever git decoration the last accepted status reply
    /// mapped to its path.
    #[must_use]
    pub fn view(&self) -> TreeView {
        let visible = self.visible_indices();
        let rows: Vec<TreeRow> = visible
            .iter()
            .map(|&i| {
                let entry = &self.entries[i];
                let mark = self.git_status.get(&entry.path).copied();
                let label = entry
                    .path
                    .file_name()
                    .map(|name| name.to_string_lossy().into_owned())
                    .unwrap_or_else(|| entry.path.to_string_lossy().into_owned());
                let row = if entry.is_dir {
                    TreeRow::dir(entry.depth, label, self.expanded.contains(&entry.path))
                } else {
                    TreeRow::leaf(entry.depth, label)
                };
                row.with_status(mark)
            })
            .collect();
        let mut view = TreeView::new(title(&self.root)).with_rows(rows);
        if !visible.is_empty() {
            view = view.with_selected(self.selected.min(visible.len() - 1));
        }
        view
    }
}

/// The tree's title: the root's last path component, or the root's full
/// display form when it has none (e.g. `/`).
fn title(root: &Path) -> String {
    root.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| root.to_string_lossy().into_owned())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    fn entry(path: &str, is_dir: bool, depth: u16) -> TreeEntry {
        TreeEntry {
            path: PathBuf::from(path),
            is_dir,
            depth,
        }
    }

    fn sample_entries() -> Vec<TreeEntry> {
        vec![
            entry("src", true, 0),
            entry("src/main.rs", false, 1),
            entry("src/lib.rs", false, 1),
            entry("target", true, 0),
            entry("target/debug", true, 1),
            entry("Cargo.toml", false, 0),
        ]
    }

    #[test]
    fn a_stale_generation_scan_reply_is_dropped() {
        let mut tree = TreeState::open(PathBuf::from("/repo"));
        let current = tree.generation();
        tree.apply_scan(current + 1, sample_entries());
        assert!(
            tree.view().rows.is_empty(),
            "a reply for a generation that was never issued must not land"
        );
        tree.apply_scan(current, sample_entries());
        assert_eq!(
            tree.view().rows.len(),
            3,
            "a matching generation lands: src, target, Cargo.toml visible,              every directory starting collapsed"
        );
    }

    #[test]
    fn collapsing_a_directory_hides_its_whole_subtree_not_just_its_children() {
        let mut tree = TreeState::open(PathBuf::from("/repo"));
        let gen = tree.generation();
        tree.apply_scan(gen, sample_entries());

        // every directory starts collapsed
        let collapsed = tree.view();
        assert_eq!(collapsed.rows.len(), 3, "src, target, Cargo.toml");
        assert_eq!(collapsed.rows[0].label, "src");
        assert_eq!(collapsed.rows[0].expanded, Some(false));

        tree.toggle_expand(0);
        let src_open = tree.view();
        assert_eq!(src_open.rows.len(), 5, "src's two children now show");
        assert_eq!(src_open.rows[1].label, "main.rs");
        assert_eq!(src_open.rows[2].label, "lib.rs");

        // target is a sibling, still collapsed -- expanding src must not
        // leak into it
        let target_row = src_open
            .rows
            .iter()
            .find(|row| row.label == "target")
            .expect("target row still present");
        assert_eq!(target_row.expanded, Some(false));

        tree.toggle_expand(0);
        let src_closed = tree.view();
        assert_eq!(
            src_closed.rows.len(),
            3,
            "collapsing src hides target/debug too if it were open, and its own children"
        );
    }

    #[test]
    fn a_leaf_row_does_not_toggle() {
        let mut tree = TreeState::open(PathBuf::from("/repo"));
        let gen = tree.generation();
        tree.apply_scan(gen, sample_entries());
        let before = tree.view();
        tree.toggle_expand(2); // Cargo.toml, a leaf
        assert_eq!(tree.view(), before, "toggling a leaf is a no-op");
    }

    #[test]
    fn git_status_decorates_matching_paths_and_an_empty_status_is_not_an_error() {
        let mut tree = TreeState::open(PathBuf::from("/repo"));
        let scan_gen = tree.generation();
        tree.apply_scan(scan_gen, sample_entries());

        let git_gen = tree
            .request_git_refresh()
            .expect("nothing else is in flight yet");
        let _ = tree.apply_git(
            git_gen,
            vec![GitEntry {
                path: PathBuf::from("Cargo.toml"),
                mark: GitMark::Modified,
            }],
        );
        let decorated = tree.view();
        let cargo = decorated
            .rows
            .iter()
            .find(|row| row.label == "Cargo.toml")
            .expect("Cargo.toml row present");
        assert_eq!(cargo.status, Some(GitMark::Modified));
        let src = decorated
            .rows
            .iter()
            .find(|row| row.label == "src")
            .expect("src row present");
        assert_eq!(src.status, None, "an entry with no status carries none");

        // no git on PATH looks identical to git reporting a clean tree: an
        // empty status vector, applied without error, decorating nothing.
        let no_git_gen = tree
            .request_git_refresh()
            .expect("the previous refresh already answered, so this allocates fresh");
        let _ = tree.apply_git(no_git_gen, Vec::new());
        assert!(tree.view().rows.iter().all(|row| row.status.is_none()));
    }

    #[test]
    fn a_stale_git_generation_reply_is_dropped() {
        let mut tree = TreeState::open(PathBuf::from("/repo"));
        let scan_gen = tree.generation();
        tree.apply_scan(scan_gen, sample_entries());
        let current = tree
            .request_git_refresh()
            .expect("nothing else is in flight yet");
        let _ = tree.apply_git(
            current + 1,
            vec![GitEntry {
                path: PathBuf::from("Cargo.toml"),
                mark: GitMark::Modified,
            }],
        );
        assert!(tree.view().rows.iter().all(|row| row.status.is_none()));
    }

    #[test]
    fn selection_clamps_to_the_visible_range_and_survives_a_rescan() {
        let mut tree = TreeState::open(PathBuf::from("/repo"));
        let gen = tree.generation();
        tree.apply_scan(gen, sample_entries());
        tree.move_selection(-5);
        assert_eq!(tree.view().selected, Some(0), "clamped at the top");
        tree.move_selection(10);
        assert_eq!(tree.view().selected, Some(2), "clamped at the bottom");

        assert_eq!(
            tree.selected_entry().map(|e| e.path.clone()),
            Some(PathBuf::from("Cargo.toml")),
            "the bottom of the 3 collapsed rows (src, target, Cargo.toml) is              Cargo.toml, not target"
        );
        assert_eq!(
            tree.selected_path(),
            Some(PathBuf::from("/repo/Cargo.toml"))
        );

        let next_gen = tree.request_rescan();
        tree.apply_scan(next_gen, vec![entry("Cargo.toml", false, 0)]);
        assert_eq!(
            tree.view().selected,
            Some(0),
            "a rescan that shrinks the tree reclamps rather than pointing past the end"
        );
    }

    #[test]
    fn an_empty_tree_has_no_selection() {
        let tree = TreeState::open(PathBuf::from("/repo"));
        assert_eq!(tree.view().selected, None);
        assert!(tree.selected_entry().is_none());
    }
}
