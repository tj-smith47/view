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
/// A matched line's `label` is `"{relative path}:{line number}: {line
/// text}"`, built via `PickerItem::grep_match`, which also records the path
/// and line as data and the byte offset the matched text begins at. The
/// matcher column pushed into `injector` is the matched *text alone*, not
/// the whole label: nucleo would otherwise happily attribute a match to a
/// byte inside the `path:line: ` prefix, highlighting part of the file name
/// or line number instead of the text a user actually searched for. The
/// matcher worker's own nucleo pass re-derives the highlighted match span
/// from that column against `needle` and shifts it back by
/// `PickerItem::match_start` before storing it, the same pipeline
/// `build_results` already runs for `Files`/`Buffers`, so this scan does not
/// compute or push its own indices.
///
/// `cancel` is checked ahead of every file the walk visits (a coarser grain
/// than `spawn_file_scan`'s per-entry check: a single file's content search
/// runs to completion once started, so a query superseded mid-file finishes
/// that one file's matches before the next check notices) and again inside
/// the search sink itself on every matched line, so a query superseded
/// mid-file stops within that file rather than running it to completion --
/// for the same reason `spawn_file_scan`'s per-entry check exists: a
/// live-grep query is replaced by nearly every keystroke, and without this
/// a stale scan over a huge tree (or one huge file) would keep pushing into
/// an injector nothing reads.
/// Ceiling on the total number of matched lines a single live-grep scan will
/// push before stopping outright, independent of `cancel`. A query with
/// almost no discriminating power (a single common letter, an empty needle
/// mid-composition) run over a large tree has no other bound: unlike
/// `Files`, where one entry costs one push, a single file here can contribute
/// arbitrarily many matches, so the per-file `cancel` check in the walk loop
/// below does nothing to bound a single pathological file, let alone a
/// pathological *tree*. Chosen generously above what a human picker session
/// scrolls through -- nucleo's own top-N ranking already narrows what's
/// shown -- but far below "stream millions of `PickerItem`s that spend
/// injector-push and channel cost for candidates nothing will ever look at."
pub const LIVE_GREP_MATCH_LIMIT: usize = 5_000;

/// Ceiling on a single matched line's rendered length, in `char`s.
/// Minified/generated/binary-ish files can contain a single line many
/// megabytes long; without this, one match on such a line turns into a
/// `PickerItem` label large enough to cost real time just laying out and
/// diffing on every keystroke, long before a human could usefully read that
/// much text in a picker row.
pub const LIVE_GREP_LINE_CHAR_LIMIT: usize = 300;

