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

use super::geometry::LIST_MARKER_COLS;
use crate::theme::ChromeGroup;

/// What a [`Span`]'s text means, resolved to a concrete [`crate::theme::ResolvedStyle`]
/// through the active colorscheme.
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
    /// one style per row -- the palette, the prompt, the message log --
    /// paints entirely in this role. A tree row's name and indentation
    /// are in it too.
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
    /// An agent transcript entry the user composed: its marker glyph and
    /// the prompt body behind it.
    ///
    /// The three message roles here are what tells one speaker from another
    /// in the panel. There is no word prefix on those rows any more, so a
    /// reader who cannot see the difference between these colors is reading
    /// their own prompt as though the agent had said it.
    AiUser,
    /// An agent transcript entry the agent replied with, the counterpart of
    /// [`Self::AiUser`].
    AiAgent,
    /// An agent transcript entry carrying the agent's reasoning rather than
    /// its answer, deliberately the dimmest of the three: it is the one
    /// voice on the panel that is not an assertion.
    AiThought,
    /// The question row of an outstanding permission request: what the
    /// agent is blocked on and cannot proceed past unanswered.
    ///
    /// The four permission roles are what separate an unanswered question
    /// from the transcript rows above it, and separate the answers from
    /// each other -- a prompt whose options all paint like ordinary text is
    /// the defect they exist to close, because the reader cannot see that
    /// anything is waiting on them or which key costs what.
    AiPermissionAsk,
    /// A permission option that lets this one call through and nothing
    /// after it.
    AiPermissionAllow,
    /// A permission option that tells the agent to stop asking about later
    /// calls. Its own role because it is the one answer whose consequence
    /// outlives the question, so it must read as a different act from
    /// allowing once.
    AiPermissionAlways,
    /// A permission option that refuses the call.
    AiPermissionReject,
    /// A transcript row view itself wrote into the conversation, such as
    /// what became of a diff review. Kept apart from the permission roles
    /// because nothing here is a question, and a row that paints like one
    /// would be waiting for a key nobody needs to press.
    AiNotice,
    /// A transcript tool call's status glyph once the call has completed.
    AiToolDone,
    /// A transcript tool call's status glyph once the call has failed.
    AiToolFailed,
    /// A transcript tool call's status glyph while the call is unresolved --
    /// the pending dot and every spinner frame that follows it. One role for
    /// both, because they are one state to a reader: the call has not
    /// answered yet.
    AiToolRunning,
    /// A notice about something wrong with the session that holds while it
    /// stands: the engine wedge banner.
    Warning,
    /// A line of a toast raised as an error. Painted as plain text; the
    /// box's frame carries the level.
    NoticeError,
    /// A line of a toast raised as a warning. Painted as plain text; the
    /// box's frame carries the level.
    NoticeWarn,
    /// A line of a toast raised as a hint or a debug message. Painted as
    /// plain text; the box's frame carries the level.
    NoticeHint,
    /// A tree row's folder glyph, in the colorscheme's directory colour.
    TreeFolder,
    /// A tree row's file-type glyph, painted in the `0xRRGGBB` colour its
    /// [`crate::native::devicons::Devicon`] carries, the same under every
    /// colorscheme, the way nvim-web-devicons paints it.
    Devicon(u32),
    /// A tree row's git glyph for a change not yet staged or a deletion,
    /// in the colorscheme's `Statement` colour.
    GitDirty,
    /// A tree row's git glyph for a staged change or a merge conflict, in
    /// the colorscheme's `Constant` colour.
    GitStaged,
    /// A tree row's git glyph for an untracked or renamed entry, in the
    /// colorscheme's `PreProc` colour.
    GitNew,
    /// A tree row's git glyph for an ignored entry, in the colorscheme's
    /// `Comment` colour.
    GitIgnored,
}

/// How much a notice asks of the person reading it, which picks the colour
/// of its toast's frame.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum NoticeLevel {
    /// Something failed.
    Error,
    /// Something needs attention.
    Warn,
    /// Something happened.
    Info,
    /// A hint or a debug message.
    Hint,
}

