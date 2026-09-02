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

/// The rows a plugin's own cmdline completion float was drawing, read back
/// off its buffer so view can render them as palette rows instead of
/// letting two menus stack (`update::surface_conflict`'s absorption).
///
/// A second source for the same palette rows, not a second kind of
/// completion: nvim's externalized popup menu never fires for a plugin that
/// draws its own float (`popupmenu_show` needs the engine's own completion),
/// so a session using one has an empty palette and a plugin's menu over it
/// until these rows arrive.
///
/// `lines` are the float buffer's own lines with their padding trimmed at
/// both ends: the capture records them as rendered rows, abbreviation and
/// kind column included (`" preflight      Text   "`), and an absorbing
/// consumer takes the text and re-renders rather than replaying a layout
/// sized for a window it just hid.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
#[non_exhaustive]
pub struct AbsorbedRows {
    /// The rows, top to bottom.
    pub lines: Vec<String>,
    /// Which row the plugin was showing as selected, or `None` when it was
    /// showing none. Read from the float's own window cursor gated on its
    /// `cursorline` option, which is the whole selection carrier for the
    /// captured menu -- its buffer holds no extmarks in any namespace.
    pub selected: Option<usize>,
}

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
    absorbed: Option<AbsorbedRows>,
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
            absorbed: None,
        }
    }

    /// The same palette over rows view took off a plugin's own completion
    /// float rather than off the wire.
    ///
    /// Never both sources at once, and the engine's own is the one that
    /// wins: `popupmenu_show` is the engine saying what it is completing,
    /// while absorbed rows are a rendering read back out of somebody else's
    /// buffer. The caller (`view-surface::render`) picks, and this
    /// constructor is the absorbed half of that choice.
    #[must_use]
    pub fn with_absorbed(cmdline: CmdlineState, absorbed: AbsorbedRows) -> Self {
        Self {
            cmdline,
            completion: None,
            absorbed: Some(absorbed),
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
        let rows = match (&self.completion, &self.absorbed) {
            (Some(pm), _) => pm
                .items
                .iter()
                .map(|item| PaletteRow::new(item.display_text()))
                .collect(),
            (None, Some(absorbed)) => absorbed
                .lines
                .iter()
                .map(|line| PaletteRow::new(line.trim()))
                .collect(),
            (None, None) => Vec::new(),
        };
        let view = PaletteView::new(title_for(&self.cmdline.firstc))
            .with_query(self.query())
            .with_rows(rows);
        // the engine's `selected` is a signed sentinel (-1 for "nothing
        // selected"), the absorbed one an `Option` already decided where the
        // window cursor was read; both land on the same `with_selected`
        let selected = match (&self.completion, &self.absorbed) {
            (Some(pm), _) => usize::try_from(pm.selected).ok(),
            (None, Some(absorbed)) => absorbed.selected,
            (None, None) => None,
        };
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

/// A snapshot of `ToastHistory` at the moment the message-history view was
/// opened: a `:messages`-style browse of what already happened, not a live
/// window onto the ring. Taken once, like `PickerState` snapshots its
/// result set, so entries scrolled past do not shift under a user's cursor
/// as new messages keep arriving underneath the open overlay.
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
            pending_g: false,
        }
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
            .map(|entry| PaletteRow::new(entry_text(entry)))
            .collect();
        let view = PaletteView::new("Messages").with_rows(rows);
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

    /// The rows the wire capture read off nvim-cmp's own cmdline menu
    /// buffer, verbatim padding included, with the selection it expressed as
    /// a window cursor on row 2.
    #[test]
    fn absorbed_rows_render_in_the_palette_with_the_selection_marked() {
        let state = PaletteState::with_absorbed(
            cmdline(":", "pref"),
            AbsorbedRows {
                lines: vec![
                    " preflight      Text   ".to_string(),
                    " prefabricated  Text   ".to_string(),
                ],
                selected: Some(1),
            },
        );
        let view = state.view();
        assert_eq!(
            view.rows
                .iter()
                .map(|r| r.label.clone())
                .collect::<Vec<_>>(),
            vec![
                "preflight      Text".to_string(),
                "prefabricated  Text".to_string()
            ],
            "the rows are re-rendered from the text, not replayed with the \
             padding of a window view just hid"
        );
        assert_eq!(view.selected, Some(1));
        assert_eq!(view.query, ":pref", "and the typed line is still view's");
    }

    /// The engine's own menu is the authority whenever it fires: a session
    /// that has both is a session whose plugin drew a float over a
    /// completion nvim was already externalizing, and re-rendering the
    /// float's buffer would show the same candidates through the worse
    /// source.
    #[test]
    fn a_wire_sourced_completion_outranks_absorbed_rows() {
        let completion = popupmenu(
            vec![PmItem {
                word: "number".to_string(),
                kind: String::new(),
                menu: String::new(),
                info: String::new(),
            }],
            0,
            0,
            4,
            -1,
        );
        let mut state = PaletteState::new(cmdline(":", "set nu"), Some(completion));
        state.absorbed = Some(AbsorbedRows {
            lines: vec!["absorbed".to_string()],
            selected: None,
        });
        let view = state.view();
        assert_eq!(
            view.rows
                .iter()
                .map(|r| r.label.clone())
                .collect::<Vec<_>>(),
            vec!["number".to_string()]
        );
        assert_eq!(view.selected, Some(0));
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
