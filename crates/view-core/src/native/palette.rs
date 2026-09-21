//! Presentation state for the command palette: the already-decoded
//! `CmdlineState`, paired with a `PopupmenuState` only when the wire has
//! told the difference apart from a buffer-anchored completion (see
//! `docs/palette-popupmenu-source-wire-capture.md`).
//!
//! No decoding happens here. `view-engine` decodes `ext_cmdline` into
//! `CmdlineState` and `ext_popupmenu` into `PopupmenuState` exactly once,
//! and `update()` is the only place either gets built; this module reads
//! them back and turns them into a `PaletteView`, the same
//! decode-once-present-many split every other native feature (the prompt,
//! the picker, the tree) already follows. A second decode path here would
//! be a second interpretation of the same wire traffic, free to drift from
//! the first the moment either one changes.

use crate::model::{format_at, CmdlineState, MessageEntry, PopupmenuState};
use crate::native::toast::ToastHistory;
use crate::native::views::{PaletteRow, PaletteView};

/// The command palette's state while nvim's command line is open: the
/// typed line, plus its completion candidates when the open popup menu is
/// cmdline-sourced. A buffer-anchored completion (insert-mode keyword
/// completion, LSP completion, ...) is never carried here -- the caller
/// that builds this (`view-surface::render`) hands over `None` for that
/// case, and paints the buffer completion through the ordinary
/// `LayerKind::Popupmenu` layer at the cursor instead.
#[non_exhaustive]
pub struct PaletteState {
    cmdline: CmdlineState,
    completion: Option<PopupmenuState>,
}

impl PaletteState {
    /// `completion` must already be filtered to the cmdline-sourced case
    /// (`PopupmenuState::is_cmdline_sourced`) -- this type does not check
    /// it again, since the one caller that builds it (`render()`) has
    /// already made that routing decision to decide whether to build a
    /// `PaletteState` at all.
    #[must_use]
    pub fn new(cmdline: CmdlineState, completion: Option<PopupmenuState>) -> Self {
        Self {
            cmdline,
            completion,
        }
    }

    /// The typed line, `firstc` (`:`, `/`, `?`, `=`) prepended: the same
    /// prefix nvim's own bottom-line cmdline always showed, kept here so
    /// switching the command line's rendering into a floating box costs a
    /// user nothing they used to read at a glance.
    #[must_use]
    pub fn query(&self) -> String {
        format!(
            "{}{}{}",
            self.cmdline.firstc,
            self.cmdline.prompt,
            typed_text(&self.cmdline.content)
        )
    }

    #[must_use]
    pub fn view(&self) -> PaletteView {
        let rows = match &self.completion {
            Some(pm) => pm
                .items
                .iter()
                .map(|item| PaletteRow::new(item.display_text()))
                .collect(),
            None => Vec::new(),
        };
        let view = PaletteView::new(title_for(&self.cmdline.firstc))
            .with_query(self.query())
            .with_rows(rows);
        // the engine's `selected` is a signed sentinel (-1 for "nothing
        // selected")
        let selected = self
            .completion
            .as_ref()
            .and_then(|pm| usize::try_from(pm.selected).ok());
        match selected {
            Some(index) => view.with_selected(index),
            None => view,
        }
    }
}

/// `content`'s typed text, nvim's own highlight-id-per-chunk pairing
/// dropped: nothing downstream of the palette paints per-character
/// highlighting inside the query line, so only the text survives.
fn typed_text(content: &[(u64, String)]) -> String {
    content.iter().map(|(_, text)| text.as_str()).collect()
}

/// A human title for the kind of command line `firstc` names. Every other
/// value nvim can send (`>`, the debug-mode prompt) falls back to a generic
/// title rather than growing this match for a case no capture has pinned.
fn title_for(firstc: &str) -> &'static str {
    match firstc {
        ":" => "Command",
        "/" => "Search",
        "?" => "Search (backward)",
        "=" => "Expression",
        _ => "Command Line",
    }
}

/// The title the message-history overlay is drawn under, and the one thing
/// on screen that says it is open rather than that a key aimed at it went
/// somewhere else.
pub const MESSAGE_HISTORY_TITLE: &str = "Messages";