impl StyleRole {
    /// The [`ChromeGroup`] this role resolves through, or `None` for
    /// [`StyleRole::Plain`] and the three notice roles, which paint in
    /// whatever base style their row already carries, and for the roles
    /// [`crate::theme::Theme::role_fg`] colours.
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
            Self::AiUser => Some(ChromeGroup::Question),
            Self::AiAgent => Some(ChromeGroup::MoreMsg),
            Self::AiThought | Self::AiToolRunning => Some(ChromeGroup::NonText),
            Self::AiPermissionAsk => Some(ChromeGroup::Question),
            Self::AiPermissionAllow => Some(ChromeGroup::OkMsg),
            Self::AiPermissionAlways => Some(ChromeGroup::WarningMsg),
            Self::AiPermissionReject => Some(ChromeGroup::ErrorMsg),
            Self::AiNotice => Some(ChromeGroup::WarningMsg),
            Self::AiToolDone => Some(ChromeGroup::OkMsg),
            Self::AiToolFailed => Some(ChromeGroup::ErrorMsg),
            Self::Warning => Some(ChromeGroup::WarningMsg),
            Self::TreeFolder => Some(ChromeGroup::Directory),
            Self::NoticeError
            | Self::NoticeWarn
            | Self::NoticeHint
            | Self::Devicon(_)
            | Self::GitDirty
            | Self::GitStaged
            | Self::GitNew
            | Self::GitIgnored => None,
        }
    }

    /// Whether a span in this role keeps its own foreground on a selected
    /// row. A tree row's glyphs do, so the folder, the file type and the
    /// git state still read on the cursor line; its name takes the
    /// selection's colours.
    #[must_use]
    pub const fn keeps_fg_on_selection(self) -> bool {
        matches!(
            self,
            Self::TreeFolder
                | Self::Devicon(_)
                | Self::GitModified
                | Self::GitAdded
                | Self::GitDeleted
                | Self::GitUntracked
                | Self::GitDirty
                | Self::GitStaged
                | Self::GitNew
                | Self::GitIgnored
        )
    }

    /// The level of the notice a toast line in this role belongs to.
    #[must_use]
    pub const fn notice_level(self) -> NoticeLevel {
        match self {
            Self::NoticeError => NoticeLevel::Error,
            Self::NoticeWarn | Self::Warning => NoticeLevel::Warn,
            Self::NoticeHint => NoticeLevel::Hint,
            _ => NoticeLevel::Info,
        }
    }

    /// Whether a `reverse` on this role's chrome group was written for the
    /// surface this role paints.
    ///
    /// It was for every role whose group nvim itself renders reversed --
    /// a selected tab, a selected popup row, an incremental-search match --
    /// and it was not for any role resolving through a diff group. A
    /// colorscheme designs `:h hl-DiffAdd` and its neighbours for diff
    /// mode, where they color the cells of a diffed line; dracula defines
    /// `DiffDelete` foreground-only and `reverse`, and a tree row's git
    /// chip is two cells wide, so inheriting that paints a solid block of
    /// the color with the glyph knocked out of it. The color is what the
    /// chip wants from the group, which is the same split the inline
    /// review's own `ViewReview*` groups are derived on.
    ///
    /// Read off [`Self::chrome_group`] rather than off a list of roles, so
    /// a role mapped onto a diff group later inherits the answer instead of
    /// needing to be remembered here.
    #[must_use]
    pub const fn keeps_group_reverse(self) -> bool {
        !matches!(
            self.chrome_group(),
            Some(ChromeGroup::DiffAdd | ChromeGroup::DiffChange | ChromeGroup::DiffDelete)
        )
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
    /// The 0-based index into `preview` of the line the selected candidate
    /// points at (a grep match's line), or `None` to preview from the top.
    pub preview_line: Option<usize>,
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

    /// The same view with the preview opening on line `index` (0-based).
    #[must_use]
    pub fn with_preview_line(self, index: Option<usize>) -> Self {
        Self {
            preview_line: index,
            ..self
        }
    }

    /// The preview lines a pane `rows` tall shows from its top, and the
    /// index among them of [`Self::preview_line`].
    ///
    /// The line sits a third of the way down the pane, so the code leading
    /// up to it is in view, and the window clamps at the file's first and
    /// last line. A line past the end of `preview` previews from the top,
    /// as a candidate with no line does. That happens when the file has
    /// fewer lines than when it was matched (an edit in its buffer, a
    /// change on disk), and when the read found nothing to show (a path
    /// gone or unreadable as UTF-8), which leaves `preview` empty.
    #[must_use]
    pub fn preview_window(&self, rows: usize) -> (&[String], Option<usize>) {
        let len = self.preview.len();
        let Some(line) = self.preview_line.filter(|line| *line < len) else {
            return (&self.preview, None);
        };
        let start = line.saturating_sub(rows / 3).min(len.saturating_sub(rows));
        (&self.preview[start..], Some(line - start))
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
    Ignored,
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
            Self::Ignored => '!',
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
            Self::Ignored => StyleRole::GitIgnored,
        }
    }
}

