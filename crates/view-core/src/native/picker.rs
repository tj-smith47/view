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
use std::sync::Arc;

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

/// The result a picker key opens: a file, at a line for a grep match, or
/// one of nvim's listed buffers.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Picked {
    /// The file's path, or the buffer's name as `nvim_buf_get_name` gives
    /// it (empty for an unnamed buffer).
    pub name: String,
    /// The 1-based line the cursor lands on.
    pub line: Option<u64>,
    /// Whether `name` names a listed buffer.
    pub buffer: bool,
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
    /// The 1-based first line of the window most recently requested for
    /// `preview_path`.
    preview_first: u64,
    /// The query generation and the line the most recent preview request
    /// was issued for. A line past a file's end is read once per query,
    /// because every streamed result batch would otherwise read it again.
    preview_for: (u64, Option<u64>),
    /// The preview pane's last-known-good content. Left in place across a
    /// selection change until the new selection's own reply lands, rather
    /// than cleared immediately: a picker with a fast typist and a slow
    /// preview round trip should never flash an empty pane between every
    /// keystroke. Shared with every frame's view, which copies the pointer.
    preview_lines: Arc<[String]>,
    /// The candidate path `preview_lines` was read from, `None` until a
    /// reply lands. A line is marked only while it names the selection's
    /// own file: until a new file's reply lands, the pane still holds the
    /// previous file.
    applied_path: Option<String>,
    /// The 1-based line number of `preview_lines[0]`.
    applied_first: u64,
    /// The line the pane last opened on with its mark in the applied
    /// window. While a window for another line is read, in this file or
    /// another, the pane stays on this line unmarked, so the only change a
    /// person sees is the new window arriving.
    shown_line: Option<u64>,
}

/// How many lines one preview read returns. A pane is at most a few
/// hundred rows tall, and the window holds half this many on each side of
/// the line it opens on, so the pane fills wherever that line sits in it.
/// A whole file is never read: a log of gigabytes costs the same as this
/// window.
pub const PREVIEW_WINDOW_LINES: u64 = 1000;

/// How many bytes of one line a preview read keeps, cut on a character
/// boundary. The pane cuts a line at its own width, and the widest pane is
/// a few hundred cells, which this holds in any script. The disk read and
/// the engine's buffer read both cut here, so one window is at most
/// [`PREVIEW_WINDOW_LINES`] lines of this many bytes, 4 MiB, whatever the
/// file holds.
pub const PREVIEW_LINE_BYTES: u64 = 4096;

/// How many lines a window has to hold on each side of a line before the
/// pane can open on that line from it. The picker does not know the pane's
/// height, and the pane puts the line a third of the way down, so this
/// fills a pane up to 450 rows tall. It is under half of
/// [`PREVIEW_WINDOW_LINES`], so a window always holds the line it was
/// requested for.
const PREVIEW_WINDOW_MARGIN: u64 = 300;

/// The 1-based first line of the preview window for a candidate that opens
/// on `line`, from the top for a candidate with none.
fn window_first(line: Option<u64>) -> u64 {
    line.map_or(1, |line| {
        line.saturating_sub(PREVIEW_WINDOW_LINES / 2).max(1)
    })
}

