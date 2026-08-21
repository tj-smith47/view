//! What a native overlay puts on screen, and nothing else.
//!
//! Each type here is the paint-facing projection of one feature's state:
//! the rows, the query, the selection index, the title. A feature's own
//! state (the file list it filtered, the tree it walked, the keymap it
//! resolved) stays with that feature and converts into one of these for
//! the frame. Splitting it this way keeps the paint path free of feature
//! logic in both directions: `view-surface` lays these out without knowing
//! what produced them, and a feature can restructure its state without
//! reshaping the layer that draws it.
//!
//! Every field is display text already: no path resolution, no filtering,
//! no key lookup happens downstream of this. A row that should read
//! `src/main.rs` arrives as that string.

use crate::theme::ChromeGroup;

/// What a [`Span`]'s text means, resolved to a concrete [`crate::theme::ResolvedStyle`]
/// through the active colorscheme -- never a raw color chosen here.
///
/// The single vocabulary both painters (`view-tui`'s terminal backend and
/// `view-oracle`'s raster) resolve through [`StyleRole::chrome_group`]: a
/// role that meant one thing to one painter and something else to the other
/// is exactly the divergence a differential-tested editor cannot afford, so
/// there is one mapping function, not two independently maintained copies
/// of it.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum StyleRole {
    /// Unstyled text: rendered in whatever base style the row it sits on
    /// already carries (a popup's `Pmenu` colors, the statusline's own
    /// `StatusLine` colors). Every overlay that has never needed more than
    /// one style per row -- the picker, the palette, the prompt, the
    /// message log -- paints entirely in this role; a tree row does too
    /// unless it carries a [`GitMark`], which adds one glyph span in a
    /// `Git*` role of its own.
    #[default]
    Plain,
    /// The statusline's mode text (`-- INSERT --`, `recording @q`, ...),
    /// verbatim from `msg_showmode`.
    Mode,
    /// The statusline's current-buffer name.
    File,
    /// The statusline's unsaved-buffer marker.
    Modified,
    /// The statusline's current git branch.
    GitBranch,
    /// The statusline's cursor-position/search-count text.
    Ruler,
    /// The statusline's error-diagnostic glyph and count.
    DiagnosticError,
    /// The statusline's warning-diagnostic glyph and count.
    DiagnosticWarning,
    /// A picker candidate row's matched substring: the byte ranges nucleo
    /// scored as part of the fuzzy match, so the user sees which characters
    /// of a long path or buffer name actually satisfied their query.
    Match,
    /// A tree row's git decoration for a modified or renamed entry.
    GitModified,
    /// A tree row's git decoration for a newly added or copied entry.
    GitAdded,
    /// A tree row's git decoration for a deleted or conflicted entry.
    GitDeleted,
    /// A tree row's git decoration for an untracked entry.
    GitUntracked,
    /// A diff review row proposing a line be added.
    ///
    /// Its own role rather than a reuse of [`Self::GitAdded`], which means
    /// a tree entry's git state: the two resolve to the same
    /// [`ChromeGroup`] today, but they answer different questions ("what
    /// did git say about this file" against "what would this hunk do to
    /// this line"), and a role whose name lies about its subject is how a
    /// later theme change to one of them silently repaints the other.
    DiffAdded,
    /// A diff review row proposing a line be removed, the counterpart of
    /// [`Self::DiffAdded`].
    DiffRemoved,
    /// An overlay's own title, as set into its top border. Its own role
    /// rather than part of the frame it sits in: a title is the only text
    /// on that row, and painting it in the border's deliberately dimmed
    /// color makes the one label naming what the overlay IS the least
    /// legible thing on it.
    Title,
}

impl StyleRole {
    /// The [`ChromeGroup`] this role resolves through, or `None` for
    /// [`StyleRole::Plain`], which paints in whatever base style its row
    /// already carries rather than a group of its own.
    #[must_use]
    pub const fn chrome_group(self) -> Option<ChromeGroup> {
        match self {
            Self::Plain => None,
            Self::Mode => Some(ChromeGroup::ModeMsg),
            Self::File | Self::Ruler => Some(ChromeGroup::StatusLine),
            Self::Modified => Some(ChromeGroup::WarningMsg),
            Self::GitBranch => Some(ChromeGroup::Directory),
            Self::DiagnosticError => Some(ChromeGroup::ErrorMsg),
            Self::DiagnosticWarning => Some(ChromeGroup::WarningMsg),
            Self::Match => Some(ChromeGroup::IncSearch),
            Self::DiffAdded => Some(ChromeGroup::DiffAdd),
            Self::DiffRemoved => Some(ChromeGroup::DiffDelete),
            Self::GitModified => Some(ChromeGroup::DiffChange),
            Self::GitAdded => Some(ChromeGroup::DiffAdd),
            Self::GitDeleted => Some(ChromeGroup::DiffDelete),
            Self::GitUntracked => Some(ChromeGroup::Directory),
            Self::Title => Some(ChromeGroup::FloatTitle),
        }
    }
}