/// One of the git states a tree row draws a Nerd Font glyph for, in the
/// order a row draws them.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum GitIcon {
    Staged,
    Unstaged,
    Renamed,
    Deleted,
    Unmerged,
    Untracked,
    Ignored,
}

impl GitIcon {
    /// Every state, in the order a row draws them.
    pub const ALL: [Self; 7] = [
        Self::Staged,
        Self::Unstaged,
        Self::Renamed,
        Self::Deleted,
        Self::Unmerged,
        Self::Untracked,
        Self::Ignored,
    ];

    /// The glyph a row draws for this state.
    #[must_use]
    pub const fn glyph(self) -> &'static str {
        match self {
            Self::Staged => "\u{2713}",
            Self::Unstaged => "\u{2717}",
            Self::Renamed => "\u{279c}",
            Self::Deleted => "\u{f458}",
            Self::Unmerged => "\u{e727}",
            Self::Untracked => "\u{2605}",
            Self::Ignored => "\u{25cc}",
        }
    }

    /// The [`StyleRole`] this state's glyph is painted in.
    #[must_use]
    pub const fn style_role(self) -> StyleRole {
        match self {
            Self::Staged | Self::Unmerged => StyleRole::GitStaged,
            Self::Unstaged | Self::Deleted => StyleRole::GitDirty,
            Self::Renamed | Self::Untracked => StyleRole::GitNew,
            Self::Ignored => StyleRole::GitIgnored,
        }
    }

    /// The [`GitMark`] whose letter and colour draw this state where the
    /// row has no Nerd Font glyphs.
    #[must_use]
    pub const fn mark(self) -> GitMark {
        match self {
            Self::Staged => GitMark::Added,
            Self::Unstaged => GitMark::Modified,
            Self::Renamed => GitMark::Renamed,
            Self::Deleted => GitMark::Deleted,
            Self::Unmerged => GitMark::Conflicted,
            Self::Untracked => GitMark::Untracked,
            Self::Ignored => GitMark::Ignored,
        }
    }

    const fn bit(self) -> u8 {
        1 << self as u8
    }
}

/// The git states one tree row carries: a file's own, or on a folder
/// every state found beneath it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct GitIcons(u8);

impl GitIcons {
    /// The set holding `icons`.
    #[must_use]
    pub fn of(icons: &[GitIcon]) -> Self {
        Self(icons.iter().fold(0, |bits, icon| bits | icon.bit()))
    }

    /// The states `git status --porcelain=v2` reports as the two-character
    /// code `xy`, where `.` or a space is an unchanged side, `?` untracked
    /// and `!` ignored.
    #[must_use]
    pub fn from_xy(xy: &str) -> Self {
        use GitIcon::{Deleted, Ignored, Renamed, Staged, Unmerged, Unstaged, Untracked};
        let mut chars = xy.chars().map(|c| if c == '.' { ' ' } else { c });
        let (Some(x), Some(y)) = (chars.next(), chars.next()) else {
            return Self::default();
        };
        let icons: &[GitIcon] = match (x, y) {
            ('M' | 'C' | 'T' | 'A', ' ') | ('M', 'D') | ('A', 'D') => &[Staged],
            (' ', 'M' | 'C' | 'T') | ('C', 'M') | ('D', 'A') => &[Unstaged],
            ('T' | 'M' | 'A', 'M') => &[Staged, Unstaged],
            (' ', 'A') | ('?', '?') => &[Untracked],
            ('A', 'A' | 'U') => &[Unmerged, Untracked],
            ('R', ' ') | (' ', 'R') => &[Renamed],
            ('R', 'M') => &[Unstaged, Renamed],
            ('U', 'U' | 'D' | 'A') => &[Unmerged],
            (' ', 'D') | ('D' | 'R', ' ' | 'D') => &[Deleted],
            ('D', 'U') => &[Deleted, Unmerged],
            ('!', '!') => &[Ignored],
            _ => &[Unstaged],
        };
        Self::of(icons)
    }

