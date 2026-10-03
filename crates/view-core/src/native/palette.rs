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

use crate::grid::registry::GridId;
use crate::model::{format_at, CmdlineState, MessageEntry, Model, PopupmenuState};
use crate::native::toast::ToastHistory;
use crate::native::views::{PaletteRow, PaletteView, Span, StyleRole};

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
    drawn: Vec<Vec<Span>>,
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
            drawn: Vec::new(),
        }
    }

    /// The same state listing `drawn`, the rows of the window the command
    /// line opened ([`drawn_rows`]).
    #[must_use]
    pub fn with_drawn(self, drawn: Vec<Vec<Span>>) -> Self {
        Self { drawn, ..self }
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

    /// The view the palette paints, taking the drawn rows with it.
    #[must_use]
    pub fn into_view(self) -> PaletteView {
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
            .with_rows(rows)
            .with_drawn(self.drawn);
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

/// The stacking order nvim gives its own insert completion menu
/// (`:help api-win_config`, `zindex`).
///
/// nvim asks floats to stay below it unless they mean to cover its own
/// menus, so a float at or above it is a menu. The completion menus
/// captured on the command line stack at 1001 and 1003, and the
/// notification and progress floats at 50 and 45
/// (`docs/surface-ownership.md`).
pub const COMPLETION_MENU_ZINDEX: u32 = 100;

/// Whether a float at stacking order `zindex`, hanging from `anchor`, is
/// stacked as a completion menu: at or above [`COMPLETION_MENU_ZINDEX`],
/// and anchored on its left edge (`NW` or `SW`).
///
/// A completion menu hangs from the command line's text, so its left edge
/// is the fixed one. A notifier stacked as high hugs a grid corner on the
/// right (`NE` or `SE`), at the top or the bottom.
#[must_use]
pub fn stacks_as_menu(zindex: u32, anchor: crate::native::surfaces::FloatAnchor) -> bool {
    use crate::native::surfaces::FloatAnchor;
    zindex >= COMPLETION_MENU_ZINDEX
        && matches!(anchor, FloatAnchor::NorthWest | FloatAnchor::SouthWest)
}

/// The floats a command line the palette draws has opened, held off the
/// screen so the palette can paint the list among them in its own rows.
///
/// A float placed while that command line is open belongs to it when it is
/// stacked as a completion menu ([`stacks_as_menu`]): the command line is
/// the only thing on screen taking input, so a menu opened then is the
/// command line's own. Any other float (a notification, a progress
/// message) stays where it opened, and so does a float already standing
/// when the line opened. A command line opened inside another (`<C-r>=`)
/// belongs to the outer one, so its floats stay held until the outermost
/// line closes.
#[derive(Debug, Clone, Default)]
pub struct CmdlineFloats {
    /// The level of the outermost command line open, or `None` with none
    /// open.
    level: Option<u64>,
    /// Every grid nvim had named when the command line opened.
    before: Vec<GridId>,
    /// The floats taken since, as `(grid, window)`, in arrival order.
    taken: Vec<(GridId, u64)>,
    /// The windows whose floats were first placed while the line is open.
    arrived: Vec<u64>,
    /// The rows and columns around each window grid's text, as nvim last
    /// reported them (`top, bottom, left, right`): a float's border. Kept
    /// for every grid because nvim sends them before the placement that
    /// decides whether a float is taken.
    margins: Vec<(GridId, [u16; 4])>,
}

impl CmdlineFloats {
    /// Whether a command line is open.
    #[must_use]
    pub fn is_open(&self) -> bool {
        self.level.is_some()
    }

    /// Starts a command line at `level` with `before` already on the wire,
    /// answering the grids still held from one whose close never arrived,
    /// which the caller gives back to the screen.
    #[must_use]
    pub(crate) fn open(&mut self, level: u64, before: Vec<GridId>) -> Vec<GridId> {
        self.level = Some(level);
        self.before = before;
        self.arrived.clear();
        self.taken.drain(..).map(|(grid, _)| grid).collect()
    }

    /// Closes the command line at `level`, answering the grids the session
    /// held once the outermost line closes, and nothing for a nested one.
    #[must_use]
    pub(crate) fn close(&mut self, level: u64) -> Vec<GridId> {
        if self.level.is_some_and(|outer| level > outer) {
            return Vec::new();
        }
        self.level = None;
        self.before.clear();
        self.arrived.clear();
        self.taken.drain(..).map(|(grid, _)| grid).collect()
    }

    /// Forgets everything a replacement engine invalidates, called beside
    /// [`EngineModel::forget_overlays`](crate::model::EngineModel::forget_overlays)
    /// from the restart.
    ///
    /// | field | why |
    /// | --- | --- |
    /// | `level` | the dead engine's command line closed with it and sends no `cmdline_hide` |
    /// | `before`, `taken`, `arrived` | grid ids and window handles, which the replacement numbers again from the start: a held id would hide the replacement's own float and answer for its window |
    /// | `margins` | the borders of the dead engine's windows, which a borderless float on a reused grid id would lose rows and columns to |
    pub fn forget_engine(&mut self) {
        *self = Self::default();
    }

    /// Whether `grid` was named before the command line opened.
    #[must_use]
    pub(crate) fn existed(&self, grid: GridId) -> bool {
        self.before.contains(&grid)
    }

    /// Notes a placement of `grid`, the float of window `win`: one the line
    /// did not find standing when it opened arrived while it is open.
    pub(crate) fn placed(&mut self, grid: GridId, win: u64) {
        if self.is_open() && !self.existed(grid) && !self.arrived.contains(&win) {
            self.arrived.push(win);
        }
    }

    /// Whether window `win`'s float was first placed while the command
    /// line is open.
    #[must_use]
    pub fn arrived(&self, win: u64) -> bool {
        self.arrived.contains(&win)
    }

    /// Takes `grid`, the float of window `win`.
    pub(crate) fn take(&mut self, grid: GridId, win: u64) {
        if !self.holds(grid) {
            self.taken.push((grid, win));
        }
    }

    /// Lets `grid` go, answering whether it was held.
    pub(crate) fn forget(&mut self, grid: GridId) -> bool {
        let held = self.holds(grid);
        self.taken.retain(|(taken, _)| *taken != grid);
        held
    }

    /// Lets `grid` go for good: nvim reuses no grid id once it is closed.
    pub(crate) fn closed(&mut self, grid: GridId) {
        self.forget(grid);
        self.margins.retain(|(known, _)| *known != grid);
    }

    /// Records the margins nvim reported around `grid`'s text.
    pub(crate) fn set_margins(&mut self, grid: GridId, margins: [u16; 4]) {
        match self.margins.iter_mut().find(|(known, _)| *known == grid) {
            Some((_, known)) => *known = margins,
            None => self.margins.push((grid, margins)),
        }
    }

    /// The margins around `grid`'s text, `top, bottom, left, right`, or
    /// none where nvim reported none.
    #[must_use]
    pub fn margins(&self, grid: GridId) -> [u16; 4] {
        self.margins
            .iter()
            .find(|(known, _)| *known == grid)
            .map_or([0; 4], |(_, margins)| *margins)
    }

    /// Whether the command line took `grid`.
    #[must_use]
    pub fn holds(&self, grid: GridId) -> bool {
        self.taken.iter().any(|(taken, _)| *taken == grid)
    }

    /// Whether the command line took window `win`'s float.
    #[must_use]
    pub fn holds_window(&self, win: u64) -> bool {
        self.taken.iter().any(|(_, taken)| *taken == win)
    }
}

/// Whether the palette is drawing an open command line whose floats it
/// takes: the palette is on, it owns the command line and the completion
/// menu, nvim's command line is open, and no prompt box is drawing it.
/// The line counts as open between a nested line's close and the outer
/// line's next `cmdline_show`.
#[must_use]
pub fn takes_cmdline_floats(model: &Model) -> bool {
    model.palette_enabled
        && model.owns(crate::native::ext::Ext::Cmdline)
        && model.cmdline_floats.is_open()
        && crate::native::surfaces::view_draws(crate::native::surfaces::Surface::Popupmenu, model)
        && !matches!(
            model.overlays().last().map(|open| &open.kind),
            Some(crate::model::OverlayKind::Prompt(_))
        )
}

/// The grid whose rows the palette lists, or `None` when it lists nvim's
/// own completion or nothing.
///
/// The tallest float the command line took is the list; the rest (a 1x1
/// scrollbar thumb, a documentation window) stay held and unpainted. The
/// first taken wins a tie. nvim's own cmdline completion wins over all of
/// them: it is the completion state nvim's keys act on, and its rows carry
/// the selection by index.
#[must_use]
pub fn listed_grid(model: &Model) -> Option<GridId> {
    if model
        .engine
        .popupmenu
        .as_ref()
        .is_some_and(PopupmenuState::is_cmdline_sourced)
    {
        return None;
    }
    let grids = model.engine.painted_grids();
    let mut list: Option<(GridId, u16)> = None;
    for (grid, _) in &model.cmdline_floats.taken {
        let Some((_, height)) = grids.grid(*grid).map(crate::grid::Grid::size) else {
            continue;
        };
        if list.is_none_or(|(_, tallest)| height > tallest) {
            list = Some((*grid, height));
        }
    }
    list.map(|(grid, _)| grid)
}

/// The rows of [`listed_grid`] inside its border, one span per run of
/// cells sharing a highlight id, or nothing when no grid is listed.
#[must_use]
pub fn drawn_rows(model: &Model) -> Vec<Vec<Span>> {
    let Some((id, grid)) =
        listed_grid(model).and_then(|id| Some((id, model.engine.painted_grids().grid(id)?)))
    else {
        return Vec::new();
    };
    let (width, height) = grid.size();
    let [top, bottom, left, right] = model.cmdline_floats.margins(id);
    (top..height.saturating_sub(bottom))
        .map(|row| {
            let mut spans: Vec<Span> = Vec::new();
            for col in left..width.saturating_sub(right) {
                let Some(cell) = grid.cell(row, col) else {
                    continue;
                };
                let role = StyleRole::Highlight(cell.hl_id);
                match spans.last_mut() {
                    Some(span) if span.role == role => span.text.push_str(&cell.text),
                    _ => spans.push(Span::new(cell.text.clone(), role)),
                }
            }
            spans
        })
        .collect()
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

    /// The ring's re-wording count at the last read.
    revised: usize,

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
            revised: history.revised(),
            pending_g: false,
        }
    }

    /// Re-reads `history` into the open overlay, reporting whether
    /// anything changed (the caller's cue to repaint).
    ///
    /// Entries that arrived since the last read are prepended, so the
    /// selection moves down by as many to stay on the entry it was on. An
    /// entry re-worded in place keeps its row.
    pub fn refresh(&mut self, history: &ToastHistory) -> bool {
        if history.pushed() == self.read && history.revised() == self.revised {
            return false;
        }
        let arrived = history.pushed() - self.read;
        self.read = history.pushed();
        self.revised = history.revised();
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
    pub fn view(&self, utc_offset_secs: i64) -> PaletteView {
        self.view_for_width(u16::MAX, utc_offset_secs)
    }

    /// The same rows [`Self::view`] builds, with the timestamp shortened to
    /// `HH:MM:SS` below [`NARROW_STREAM_STAMP_WIDTH`] -- the windowed
    /// notification stream and ticker's tile can be narrower than the full
    /// `YYYY-MM-DD HH:MM:SS` stamp leaves room for a message beside, while
    /// the history overlay always has the whole terminal width and keeps
    /// the full stamp by calling [`Self::view`].
    #[must_use]
    pub fn view_for_width(&self, width: u16, utc_offset_secs: i64) -> PaletteView {
        let short = width < NARROW_STREAM_STAMP_WIDTH;
        let rows: Vec<PaletteRow> = self
            .entries
            .iter()
            .map(|entry| PaletteRow::new(entry_label_for(entry, short, utc_offset_secs)))
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
/// hands the copy key, byte for byte; see [`entry_label_for`] for the row
/// the overlay actually draws.
fn entry_text(entry: &MessageEntry) -> String {
    entry
        .content()
        .iter()
        .map(|(_, text)| text.as_str())
        .collect()
}

/// The tile width below which a windowed stream or ticker row shortens its
/// stamp to `HH:MM:SS`: twice the full `YYYY-MM-DD HH:MM:SS` stamp's own 20
/// columns (19 characters plus the space before the message), so the stamp
/// never costs half a narrow tile's row or more.
const NARROW_STREAM_STAMP_WIDTH: u16 = 40;

/// One history row's label: `entry_text` with its timestamp in front, full
/// (`YYYY-MM-DD HH:MM:SS`) or `short` (`HH:MM:SS`, the last 8 characters of
/// the same rendering), local to the viewer per `utc_offset_secs`
/// (see [`crate::model::Model::set_utc_offset`]). [`MessageHistoryState::view`]
/// and [`MessageHistoryState::view_for_width`] are what draw this and
/// nothing else reads it -- a copy takes [`entry_text`] alone, so a path or
/// a message containing today's date is never mistaken for one this label
/// prepended.
fn entry_label_for(entry: &MessageEntry, short: bool, utc_offset_secs: i64) -> String {
    let stamp = format_at(entry.at(), utc_offset_secs);
    let stamp = if short { &stamp[11..] } else { &stamp[..] };
    format!("{stamp} {}", entry_text(entry))
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

    /// The rows read off the menu's grid reach the view by move: a rebuild
    /// copies the grid once, when they are read.
    #[test]
    fn the_drawn_rows_move_into_the_view_uncopied() {
        let drawn = vec![vec![Span::new("edit".to_string(), StyleRole::Highlight(7))]];
        let text = drawn[0][0].text.as_ptr();
        let view = PaletteState::new(cmdline(":", "e"), None)
            .with_drawn(drawn)
            .into_view();
        assert_eq!(view.drawn[0][0].text.as_ptr(), text);
    }

    #[test]
    fn a_bare_colon_renders_an_empty_command_query() {
        let state = PaletteState::new(cmdline(":", ""), None);
        let view = state.into_view();
        assert_eq!(view.title, "Command");
        assert_eq!(view.query, ":");
        assert!(view.rows.is_empty());
        assert_eq!(view.selected, None);
    }

    #[test]
    fn a_typed_command_keeps_its_firstc_prefix_in_the_query() {
        let state = PaletteState::new(cmdline(":", "set nu"), None);
        assert_eq!(state.into_view().query, ":set nu");
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
        assert_eq!(state.into_view().query, "New file: foo");
    }

    #[test]
    fn a_search_prompt_titles_itself_search() {
        let state = PaletteState::new(cmdline("/", "needle"), None);
        assert_eq!(state.into_view().title, "Search");
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
        let view = state.into_view();
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
        assert_eq!(state.into_view().selected, None);
    }

    #[test]
    fn a_history_snapshot_carries_every_entry_at_the_moment_it_was_taken() {
        let mut history = ToastHistory::new();
        history.push(&message_entry("first"));
        history.push(&message_entry("second"));

        let state = MessageHistoryState::snapshot(&history);
        history.push(&message_entry("third, after the snapshot"));

        let view = state.view(0);
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
        let view = state.view(0);

        assert_eq!(view.rows.len(), 1);
        assert_eq!(view.rows[0].label, "2023-11-14 22:13:20 build finished");
    }

    #[test]
    fn the_history_overlay_renders_the_stamp_local_to_the_offset_it_is_given() {
        let stamp =
            std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_000);
        let mut history = ToastHistory::new();
        history.push(&message_entry_at("build finished", stamp));

        let state = MessageHistoryState::snapshot(&history);
        // UTC-7: the same instant `the_history_overlay_renders_the_full_timestamp`
        // reads as 22:13:20 UTC crosses midnight into the day before, local
        let view = state.view(-7 * 3600);

        assert_eq!(view.rows.len(), 1);
        assert_eq!(view.rows[0].label, "2023-11-14 15:13:20 build finished");
    }

    #[test]
    fn a_narrow_windowed_streams_shortened_stamp_is_local_too() {
        let stamp =
            std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_000);
        let mut history = ToastHistory::new();
        history.push(&message_entry_at("build finished", stamp));

        let state = MessageHistoryState::snapshot(&history);
        let view = state.view_for_width(NARROW_STREAM_STAMP_WIDTH - 1, -7 * 3600);

        assert_eq!(view.rows.len(), 1);
        assert_eq!(view.rows[0].label, "15:13:20 build finished");
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

        let view = state.view(0);
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
        let selected_text = state.view(0).rows[5].label.clone();

        history.push(&message_entry("newest, after the ring is full"));
        assert!(state.refresh(&history));

        let view = state.view(0);
        let labels: Vec<String> = view.rows.iter().map(|r| r.label.clone()).collect();
        assert_eq!(
            labels[view.selected.expect("an entry is still selected")],
            selected_text,
            "an eviction moves every row by one without changing the count, \
             so the selection has to follow the entry rather than the row"
        );
    }
}
