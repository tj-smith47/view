//! Framing for native overlay layers: the border, the padding, and the
//! title bar drawn around a rect the caller already resolved.
//!
//! The rect itself is not this module's to compute. It comes from
//! `view_core::native::geometry`, through
//! `view_core::model::Model::overlay_rect`, which is the same resolution a
//! mouse click hit-tests against; a second rect derived here could disagree
//! with the one input routes by, and a click would land on a frame the user
//! is not looking at.
//!
//! What is this module's: turning one overlay's paint-facing view into the
//! exact rows that cover its rect. [`rows`] returns them as plain strings,
//! one per rect row and each exactly as wide as the rect, so the terminal
//! painter and the oracle's rasterizer draw the same picture from one
//! layout pass instead of two hand-kept-in-sync ones. Style (color,
//! reverse, bold) stays with the painter, which is the only layer that
//! knows the terminal's probed color capability.

use unicode_width::UnicodeWidthChar;
use view_core::model::Tier;
use view_core::native::views::{
    PaletteRow, PaletteView, PickerView, PromptView, StatuslineView, TreeRow, TreeView,
};

use crate::{Layer, LayerKind, Rect};

/// The horizontal edge glyph shared by every box-drawing border. Named once
/// because [`BorderSet::ROUNDED`] and [`BorderSet::PLAIN`] differ only in
/// their corners: a second literal would let the two drift into visibly
/// different edges for no reason a user could name.
const LINE_H: char = '─';
/// The vertical edge glyph shared by every box-drawing border; see
/// [`LINE_H`].
const LINE_V: char = '│';

/// The character a prompt line opens with, on every tier: one ASCII cell,
/// so the line reads the same whether or not the terminal renders
/// box-drawing glyphs.
const PROMPT_MARK: char = '>';

/// The mark separating the columns of a row that carries more than one
/// (a statusline's three segments, a palette row's label and binding).
///
/// A control character, which never survives into painted output:
/// [`fit`] is the only reader, and it turns each mark into the exact run of
/// spaces that pushes the following column where it belongs. Column
/// *spacing* cannot be decided where these rows are built, because it
/// depends on the interior width the frame leaves.
const ALIGN: char = '\u{1}';

/// The six glyphs an overlay's frame is drawn from.
///
/// A charset rather than a tier: painting is handed the glyphs to use, so
/// the tier decision happens once, where the capabilities are known, and
/// every consumer of a [`Layer`] draws the same frame without re-deriving
/// it. That includes consumers with no terminal at all, which is what lets
/// a golden snapshot depict the exact frame a terminal receives.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BorderSet {
    pub top_left: char,
    pub top_right: char,
    pub bottom_left: char,
    pub bottom_right: char,
    pub horizontal: char,
    pub vertical: char,
}

impl BorderSet {
    /// Rounded box-drawing corners: the frame a terminal with the full
    /// capability set gets.
    pub const ROUNDED: Self = Self {
        top_left: '╭',
        top_right: '╮',
        bottom_left: '╰',
        bottom_right: '╯',
        horizontal: LINE_H,
        vertical: LINE_V,
    };

    /// Square box-drawing corners: the same layout and the same edges as
    /// [`BorderSet::ROUNDED`], without the rounded joins. Degradation here
    /// is a deliberate second look, not a fallback apology: the box keeps
    /// its size, its title, and its content rows, so nothing reflows
    /// between tiers.
    pub const PLAIN: Self = Self {
        top_left: '┌',
        top_right: '┐',
        bottom_left: '└',
        bottom_right: '┘',
        horizontal: LINE_H,
        vertical: LINE_V,
    };

    /// Pure ASCII: the frame a terminal that cannot be trusted with
    /// box-drawing glyphs gets. Every glyph is one cell wide in every font,
    /// so a frame drawn with this set can never straddle a column boundary.
    pub const ASCII: Self = Self {
        top_left: '+',
        top_right: '+',
        bottom_left: '+',
        bottom_right: '+',
        horizontal: '-',
        vertical: '|',
    };

    /// The border charset for `tier`.
    ///
    /// Tier is the right predicate here, unlike the synchronized-update and
    /// color decisions that gate on their own probed bit: nothing in the
    /// capability probe reports whether a terminal renders box-drawing
    /// glyphs, so the coarse tier is the only signal that exists for this
    /// choice rather than a stand-in for a finer one.
    #[must_use]
    pub fn for_tier(tier: Tier) -> Self {
        match tier {
            Tier::Full => Self::ROUNDED,
            Tier::Standard => Self::PLAIN,
            Tier::Basic => Self::ASCII,
            // `Tier` is `#[non_exhaustive]`; a tier this build predates
            // draws the frame every terminal can render rather than one it
            // may not.
            _ => Self::ASCII,
        }
    }
}