    /// Every state in either set.
    #[must_use]
    pub const fn union(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }

    /// Whether the set holds no state.
    #[must_use]
    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }

    /// The states in the set, in the order a row draws them.
    pub fn iter(self) -> impl Iterator<Item = GitIcon> {
        GitIcon::ALL
            .into_iter()
            .filter(move |icon| self.0 & icon.bit() != 0)
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
    /// The git states the row draws glyphs for: a file's own, or on a
    /// folder every state beneath it.
    pub git: GitIcons,
    /// A file's icon, resolved when the tree took its entries. `None` on a
    /// folder, and on a file row built without one, which draws
    /// [`crate::native::devicons::DEFAULT_FILE`].
    pub icon: Option<crate::native::devicons::Devicon>,
    /// Where a symbolic link points, as the row shows it, or `None` for an
    /// entry that is no link.
    pub link: Option<String>,
    /// Whether the scan entered a folder and found nothing inside it.
    pub empty: bool,
}

impl TreeRow {
    /// A leaf row at `depth`.
    #[must_use]
    pub fn leaf(depth: u16, label: impl Into<String>) -> Self {
        Self {
            depth,
            label: label.into(),
            expanded: None,
            ..Self::default()
        }
    }

    /// A directory row at `depth`, open when `expanded`.
    #[must_use]
    pub fn dir(depth: u16, label: impl Into<String>, expanded: bool) -> Self {
        Self {
            depth,
            label: label.into(),
            expanded: Some(expanded),
            ..Self::default()
        }
    }

    /// The same row carrying `status`'s git decoration, or none.
    #[must_use]
    pub fn with_status(self, status: Option<GitMark>) -> Self {
        Self { status, ..self }
    }

    /// The same row drawing glyphs for the git states in `git`.
    #[must_use]
    pub fn with_git(self, git: GitIcons) -> Self {
        Self { git, ..self }
    }

    /// The same row opening with `icon`.
    #[must_use]
    pub fn with_icon(self, icon: crate::native::devicons::Devicon) -> Self {
        Self {
            icon: Some(icon),
            ..self
        }
    }

    /// The same row as a symbolic link pointing at `target`.
    #[must_use]
    pub fn with_link(self, target: impl Into<String>) -> Self {
        Self {
            link: Some(target.into()),
            ..self
        }
    }