pub fn spawn_live_grep_scan(
    root: PathBuf,
    needle: String,
    injector: Injector<PickerItem>,
    cancel: Arc<AtomicBool>,
) -> JoinHandle<()> {
    std::thread::spawn(move || {
        #[cfg(test)]
        scan_probe::note(&needle, "scan thread started".to_string());
        let Ok(matcher) = RegexMatcher::new(&escape_literal(&needle)) else {
            #[cfg(test)]
            scan_probe::note(&needle, "regex build failed; scan abandoned".to_string());
            return;
        };
        let mut searcher = Searcher::new();
        let mut matched = 0usize;
        #[cfg(test)]
        let mut visited = 0usize;
        for entry in ignore::WalkBuilder::new(&root).build() {
            if cancel.load(Ordering::Acquire) || matched >= LIVE_GREP_MATCH_LIMIT {
                #[cfg(test)]
                scan_probe::note(
                    &needle,
                    format!(
                        "scan stopped early (cancelled={}, matched={matched}, visited={visited})",
                        cancel.load(Ordering::Acquire)
                    ),
                );
                return;
            }
            let entry = match entry {
                Ok(entry) => entry,
                Err(_err) => {
                    #[cfg(test)]
                    scan_probe::note(&needle, format!("walk entry error: {_err}"));
                    continue;
                }
            };
            let is_file = entry.file_type().is_some_and(|ft| ft.is_file());
            if !is_file {
                continue;
            }
            #[cfg(test)]
            {
                visited += 1;
                if visited == 1 {
                    scan_probe::note(&needle, format!("first file visited: {:?}", entry.path()));
                }
            }
            let rel = entry
                .path()
                .strip_prefix(&root)
                .unwrap_or_else(|_| entry.path())
                .to_string_lossy()
                .into_owned();
            // Reads `entry.path()` straight off disk via `grep-searcher`,
            // with no RPC round trip and no check of whether nvim already
            // has this exact path open with unsaved edits -- unlike
            // `preview.rs`'s disk-read fallback (see its own module doc),
            // which only ever reads a path nvim has *no* buffer for. A
            // live-grep match against a dirty buffer can therefore show
            // stale text, or a stale line number, relative to what the
            // buffer actually holds: accepted here because the walker runs
            // on its own background thread over an entire tree, and an RPC
            // round trip per candidate file would defeat the whole point of
            // scanning off the paint loop (see this module's own doc). The
            // picker's `<CR>`-open flow re-resolves through the live buffer
            // regardless, so a stale grep result never writes anything; it
            // can only mislead the label shown before that.
            let result = searcher.search_path(
                &matcher,
                entry.path(),
                UTF8(|line_number, line| {
                    let text = truncate_line(line.trim_end_matches('\n'));
                    let item = PickerItem::grep_match(rel.clone(), line_number, &text);
                    let match_start = item.match_start;
                    injector.push(item, |item, cols| {
                        cols[0] = item.label[match_start..].into();
                    });
                    matched += 1;
                    #[cfg(test)]
                    if matched == 1 {
                        scan_probe::note(&needle, format!("first push: {rel}:{line_number}"));
                    }
                    // stopping the sink here (rather than only the outer
                    // walk loop, checked next iteration) means the ceiling
                    // is exact: the file currently being searched stops
                    // mid-file the instant the limit is reached, instead of
                    // finishing out whatever matches remain in it first.
                    // The cancel check alongside it means a query superseded
                    // mid-file also stops within the file currently being
                    // searched, not just between files -- without it, a scan
                    // could run a huge single file to completion after the
                    // query that started it no longer existed.
                    Ok(matched < LIVE_GREP_MATCH_LIMIT && !cancel.load(Ordering::Acquire))
                }),
            );
            #[cfg(test)]
            if let Err(_err) = &result {
                scan_probe::note(&needle, format!("search error on {rel}: {_err}"));
            }
            let _ = result;
        }
        #[cfg(test)]
        scan_probe::note(
            &needle,
            format!("walk finished (visited={visited}, matched={matched})"),
        );
    })
}

/// Per-needle event log the live-grep scan thread appends to, keyed by the
/// scan's own needle. Exists because the starvation class this diagnoses
/// reads identically from the worker channel's receiving end no matter
/// which leg stalled -- results stay empty whether the scan thread was
/// never scheduled, the walker filtered the fixture out, a read stalled,
/// or the matcher never digested a successful push -- and the
/// discriminating facts are only observable from inside the scan thread
/// while the failure window is still open. A timed-out matcher test
/// attaches [`scan_probe::report`] for its needle to its panic, so a
/// one-in-many CI starvation self-reports instead of demanding another
/// blind reproduction hunt. Keyed by needle so concurrent scans from other
/// tests never interleave their events into the failing test's report;
/// tests that read a report use needles unique to themselves.
#[cfg(test)]
pub(crate) mod scan_probe {
    use std::collections::HashMap;
    use std::sync::{Mutex, OnceLock, PoisonError};
    use std::time::Instant;

    fn events() -> &'static Mutex<HashMap<String, Vec<String>>> {
        static EVENTS: OnceLock<Mutex<HashMap<String, Vec<String>>>> = OnceLock::new();
        EVENTS.get_or_init(|| Mutex::new(HashMap::new()))
    }

    /// Timestamps are relative to the first probe event in the process --
    /// only the gaps between one needle's events carry meaning.
    fn epoch() -> Instant {
        static EPOCH: OnceLock<Instant> = OnceLock::new();
        *EPOCH.get_or_init(Instant::now)
    }

    pub(crate) fn note(needle: &str, event: String) {
        let elapsed = epoch().elapsed().as_millis();
        events()
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .entry(needle.to_string())
            .or_default()
            .push(format!("t={elapsed}ms {event}"));
    }

    pub(crate) fn report(needle: &str) -> String {
        events()
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(needle)
            .map_or_else(
                || "<no scan events recorded for this needle>".to_string(),
                |log| log.join("; "),
            )
    }
}