/// One run of text sharing a single [`StyleRole`]: the smallest unit a
/// painter resolves into styled cells. `view-core` names the role,
/// `view-surface` places the span in a row, and only a painter turns a role
/// into a concrete color -- see [`StyleRole::chrome_group`].
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Span {
    pub text: String,
    pub role: StyleRole,
}

impl Span {
    /// A span carrying `role`.
    #[must_use]
    pub fn new(text: impl Into<String>, role: StyleRole) -> Self {
        Self {
            text: text.into(),
            role,
        }
    }

    /// An unstyled span: the honest representation for a row whose text has
    /// never carried more than one style.
    #[must_use]
    pub fn plain(text: impl Into<String>) -> Self {
        Self::new(text, StyleRole::Plain)
    }
}

/// A fuzzy picker's frame: the prompt line and the candidate rows under it.
///
/// ```
/// use view_core::native::views::PickerView;
/// let picker = PickerView::new("Files")
///     .with_query("mai")
///     .with_rows(vec!["src/main.rs".to_string(), "src/lib.rs".to_string()])
///     .with_selected(0);
/// assert_eq!(picker.selected, Some(0));
/// ```
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct PickerView {
    /// The overlay's title, drawn into its top border.
    pub title: String,
    /// The query as typed so far, drawn on the prompt line.
    pub query: String,
    /// The candidate rows, best match first, already formatted for display
    /// and already carrying [`StyleRole::Match`] spans over whatever
    /// substrings the matcher scored -- see [`PickerView::with_span_rows`].
    /// [`PickerView::with_rows`] builds this from plain strings for callers
    /// that never had match indices to begin with (every pre-picker test
    /// fixture, and any future feature that reuses this view for a row with
    /// no highlighting).
    pub rows: Vec<Vec<Span>>,
    /// Index into `rows` of the highlighted candidate, or `None` when the
    /// query matched nothing. An index past the end of `rows` highlights
    /// nothing rather than being clamped onto a row the feature did not
    /// choose.
    pub selected: Option<usize>,
    /// The preview pane's lines for the currently selected candidate, empty
    /// until a preview reply (RPC or disk-fallback) has landed for it -- see
    /// `docs/picker-preview-wire-capture.md`.
    pub preview: Vec<String>,
}

impl PickerView {
    /// An empty picker titled `title`: no query typed, no candidates yet.
    #[must_use]
    pub fn new(title: impl Into<String>) -> Self {
        Self {
            title: title.into(),
            ..Self::default()
        }
    }

    /// The same view with `query` on its prompt line.
    #[must_use]
    pub fn with_query(self, query: impl Into<String>) -> Self {
        Self {
            query: query.into(),
            ..self
        }
    }

    /// The same view showing `rows` as its candidates, each rendered as one
    /// unstyled [`Span`]. For a picker with match-highlighted rows, use
    /// [`PickerView::with_span_rows`] instead.
    #[must_use]
    pub fn with_rows(self, rows: Vec<String>) -> Self {
        Self {
            rows: rows
                .into_iter()
                .map(|text| vec![Span::plain(text)])
                .collect(),
            ..self
        }
    }

    /// The same view showing `rows` as its candidates, each row already
    /// split into styled spans (e.g. plain text around a
    /// [`StyleRole::Match`] run over the matched substring).
    #[must_use]
    pub fn with_span_rows(self, rows: Vec<Vec<Span>>) -> Self {
        Self { rows, ..self }
    }

    /// The same view with row `index` highlighted.
    #[must_use]
    pub fn with_selected(self, index: usize) -> Self {
        Self {
            selected: Some(index),
            ..self
        }
    }