    /// The same row as a folder that lists nothing inside it, when
    /// `empty`.
    #[must_use]
    pub fn with_empty(self, empty: bool) -> Self {
        Self { empty, ..self }
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
    /// The glyphs each row opens with.
    pub icons: crate::native::devicons::TreeIcons,
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

    /// The same view drawing its rows with `icons`.
    #[must_use]
    pub fn with_icons(self, icons: crate::native::devicons::TreeIcons) -> Self {
        Self { icons, ..self }
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
    /// A bar with the three segments given, each a single unstyled span -- the honest
    /// representation for a caller (a test, a golden) that only cares about placement.
    /// [`StatuslineState::view`](crate::native::statusline::StatuslineState::view) is the one
    /// caller that needs real per-segment roles, and builds through [`StatuslineView::from_spans`]
    /// instead.
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

    /// The message broken into rows of at most `width` cells: at every
    /// line break it carries, then at the last space that fits.
    #[must_use]
    pub fn message_rows(&self, width: u16) -> Vec<String> {
        let width = usize::from(width).max(1);
        self.message
            .split('\n')
            .flat_map(|line| super::text::wrap_line(line, width))
            .collect()
    }

    /// The interior rows this prompt fills at `width` cells: its wrapped
    /// message, the input line, the rule under it and one row per choice.
    #[must_use]
    pub fn rows_at(&self, width: u16) -> u16 {
        let rows = self.full_rows(self.message_rows(width).len());
        u16::try_from(rows).unwrap_or(u16::MAX)
    }

    /// The rows of the full layout for a question `message` rows long,
    /// which both a modal's placement and its paint decide on.
    fn full_rows(&self, message: usize) -> usize {
        message + 2 + self.choices.len()
    }

    /// How this prompt's rows fit an interior `width` cells wide and
    /// `height` rows tall.
    ///
    /// When every row fits ([`Self::rows_at`]), the question, the input
    /// line, the rule and one row per choice are all shown. Otherwise rows
    /// are given out in this order: the input line, the choices on one row,
    /// one question row, the rule, then the rest of the question. So a
    /// two-row interior holds the input line and the choices, and a
    /// one-row interior the input line alone. The choices row holds every
    /// choice when they fit `width` side by side, else the selected one
    /// alone, or the first when none is selected. A prompt with no choices
    /// draws the rule only on a row the question leaves spare. A cut
    /// question's last shown row is filled from the rest of the question up
    /// to `width` and ends in `…`.
    #[must_use]
    pub fn fit(&self, width: u16, height: u16) -> PromptFit {
        let height = usize::from(height);
        let count = self.choices.len();
        let message = self.message_rows(width);
        if count == 0 {
            let Some(room) = height.checked_sub(1) else {
                return PromptFit::default();
            };
            return PromptFit {
                rule: room > message.len(),
                message: self.cut_message(message, room, width),
                choices: PromptChoices::Stacked,
            };
        }
        if height >= self.full_rows(message.len()) {
            return PromptFit {
                message,
                choices: PromptChoices::Stacked,
                rule: true,
            };
        }
        let Some(room) = height.checked_sub(2) else {
            return PromptFit::default();
        };
        let shown = if room >= 2 { room - 1 } else { room }.min(message.len());
        PromptFit {
            message: self.cut_message(message, shown, width),
            choices: self.one_row_choices(width),
            rule: room > shown,
        }
    }

    /// The choices on one row `width` cells wide: all of them when they
    /// fit side by side, else the one the selection marks.
    fn one_row_choices(&self, width: u16) -> PromptChoices {
        let count = self.choices.len();
        let joined = self
            .choices
            .iter()
            .map(|choice| {
                usize::from(super::text::text_width(choice)) + usize::from(LIST_MARKER_COLS)
            })
            .sum::<usize>()
            + usize::from(super::text::text_width(INLINE_CHOICE_GAP)) * count.saturating_sub(1);
        if joined <= usize::from(width) {
            PromptChoices::Inline
        } else {
            PromptChoices::One(self.selected.filter(|&i| i < count).unwrap_or(0))
        }
    }

    /// `message` cut to its first `rows` rows, the last of them filled from
    /// the rest of the question.
    fn cut_message(&self, mut message: Vec<String>, rows: usize, width: u16) -> Vec<String> {
        if message.len() <= rows {
            return message;
        }
        message.truncate(rows);
        if let Some((last, shown)) = message.split_last_mut() {
            let rest = self.message_after(shown);
            *last = if super::text::text_width(&rest) <= width {
                rest
            } else {
                ending_in_mark(&rest, width)
            };
        }
        message
    }

    /// The question past the rows `shown`, on one line: each wrapped row
    /// is a run of the question, so each is found in turn from where the
    /// last one ended.
    fn message_after(&self, shown: &[String]) -> String {
        let mut at = 0;
        for row in shown {
            if let Some(found) = self
                .message
                .get(at..)
                .and_then(|rest| rest.find(row.as_str()))
            {
                at += found + row.len();
            }
        }
        self.message
            .get(at..)
            .unwrap_or_default()
            .trim_start()
            .replace('\n', " ")
    }
}

/// How [`PromptView::fit`] lays a prompt's choices.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PromptChoices {
    /// Not laid: the interior holds the input line alone.
    #[default]
    Hidden,
    /// One row per choice under the input line.
    Stacked,
    /// Every choice on the one row under the input line.
    Inline,
    /// The choice at this index alone on the row under the input line,
    /// since the choices side by side are wider than that row.
    One(usize),
}

/// The cells between two choices laid side by side on one row.
pub const INLINE_CHOICE_GAP: &str = " ";

/// A prompt laid into one interior, as [`PromptView::fit`] cut it. Paint
/// and the caret both read it, so the input line is where both put it.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct PromptFit {
    /// The question rows shown above the input line.
    pub message: Vec<String>,
    /// How the choices are laid under the input line.
    pub choices: PromptChoices,
    /// Whether the rule between the input line and the choices is drawn.
    pub rule: bool,
}

