//! The `Files` and `LiveGrep` sources: each walks its root on a background
//! thread using the `ignore` crate (ripgrep's own walker). `Files` pushes
//! each regular file it finds straight into the matcher's `Injector` as it
//! goes; `LiveGrep` additionally searches each file's content in-process
//! via `grep-searcher`/`grep-regex` (also ripgrep's own crates) and pushes
//! one item per matching line -- see [`spawn_live_grep_scan`].
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

use grep_regex::RegexMatcher;
use grep_searcher::sinks::UTF8;
use grep_searcher::Searcher;
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

/// Walks `root` the same way [`spawn_file_scan`] does, but pushes one
/// `PickerItem` per matching *line* rather than per file: `needle` is
/// matched against each regular file's content in-process via
/// `grep-searcher`/`grep-regex` (ripgrep's own constituent crates, chosen
/// over shelling out to `rg` -- parsing another tool's output is a compat
/// surface view does not control, and a machine without `rg` installed
/// would otherwise break live-grep entirely). `needle` is treated as a
/// literal substring, not a regex: a picker query is text a user typed to
/// find, not a pattern they composed, so `.`/`*`/`(` in it must match
/// themselves.
///
/// A matched line's label is `"{relative path}:{line number}: {line
/// text}"`; the matcher worker's own nucleo pass re-derives the highlighted
/// match span from this label against `needle`, the same pipeline
/// `build_results` already runs for `Files`/`Buffers`, so this scan does not
/// compute or push its own indices.
///
/// `cancel` is checked ahead of every file the walk visits (a coarser grain
/// than `spawn_file_scan`'s per-entry check: a single file's content search
/// runs to completion once started, so a query superseded mid-file finishes
/// that one file's matches before the next check notices), for the same
/// reason `spawn_file_scan`'s does -- a live-grep query is replaced by
/// nearly every keystroke, and without this a stale scan over a huge tree
/// would keep pushing into an injector nothing reads.
pub fn spawn_live_grep_scan(
    root: PathBuf,
    needle: String,
    injector: Injector<PickerItem>,
    cancel: Arc<AtomicBool>,
) -> JoinHandle<()> {
    std::thread::spawn(move || {
        let Ok(matcher) = RegexMatcher::new(&escape_literal(&needle)) else {
            return;
        };
        let mut searcher = Searcher::new();
        for entry in ignore::WalkBuilder::new(&root).build() {
            if cancel.load(Ordering::Acquire) {
                return;
            }
            let Ok(entry) = entry else { continue };
            let is_file = entry.file_type().is_some_and(|ft| ft.is_file());
            if !is_file {
                continue;
            }
            let rel = entry
                .path()
                .strip_prefix(&root)
                .unwrap_or_else(|_| entry.path())
                .to_string_lossy()
                .into_owned();
            let _ = searcher.search_path(
                &matcher,
                entry.path(),
                UTF8(|line_number, line| {
                    let label = format!("{rel}:{line_number}: {}", line.trim_end_matches('\n'));
                    injector.push(PickerItem::new(label), |item, cols| {
                        cols[0] = item.label.as_str().into();
                    });
                    Ok(true)
                }),
            );
        }
    })
}