/// One native overlay [`Layer`]: `kind`'s content framed by `borders`
/// inside `rect`.
///
/// The single constructor for a framed layer, so every native feature
/// places its overlay through the same geometry and gets the same border,
/// padding, and title treatment. A feature computing its own rect from the
/// terminal size instead would be one more independent clamp to get wrong.
#[must_use]
pub fn framed(rect: Rect, kind: LayerKind, borders: BorderSet) -> Layer {
    Layer {
        rect,
        kind,
        borders: Some(borders),
    }
}

/// The painted rows of one framed overlay, plus which of them holds the
/// selection.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Rows {
    /// One string per rect row, top to bottom. Each is exactly the rect's
    /// width in terminal display cells, padded with spaces, so a painter
    /// can blit a row without measuring it and no cell of the rect keeps
    /// whatever was underneath.
    pub lines: Vec<String>,
    /// Index into `lines` of the row carrying the overlay's selection, or
    /// `None` when nothing is selected. Returned alongside the rows rather
    /// than recomputed by the painter, because it depends on the same
    /// scroll window the rows were cut from.
    pub selected: Option<u16>,
    /// Whether the first and last row, and the first and last cell of every
    /// row between them, are the frame's own glyphs.
    ///
    /// `false` for a rect too small to hold a frame, whose rows are content
    /// edge to edge. A painter styling the frame differently from the
    /// content needs to know which cells are which, and re-deriving the
    /// same size predicate on its side is how the two come to disagree
    /// about a degenerate rect.
    pub framed: bool,
}

/// Lays `kind` out into a `width` by `height` rect framed by `borders`.
///
/// Total: any width, any height, and any view content yield exactly
/// `height` rows of exactly `width` display cells. A layer kind that is not
/// a native overlay yields no rows at all, so a painter that reaches here
/// with an engine grid or a toast paints nothing rather than blanking the
/// rect that layer owns.
///
/// A rect under two cells on either axis has no distinct edge cells to draw
/// and yields plain content rows, matching what the message toast does at
/// the same size rather than stacking corner glyphs on top of each other.
#[must_use]
pub fn rows(width: u16, height: u16, kind: &LayerKind, borders: BorderSet) -> Rows {
    let Some(body) = body(kind) else {
        return Rows::default();
    };
    if width == 0 || height == 0 {
        return Rows::default();
    }
    if width < 2 || height < 2 {
        return lay_out(&body, width, height, borders);
    }

    // one blank column inside each vertical edge, dropped entirely when the
    // rect is too narrow to spare it: padding that eats the last two cells
    // of content is worse than an unpadded box
    let pad = u16::from(width >= 6);
    let text_width = width
        .saturating_sub(2)
        .saturating_sub(pad.saturating_mul(2));
    let interior = height - 2;
    let laid = lay_out(&body, text_width, interior, borders);

    let mut lines = Vec::with_capacity(usize::from(height));
    lines.push(top_edge(width, borders, &body.title));
    let blank = " ".repeat(usize::from(pad));
    for row in 0..interior {
        let text = laid.lines.get(usize::from(row)).map_or("", String::as_str);
        let mut line = String::new();
        line.push(borders.vertical);
        line.push_str(&blank);
        line.push_str(text);
        line.push_str(&blank);
        line.push(borders.vertical);
        lines.push(line);
    }
    lines.push(bottom_edge(width, borders));
    Rows {
        lines,
        selected: laid.selected.map(|r| r.saturating_add(1)),
        framed: true,
    }
}

/// The frame's top row: the two corners with the title, if it fits, set
/// into the horizontal run between them.
fn top_edge(width: u16, borders: BorderSet, title: &str) -> String {
    let span = width - 2;
    let mut middle = String::new();
    let label = if title.is_empty() {
        String::new()
    } else {
        format!(" {title} ")
    };
    let label_cells = cells(&label);
    // the title needs a horizontal glyph on each side of it to read as set
    // into the edge rather than as replacing it
    if label_cells > 0 && label_cells.saturating_add(2) <= span {
        middle.push(borders.horizontal);
        middle.push_str(&label);
        for _ in 0..span - label_cells - 1 {
            middle.push(borders.horizontal);
        }
    } else {
        for _ in 0..span {
            middle.push(borders.horizontal);
        }
    }
    format!("{}{middle}{}", borders.top_left, borders.top_right)
}