/// `row` with [`TRUNCATION_MARK`](super::text::TRUNCATION_MARK) as its
/// last cell, dropping as much of its end as the mark needs to stay within
/// `width` cells.
fn ending_in_mark(row: &str, width: u16) -> String {
    let (kept, _) = super::text::cut_before_mark(row, super::text::text_width(row), width.max(1));
    format!("{kept}{}", super::text::TRUNCATION_MARK)
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
    /// The composer's painted rows: the prompt as typed so far, wrapped to
    /// what one row of the panel holds and cut to its last rows, so the end
    /// of the input -- where the next character lands -- is always the last
    /// of them. Empty only for a view built without a composer at all,
    /// which the framing draws as the empty prompt line.
    pub input: Vec<String>,
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
    /// Where the keyboard waits inside [`Self::pending_permission`], as a
    /// `(row, column)` offset into those rows -- the first option's digit,
    /// or the question's own head when there is no option to press. Meaning
    /// nothing while `pending_permission` is empty.
    pub permission_answer: (usize, usize),
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

    /// The same panel with `input` as its one composer row -- what a prompt
    /// short enough to fit the panel's width comes out as.
    #[must_use]
    pub fn with_input(self, input: impl Into<String>) -> Self {
        self.with_input_rows(vec![input.into()])
    }

    /// The same panel with `rows` as its composer, already wrapped and cut
    /// by [`AiPanelState::view`](crate::native::ai_panel::AiPanelState::view).
    #[must_use]
    pub fn with_input_rows(self, rows: Vec<String>) -> Self {
        Self {
            input: rows,
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

    /// The same panel answering its pending question at `cell`.
    #[must_use]
    pub fn with_permission_answer(self, cell: (usize, usize)) -> Self {
        Self {
            permission_answer: cell,
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

    /// The same panel showing `rows` as its open review's summary.
    #[must_use]
    pub fn with_review(self, rows: Vec<Vec<Span>>) -> Self {
        Self {
            review: rows,
            ..self
        }
    }

    /// Where the next character typed lands in the composer rows this view
    /// carries -- see
    /// [`composer_cursor_of`](crate::native::ai_panel::composer_cursor_of),
    /// which is its one definition.
    ///
    /// Asked of the painted view rather than of the state, so the caret a
    /// frame places and the rows that frame painted are one derivation: a
    /// second wrap of the same input is a second chance to disagree about
    /// which row the last character is on.
    #[must_use]
    pub fn composer_cursor(&self) -> (usize, usize) {
        crate::native::ai_panel::composer_cursor_of(&self.input)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    /// Every interior height a two-choice confirm can be given. Short of
    /// every row, rows go to the input line, the choices on one row, one
    /// question row, the rule, then the rest of the question, so an
    /// interior of three rows or more always shows the question. A cut
    /// question's last row is filled from the rest of the question and ends
    /// in `…` inside the row's width.
    #[test]
    fn a_short_interior_shows_a_question_row_before_the_rule() {
        let view = PromptView::new("Confirm", "one two three four five six seven")
            .with_choices(vec!["Yes".to_string(), "No".to_string()]);
        assert_eq!(
            view.message_rows(10),
            ["one two", "three four", "five six", "seven"]
        );
        let inline = PromptChoices::Inline;
        let stacked = PromptChoices::Stacked;
        let expected: [(u16, PromptChoices, bool, &[&str]); 8] = [
            (2, inline, false, &[]),
            (3, inline, false, &["one two t…"]),
            (4, inline, true, &["one two t…"]),
            (5, inline, true, &["one two", "three fou…"]),
            (6, inline, true, &["one two", "three four", "five six…"]),
            (
                7,
                inline,
                true,
                &["one two", "three four", "five six", "seven"],
            ),
            (
                8,
                stacked,
                true,
                &["one two", "three four", "five six", "seven"],
            ),
            (
                9,
                stacked,
                true,
                &["one two", "three four", "five six", "seven"],
            ),
        ];
        for height in [0, 1] {
            assert_eq!(
                view.fit(10, height),
                PromptFit::default(),
                "height {height}"
            );
        }
        for (height, choices, rule, message) in expected {
            let fit = view.fit(10, height);
            assert_eq!((fit.choices, fit.rule), (choices, rule), "height {height}");
            assert_eq!(fit.message, message, "height {height}");
        }
    }

    /// Three choices stack only at an interior of seven rows, where every
    /// row of the full layout fits. At three to six rows they sit on one
    /// row under a question row, and from four rows the rule sits between
    /// the two.
    #[test]
    fn three_choices_short_of_the_full_layout_sit_on_one_row_beside_the_rule() {
        let view =
            PromptView::new("Confirm", "one two three four five six seven").with_choices(vec![
                "Yes".to_string(),
                "No".to_string(),
                "Cancel".to_string(),
            ]);
        assert_eq!(
            view.message_rows(20),
            ["one two three four", "five six seven"]
        );
        assert_eq!(view.rows_at(20), 7);
        let inline = PromptChoices::Inline;
        let expected: [(u16, PromptChoices, bool, &[&str]); 5] = [
            (3, inline, false, &["one two three four…"]),
            (4, inline, true, &["one two three four…"]),
            (5, inline, true, &["one two three four", "five six seven"]),
            (6, inline, true, &["one two three four", "five six seven"]),
            (
                7,
                PromptChoices::Stacked,
                true,
                &["one two three four", "five six seven"],
            ),
        ];
        for (height, choices, rule, message) in expected {
            let fit = view.fit(20, height);
            assert_eq!((fit.choices, fit.rule), (choices, rule), "height {height}");
            assert_eq!(fit.message, message, "height {height}");
        }
    }

    /// Choices too wide to sit side by side on the interior show the
    /// selected one alone, or the first when none is selected, so no choice
    /// is cut and the selected one is on screen.
    #[test]
    fn choices_wider_than_the_interior_show_the_selected_one_alone() {
        let view = PromptView::new("Confirm", "Save?").with_choices(vec![
            "Yes".to_string(),
            "No".to_string(),
            "Cancel".to_string(),
        ]);
        // `> Yes   No   Cancel`: three two-cell markers, the choices and
        // two gaps
        assert_eq!(view.fit(19, 3).choices, PromptChoices::Inline);
        assert_eq!(view.fit(9, 3).choices, PromptChoices::One(0));
        assert_eq!(view.fit(18, 2).choices, PromptChoices::One(0));
        let cancel = view.clone().with_selected(2);
        assert_eq!(cancel.fit(9, 3).choices, PromptChoices::One(2));
        assert_eq!(cancel.fit(9, 2).choices, PromptChoices::One(2));
        let busy = super::super::supervision::EngineBusyState::new(
            super::super::supervision::WedgeKind::ReadSide,
            super::super::supervision::SinceStamp::default(),
        )
        .view();
        let width = busy.choices.iter().map(|c| c.len() + 2).max().unwrap_or(0);
        let fit = busy.fit(u16::try_from(width).unwrap(), 3);
        assert_eq!(fit.choices, PromptChoices::One(0));
    }

    /// A free-text prompt keeps its input line on every interior of a row
    /// or more.
    #[test]
    fn a_free_text_prompt_keeps_its_input_line_on_one_row() {
        let view = PromptView::new("Rename", "new name");
        assert_eq!(view.fit(9, 0), PromptFit::default());
        let one = view.fit(9, 1);
        assert!(one.message.is_empty() && one.choices == PromptChoices::Stacked);
    }

    /// An interior zero or one cell wide has room for the mark alone on a
    /// cut row.
    #[test]
    fn a_cut_row_one_cell_wide_or_less_is_the_mark() {
        let view = PromptView::new("Confirm", "one two three")
            .with_choices(vec!["Yes".to_string(), "No".to_string()]);
        for width in [0, 1] {
            assert_eq!(view.fit(width, 4).message, ["…"], "width {width}");
        }
    }

    /// A wide glyph that would straddle the cut is dropped, and the mark
    /// takes the cell after the last glyph that fits whole.
    #[test]
    fn a_cut_row_drops_a_wide_glyph_that_straddles_the_mark() {
        let view = PromptView::new("Confirm", "a日本語の質問")
            .with_choices(vec!["Yes".to_string(), "No".to_string()]);
        assert_eq!(view.message_rows(9), ["a日本語の", "質問"]);
        let fit = view.fit(9, 4);
        assert_eq!(fit.message, ["a日本語…"]);
        assert_eq!(super::super::text::text_width(&fit.message[0]), 8);
    }

    /// A cut row that the wrap leaves blank, a line break in the question,
    /// is filled from the line after it.
    #[test]
    fn a_blank_cut_row_is_filled_from_the_next_line() {
        let view = PromptView::new("Confirm", "one\n\ntwo three four five")
            .with_choices(vec!["Yes".to_string(), "No".to_string()]);
        assert_eq!(view.message_rows(9)[..2], ["one", ""]);
        assert_eq!(view.fit(9, 5).message, ["one", "two thre…"]);
    }

    /// A prompt with no choices gives the question every row past the
    /// input line and draws the rule under the input line only on a row
    /// the question leaves spare.
    #[test]
    fn a_prompt_with_no_choices_gives_the_rule_a_spare_row_only() {
        let view = PromptView::new("Rename", "one two three four five six seven");
        for height in 0..8u16 {
            let fit = view.fit(9, height);
            if height == 0 {
                assert_eq!(fit, PromptFit::default());
                continue;
            }
            let room = usize::from(height) - 1;
            assert_eq!(fit.choices, PromptChoices::Stacked, "height {height}");
            assert_eq!(fit.message.len(), room.min(4), "height {height}");
            assert_eq!(fit.rule, room > 4, "height {height}");
            if room == 1 {
                assert_eq!(fit.message, ["one two…"], "height {height}");
            }
        }
    }

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

    /// A colorscheme's `reverse` on a diff group belongs to diff mode's
    /// whole-line paint, and no role borrowing that group inherits it --
    /// including one added later, since the answer is read off the
    /// `chrome_group` mapping rather than off a list kept beside it. Every
    /// other role keeps whatever its group says, which is what leaves a
    /// selected tab and a search match reversed the way nvim draws them.
    #[test]
    fn no_role_borrowing_a_diff_group_inherits_its_reverse() {
        for role in [
            StyleRole::GitAdded,
            StyleRole::GitModified,
            StyleRole::GitDeleted,
            StyleRole::DiffAdded,
            StyleRole::DiffRemoved,
        ] {
            assert!(
                !role.keeps_group_reverse(),
                "{role:?} resolves through {:?} and must take its color alone",
                role.chrome_group()
            );
        }
        for role in [
            StyleRole::Plain,
            StyleRole::Match,
            StyleRole::Mode,
            StyleRole::Title,
            StyleRole::AiUser,
        ] {
            assert!(
                role.keeps_group_reverse(),
                "{role:?} names no diff group, so its own group decides"
            );
        }
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
    fn tree_git_icons_split_staged_from_unstaged_and_draw_in_order() {
        let glyphs = |xy: &str| {
            GitIcons::from_xy(xy)
                .iter()
                .map(GitIcon::glyph)
                .collect::<String>()
        };
        assert_eq!(glyphs("MM"), "\u{2713}\u{2717}");
        assert_eq!(glyphs("M."), "\u{2713}");
        assert_eq!(glyphs(".M"), "\u{2717}");
        assert_eq!(glyphs("RM"), "\u{2717}\u{279c}");
        assert_eq!(glyphs("DU"), "\u{f458}\u{e727}");
        assert_eq!(glyphs("??"), "\u{2605}");
        assert_eq!(glyphs("!!"), "\u{25cc}");

        let folded = GitIcons::of(&[GitIcon::Untracked])
            .union(GitIcons::from_xy(".M"))
            .union(GitIcons::from_xy("A."));
        assert_eq!(
            folded.iter().collect::<Vec<_>>(),
            [GitIcon::Staged, GitIcon::Unstaged, GitIcon::Untracked]
        );
        assert_eq!(GitIcon::Staged.style_role(), StyleRole::GitStaged);
        assert_eq!(GitIcon::Unmerged.style_role(), StyleRole::GitStaged);
        assert_eq!(GitIcon::Unstaged.style_role(), StyleRole::GitDirty);
        assert_eq!(GitIcon::Deleted.style_role(), StyleRole::GitDirty);
        assert_eq!(GitIcon::Renamed.style_role(), StyleRole::GitNew);
        assert_eq!(GitIcon::Untracked.style_role(), StyleRole::GitNew);
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
