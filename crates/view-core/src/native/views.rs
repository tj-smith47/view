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
    /// The candidate rows, best match first, already formatted for display.
    pub rows: Vec<String>,
    /// Index into `rows` of the highlighted candidate, or `None` when the
    /// query matched nothing. An index past the end of `rows` highlights
    /// nothing rather than being clamped onto a row the feature did not
    /// choose.
    pub selected: Option<usize>,
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

    /// The same view showing `rows` as its candidates.
    #[must_use]
    pub fn with_rows(self, rows: Vec<String>) -> Self {
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
}

impl TreeRow {
    /// A leaf row at `depth`.
    #[must_use]
    pub fn leaf(depth: u16, label: impl Into<String>) -> Self {
        Self {
            depth,
            label: label.into(),
            expanded: None,
        }
    }

    /// A directory row at `depth`, open when `expanded`.
    #[must_use]
    pub fn dir(depth: u16, label: impl Into<String>, expanded: bool) -> Self {
        Self {
            depth,
            label: label.into(),
            expanded: Some(expanded),
        }
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
    pub left: String,
    /// Centered on the row, as far as the left and right segments allow.
    pub center: String,
    /// Flush against the row's right edge.
    pub right: String,
    /// The overlay's title, drawn into its top border. Empty for the
    /// ordinary bar, which is identified by its position rather than by a
    /// label.
    pub title: String,
}

impl StatuslineView {
    /// A bar with the three segments given.
    #[must_use]
    pub fn new(
        left: impl Into<String>,
        center: impl Into<String>,
        right: impl Into<String>,
    ) -> Self {
        Self {
            left: left.into(),
            center: center.into(),
            right: right.into(),
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
        assert_eq!(picker.rows, vec!["src/main.rs".to_string()]);
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