/// The frame's bottom row: two corners and an unbroken horizontal run.
fn bottom_edge(width: u16, borders: BorderSet) -> String {
    let mut line = String::new();
    line.push(borders.bottom_left);
    for _ in 0..width - 2 {
        line.push(borders.horizontal);
    }
    line.push(borders.bottom_right);
    line
}

/// One interior row before it is fitted to a width.
enum Line {
    /// Literal content, possibly carrying [`ALIGN`] marks.
    Text(String),
    /// A horizontal rule spanning the full interior width, drawn from the
    /// frame's own edge glyph. Kept as an intent rather than a built string
    /// because the width it spans is only known once the frame is sized.
    Rule,
}

/// An overlay's content before any frame or window is applied: rows that
/// always show (a prompt line, a rule) and rows that scroll under them.
struct Body {
    title: String,
    header: Vec<Line>,
    items: Vec<String>,
    selected: Option<usize>,
}

/// Cuts `body` down to exactly `height` rows of exactly `width` cells: the
/// header first, then a window over the items that keeps the selection on
/// screen.
///
/// The window is the smallest scroll that shows the selection: it stays at
/// the top of the list while the selection is already visible, and
/// otherwise moves down just far enough. Anchoring the window on the
/// selection instead (centering it, or starting from it) would jump the
/// list on every cursor move.
fn lay_out(body: &Body, width: u16, height: u16, borders: BorderSet) -> Rows {
    let mut lines: Vec<String> = Vec::with_capacity(usize::from(height));
    for line in &body.header {
        if lines.len() >= usize::from(height) {
            break;
        }
        lines.push(match line {
            Line::Text(text) => fit(text, width),
            Line::Rule => borders.horizontal.to_string().repeat(usize::from(width)),
        });
    }
    let header_rows = lines.len();
    let item_rows = usize::from(height).saturating_sub(header_rows);
    // an index past the end selects nothing rather than being clamped onto
    // a row the feature never chose
    let selected = body.selected.filter(|i| *i < body.items.len());
    let first = match selected {
        Some(i) if item_rows > 0 && i >= item_rows => i + 1 - item_rows,
        _ => 0,
    };
    for (offset, item) in body.items.iter().skip(first).take(item_rows).enumerate() {
        let marker = if selected == Some(first + offset) {
            "> "
        } else {
            "  "
        };
        lines.push(fit(&format!("{marker}{item}"), width));
    }
    while lines.len() < usize::from(height) {
        lines.push(fit("", width));
    }
    let selected_row = selected
        .filter(|i| item_rows > 0 && *i >= first)
        .and_then(|i| u16::try_from(header_rows + (i - first)).ok());
    Rows {
        lines,
        selected: selected_row,
        framed: false,
    }
}

/// The [`Body`] for a native overlay layer, or `None` for a layer kind that
/// is not a native overlay at all.
fn body(kind: &LayerKind) -> Option<Body> {
    match kind {
        LayerKind::Picker(view) => Some(picker_body(view)),
        LayerKind::Tree(view) => Some(tree_body(view)),
        LayerKind::Statusline(view) => Some(statusline_body(view)),
        LayerKind::Prompt(view) => Some(prompt_body(view)),
        LayerKind::Palette(view) => Some(palette_body(view)),
        _ => None,
    }
}

fn picker_body(view: &PickerView) -> Body {
    Body {
        title: view.title.clone(),
        header: vec![
            Line::Text(format!("{PROMPT_MARK} {}", view.query)),
            Line::Rule,
        ],
        items: view.rows.clone(),
        selected: view.selected,
    }
}

fn tree_body(view: &TreeView) -> Body {
    Body {
        title: view.title.clone(),
        header: Vec::new(),
        items: view.rows.iter().map(tree_row_text).collect(),
        selected: view.selected,
    }
}

/// One tree row's text: indentation for its depth, then an expand marker
/// that distinguishes an open directory from a shut one and from a leaf.
fn tree_row_text(row: &TreeRow) -> String {
    let indent = "  ".repeat(usize::from(row.depth));
    let marker = match row.expanded {
        Some(true) => "- ",
        Some(false) => "+ ",
        None => "  ",
    };
    format!("{indent}{marker}{}", row.label)
}

fn statusline_body(view: &StatuslineView) -> Body {
    Body {
        title: view.title.clone(),
        header: vec![Line::Text(format!(
            "{}{ALIGN}{}{ALIGN}{}",
            view.left, view.center, view.right
        ))],
        items: Vec::new(),
        selected: None,
    }
}

