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

use crate::model::{CmdlineState, MessageEntry, PopupmenuState};
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
            "{}{}",
            self.cmdline.firstc,
            typed_text(&self.cmdline.content)
        )
    }

    #[must_use]
    pub fn view(&self) -> PaletteView {
        let rows = self.completion.as_ref().map_or_else(Vec::new, |pm| {
            pm.items
                .iter()
                .map(|item| PaletteRow::new(item.display_text()))
                .collect()
        });
        let view = PaletteView::new(title_for(&self.cmdline.firstc))
            .with_query(self.query())
            .with_rows(rows);
        match self
            .completion
            .as_ref()
            .and_then(|pm| usize::try_from(pm.selected).ok())
        {
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

/// A snapshot of `ToastHistory` at the moment the message-history view was
/// opened: a `:messages`-style browse of what already happened, not a live
/// window onto the ring. Taken once, like `PickerState` snapshots its
/// result set, so entries scrolled past do not shift under a user's cursor
/// as new messages keep arriving underneath the open overlay.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq)]
pub struct MessageHistoryState {
    entries: Vec<MessageEntry>,
}

impl MessageHistoryState {
    #[must_use]
    pub fn snapshot(history: &ToastHistory) -> Self {
        Self {
            entries: history.entries().cloned().collect(),
        }
    }

    #[must_use]
    pub fn view(&self) -> PaletteView {
        let rows = self
            .entries
            .iter()
            .map(|entry| PaletteRow::new(entry_text(entry)))
            .collect();
        PaletteView::new("Messages").with_rows(rows)
    }
}

/// One history entry's display text: its `content` chunks, joined the same
/// way `typed_text` joins a cmdline's -- nothing downstream repaints a
/// history row's internal highlighting either.
fn entry_text(entry: &MessageEntry) -> String {
    entry
        .content
        .iter()
        .map(|(_, text)| text.as_str())
        .collect()
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::events::PmItem;
    use crate::model::Messages;

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
        // MessageEntry has no public constructor -- built through a real
        // Messages::push, the same way toast.rs's own tests do.
        let mut messages = Messages::default();
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
        assert_eq!(labels.len(), 2, "the snapshot must not see the later push");
        assert!(labels.iter().any(|l| l.contains("first")));
        assert!(labels.iter().any(|l| l.contains("second")));
    }
}
