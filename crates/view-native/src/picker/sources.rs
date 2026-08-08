//! The `Files` source: walks `Source::Files { root }` on a background
//! thread using the `ignore` crate (ripgrep's own walker), and pushes each
//! regular file it finds straight into the matcher's `Injector` as it goes.
//!
//! # `.gitignore` semantics
//!
//! `ignore::WalkBuilder`'s defaults (unchanged here) mean this walk:
//! - Skips hidden files and directories (dotfiles), the same as `rg`'s
//!   default.
//! - Honors `.gitignore` at every directory level, `.ignore` (`rg`/`fd`'s
//!   own extra ignore file), and `.git/info/exclude` plus the global git
//!   config's `core.excludesFile`, when `root` sits inside a git repository
//!   -- stacked the same way `rg` stacks them. Outside a git repository,
//!   only `.gitignore`/`.ignore` files themselves apply.
//! - Does not follow symlinks (`follow_links` defaults to `false`): a
//!   symlinked subtree is listed as one entry, never descended into.
//!
//! `Source::Buffers` is not walked here at all: it arrives pre-gathered
//! through `Effect::PickerQuery`'s `resolved` field, since only
//! `view-engine` can speak RPC to list nvim's buffers (see
//! `matcher::seed_or_scan`).

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;

use nucleo::Injector;
use view_core::native::picker::PickerItem;

/// Walks `root` on a new background thread, pushing one `PickerItem` per
/// regular file the walk yields into `injector` as it is found -- this is
/// what lets a query typed while a large tree is still being walked see
/// ranked results long before the walk finishes (see `matcher`'s streaming
/// test). A malformed entry (a permission error, a broken symlink `ignore`
/// could not stat) is skipped rather than aborting the whole walk: one
/// unreadable subtree should not hide every other file the picker could
/// otherwise offer.
///
/// `cancel` is checked ahead of every entry the walk visits: a caller flips
/// it to stop the walk before it reaches the end of a possibly huge tree,
/// e.g. when the `Session` that owns this scan is replaced or torn down
/// (see `matcher::Session`'s `Drop`) -- without this, closing the picker or
/// switching sources mid-scan would leave a thread walking a million-entry
/// tree to completion in the background, pushing into an injector nothing
/// reads.
pub fn spawn_file_scan(
    root: PathBuf,
    injector: Injector<PickerItem>,
    cancel: Arc<AtomicBool>,
) -> JoinHandle<()> {
    std::thread::spawn(move || {
        for entry in ignore::WalkBuilder::new(&root).build() {
            if cancel.load(Ordering::Acquire) {
                return;
            }
            let Ok(entry) = entry else { continue };
            let is_file = entry.file_type().is_some_and(|ft| ft.is_file());
            if !is_file {
                continue;
            }
            let label = entry
                .path()
                .strip_prefix(&root)
                .unwrap_or_else(|_| entry.path())
                .to_string_lossy()
                .into_owned();
            injector.push(PickerItem::new(label), |item, cols| {
                cols[0] = item.label.as_str().into();
            });
        }
    })
}