/// Whether the window starting at line `first` can open a pane on `line`
/// with the file's own lines above and below it: the margin on each side
/// lies inside the window, or the window reaches the file's first line on
/// that side. `end` is how many lines the window holds when it is known to
/// end at the file's last line. Below the line, a known end decides alone:
/// a line past it is one the file has gained since, and is not held.
fn window_holds(first: u64, end: Option<u64>, line: u64) -> bool {
    let Some(offset) = line.checked_sub(first) else {
        return false;
    };
    let top = first == 1 || offset >= PREVIEW_WINDOW_MARGIN;
    let bottom = match end {
        Some(len) => offset < len,
        None => offset + PREVIEW_WINDOW_MARGIN < PREVIEW_WINDOW_LINES,
    };
    top && bottom
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
            preview_first: 1,
            preview_for: (0, None),
            preview_lines: Arc::from([]),
            applied_path: None,
            applied_first: 1,
            shown_line: None,
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

    /// Applies one key-notation edit to the query: a key that types a
    /// character inserts it, `<BS>` deletes the last character, anything else
    /// is a no-op edit. Bumps and returns the new generation regardless --
    /// the caller decides which keys reach this at all (see `update()`'s
    /// picker key routing), so by the time a call lands here the edit is
    /// always meant to be attempted.
    pub fn edit_query(&mut self, notation: &str) -> u64 {
        if notation == "<BS>" {
            self.query.pop();
        } else if let Some(c) = crate::native::keys::notation_char(notation) {
            self.query.push(c);
        }
        self.requery()
    }

    /// Inserts one pasted `text` into the query as a single edit: trailing
    /// whitespace dropped, and every control character left inside it
    /// standing in as the space it would paint as.
    ///
    /// A query is one line and the filter matches against one. Dropping the
    /// interior line breaks out of a pasted list instead would run its last
    /// word into the next one's first and filter for a needle that was
    /// never on the clipboard, while keeping the trailing one would search
    /// for `"needle "`: a copied line arrives with its newline, and live
    /// grep matches the needle literally (`view-native`'s
    /// `escape_literal`), so that trailing space is the difference between
    /// every match and none.
    pub fn paste_query(&mut self, text: &str) -> u64 {
        self.query.extend(
            text.trim_end()
                .chars()
                .map(|c| if c.is_control() { ' ' } else { c }),
        );
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

    /// Moves the selection `delta` rows, held between the first result and
    /// the last. Returns whether the selection changed, so a move at either
    /// end asks for no preview.
    pub fn move_selection(&mut self, delta: isize) -> bool {
        let last = self.items.len().saturating_sub(1);
        let next = self.selected.saturating_add_signed(delta).min(last);
        let moved = next != self.selected;
        self.selected = next;
        moved
    }

    /// What opening the selected candidate reaches, or `None` with no
    /// results. A `Buffers` candidate names its buffer by the name nvim
    /// gave it, empty for an unnamed one.
    #[must_use]
    pub fn selected_target(&self) -> Option<Picked> {
        let item = self.items.get(self.selected)?;
        let buffer = matches!(self.source, Source::Buffers);
        let name = if buffer {
            self.selected_path().unwrap_or_default()
        } else {
            self.selected_path()?
        };
        Some(Picked {
            name,
            line: item.line,
            buffer,
        })
    }

    /// Allocates a fresh preview generation for the currently selected
    /// candidate and records it as this session's outstanding preview
    /// request, or does nothing (returning `None`) when there is no
    /// selection to preview, *or* when `preview_path` already names this
    /// same candidate and the window requested for it holds the line the
    /// candidate opens on with a pane's worth of lines on each side
    /// (`window_holds`). A line nearer a window edge than that is read
    /// again with its own window, and its mark waits for that read, so the
    /// pane never opens as if the file began or ended at the window's edge.
    /// `preview_path` is set the instant a request is
    /// issued (not only once its reply lands -- see the field's own doc),
    /// so this one comparison covers both "the preview already shown is
    /// this path" and "a request for this path is already in flight":
    /// without it, a streamed result batch that reorders rows without
    /// changing the selected candidate (a `LiveGrep` query commonly
    /// resolves several rows to the same file, differing only by line) would
    /// re-issue a preview request for a path already current, storming the
    /// RPC channel with redundant reads of a buffer that has not changed.
    /// The caller issues the actual
    /// `Effect::Rpc(RpcCall::PreviewBufferWindow)` with the returned pair
    /// and [`Self::preview_first_line`] -- this method only allocates the
    /// generation, mirroring `PickerState::open`'s own "allocate, do not
    /// itself emit an effect" contract.
    pub fn refresh_preview(&mut self) -> Option<(u64, String)> {
        let path = self.selected_path()?;
        let line = self.items.get(self.selected).and_then(|item| item.line);
        let requested_is_applied =
            self.applied_path == self.preview_path && self.applied_first == self.preview_first;
        let end = self.applied_end().filter(|_| requested_is_applied);
        if let Some(marked) = self.marked_line() {
            self.shown_line = Some(marked);
        }
        if self.preview_path.as_deref() == Some(path.as_str())
            && (window_holds(self.preview_first, end, line.unwrap_or(1))
                || self.preview_for == (self.generation, line))
        {
            return None;
        }
        let generation = next_generation();
        self.preview_generation = generation;
        self.preview_path = Some(path.clone());
        self.preview_first = window_first(line);
        self.preview_for = (self.generation, line);
        Some((generation, path))
    }

    /// The 1-based first line of the outstanding preview request's window,
    /// [`PREVIEW_WINDOW_LINES`] long.
    #[must_use]
    pub fn preview_first_line(&self) -> u64 {
        self.preview_first
    }

    /// How many lines the applied window holds when it ends at the file's
    /// last line: the read returned fewer lines than a window holds.
    fn applied_end(&self) -> Option<u64> {
        u64::try_from(self.preview_lines.len())
            .ok()
            .filter(|len| *len < PREVIEW_WINDOW_LINES)
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
        self.preview_lines = lines.into();
        self.applied_path.clone_from(&self.preview_path);
        self.applied_first = self.preview_first;
        self.shown_line = self.marked_line();
    }

    /// The selected candidate's line when the applied window marks it: the
    /// window is from the selection's own file and holds that line with a
    /// pane's worth of lines on each side.
    fn marked_line(&self) -> Option<u64> {
        let shows_selected_file =
            self.applied_path.is_some() && self.applied_path == self.selected_path();
        self.items
            .get(self.selected)
            .and_then(|item| item.line)
            .filter(|line| {
                shows_selected_file && window_holds(self.applied_first, self.applied_end(), *line)
            })
    }

    /// The 0-based index into the applied window of 1-based `line`.
    fn applied_index(&self, line: u64) -> Option<usize> {
        line.checked_sub(self.applied_first)
            .and_then(|index| usize::try_from(index).ok())
    }

    /// This session's paint-facing projection: the query line, the
    /// candidate rows with their matched substrings carrying
    /// [`StyleRole::Match`], which row is highlighted, and the line the
    /// selected candidate's preview opens on.
    #[must_use]
    pub fn view(&self) -> PickerView {
        let title = match &self.source {
            Source::Files { .. } => "Files",
            Source::Buffers => "Buffers",
            Source::LiveGrep { .. } => "Live Grep",
        };
        let rows = self.items.iter().map(item_spans).collect();
        let marked = self.marked_line();
        let selected_line = self.items.get(self.selected).and_then(|item| item.line);
        let line_in_flight =
            marked.is_none() && selected_line.is_some() && self.applied_path.is_some();
        let mut view = PickerView::new(title)
            .with_query(self.query.clone())
            .with_span_rows(rows)
            .with_preview(Arc::clone(&self.preview_lines))
            .with_preview_line(marked.and_then(|line| self.applied_index(line)))
            .with_preview_anchor(
                self.shown_line
                    .filter(|_| line_in_flight)
                    .and_then(|line| self.applied_index(line)),
            );
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
    fn edit_query_types_the_character_a_named_key_spells() {
        let mut state = PickerState::open(Source::Buffers);
        state.edit_query("<lt>");
        state.edit_query("<Space>");
        assert_eq!(state.query(), "< ");
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
    fn paste_query_spaces_out_the_breaks_inside_the_text_and_drops_the_last() {
        let mut state = PickerState::open(Source::Buffers);
        state.edit_query("s");
        let before = state.generation();
        let after = state.paste_query("rc/main.rs\nsrc/lib.rs\n");

        assert_eq!(
            state.query(),
            "src/main.rs src/lib.rs",
            "the paste lands whole, each interior break standing in as a \
             space -- and the copied line's own newline is not a needle \
             live grep would search for literally"
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
        // a relative path containing its own ':': the old
        // `item.label.split(':').next()` resolved this to "src/mod",
        // previewing (or opening) the wrong path entirely
        let mut state = PickerState::open(Source::LiveGrep {
            root: PathBuf::from("/repo"),
        });
        let gen = state.generation();
        let item = PickerItem::grep_match("src/mod:weird.rs", 3, "let x = 1;");
        state.apply_results(gen, vec![item]);
        // the separator between root and match is the platform's, and a
        // hardcoded '/' asserts a Unix rendering on a Windows host that
        // never produced one; the ':' this test is about is a path
        // character on Unix and the drive separator on Windows, where
        // splitting a label at it resolves `C:\repo\src\main.rs` to `C`.
        // Building `expected` with the same join production uses makes the
        // separator half of this assertion unfalsifiable by construction;
        // the ':' surviving intact is the half under test
        let expected = PathBuf::from("/repo").join("src/mod:weird.rs");
        assert_eq!(
            state.selected_path().as_deref(),
            Some(expected.to_string_lossy().as_ref()),
            "a live-grep path containing ':' must resolve to the real file, \
             not the text before the first ':' in the display label"
        );
    }

    /// A picker over `items` whose preview reply has landed for the first
    /// item: the window it asked for of a file of `len` numbered lines.
    fn previewed(items: Vec<PickerItem>, len: u64) -> PickerView {
        let mut state = PickerState::open(Source::LiveGrep {
            root: PathBuf::from("/repo"),
        });
        let gen = state.generation();
        state.apply_results(gen, items);
        let (preview_gen, _) = state.refresh_preview().expect("a selection");
        let first = state.preview_first_line();
        let last = (first + PREVIEW_WINDOW_LINES - 1).min(len);
        let lines = (first..=last).map(|n| format!("line {n}")).collect();
        state.apply_preview(preview_gen, lines);
        state.view()
    }

    const PANE_ROWS: usize = 30;

    #[test]
    fn a_grep_match_previews_its_own_line_a_third_of_the_way_down() {
        let view = previewed(
            vec![PickerItem::grep_match("src/a.rs", 1089, "BUFFER")],
            1200,
        );
        let (window, marked) = view.preview_window(PANE_ROWS);
        let marked = marked.expect("the match line is marked");
        assert_eq!(window[marked], "line 1089");
        assert_eq!(marked, PANE_ROWS / 3, "context above the match");
    }

    #[test]
    fn a_match_near_the_top_clamps_to_the_first_line() {
        let view = previewed(vec![PickerItem::grep_match("src/a.rs", 2, "x")], 1200);
        let (window, marked) = view.preview_window(PANE_ROWS);
        assert_eq!(window[0], "line 1");
        assert_eq!(marked, Some(1));
    }

    #[test]
    fn a_match_on_the_last_line_clamps_to_the_bottom() {
        let view = previewed(vec![PickerItem::grep_match("src/a.rs", 1200, "x")], 1200);
        let (window, marked) = view.preview_window(PANE_ROWS);
        assert_eq!(window.len(), PANE_ROWS, "the pane is filled to the end");
        assert_eq!(marked, Some(PANE_ROWS - 1));
        assert_eq!(window[PANE_ROWS - 1], "line 1200");
    }

    #[test]
    fn an_item_with_no_line_previews_from_the_top() {
        let item = PickerItem {
            path: Some("src/a.rs".to_string()),
            ..PickerItem::new("src/a.rs")
        };
        let view = previewed(vec![item], 1200);
        let (window, marked) = view.preview_window(PANE_ROWS);
        assert_eq!(window[0], "line 1");
        assert_eq!(marked, None);
    }

    fn numbered(len: usize) -> Vec<String> {
        (1..=len).map(|n| format!("line {n}")).collect()
    }

    /// A picker whose first result, in `a.rs`, has its preview applied and
    /// whose next result set selects a match in `b.rs`, its preview
    /// requested and not yet answered.
    fn moved_to_another_file() -> (PickerState, u64, u64) {
        let mut state = PickerState::open(Source::LiveGrep {
            root: PathBuf::from("/repo"),
        });
        let gen = state.generation();
        state.apply_results(gen, vec![PickerItem::grep_match("a.rs", 40, "x")]);
        let (first, _) = state.refresh_preview().expect("a selection");
        state.apply_preview(first, numbered(100));
        let gen = state.edit_query("x");
        state.apply_results(gen, vec![PickerItem::grep_match("b.rs", 70, "x")]);
        let (second, path) = state.refresh_preview().expect("a new file to preview");
        assert!(path.ends_with("b.rs"), "{path}");
        (state, first, second)
    }

    #[test]
    fn a_preview_in_flight_for_another_file_marks_no_line() {
        let (state, _, _) = moved_to_another_file();
        assert_eq!(state.view().preview_line, None);
    }

    #[test]
    fn while_another_files_window_is_read_the_pane_keeps_its_rows() {
        let (mut state, _) = held_window_then_moved(5000, 5000);
        let gen = state.edit_query("y");
        state.apply_results(gen, vec![PickerItem::grep_match("b.rs", 20, "x")]);
        assert!(state.refresh_preview().is_some(), "b.rs is requested");
        let view = state.view();
        let (window, marked) = view.preview_window(PANE_ROWS);
        assert_eq!(marked, None, "no mark meanwhile");
        assert_eq!(window[PANE_ROWS / 3], "line 5000", "the rows shown before");
    }

    #[test]
    fn a_line_past_a_files_end_is_read_once_per_query() {
        let mut state = PickerState::open(Source::LiveGrep {
            root: PathBuf::from("/repo"),
        });
        let gen = state.generation();
        let hit = || vec![PickerItem::grep_match("a.rs", 60, "x")];
        state.apply_results(gen, hit());
        let (first, _) = state.refresh_preview().expect("a selection");
        state.apply_preview(first, numbered(50));
        state.apply_results(gen, hit());
        assert_eq!(state.refresh_preview(), None, "the same query holds it");
        let gen = state.edit_query("x");
        state.apply_results(gen, hit());
        assert!(state.refresh_preview().is_some(), "a new query reads it");
    }

    #[test]
    fn the_mark_returns_when_the_selected_files_preview_lands() {
        let (mut state, _, second) = moved_to_another_file();
        state.apply_preview(second, numbered(100));
        assert_eq!(state.view().preview_line, Some(69));
    }

    #[test]
    fn a_stale_reply_does_not_mark_a_line() {
        let (mut state, first, _) = moved_to_another_file();
        state.apply_preview(first, numbered(100));
        assert_eq!(state.view().preview_line, None);
    }

    #[test]
    fn moving_between_matches_in_one_file_keeps_the_mark() {
        let mut state = PickerState::open(Source::LiveGrep {
            root: PathBuf::from("/repo"),
        });
        let gen = state.generation();
        state.apply_results(gen, vec![PickerItem::grep_match("a.rs", 40, "x")]);
        let (first, _) = state.refresh_preview().expect("a selection");
        state.apply_preview(first, numbered(100));
        let gen = state.edit_query("x");
        state.apply_results(gen, vec![PickerItem::grep_match("a.rs", 90, "x")]);
        assert_eq!(state.refresh_preview(), None, "no new request");
        assert_eq!(state.view().preview_line, Some(89));
    }

    #[test]
    fn a_window_read_from_past_the_top_marks_the_matched_row() {
        let mut state = PickerState::open(Source::LiveGrep {
            root: PathBuf::from("/repo"),
        });
        let gen = state.generation();
        state.apply_results(gen, vec![PickerItem::grep_match("a.rs", 5000, "x")]);
        let (preview_gen, _) = state.refresh_preview().expect("a selection");
        let first = state.preview_first_line();
        assert_eq!(first, 5000 - PREVIEW_WINDOW_LINES / 2);
        let lines = (first..first + PREVIEW_WINDOW_LINES)
            .map(|n| format!("line {n}"))
            .collect();
        state.apply_preview(preview_gen, lines);
        let view = state.view();
        let (window, marked) = view.preview_window(PANE_ROWS);
        assert_eq!(window[marked.expect("the match is marked")], "line 5000");
    }

    #[test]
    fn a_same_file_match_past_the_window_reads_its_own_window() {
        let mut state = PickerState::open(Source::LiveGrep {
            root: PathBuf::from("/repo"),
        });
        let gen = state.generation();
        state.apply_results(gen, vec![PickerItem::grep_match("a.rs", 40, "x")]);
        let (first, _) = state.refresh_preview().expect("a selection");
        state.apply_preview(first, numbered(100));
        let gen = state.edit_query("x");
        state.apply_results(gen, vec![PickerItem::grep_match("a.rs", 3000, "x")]);
        assert!(state.refresh_preview().is_some(), "a new window is read");
        assert_eq!(state.preview_first_line(), 3000 - PREVIEW_WINDOW_LINES / 2);
        let view = state.view();
        assert_eq!(view.preview_window(PANE_ROWS).1, None, "no mark meanwhile");
    }

    /// A picker whose match at `line` of a 10000-line `a.rs` has its whole
    /// window applied, with the selection then moved to `to` in that file.
    /// Returns the state and whatever request the move issued.
    fn held_window_then_moved(line: u64, to: u64) -> (PickerState, Option<(u64, String)>) {
        let mut state = PickerState::open(Source::LiveGrep {
            root: PathBuf::from("/repo"),
        });
        let gen = state.generation();
        state.apply_results(gen, vec![PickerItem::grep_match("a.rs", line, "x")]);
        let (preview_gen, _) = state.refresh_preview().expect("a selection");
        let first = state.preview_first_line();
        let lines = (first..first + PREVIEW_WINDOW_LINES)
            .map(|n| format!("line {n}"))
            .collect();
        state.apply_preview(preview_gen, lines);
        let gen = state.edit_query("x");
        state.apply_results(gen, vec![PickerItem::grep_match("a.rs", to, "x")]);
        let request = state.refresh_preview();
        (state, request)
    }

    /// Lands the window the outstanding request asked for, from a
    /// 10000-line file.
    fn land_requested_window(state: &mut PickerState) {
        let first = state.preview_first_line();
        let lines = (first..first + PREVIEW_WINDOW_LINES)
            .map(|n| format!("line {n}"))
            .collect();
        state.apply_preview(state.preview_generation(), lines);
    }

    #[test]
    fn a_move_near_the_top_of_the_held_window_reads_a_new_one() {
        let (mut state, request) = held_window_then_moved(5000, 4503);
        assert!(request.is_some(), "a new window is requested");
        assert_eq!(state.view().preview_line, None, "no mark meanwhile");
        land_requested_window(&mut state);
        let view = state.view();
        let (window, marked) = view.preview_window(PANE_ROWS);
        let marked = marked.expect("the match is marked");
        assert_eq!(window[marked], "line 4503");
        assert_eq!(marked, PANE_ROWS / 3, "context above the match");
    }

    #[test]
    fn a_move_near_the_bottom_of_the_held_window_reads_a_new_one() {
        let (mut state, request) = held_window_then_moved(5000, 5495);
        assert!(request.is_some(), "a new window is requested");
        assert_eq!(state.view().preview_line, None, "no mark meanwhile");
        land_requested_window(&mut state);
        let view = state.view();
        let (window, marked) = view.preview_window(PANE_ROWS);
        let marked = marked.expect("the match is marked");
        assert_eq!(window[marked], "line 5495");
        assert_eq!(window.len().min(PANE_ROWS), PANE_ROWS, "context below");
        assert_eq!(marked, PANE_ROWS / 3);
    }

    #[test]
    fn a_move_well_inside_the_held_window_marks_at_once() {
        let (state, request) = held_window_then_moved(5000, 5100);
        assert_eq!(request, None, "no new request");
        let view = state.view();
        let (window, marked) = view.preview_window(PANE_ROWS);
        assert_eq!(window[marked.expect("marked at once")], "line 5100");
    }

    #[test]
    fn a_line_past_a_short_files_known_end_reads_the_file_again() {
        let mut state = PickerState::open(Source::LiveGrep {
            root: PathBuf::from("/repo"),
        });
        let gen = state.generation();
        state.apply_results(gen, vec![PickerItem::grep_match("a.rs", 40, "x")]);
        let (first, _) = state.refresh_preview().expect("a selection");
        state.apply_preview(first, numbered(50));
        let gen = state.edit_query("x");
        state.apply_results(gen, vec![PickerItem::grep_match("a.rs", 60, "x")]);
        assert!(state.refresh_preview().is_some(), "the grown file is read");
        state.apply_preview(state.preview_generation(), numbered(80));
        assert_eq!(state.view().preview_line, Some(59));
    }

    #[test]
    fn while_a_same_file_window_is_read_the_pane_keeps_its_rows() {
        let (state, request) = held_window_then_moved(5000, 5495);
        assert!(request.is_some(), "a new window is requested");
        let view = state.view();
        let (window, marked) = view.preview_window(PANE_ROWS);
        assert_eq!(marked, None, "no mark meanwhile");
        assert_eq!(window[PANE_ROWS / 3], "line 5000", "the rows shown before");
    }

    #[test]
    fn the_rows_kept_are_the_last_ones_marked_in_the_held_window() {
        let (mut state, request) = held_window_then_moved(5000, 5100);
        assert_eq!(request, None, "held");
        let gen = state.edit_query("x");
        state.apply_results(gen, vec![PickerItem::grep_match("a.rs", 5495, "x")]);
        assert!(
            state.refresh_preview().is_some(),
            "a new window is requested"
        );
        let view = state.view();
        let (window, marked) = view.preview_window(PANE_ROWS);
        assert_eq!(marked, None, "no mark meanwhile");
        assert_eq!(window[PANE_ROWS / 3], "line 5100", "the rows shown before");
    }

    #[test]
    fn a_held_window_from_the_first_line_marks_line_two_at_once() {
        let (state, request) = held_window_then_moved(3, 2);
        assert_eq!(request, None, "no new request");
        assert_eq!(state.view().preview_line, Some(1));
    }
}