    /// The same view with `lines` shown in the preview pane.
    #[must_use]
    pub fn with_preview(self, lines: Vec<String>) -> Self {
        Self {
            preview: lines,
            ..self
        }
    }
}

/// One file tree row's git-status decoration, resolved from a `git status
/// --porcelain=v2` line's two-character `XY` code down to the single glyph
/// and [`StyleRole`] a row can carry -- see `view_native::tree::git`'s doc
/// for exactly how a code collapses to one of these.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum GitMark {
    Modified,
    Added,
    Deleted,
    Renamed,
    Copied,
    Conflicted,
    Untracked,
}

impl GitMark {
    /// The single glyph painted before a decorated row's label.
    #[must_use]
    pub const fn glyph(self) -> char {
        match self {
            Self::Modified => 'M',
            Self::Added => 'A',
            Self::Deleted => 'D',
            Self::Renamed => 'R',
            Self::Copied => 'C',
            Self::Conflicted => 'U',
            Self::Untracked => '?',
        }
    }

    /// The [`StyleRole`] a row carrying this mark paints its glyph in.
    /// `Renamed` reads as a modification and `Copied`/`Conflicted` read as
    /// added/deleted respectively, rather than growing a chrome group and
    /// style role each rare git state would only ever reach alone.
    #[must_use]
    pub const fn style_role(self) -> StyleRole {
        match self {
            Self::Added | Self::Copied => StyleRole::GitAdded,
            Self::Modified | Self::Renamed => StyleRole::GitModified,
            Self::Deleted | Self::Conflicted => StyleRole::GitDeleted,
            Self::Untracked => StyleRole::GitUntracked,
        }
    }
}

/// One row of a [`TreeView`]: how deep it sits, what it is called, and
/// whether it is a directory that is open or shut.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct TreeRow {
    /// Nesting depth, zero at the tree's root entries. Indentation is the
    /// painter's to apply, so a depth is a fact about the tree rather than a
    /// count of leading spaces some other producer would have to match.
    pub depth: u16,
    /// The entry's display name, without indentation or any expand marker.
    pub label: String,
    /// `Some(true)` for an expanded directory, `Some(false)` for a
    /// collapsed one, `None` for a leaf that cannot be expanded at all.
    pub expanded: Option<bool>,
    /// The entry's git decoration, or `None` when it carries no status (the
    /// common case, and the only possible case with `git` absent from
    /// `PATH` -- see [`GitMark`]'s doc). Absence is not an error.
    pub status: Option<GitMark>,
}

impl TreeRow {
    /// A leaf row at `depth`.
    #[must_use]
    pub fn leaf(depth: u16, label: impl Into<String>) -> Self {
        Self {
            depth,
            label: label.into(),
            expanded: None,
            status: None,
        }
    }

    /// A directory row at `depth`, open when `expanded`.
    #[must_use]
    pub fn dir(depth: u16, label: impl Into<String>, expanded: bool) -> Self {
        Self {
            depth,
            label: label.into(),
            expanded: Some(expanded),
            status: None,
        }
    }

    /// The same row carrying `status`'s git decoration, or none.
    #[must_use]
    pub fn with_status(self, status: Option<GitMark>) -> Self {
        Self { status, ..self }
    }
}

/// A file tree's frame: the visible rows in display order, already
/// flattened from whatever shape the feature holds them in.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct TreeView {
    /// The overlay's title, drawn into its top border.
    pub title: String,
    /// The visible entries, top to bottom. A collapsed directory's children
    /// are absent here rather than present and skipped downstream.
    pub rows: Vec<TreeRow>,
    /// Index into `rows` of the cursor line, or `None` for an empty tree.
    pub selected: Option<usize>,
}

impl TreeView {
    /// An empty tree titled `title`.
    #[must_use]
    pub fn new(title: impl Into<String>) -> Self {
        Self {
            title: title.into(),
            ..Self::default()
        }
    }

    /// The same view showing `rows`.
    #[must_use]
    pub fn with_rows(self, rows: Vec<TreeRow>) -> Self {
        Self { rows, ..self }
    }

    /// The same view with row `index` under the cursor.
    #[must_use]
    pub fn with_selected(self, index: usize) -> Self {
        Self {
            selected: Some(index),
            ..self
        }
    }
}

