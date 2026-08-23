//! Pure state for the fuzzy picker overlay: the query text, the corpus it
//! searches, and the last result set the matcher worker delivered. No
//! matcher handle lives here -- `view-native`'s nucleo-backed worker owns
//! matching and streaming; this module only tracks what to ask it and what
//! it last answered.
//!
//! # The generation stamp
//!
//! `PickerState` never reuses a generation across an open/close cycle: every
//! generation this module hands out comes from one process-wide monotonic
//! counter ([`next_generation`]), not from a per-`PickerState` sequence
//! starting at zero. A per-instance counter would let a reply the matcher
//! worker is still in flight to answer -- issued by a picker session that
//! has since closed -- land on a brand-new session whose own counter
//! happens to have reached the same small number, and be wrongly accepted
//! as current. The worker thread that answers these requests is long-lived
//! (spawned once, outliving any one picker session, the same shape
//! `view::clipboard`'s worker takes), so that collision is not theoretical:
//! a query in flight when a picker closes can still be being matched when
//! the next one opens. A process-wide counter makes every generation this
//! process ever issues unique for the run's whole lifetime, closing the
//! hole the same way `HlTable::probe_generation` closes it for highlight
//! probes -- see that type's doc for the identical hazard at lower
//! frequency.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use super::views::{PickerView, Span, StyleRole};

/// The next generation [`PickerState::open`] or [`PickerState::edit_query`]
/// will hand out. Starts at `1`: `0` is reserved as "no query has ever been
/// issued", available to a future caller that wants a sentinel default
/// without colliding with a real generation.
static NEXT_GENERATION: AtomicU64 = AtomicU64::new(1);

fn next_generation() -> u64 {
    NEXT_GENERATION.fetch_add(1, Ordering::Relaxed)
}

/// What a picker session searches.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Source {
    /// Every file under `root` that `ignore`'s `.gitignore`-aware walk
    /// yields, matched against the query as a path.
    Files { root: PathBuf },
    /// The current session's listed buffers (`:ls`'s own set: loaded and
    /// `buflisted`), resolved from engine state -- see
    /// `docs/picker-buffer-list-wire-capture.md`.
    Buffers,
    /// A live `ripgrep`-style content search under `root`: the query text is
    /// the search pattern itself, matched against file content in-process
    /// (`view_native::picker::sources::spawn_live_grep_scan`), not a fuzzy
    /// filter over a pre-walked corpus -- see [`Source::LiveGrep`]'s
    /// re-scan-per-query contract documented on `Effect::PickerQuery`.
    LiveGrep { root: PathBuf },
}

/// One candidate the matcher scored: its display text and the byte ranges
/// within it nucleo's matcher attributed to the query, already sorted and
/// deduplicated by the worker that produced them.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct PickerItem {
    /// The text matched against and displayed: a path relative to
    /// `Source::Files`'s root, a buffer name (`[No Name]` for an unnamed
    /// buffer -- see the wire-capture doc's conclusions), or for a
    /// `LiveGrep` match, `"{path}:{line}: {text}"` -- see [`Self::grep_match`].
    pub label: String,
    /// Byte offsets into `label` the match touched. Empty for an item that
    /// has not been scored against a query yet (the unfiltered corpus, or a
    /// pre-resolved `Buffers` seed before its first pattern reparse).
    pub indices: Vec<u32>,
    /// Byte offset into `label` where the substring nucleo actually matches
    /// against begins. `0` for every item except a [`Self::grep_match`]
    /// one, whose `label` carries a `path:line: ` prefix ahead of the
    /// matched text; the matcher worker shifts nucleo's own offsets by this
    /// amount before storing them in `indices`, so a match can never be
    /// attributed to a byte inside that prefix.
    pub match_start: usize,
    /// The file this candidate previews to and jumps to on selection,
    /// resolved once here rather than re-derived from `label` at selection
    /// time. `None` for a source with no file of its own (an unnamed
    /// `Buffers` entry).
    pub path: Option<String>,
    /// The 1-based line number `path` should open at, alongside `path`.
    /// `None` for a source with no line concept (`Files`, `Buffers`).
    pub line: Option<u64>,
}

impl PickerItem {
    /// A candidate with no match indices yet.
    #[must_use]
    pub fn new(label: impl Into<String>) -> Self {
        Self {
            label: label.into(),
            ..Self::default()
        }
    }