fn prompt_body(view: &PromptView) -> Body {
    Body {
        title: view.title.clone(),
        // the same shape every other overlay with a text field uses: the
        // typed line above the rule, the selectable rows below it. The
        // prompt mark and the selection marker are the same glyph, so an
        // input line sharing a side of the rule with the choices reads as
        // a second selected row
        header: vec![
            Line::Text(view.message.clone()),
            Line::Text(format!("{PROMPT_MARK} {}", view.input)),
            Line::Rule,
        ],
        items: view.choices.clone(),
        selected: view.selected,
    }
}

fn palette_body(view: &PaletteView) -> Body {
    Body {
        title: view.title.clone(),
        header: vec![
            Line::Text(format!("{PROMPT_MARK} {}", view.query)),
            Line::Rule,
        ],
        items: view.rows.iter().map(palette_row_text).collect(),
        selected: view.selected,
    }
}

/// One palette row's text: the command's name, then its binding pushed
/// against the row's right edge.
fn palette_row_text(row: &PaletteRow) -> String {
    match &row.binding {
        Some(binding) => format!("{}{ALIGN}{binding}", row.label),
        None => row.label.clone(),
    }
}

/// Fits `text` to exactly `width` display cells: alignment marks expand to
/// the spacing they call for, then the result is truncated or space-padded.
///
/// Display cells, never characters: a wide (CJK) glyph occupies two
/// columns, and a row measured in characters would leave the frame's right
/// edge one column out of place for every wide glyph on it. A glyph that
/// would straddle the last column is dropped rather than half-drawn, which
/// is what the terminal painter does with one too.
fn fit(text: &str, width: u16) -> String {
    let expanded = expand_alignment(text, width);
    let mut out = String::new();
    let mut used = 0_u16;
    for ch in expanded.chars() {
        let w = cell_width(ch);
        if used.saturating_add(w) > width {
            break;
        }
        out.push(ch);
        used = used.saturating_add(w);
    }
    for _ in used..width {
        out.push(' ');
    }
    out
}

/// Replaces each [`ALIGN`] mark in `text` with the spaces that distribute
/// its columns across `width`.
///
/// One mark yields a left column and a right column flush against the far
/// edge. Two marks yield left, centered, and right, with the middle column
/// centered on the row itself rather than on the space left between its
/// neighbours, so a long left segment does not drag it off centre. Columns
/// wider than the row keep a single separating space and let [`fit`]
/// truncate, rather than producing a negative gap.
fn expand_alignment(text: &str, width: u16) -> String {
    let parts: Vec<&str> = text.split(ALIGN).collect();
    match parts.as_slice() {
        [left, right] => {
            let gap = gap_before_right(cells(left), cells(right), width);
            format!("{left}{}{right}", " ".repeat(usize::from(gap)))
        }
        [left, center, right] => {
            let start = width.saturating_sub(cells(center)) / 2;
            let lead = min_gap(start.saturating_sub(cells(left)), center);
            let placed = cells(left)
                .saturating_add(lead)
                .saturating_add(cells(center));
            let gap = gap_before_right(placed, cells(right), width);
            format!(
                "{left}{}{center}{}{right}",
                " ".repeat(usize::from(lead)),
                " ".repeat(usize::from(gap))
            )
        }
        _ => text.to_string(),
    }
}

/// The run of spaces that pushes a `right`-wide column flush against the
/// far edge of a `width`-wide row already carrying `placed` cells.
fn gap_before_right(placed: u16, right: u16, width: u16) -> u16 {
    min_gap(
        width.saturating_sub(right).saturating_sub(placed),
        if right == 0 { "" } else { " " },
    )
}

/// `gap`, or one cell when the row has no room left and the column that
/// follows has content: two columns running together read as one word, and
/// a single space keeps them apart for the truncation that follows.
fn min_gap(gap: u16, next: &str) -> u16 {
    if next.is_empty() {
        gap
    } else {
        gap.max(1)
    }
}

/// `text`'s width in terminal display cells, saturating rather than
/// wrapping on a string too wide to count in a `u16`.
fn cells(text: &str) -> u16 {
    text.chars()
        .map(cell_width)
        .fold(0_u16, |acc, c| acc.saturating_add(c))
}

/// One character's width in terminal display cells. A control character or
/// combining mark occupies none.
fn cell_width(ch: char) -> u16 {
    u16::try_from(ch.width().unwrap_or(0)).unwrap_or(u16::MAX)
}

#[cfg(test)]
mod tests;