/// A statusline's frame: three already-composed segments, laid out left,
/// centered, and right on one row.
///
/// Three strings rather than a list of components: what a segment contains
/// (mode, file name, diagnostics counts) is the feature's composition
/// problem, while placement across the row is the only part painting has an
/// opinion about.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct StatuslineView {
    /// Flush against the row's left edge.
    pub left: Vec<Span>,
    /// Centered on the row, as far as the left and right segments allow.
    pub center: Vec<Span>,
    /// Flush against the row's right edge.
    pub right: Vec<Span>,
    /// The overlay's title, drawn into its top border. Empty for the
    /// ordinary bar, which is identified by its position rather than by a
    /// label.
    pub title: String,
}

impl StatuslineView {
    /// A bar with the three segments given, each a single unstyled span --
    /// the honest representation for a caller (a test, a golden) that only
    /// cares about placement. [`StatuslineState::view`](crate::native::statusline::StatuslineState::view)
    /// is the one caller that needs real per-segment roles, and builds
    /// through [`StatuslineView::from_spans`] instead.
    #[must_use]
    pub fn new(
        left: impl Into<String>,
        center: impl Into<String>,
        right: impl Into<String>,
    ) -> Self {
        Self::from_spans(one_span(left), one_span(center), one_span(right))
    }

    /// A bar with each zone already broken into its own styled spans.
    #[must_use]
    pub fn from_spans(left: Vec<Span>, center: Vec<Span>, right: Vec<Span>) -> Self {
        Self {
            left,
            center,
            right,
            title: String::new(),
        }
    }

    /// The same bar labelled `title` in its top border.
    #[must_use]
    pub fn with_title(self, title: impl Into<String>) -> Self {
        Self {
            title: title.into(),
            ..self
        }
    }
}

/// `text` as a zone's whole span list: empty for empty text (an absent
/// segment contributes nothing to lay out), one plain span otherwise.
fn one_span(text: impl Into<String>) -> Vec<Span> {
    let text = text.into();
    if text.is_empty() {
        Vec::new()
    } else {
        vec![Span::plain(text)]
    }
}

/// A prompt's frame: a question, the answer as typed so far, and any fixed
/// choices offered instead of free text.
///
/// Both shapes live in one type because both paint the same way: a confirm
/// leaves `input` empty and fills `choices`, a text prompt does the
/// reverse, and a prompt offering a default answer plus alternatives fills
/// both. Splitting them into two types would duplicate the frame, the
/// title, and the message across both for no painting difference.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct PromptView {
    /// The overlay's title, drawn into its top border.
    pub title: String,
    /// The question, on the first interior row.
    pub message: String,
    /// The answer as typed so far.
    pub input: String,
    /// Fixed answers offered under the input line, empty for a free-text
    /// prompt.
    pub choices: Vec<String>,
    /// Index into `choices` of the highlighted answer, or `None` when the
    /// input line holds focus.
    pub selected: Option<usize>,
}

impl PromptView {
    /// A free-text prompt titled `title` asking `message`.
    #[must_use]
    pub fn new(title: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            title: title.into(),
            message: message.into(),
            ..Self::default()
        }
    }

    /// The same prompt with `input` typed into it.
    #[must_use]
    pub fn with_input(self, input: impl Into<String>) -> Self {
        Self {
            input: input.into(),
            ..self
        }
    }

    /// The same prompt offering `choices` under its input line.
    #[must_use]
    pub fn with_choices(self, choices: Vec<String>) -> Self {
        Self { choices, ..self }
    }

    /// The same prompt with choice `index` highlighted.
    #[must_use]
    pub fn with_selected(self, index: usize) -> Self {
        Self {
            selected: Some(index),
            ..self
        }
    }
}

/// One command in a [`PaletteView`]: what it is called and the keys that
/// reach it without opening the palette at all.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct PaletteRow {
    /// The command's display name.
    pub label: String,
    /// The key sequence bound to it, right-aligned on the row, or `None`
    /// for a command with no binding.
    pub binding: Option<String>,
}

impl PaletteRow {
    /// An unbound command named `label`.
    #[must_use]
    pub fn new(label: impl Into<String>) -> Self {
        Self {
            label: label.into(),
            binding: None,
        }
    }

    /// The same command showing `binding` as the keys that reach it.
    #[must_use]
    pub fn with_binding(self, binding: impl Into<String>) -> Self {
        Self {
            binding: Some(binding.into()),
            ..self
        }
    }
}