    /// A live-grep match at `path`:`line`, whose matched text is `text`.
    ///
    /// `path` and `line` are stored as data rather than re-derived by
    /// splitting `label` on `:` at selection time: a `label` here is
    /// `"{path}:{line}: {text}"`, and a `path` that itself contains `:`
    /// (a Windows drive letter, or simply a colon in a filename) made the
    /// naive `label.split(':').next()` split resolve to the wrong file --
    /// see `PickerState::selected_path`'s own doc for the bug this closed.
    ///
    /// `match_start` is set to the byte offset where `text` begins within
    /// `label`, so the matcher worker's own offset-shift keeps every
    /// highlighted match inside `text`, never inside the `path:line: `
    /// prefix ahead of it.
    #[must_use]
    pub fn grep_match(path: impl Into<String>, line: u64, text: &str) -> Self {
        let path = path.into();
        let label = format!("{path}:{line}: {text}");
        let match_start = label.len() - text.len();
        Self {
            label,
            indices: Vec::new(),
            match_start,
            path: Some(path),
            line: Some(line),
        }
    }
}

/// One open picker session: which corpus it searches, the query typed so
/// far, and the last (never stale) result set the matcher answered.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq)]
pub struct PickerState {
    source: Source,
    query: String,
    generation: u64,
    items: Vec<PickerItem>,
    selected: usize,
    /// The generation stamped on the most recent preview request this
    /// session issued (RPC or disk-fallback) -- a fresh preview counter
    /// rather than reusing `generation`, since a query result landing does
    /// not by itself mean the previously requested preview is stale (the
    /// selected candidate can be unchanged across a result set that only
    /// reordered other rows). `0` before any preview has ever been
    /// requested, the same reserved sentinel `next_generation`'s doc names.
    preview_generation: u64,
    /// The candidate path the current `preview_lines` belongs to, or the
    /// path most recently requested while a reply is still in flight.
    preview_path: Option<String>,
    /// The preview pane's last-known-good content. Left in place across a
    /// selection change until the new selection's own reply lands, rather
    /// than cleared immediately: a picker with a fast typist and a slow
    /// preview round trip should never flash an empty pane between every
    /// keystroke.
    preview_lines: Vec<String>,
}

impl PickerState {
    /// Opens a session over `source` with an empty query and no results yet.
    /// The caller issues the initial `Effect::PickerQuery` at
    /// [`PickerState::generation`] itself -- `open` only allocates the
    /// generation, it does not query, so a pure constructor never has to
    /// smuggle an effect out through a side channel.
    #[must_use]
    pub fn open(source: Source) -> Self {
        Self {
            source,
            query: String::new(),
            generation: next_generation(),
            items: Vec::new(),
            selected: 0,
            preview_generation: 0,
            preview_path: None,
            preview_lines: Vec::new(),
        }
    }

    /// This session's corpus.
    #[must_use]
    pub fn source(&self) -> &Source {
        &self.source
    }

    /// The query as typed so far.
    #[must_use]
    pub fn query(&self) -> &str {
        &self.query
    }

    /// This session's current generation: the one its most recent query was
    /// tagged with, and the one a fresh [`PickerState::apply_results`] must
    /// match to be accepted.
    #[must_use]
    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// Applies one key-notation edit to the query: a single non-`<`-prefixed
    /// character inserts, `<BS>` deletes the last character, anything else
    /// is a no-op edit. Bumps and returns the new generation regardless --
    /// the caller decides which keys reach this at all (see `update()`'s
    /// picker key routing), so by the time a call lands here the edit is
    /// always meant to be attempted.
    pub fn edit_query(&mut self, notation: &str) -> u64 {
        if notation == "<BS>" {
            self.query.pop();
        } else if let Some(c) = single_char(notation) {
            self.query.push(c);
        }
        self.requery()
    }

    /// Inserts one pasted `text` into the query as a single edit, every
    /// control character in it standing in as the space it would paint as.
    ///
    /// A query is one line and the filter matches against one: dropping the
    /// line breaks out of a pasted list instead would run its last word into
    /// the next one's first and filter for a needle that was never on the
    /// clipboard.
    pub fn paste_query(&mut self, text: &str) -> u64 {
        self.query
            .extend(text.chars().map(|c| if c.is_control() { ' ' } else { c }));
        self.requery()
    }

    /// The bookkeeping every query edit ends with: the fresh generation the
    /// matcher worker's answer must carry back, and the selection returned
    /// to the top of results that have not arrived yet.
    fn requery(&mut self) -> u64 {
        self.generation = next_generation();
        self.selected = 0;
        self.generation
    }