/// Truncates `line` to [`LIVE_GREP_LINE_CHAR_LIMIT`] `char`s (not bytes --
/// `grep-searcher` hands back already UTF-8-decoded text, and slicing by
/// byte length risks splitting a multi-byte sequence), appending an ellipsis
/// marker so a truncated label is visibly distinct from a short line that
/// happens to end mid-word.
fn truncate_line(line: &str) -> String {
    if line.chars().count() <= LIVE_GREP_LINE_CHAR_LIMIT {
        return line.to_string();
    }
    let mut truncated: String = line.chars().take(LIVE_GREP_LINE_CHAR_LIMIT).collect();
    truncated.push('…');
    truncated
}

/// Escapes every regex metacharacter in `needle` so `RegexMatcher` treats it
/// as a literal substring rather than a pattern -- a picker query is text a
/// user typed to find, not a regex they composed (see
/// [`spawn_live_grep_scan`]'s doc). A small hand-written escape rather than
/// pulling in `regex`/`regex-syntax` as an extra explicit dependency just
/// for `regex::escape`: `grep-regex` already depends on `regex-syntax`
/// transitively, and this crate's dependency surface for the picker source
/// stays exactly `ignore` + `grep-searcher` + `grep-regex`, the live-grep
/// source's chosen dependency set.
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
            elapsed < view_test_support::host_deadline(std::time::Duration::from_secs(20)),
            "live-grep scan over crates/ took {elapsed:?}, far past a debug-build-safe ceiling"
        );

        // the scan produced results at all -- an empty pass would make the
        // latency measurement meaningless (nothing was actually searched)
        let deadline = std::time::Instant::now()
            + view_test_support::host_deadline(std::time::Duration::from_secs(5));
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
    /// `"{path}:{line}: {text}"` -- the display shape `PickerItem::grep_match`
    /// builds, with `path`/`line` also carried as their own fields for
    /// `Source::LiveGrep` preview.
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

        let deadline = std::time::Instant::now()
            + view_test_support::host_deadline(std::time::Duration::from_secs(5));
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

    /// Pins the exact tradeoff `spawn_live_grep_scan`'s own comment above
    /// its `search_path` call discloses: the scan has zero awareness of
    /// buffer state and reflects only whatever bytes sit on disk the
    /// instant it reads them. Writes a needle to disk, then overwrites the
    /// same path with content that has no needle *before* the scan ever
    /// runs -- if this scanner cached, buffered, or otherwise remembered
    /// the file's earlier content the way an open nvim buffer's unsaved
    /// edits would, the stale needle would still be found. It is not: the
    /// scan sees only the final, current disk state.
    #[test]
    fn live_grep_reads_current_disk_bytes_never_a_remembered_earlier_version() {
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
            .join(format!("picker-grep-staleness-{nonce}"));
        std::fs::create_dir_all(&root).expect("create test root");
        let file = root.join("dirty.txt");
        std::fs::write(&file, "an earlier version has needlemark here\n").expect("write baseline");
        std::fs::write(&file, "the current version has no trace of it\n")
            .expect("overwrite before scanning");

        let mut nucleo: nucleo::Nucleo<PickerItem> =
            nucleo::Nucleo::new(nucleo::Config::DEFAULT, Arc::new(|| {}), None, 1);
        let injector = nucleo.injector();
        let cancel = Arc::new(AtomicBool::new(false));
        let handle = spawn_live_grep_scan(
            root.clone(),
            "needlemark".to_string(),
            injector,
            cancel.clone(),
        );
        handle.join().expect("grep scan thread panicked");

        let deadline = std::time::Instant::now()
            + view_test_support::host_deadline(std::time::Duration::from_secs(5));
        loop {
            nucleo.tick(10);
            if std::time::Instant::now() > deadline {
                break;
            }
        }
        assert_eq!(
            nucleo.snapshot().item_count(),
            0,
            "the scan found the overwritten version's earlier content, meaning it read \
             something other than the file's current bytes on disk"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    /// `PickerItem::path`/`line` are read nowhere in production yet (the
    /// `<CR>`-open flow still resolves the selection through
    /// `PickerState::selected_path`, not these fields directly), so nothing
    /// else in this suite pins their values -- only the display label's
    /// shape. This proves the fields themselves carry the right data end to
    /// end from a live scan, independent of the label they were built
    /// alongside.
    #[test]
    fn a_matching_lines_item_carries_its_path_and_line_as_real_fields() {
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
            .join(format!("picker-grep-fields-{nonce}"));
        std::fs::create_dir_all(&root).expect("create test root");
        std::fs::write(
            root.join("needle.txt"),
            "no match on this line\na needle sits here\n",
        )
        .expect("write test file");

        let mut nucleo: nucleo::Nucleo<PickerItem> =
            nucleo::Nucleo::new(nucleo::Config::DEFAULT, Arc::new(|| {}), None, 1);
        let injector = nucleo.injector();
        let cancel = Arc::new(AtomicBool::new(false));
        let handle =
            spawn_live_grep_scan(root.clone(), "needle".to_string(), injector, cancel.clone());
        handle.join().expect("grep scan thread panicked");

        let deadline = std::time::Instant::now()
            + view_test_support::host_deadline(std::time::Duration::from_secs(5));
        loop {
            nucleo.tick(10);
            if nucleo.snapshot().item_count() >= 1 || std::time::Instant::now() > deadline {
                break;
            }
        }
        let snapshot = nucleo.snapshot();
        let items: Vec<PickerItem> = (0..snapshot.item_count())
            .filter_map(|i| snapshot.get_item(i))
            .map(|item| item.data.clone())
            .collect();
        assert_eq!(items.len(), 1, "expected exactly one match, got {items:?}");
        assert_eq!(
            items[0].path.as_deref(),
            Some("needle.txt"),
            "PickerItem::path must carry the match's relative path, not just its label"
        );
        assert_eq!(
            items[0].line,
            Some(2),
            "PickerItem::line must carry the match's 1-based line number, not just its label"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    /// A query matching far more lines than [`LIVE_GREP_MATCH_LIMIT`] must
    /// stop pushing at exactly that many items -- not zero (the cap must not
    /// silently suppress everything), not "some smaller number" (the walk
    /// must not stop early on the first file that alone exceeds the limit),
    /// and not "more than the limit" (the cap must actually bound the sink,
    /// not just the outer per-file loop).
    #[test]
    fn a_scan_past_the_match_limit_stops_at_exactly_the_limit() {
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
            .join(format!("picker-grep-limit-{nonce}"));
        std::fs::create_dir_all(&root).expect("create test root");

        // one matching line per row, well past the limit, split across two
        // files so the outer walk loop's own `matched >= LIMIT` check (not
        // just the sink's) is exercised too
        let rows_per_file = LIVE_GREP_MATCH_LIMIT;
        let mut contents = String::new();
        for i in 0..rows_per_file {
            contents.push_str(&format!("needle row {i}\n"));
        }
        std::fs::write(root.join("a.txt"), &contents).expect("write first fixture file");
        std::fs::write(root.join("b.txt"), &contents).expect("write second fixture file");

        let mut nucleo: nucleo::Nucleo<PickerItem> =
            nucleo::Nucleo::new(nucleo::Config::DEFAULT, Arc::new(|| {}), None, 1);
        let injector = nucleo.injector();
        let cancel = Arc::new(AtomicBool::new(false));
        let handle =
            spawn_live_grep_scan(root.clone(), "needle".to_string(), injector, cancel.clone());
        handle.join().expect("grep scan thread panicked");

        let deadline = std::time::Instant::now()
            + view_test_support::host_deadline(std::time::Duration::from_secs(10));
        loop {
            nucleo.tick(10);
            if nucleo.snapshot().item_count() as usize >= LIVE_GREP_MATCH_LIMIT
                || std::time::Instant::now() > deadline
            {
                break;
            }
        }
        let count = nucleo.snapshot().item_count() as usize;
        assert_eq!(
            count,
            LIVE_GREP_MATCH_LIMIT,
            "a scan with {} available matches across two files must stop at \
             exactly LIVE_GREP_MATCH_LIMIT ({LIVE_GREP_MATCH_LIMIT}), got {count}",
            rows_per_file * 2,
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    /// The cancel flag must be checked inside a single file's search, not
    /// only between files: before this, the sink closure passed to
    /// `Searcher::search_path` never consulted `cancel` at all, so a query
    /// superseded mid-file kept searching that one file to completion in
    /// the background no matter how large it was -- the outer walk loop's
    /// own per-file check does nothing for a scan that never leaves its
    /// first file. Deterministic rather than a sleep-and-hope, the same
    /// shape `matcher`'s own `replacing_a_session_cancels_its_files_scan_in_flight`
    /// uses: waits for real progress, flips `cancel`, then waits for the
    /// injected count to stop growing and asserts it settled below every
    /// matching line the file actually holds.
    #[test]
    fn a_cancelled_scan_stops_inside_a_single_file_not_only_between_files() {
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
            .join(format!("picker-grep-cancel-mid-file-{nonce}"));
        std::fs::create_dir_all(&root).expect("create test root");

        // one file, far more matching lines than the scan could plausibly
        // finish pushing before the polling loop below observes progress
        // and flips cancel -- large enough that "the whole file" and
        // "cancelled mid-file" read as unambiguously different outcomes
        let total_rows = 500_000usize;
        let mut contents = String::with_capacity(total_rows * 16);
        for i in 0..total_rows {
            contents.push_str(&format!("needle row {i}\n"));
        }
        std::fs::write(root.join("huge.txt"), &contents).expect("write huge fixture file");

        let nucleo: nucleo::Nucleo<PickerItem> =
            nucleo::Nucleo::new(nucleo::Config::DEFAULT, Arc::new(|| {}), None, 1);
        let injector = nucleo.injector();
        let cancel = Arc::new(AtomicBool::new(false));
        let handle = spawn_live_grep_scan(
            root.clone(),
            "needle".to_string(),
            injector.clone(),
            cancel.clone(),
        );

        let start_deadline = std::time::Instant::now()
            + view_test_support::host_deadline(std::time::Duration::from_secs(10));
        while injector.injected_items() == 0 {
            assert!(
                std::time::Instant::now() < start_deadline,
                "the scan never produced a single match"
            );
        }
        cancel.store(true, Ordering::Release);

        let settle_deadline = std::time::Instant::now()
            + view_test_support::host_deadline(std::time::Duration::from_secs(10));
        let mut last = injector.injected_items();
        loop {
            std::thread::sleep(std::time::Duration::from_millis(20));
            let now = injector.injected_items();
            if now == last {
                break;
            }
            last = now;
            assert!(
                std::time::Instant::now() < settle_deadline,
                "the injected item count never stopped growing after cancel was set"
            );
        }
        handle.join().expect("grep scan thread panicked");

        assert!(
            last < total_rows as u32,
            "expected the scan to stop before matching every line in a single \
             {total_rows}-line file once cancelled, got {last} items"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn truncate_line_leaves_short_lines_untouched() {
        assert_eq!(truncate_line("short line"), "short line");
    }

    #[test]
    fn truncate_line_caps_long_lines_with_an_ellipsis_marker() {
        let long_line = "x".repeat(LIVE_GREP_LINE_CHAR_LIMIT + 50);
        let truncated = truncate_line(&long_line);
        assert_eq!(truncated.chars().count(), LIVE_GREP_LINE_CHAR_LIMIT + 1);
        assert!(truncated.ends_with('…'));
    }
}