/// A command palette's frame: the prompt line plus the matching commands
/// and their bindings.
///
/// Separate from [`PickerView`] rather than a picker over command names,
/// because a palette row carries a second, right-aligned column (the
/// binding) that a picker row has no place for, and flattening the two
/// columns into one string upstream would leave the alignment to whichever
/// producer got there first.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct PaletteView {
    /// The overlay's title, drawn into its top border.
    pub title: String,
    /// The query as typed so far, drawn on the prompt line.
    pub query: String,
    /// The matching commands, best match first.
    pub rows: Vec<PaletteRow>,
    /// Index into `rows` of the highlighted command, or `None` when the
    /// query matched nothing.
    pub selected: Option<usize>,
}

impl PaletteView {
    /// An empty palette titled `title`.
    #[must_use]
    pub fn new(title: impl Into<String>) -> Self {
        Self {
            title: title.into(),
            ..Self::default()
        }
    }

    /// The same palette with `query` on its prompt line.
    #[must_use]
    pub fn with_query(self, query: impl Into<String>) -> Self {
        Self {
            query: query.into(),
            ..self
        }
    }

    /// The same palette showing `rows` as its commands.
    #[must_use]
    pub fn with_rows(self, rows: Vec<PaletteRow>) -> Self {
        Self { rows, ..self }
    }

    /// The same palette with row `index` highlighted.
    #[must_use]
    pub fn with_selected(self, index: usize) -> Self {
        Self {
            selected: Some(index),
            ..self
        }
    }
}

/// The agent panel's frame: its composer line and the transcript rows
/// beneath it. Carries no selection index -- a transcript scrolls, it does
/// not offer a row to act on the way a picker's or a palette's rows do.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct AiPanelView {
    /// The overlay's title, drawn into its top border.
    pub title: String,
    /// The composer line's text as typed so far.
    pub input: String,
    /// The transcript, oldest first, each entry already formatted as one
    /// row of spans.
    pub rows: Vec<Vec<Span>>,
    /// What the session has spent so far, as the agent last reported it:
    /// context window used against its size, and the running cost when the
    /// agent priced the turn. Empty until the first `usage_update` arrives,
    /// on the same "empty means nothing extra to draw" terms the rows below
    /// use. Deliberately the header's first row and so the first sacrificed
    /// under truncation: it is ambient accounting, and it must never cost
    /// the panel the crash banner or the question an agent is blocked on.
    pub usage: Vec<Vec<Span>>,
    /// The pending permission prompt's own rows -- the question first, then
    /// one row per option the agent offered, each naming its kind. Empty
    /// when nothing is pending, which is what tells `view-surface`'s own
    /// `ai_body` there is nothing extra to draw above the transcript.
    pub pending_permission: Vec<Vec<Span>>,
    /// The panel-local crash banner's own row, when the session it belongs
    /// to has one -- see `AiPanelState::local_error`'s own doc for why this
    /// is never a transient toast. Empty when nothing crashed, on the same
    /// "empty means nothing extra to draw" terms `pending_permission` uses.
    pub local_error: Vec<Vec<Span>>,
    /// The open diff review's always-visible summary: which file, which
    /// hunk of how many, and either the review's keys or the reason it can
    /// no longer be acted on. Empty when no review is open, on the same
    /// "empty means nothing extra to draw" terms above.
    pub review: Vec<Vec<Span>>,
    /// Which of [`Self::rows`] the scroll window keeps on screen, or `None`
    /// when nothing anchors it. Set only while a review is open, where the
    /// rows are that review's hunks and this names the focused one's header
    /// -- hunk-jump is the review's navigation, so the window follows the
    /// cursor rather than offering a free scroll across a file the proposal
    /// mostly does not touch.
    pub selected: Option<usize>,
}

impl AiPanelView {
    /// An empty panel titled `title`: no transcript yet, nothing typed.
    #[must_use]
    pub fn new(title: impl Into<String>) -> Self {
        Self {
            title: title.into(),
            ..Self::default()
        }
    }

    /// The same panel with `input` on its composer line.
    #[must_use]
    pub fn with_input(self, input: impl Into<String>) -> Self {
        Self {
            input: input.into(),
            ..self
        }
    }

    /// The same panel showing `rows` as its transcript.
    #[must_use]
    pub fn with_rows(self, rows: Vec<Vec<Span>>) -> Self {
        Self { rows, ..self }
    }