    /// Applies the matcher worker's answer for `generation`, or drops it if
    /// a later query has since superseded it -- the identical hazard
    /// `HlTable::probe_generation`'s doc names, at far higher frequency (see
    /// this module's doc).
    pub fn apply_results(&mut self, generation: u64, items: Vec<PickerItem>) {
        if generation != self.generation {
            return;
        }
        self.items = items;
        if self.selected >= self.items.len() {
            self.selected = self.items.len().saturating_sub(1);
        }
    }

    /// The full path a preview should be issued for, resolved from the
    /// currently selected candidate and this session's source. `None` for
    /// an empty result set, or for a `Buffers` selection with no real name
    /// (nvim's own unnamed scratch buffer, displayed as `[No Name]` -- see
    /// `docs/picker-buffer-list-wire-capture.md`): there is nothing on disk
    /// or in a named buffer to preview for either.
    #[must_use]
    pub fn selected_path(&self) -> Option<String> {
        let item = self.items.get(self.selected)?;
        match &self.source {
            Source::Files { root } => Some(join_display(root, &item.label)),
            Source::Buffers => {
                if item.label == "[No Name]" {
                    None
                } else {
                    Some(item.label.clone())
                }
            }
            // `item.path` is set at push time by `PickerItem::grep_match`,
            // not re-derived by splitting `label` on `:` -- a relative path
            // containing its own `:` made that split resolve to the wrong
            // file (see `PickerItem::grep_match`'s doc).
            Source::LiveGrep { root } => item.path.as_deref().map(|rel| join_display(root, rel)),
        }
    }

    /// Allocates a fresh preview generation for the currently selected
    /// candidate and records it as this session's outstanding preview
    /// request, or does nothing (returning `None`) when there is no
    /// selection to preview, *or* when `preview_path` already names this
    /// same candidate. `preview_path` is set the instant a request is
    /// issued (not only once its reply lands -- see the field's own doc),
    /// so this one comparison covers both "the preview already shown is
    /// this path" and "a request for this path is already in flight":
    /// without it, a streamed result batch that reorders rows without
    /// changing the selected candidate (a `LiveGrep` query commonly
    /// resolves several rows to the same file, differing only by line) would
    /// re-issue a preview request for a path already current, storming the
    /// RPC channel with redundant reads of a buffer that has not changed.
    /// The caller issues the actual `Effect::Rpc(RpcCall::PreviewBuffer)`
    /// with the returned pair -- this method only allocates the generation,
    /// mirroring `PickerState::open`'s own "allocate, do not itself emit an
    /// effect" contract.
    pub fn refresh_preview(&mut self) -> Option<(u64, String)> {
        let path = self.selected_path()?;
        if self.preview_path.as_deref() == Some(path.as_str()) {
            return None;
        }
        let generation = next_generation();
        self.preview_generation = generation;
        self.preview_path = Some(path.clone());
        Some((generation, path))
    }

    /// This session's outstanding preview generation, for a caller (the
    /// disk-fallback path) that needs to re-tag a follow-up request with the
    /// same generation an RPC reply already carried, rather than allocating
    /// a new one that would make the fallback its own, separately-gated
    /// request.
    #[must_use]
    pub fn preview_generation(&self) -> u64 {
        self.preview_generation
    }

    /// Applies a preview reply (from RPC or disk-fallback) if `generation`
    /// still matches the outstanding request, the same stale-drop contract
    /// [`PickerState::apply_results`] documents for `PickerState::generation`.
    /// A dropped reply leaves `preview_lines` exactly as it was -- the last
    /// known content for whatever the previous selection was stays visible
    /// rather than being cleared by an answer that no longer applies.
    pub fn apply_preview(&mut self, generation: u64, lines: Vec<String>) {
        if generation != self.preview_generation {
            return;
        }
        self.preview_lines = lines;
    }

    /// This session's paint-facing projection: the query line, the
    /// candidate rows with their matched substrings carrying
    /// [`StyleRole::Match`], and which row is highlighted.
    #[must_use]
    pub fn view(&self) -> PickerView {
        let title = match &self.source {
            Source::Files { .. } => "Files",
            Source::Buffers => "Buffers",
            Source::LiveGrep { .. } => "Live Grep",
        };
        let rows = self.items.iter().map(item_spans).collect();
        let mut view = PickerView::new(title)
            .with_query(self.query.clone())
            .with_span_rows(rows)
            .with_preview(self.preview_lines.clone());
        if !self.items.is_empty() {
            view = view.with_selected(self.selected);
        }
        view
    }
}