/// The entries of `ToastHistory` as the message-history view is showing
/// them: a `:messages`-style browse, kept level with the ring by
/// [`Self::refresh`] so a notice raised while the overlay is open is in
/// the list the user is looking at rather than waiting for the next open.
/// The selection is carried across a refresh by the entry it is on, not by
/// its row, since the ring reads newest-first and a new entry lands above
/// every row already drawn.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq)]
pub struct MessageHistoryState {
    entries: Vec<MessageEntry>,
    /// Which entry the overlay's own keys act on. An index rather than a
    /// scroll offset because the one thing that scrolls this overlay is
    /// `overlay::lay_out`, which windows a body's items around its
    /// `selected` row already -- a second offset here would be a second
    /// opinion about which rows are on screen.
    selected: usize,

    /// The ring's push count at the last read, which is how a refresh
    /// costs two integers on a message that added nothing.
    read: usize,

    /// Whether the last key was the first `g` of a `gg`.
    ///
    /// Held here rather than in the router's shared `pending_chord`, which
    /// belongs to the sidebars' configurable bindings and is dropped for
    /// every other overlay on the way in. One overlay-local prefix is the
    /// whole of what this needs, and it dies with the snapshot.
    pending_g: bool,
}

impl MessageHistoryState {
    #[must_use]
    pub fn snapshot(history: &ToastHistory) -> Self {
        Self {
            entries: history.entries().cloned().collect(),
            selected: 0,
            read: history.pushed(),
            pending_g: false,
        }
    }

    /// Re-reads `history` into the open overlay, reporting whether
    /// anything changed (the caller's cue to repaint).
    ///
    /// Entries that arrived since the last read are prepended, so the
    /// selection moves down by as many to stay on the entry it was on.
    pub fn refresh(&mut self, history: &ToastHistory) -> bool {
        if history.pushed() == self.read {
            return false;
        }
        let arrived = history.pushed() - self.read;
        self.read = history.pushed();
        let entries: Vec<MessageEntry> = history.entries().cloned().collect();
        self.entries = entries;
        let last = self.entries.len().saturating_sub(1);
        self.selected = self.selected.saturating_add(arrived).min(last);
        true
    }

    /// Arms the `g` prefix, so the next `g` is a `gg`.
    pub fn arm_g(&mut self) {
        self.pending_g = true;
    }

    /// Whether a `g` was pending, clearing it either way: a keystroke
    /// spends the prefix whatever the key turns out to be, so `gj` moves
    /// down by one rather than leaving a `g` armed behind it.
    pub fn take_g(&mut self) -> bool {
        std::mem::take(&mut self.pending_g)
    }

    #[must_use]
    pub fn view(&self) -> PaletteView {
        let rows: Vec<PaletteRow> = self
            .entries
            .iter()
            .map(|entry| PaletteRow::new(entry_label(entry)))
            .collect();
        let view = PaletteView::new(MESSAGE_HISTORY_TITLE).with_rows(rows);
        if self.entries.is_empty() {
            return view;
        }
        view.with_selected(self.selected)
    }

    /// Moves the selection to `index`, clamped to the last entry, and
    /// reports whether it actually moved (the caller's cue to repaint).
    /// An empty snapshot has nothing to select and never moves.
    #[must_use]
    pub fn select(&mut self, index: usize) -> bool {
        let Some(last) = self.entries.len().checked_sub(1) else {
            return false;
        };
        let next = index.min(last);
        let moved = next != self.selected;
        self.selected = next;
        moved
    }

    /// [`Self::select`] relative to where the selection already is, saturating
    /// at both ends rather than wrapping: a `j` at the bottom of the history
    /// stays at the bottom, the same as it does in a buffer.
    #[must_use]
    pub fn move_selection(&mut self, delta: isize) -> bool {
        let step = delta.unsigned_abs();
        let target = if delta < 0 {
            self.selected.saturating_sub(step)
        } else {
            self.selected.saturating_add(step)
        };
        self.select(target)
    }