    /// The same panel showing `rows` as its pending permission prompt.
    #[must_use]
    pub fn with_pending_permission(self, rows: Vec<Vec<Span>>) -> Self {
        Self {
            pending_permission: rows,
            ..self
        }
    }

    /// The same panel showing `rows` as its panel-local crash banner.
    #[must_use]
    pub fn with_local_error(self, rows: Vec<Vec<Span>>) -> Self {
        Self {
            local_error: rows,
            ..self
        }
    }

    /// The same panel showing `row` as its session accounting.
    #[must_use]
    pub fn with_usage(self, row: Vec<Span>) -> Self {
        Self {
            usage: vec![row],
            ..self
        }
    }

    /// The same panel showing `rows` as its open review's summary, with
    /// `selected` anchoring the scroll window on the focused hunk.
    #[must_use]
    pub fn with_review(self, rows: Vec<Vec<Span>>, selected: Option<usize>) -> Self {
        Self {
            review: rows,
            selected,
            ..self
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    #[test]
    fn a_builder_chain_sets_every_field_it_names_and_leaves_the_rest_default() {
        let picker = PickerView::new("Files")
            .with_query("mai")
            .with_rows(vec!["src/main.rs".to_string()])
            .with_selected(0);
        assert_eq!(picker.title, "Files");
        assert_eq!(picker.query, "mai");
        assert_eq!(picker.rows, vec![vec![Span::plain("src/main.rs")]]);
        assert_eq!(picker.selected, Some(0));

        let bare = PickerView::new("Files");
        assert!(bare.query.is_empty());
        assert!(bare.rows.is_empty());
        assert_eq!(
            bare.selected, None,
            "an unqueried picker highlights nothing"
        );
    }

    #[test]
    fn a_tree_row_records_expandability_rather_than_an_indent_string() {
        let dir = TreeRow::dir(0, "src", true);
        assert_eq!(dir.expanded, Some(true));
        assert_eq!(dir.label, "src", "the label carries no indentation");
        assert_eq!(TreeRow::dir(1, "target", false).expanded, Some(false));
        assert_eq!(TreeRow::leaf(1, "main.rs").expanded, None);
        assert_eq!(TreeRow::leaf(3, "deep.rs").depth, 3);
        assert_eq!(dir.status, None, "an undecorated row carries no mark");
    }

    #[test]
    fn a_git_mark_resolves_to_one_style_role_and_one_glyph() {
        assert_eq!(GitMark::Modified.glyph(), 'M');
        assert_eq!(GitMark::Modified.style_role(), StyleRole::GitModified);
        assert_eq!(GitMark::Renamed.style_role(), StyleRole::GitModified);
        assert_eq!(GitMark::Added.style_role(), StyleRole::GitAdded);
        assert_eq!(GitMark::Copied.style_role(), StyleRole::GitAdded);
        assert_eq!(GitMark::Deleted.style_role(), StyleRole::GitDeleted);
        assert_eq!(GitMark::Conflicted.style_role(), StyleRole::GitDeleted);
        assert_eq!(GitMark::Untracked.style_role(), StyleRole::GitUntracked);

        let decorated = TreeRow::leaf(0, "main.rs").with_status(Some(GitMark::Added));
        assert_eq!(decorated.status, Some(GitMark::Added));
    }

    #[test]
    fn a_prompt_holds_free_text_and_fixed_choices_in_one_shape() {
        let confirm = PromptView::new("Confirm", "Overwrite file?")
            .with_choices(vec!["Yes".to_string(), "No".to_string()])
            .with_selected(1);
        assert!(confirm.input.is_empty());
        assert_eq!(confirm.selected, Some(1));

        let text = PromptView::new("Rename", "New name:").with_input("lib.rs");
        assert!(text.choices.is_empty());
        assert_eq!(text.selected, None, "the input line holds focus");
    }

    #[test]
    fn a_palette_row_keeps_its_binding_in_its_own_column() {
        let bound = PaletteRow::new("Find File").with_binding("<C-p>");
        assert_eq!(bound.label, "Find File");
        assert_eq!(bound.binding, Some("<C-p>".to_string()));
        assert_eq!(PaletteRow::new("Reload").binding, None);
    }

    #[test]
    fn a_statusline_titles_itself_only_when_asked() {
        let bar = StatuslineView::new("NORMAL", "src/main.rs", "12:4");
        assert!(bar.title.is_empty());
        assert_eq!(bar.with_title("Status").title, "Status");
    }
}