/// Joins `root` and `rel` as a path and renders it back to a `String` for
/// the RPC/disk-read call sites that need one -- `PathBuf` itself never
/// crosses into `Msg`/`Effect` (they carry plain `String`s, the same choice
/// every other picker field here makes), and `to_string_lossy` is
/// acceptable here for the same reason it is in
/// `view_native::picker::sources::spawn_file_scan`: a path with invalid
/// UTF-8 is vanishingly rare on the platforms this project targets, and a
/// lossily-rendered preview path degrades to "wrong preview" rather than a
/// panic.
fn join_display(root: &std::path::Path, rel: &str) -> String {
    root.join(rel).to_string_lossy().into_owned()
}

/// A single non-`<`-prefixed character, or `None` for an empty string, a
/// multi-character notation (`<CR>`, `<Esc>`, ...), or a multi-byte grapheme
/// this simple editor does not attempt to compose.
fn single_char(notation: &str) -> Option<char> {
    let mut chars = notation.chars();
    let c = chars.next()?;
    if c == '<' || chars.next().is_some() {
        return None;
    }
    Some(c)
}

/// Splits `item.label` into spans around its matched byte ranges: the
/// matched runs carry [`StyleRole::Match`], everything else is
/// [`StyleRole::Plain`]. `item.indices` are byte offsets of individual
/// matched characters (nucleo's own unit), coalesced into contiguous runs
/// here so adjacent matched characters paint as one span rather than one per
/// byte.
fn item_spans(item: &PickerItem) -> Vec<Span> {
    if item.indices.is_empty() {
        return vec![Span::plain(item.label.clone())];
    }
    let bytes = item.label.as_bytes();
    let mut sorted = item.indices.clone();
    sorted.sort_unstable();
    sorted.dedup();

    let mut spans = Vec::new();
    let mut cursor = 0usize;
    let mut i = 0usize;
    while i < sorted.len() {
        let start = sorted[i] as usize;
        if start >= bytes.len() {
            break;
        }
        if start > cursor {
            push_valid(&mut spans, &item.label, cursor, start, StyleRole::Plain);
        }
        let mut end = start;
        while i < sorted.len() && sorted[i] as usize == end {
            end += 1;
            i += 1;
        }
        end = end.min(bytes.len());
        push_valid(&mut spans, &item.label, start, end, StyleRole::Match);
        cursor = end;
    }
    if cursor < item.label.len() {
        push_valid(
            &mut spans,
            &item.label,
            cursor,
            item.label.len(),
            StyleRole::Plain,
        );
    }
    if spans.is_empty() {
        spans.push(Span::plain(item.label.clone()));
    }
    spans
}