    /// The selected entry's text exactly as the overlay draws it, or `None`
    /// for an empty snapshot.
    ///
    /// Verbatim on purpose, and the reason this overlay has a copy key at
    /// all: the notices worth copying name paths, and a path with a space
    /// in it survives no trimming, quoting or "copied 1 line" rewording.
    #[must_use]
    pub fn selected_text(&self) -> Option<String> {
        self.entries.get(self.selected).map(entry_text)
    }

    /// The notice family the selected entry was recorded under, or `None`
    /// when it carries none -- every wire message, and every native notice
    /// raised without a family (see [`MessageEntry::family`]).
    #[must_use]
    pub fn selected_family(&self) -> Option<&str> {
        self.entries
            .get(self.selected)
            .and_then(MessageEntry::family)
    }
}

/// One history entry's text, verbatim: its `content` chunks joined the same
/// way `typed_text` joins a cmdline's -- nothing downstream repaints a
/// history row's internal highlighting either. What [`Self::selected_text`]
/// hands the copy key, byte for byte; see [`entry_label`] for the row the
/// overlay actually draws.
fn entry_text(entry: &MessageEntry) -> String {
    entry
        .content
        .iter()
        .map(|(_, text)| text.as_str())
        .collect()
}

/// One history row's full label: `entry_text` with its timestamp in front,
/// which is what [`MessageHistoryState::view`] draws and nothing else
/// reads -- a copy takes [`entry_text`] alone, so a path or a message
/// containing today's date is never mistaken for one this label
/// prepended.
fn entry_label(entry: &MessageEntry) -> String {
    format!("{} {}", format_at(entry.at()), entry_text(entry))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::events::PmItem;
    use crate::model::Messages;
    use crate::native::toast::DEFAULT_CAPACITY;

    fn cmdline(firstc: &str, typed: &str) -> CmdlineState {
        CmdlineState {
            content: if typed.is_empty() {
                Vec::new()
            } else {
                vec![(0, typed.to_string())]
            },
            pos: typed.chars().count() as u64,
            firstc: firstc.to_string(),
            prompt: String::new(),
            indent: 0,
            level: 1,
        }
    }

    fn popupmenu(
        items: Vec<PmItem>,
        selected: i64,
        row: u64,
        col: u64,
        grid: i64,
    ) -> PopupmenuState {
        PopupmenuState {
            items,
            selected,
            row,
            col,
            grid,
        }
    }

    fn message_entry(text: &str) -> MessageEntry {
        message_entry_at(text, std::time::SystemTime::UNIX_EPOCH)
    }

    fn message_entry_at(text: &str, at: std::time::SystemTime) -> MessageEntry {
        // MessageEntry has no public constructor -- built through a real
        // Messages::push, the same way toast.rs's own tests do.
        let mut messages = Messages::default();
        messages.set_now(at);
        messages.push("native".to_string(), vec![(0, text.to_string())], false);
        messages.entries.into_iter().next().expect("just pushed")
    }

    #[test]
    fn a_bare_colon_renders_an_empty_command_query() {
        let state = PaletteState::new(cmdline(":", ""), None);
        let view = state.view();
        assert_eq!(view.title, "Command");
        assert_eq!(view.query, ":");
        assert!(view.rows.is_empty());
        assert_eq!(view.selected, None);
    }

    #[test]
    fn a_typed_command_keeps_its_firstc_prefix_in_the_query() {
        let state = PaletteState::new(cmdline(":", "set nu"), None);
        assert_eq!(state.view().query, ":set nu");
    }

    /// A `:call input("New file: ")`-style prompt carries its label in
    /// `cmdline.prompt`, not `firstc` (which is empty for this shape) --
    /// `query` must show it, matching the prefix width
    /// `cmdline_cursor_col` (in `view-surface`) already counts against.
    #[test]
    fn a_prompt_labeled_cmdline_shows_its_label_before_the_typed_text() {
        let state = PaletteState::new(
            CmdlineState {
                content: vec![(0, "foo".to_string())],
                pos: 3,
                firstc: String::new(),
                prompt: "New file: ".to_string(),
                indent: 0,
                level: 1,
            },
            None,
        );
        assert_eq!(state.view().query, "New file: foo");
    }

    #[test]
    fn a_search_prompt_titles_itself_search() {
        let state = PaletteState::new(cmdline("/", "needle"), None);
        assert_eq!(state.view().title, "Search");
    }

    #[test]
    fn cmdline_sourced_completion_items_become_palette_rows() {
        let completion = popupmenu(
            vec![
                PmItem {
                    word: "number".to_string(),
                    kind: String::new(),
                    menu: String::new(),
                    info: String::new(),
                },
                PmItem {
                    word: "numberwidth".to_string(),
                    kind: String::new(),
                    menu: String::new(),
                    info: String::new(),
                },
            ],
            1,
            0,
            4,
            -1,
        );
        let state = PaletteState::new(cmdline(":", "set nu"), Some(completion));
        let view = state.view();
        assert_eq!(
            view.rows
                .iter()
                .map(|r| r.label.clone())
                .collect::<Vec<_>>(),
            vec!["number".to_string(), "numberwidth".to_string()]
        );
        assert_eq!(view.selected, Some(1));
    }

    #[test]
    fn a_negative_selected_sentinel_carries_no_selection_into_the_view() {
        let completion = popupmenu(
            vec![PmItem {
                word: "number".to_string(),
                kind: String::new(),
                menu: String::new(),
                info: String::new(),
            }],
            -1,
            0,
            4,
            -1,
        );
        let state = PaletteState::new(cmdline(":", "set nu"), Some(completion));
        assert_eq!(state.view().selected, None);
    }

    #[test]
    fn a_history_snapshot_carries_every_entry_at_the_moment_it_was_taken() {
        let mut history = ToastHistory::new();
        history.push(&message_entry("first"));
        history.push(&message_entry("second"));

        let state = MessageHistoryState::snapshot(&history);
        history.push(&message_entry("third, after the snapshot"));

        let view = state.view();
        let labels: Vec<String> = view.rows.iter().map(|r| r.label.clone()).collect();
        assert_eq!(labels.len(), 2, "a snapshot is taken once");
        assert!(labels.iter().any(|l| l.contains("first")));
        assert!(labels.iter().any(|l| l.contains("second")));
    }

    #[test]
    fn the_history_overlay_renders_the_full_timestamp() {
        let stamp =
            std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_000);
        let mut history = ToastHistory::new();
        history.push(&message_entry_at("build finished", stamp));

        let state = MessageHistoryState::snapshot(&history);
        let view = state.view();

        assert_eq!(view.rows.len(), 1);
        assert_eq!(view.rows[0].label, "2023-11-14 22:13:20 build finished");
    }

    #[test]
    fn a_refresh_takes_the_later_push_and_keeps_the_selection_on_its_entry() {
        let mut history = ToastHistory::new();
        history.push(&message_entry("first"));
        history.push(&message_entry("second"));
        let mut state = MessageHistoryState::snapshot(&history);
        assert!(state.select(1));
        assert!(!state.refresh(&history), "an unchanged ring is no repaint");

        history.push(&message_entry("third, after the snapshot"));
        assert!(state.refresh(&history));

        let view = state.view();
        let labels: Vec<String> = view.rows.iter().map(|r| r.label.clone()).collect();
        assert_eq!(labels.len(), 3, "{labels:?}");
        assert!(labels[0].contains("third"), "newest-first: {labels:?}");
        assert_eq!(
            view.selected,
            Some(2),
            "the row above pushed the selected entry down, and the selection went with it"
        );
    }

    #[test]
    fn a_history_refresh_past_capacity_keeps_the_selection_on_its_entry() {
        let mut history = ToastHistory::new();
        for i in 0..DEFAULT_CAPACITY {
            history.push(&message_entry(&format!("entry {i}")));
        }
        let mut state = MessageHistoryState::snapshot(&history);
        assert!(state.select(5));
        let selected_text = state.view().rows[5].label.clone();

        history.push(&message_entry("newest, after the ring is full"));
        assert!(state.refresh(&history));

        let view = state.view();
        let labels: Vec<String> = view.rows.iter().map(|r| r.label.clone()).collect();
        assert_eq!(
            labels[view.selected.expect("an entry is still selected")],
            selected_text,
            "an eviction moves every row by one without changing the count, \
             so the selection has to follow the entry rather than the row"
        );
    }
}