/// Escapes every regex metacharacter in `needle` so `RegexMatcher` treats it
/// as a literal substring rather than a pattern -- a picker query is text a
/// user typed to find, not a regex they composed (see
/// [`spawn_live_grep_scan`]'s doc). A small hand-written escape rather than
/// pulling in `regex`/`regex-syntax` as an extra explicit dependency just
/// for `regex::escape`: `grep-regex` already depends on `regex-syntax`
/// transitively, and this crate's dependency surface for the picker source
/// stays exactly `ignore` + `grep-searcher` + `grep-regex`, matching the
/// brief's chosen shape.
fn escape_literal(needle: &str) -> String {
    let mut escaped = String::with_capacity(needle.len());
    for c in needle.chars() {
        if "\\.+*?()|[]{}^$".contains(c) {
            escaped.push('\\');
        }
        escaped.push(c);
    }
    escaped
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    /// The live-grep source's own latency consequence: a full scan-and-search
    /// pass over a real, representative corpus (this workspace's own
    /// `crates/` tree, ~136 files at the time this was measured), run to
    /// completion synchronously rather than streamed. Not a substitute for
    /// `task bench`'s paired `output_path` scenario -- that scenario measures
    /// redraw-to-terminal-write latency with the preview pane painted open,
    /// which needs a real PTY-paired nvim spawn this shared dev host cannot
    /// run cleanly (see the commit description for the measured `output_path`
    /// attempt and why it was substituted). This measures the other half of
    /// the same latency consequence instead: the new hot path live-grep
    /// introduces runs entirely on a background thread, off the paint loop
    /// (see this module's doc and `matcher::restart_live_grep`'s per-query
    /// cancellation), so its own wall time is what a keystroke's worth of
    /// live-grep work costs the scan thread, not what it costs a frame.
    /// Measured once in release mode for this commit's description; the
    /// ceiling here is a generous, debug-build-safe regression guard on the
    /// same terms `matcher`'s own
    /// `keystroke_to_first_results_at_100k_resident_entries` uses, not the
    /// number recorded in the commit.
    #[test]
    fn live_grep_scan_over_the_workspaces_own_crates_tree() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../crates")
            .canonicalize()
            .expect("workspace crates/ directory must exist");

        let mut nucleo: nucleo::Nucleo<PickerItem> =
            nucleo::Nucleo::new(nucleo::Config::DEFAULT, Arc::new(|| {}), None, 1);
        let injector = nucleo.injector();
        let cancel = Arc::new(AtomicBool::new(false));

        let start = std::time::Instant::now();
        let handle = spawn_live_grep_scan(root, "fn ".to_string(), injector, cancel);
        handle.join().expect("grep scan thread panicked");
        let elapsed = start.elapsed();

        assert!(
            elapsed < std::time::Duration::from_secs(20),
            "live-grep scan over crates/ took {elapsed:?}, far past a debug-build-safe ceiling"
        );

        // the scan produced results at all -- an empty pass would make the
        // latency measurement meaningless (nothing was actually searched)
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            nucleo.tick(10);
            if nucleo.snapshot().item_count() > 0 || std::time::Instant::now() > deadline {
                break;
            }
        }
        assert!(
            nucleo.snapshot().item_count() > 0,
            "expected at least one \"fn \" match across crates/"
        );
    }

    #[test]
    fn escape_literal_escapes_every_regex_metacharacter() {
        assert_eq!(escape_literal("a.b*c"), "a\\.b\\*c");
        assert_eq!(escape_literal("plain"), "plain");
        assert_eq!(escape_literal("(foo|bar)"), "\\(foo\\|bar\\)");
    }

    /// A small on-disk tree with one matching line and one non-matching
    /// line, proving the scan's label shape end to end: relative path,
    /// 1-based line number, and the matched line's own text, exactly
    /// `"{path}:{line}: {text}"` -- the format `PickerState::selected_path`
    /// parses back out for `Source::LiveGrep`.
    #[test]
    fn a_matching_line_is_pushed_as_path_colon_line_colon_text() {
        let nonce = format!(
            "{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or_default()
        );
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/tmp")
            .join(format!("picker-grep-format-{nonce}"));
        std::fs::create_dir_all(&root).expect("create test root");
        std::fs::write(
            root.join("needle.txt"),
            "no match on this line\na needle sits here\n",
        )
        .expect("write test file");

        // a synthetic Nucleo instance purely as an Injector source: this
        // test drives `spawn_live_grep_scan` directly rather than through
        // `matcher::Session`, so it needs only the `Injector` half of that
        // type, the same shape `matcher`'s own streaming test uses against
        // a synthetic producer
        let mut nucleo: nucleo::Nucleo<PickerItem> =
            nucleo::Nucleo::new(nucleo::Config::DEFAULT, Arc::new(|| {}), None, 1);
        let injector = nucleo.injector();
        let cancel = Arc::new(AtomicBool::new(false));
        let handle =
            spawn_live_grep_scan(root.clone(), "needle".to_string(), injector, cancel.clone());
        handle.join().expect("grep scan thread panicked");

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            nucleo.tick(10);
            if nucleo.snapshot().item_count() >= 1 || std::time::Instant::now() > deadline {
                break;
            }
        }
        let snapshot = nucleo.snapshot();
        let items: Vec<String> = (0..snapshot.item_count())
            .filter_map(|i| snapshot.get_item(i))
            .map(|item| item.data.label.clone())
            .collect();
        assert_eq!(
            items,
            vec!["needle.txt:2: a needle sits here".to_string()],
            "expected exactly one match at line 2, formatted as path:line: text, got {items:?}"
        );

        let _ = std::fs::remove_dir_all(&root);
    }
}