/// Pushes `label[start..end]` as a span of `role`, or does nothing for a
/// range that does not land on a UTF-8 char boundary -- nucleo's indices are
/// char-based, not byte-based, on non-ASCII input, and a boundary
/// mismatch here must degrade to "skip this run" rather than panic slicing
/// a multi-byte character in half.
fn push_valid(spans: &mut Vec<Span>, label: &str, start: usize, end: usize, role: StyleRole) {
    if let Some(slice) = label.get(start..end) {
        spans.push(Span::new(slice, role));
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn open_starts_with_no_query_and_no_results() {
        let state = PickerState::open(Source::Buffers);
        assert!(state.query().is_empty());
        assert!(state.view().rows.is_empty());
        assert_eq!(state.view().selected, None);
    }

    #[test]
    fn edit_query_inserts_a_plain_character_and_bumps_generation() {
        let mut state = PickerState::open(Source::Buffers);
        let g0 = state.generation();
        let g1 = state.edit_query("m");
        assert!(g1 > g0);
        assert_eq!(state.generation(), g1);
        assert_eq!(state.query(), "m");
    }

    #[test]
    fn edit_query_bs_deletes_the_last_character() {
        let mut state = PickerState::open(Source::Buffers);
        state.edit_query("m");
        state.edit_query("a");
        state.edit_query("<BS>");
        assert_eq!(state.query(), "m");
    }

    #[test]
    fn edit_query_ignores_multi_char_notation_but_still_bumps() {
        let mut state = PickerState::open(Source::Buffers);
        let before = state.generation();
        let after = state.edit_query("<Up>");
        assert_eq!(state.query(), "");
        assert!(after > before);
    }

    #[test]
    fn paste_query_inserts_the_whole_text_with_its_line_breaks_spaced_out() {
        let mut state = PickerState::open(Source::Buffers);
        state.edit_query("s");
        let before = state.generation();
        let after = state.paste_query("rc/main.rs\nsrc/lib.rs\n");

        assert_eq!(
            state.query(),
            "src/main.rs src/lib.rs ",
            "the paste lands whole, each break standing in as a space"
        );
        assert!(after > before, "the matcher worker gets a fresh generation");
    }

    #[test]
    fn a_reply_for_a_stale_generation_is_dropped_not_merged() {
        // feed results for an old generation after a newer one has already
        // been issued, and confirm they never reach the view -- a naive
        // `apply_results` that always overwrites passes every other test in
        // this module and only this one catches it
        let mut state = PickerState::open(Source::Files {
            root: PathBuf::from("/tmp"),
        });
        let gen1 = state.generation();
        state.apply_results(gen1, vec![PickerItem::new("a.rs")]);
        assert_eq!(state.view().rows.len(), 1);

        let gen2 = state.edit_query("a");
        assert!(gen2 > gen1);

        // the stale gen1 reply arrives after gen2 was already issued
        state.apply_results(
            gen1,
            vec![
                PickerItem::new("stale.rs"),
                PickerItem::new("also-stale.rs"),
            ],
        );
        assert_eq!(
            state.view().rows,
            vec![vec![Span::plain("a.rs")]],
            "a reply for a superseded generation must be ignored entirely, not merged"
        );

        state.apply_results(gen2, vec![PickerItem::new("b.rs")]);
        assert_eq!(state.view().rows, vec![vec![Span::plain("b.rs")]]);
    }

    #[test]
    fn view_highlights_matched_byte_ranges_and_leaves_the_rest_plain() {
        let mut state = PickerState::open(Source::Files {
            root: PathBuf::from("/tmp"),
        });
        let gen = state.generation();
        let item = PickerItem {
            label: "main.rs".to_string(),
            indices: vec![0, 1, 5],
            ..PickerItem::default()
        };
        state.apply_results(gen, vec![item]);
        let rows = state.view().rows;
        assert_eq!(
            rows,
            vec![vec![
                Span::new("ma", StyleRole::Match),
                Span::new("in.", StyleRole::Plain),
                Span::new("r", StyleRole::Match),
                Span::new("s", StyleRole::Plain),
            ]]
        );
    }

    #[test]
    fn view_selects_the_first_row_once_results_arrive() {
        let mut state = PickerState::open(Source::Buffers);
        let gen = state.generation();
        state.apply_results(gen, vec![PickerItem::new("a"), PickerItem::new("b")]);
        assert_eq!(state.view().selected, Some(0));
    }

    #[test]
    fn selection_clamps_when_a_new_result_set_is_smaller() {
        let mut state = PickerState::open(Source::Buffers);
        let gen = state.generation();
        state.apply_results(gen, vec![PickerItem::new("a"), PickerItem::new("b")]);
        state.selected = 1;
        let gen2 = state.edit_query("x");
        state.apply_results(gen2, vec![PickerItem::new("a")]);
        assert_eq!(state.view().selected, Some(0));
    }

    #[test]
    fn each_open_call_issues_a_process_unique_generation() {
        let a = PickerState::open(Source::Buffers);
        let b = PickerState::open(Source::Buffers);
        assert_ne!(a.generation(), b.generation());
    }

    #[test]
    fn selected_path_for_live_grep_uses_the_stored_path_not_a_label_split() {
        // a relative path containing its own ':' -- the old
        // `item.label.split(':').next()` resolved this to "src/mod"
        // instead of the real file, previewing (or opening) the wrong path
        // entirely
        let mut state = PickerState::open(Source::LiveGrep {
            root: PathBuf::from("/repo"),
        });
        let gen = state.generation();
        let item = PickerItem::grep_match("src/mod:weird.rs", 3, "let x = 1;");
        state.apply_results(gen, vec![item]);
        // joined rather than spelled out: the separator between root and
        // match is the platform's, and a hardcoded '/' asserts a Unix
        // rendering on a Windows host that never produced one -- while the
        // ':' this test is about is a path character on Unix and the drive
        // separator on Windows, where splitting a label at it resolves
        // `C:\repo\src\main.rs` to `C`. Building `expected` with the same
        // join production uses makes the separator half of this assertion
        // unfalsifiable by construction; the ':' surviving intact is the
        // half under test
        let expected = PathBuf::from("/repo").join("src/mod:weird.rs");
        assert_eq!(
            state.selected_path().as_deref(),
            Some(expected.to_string_lossy().as_ref()),
            "a live-grep path containing ':' must resolve to the real file, \
             not the text before the first ':' in the display label"
        );
    }
}
