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
//! exact rows that cover its rect. [`rows`] returns them as styled spans,
//! one row per rect row and each row's spans exactly as wide as the rect,
//! so the terminal painter and the oracle's rasterizer draw the same
//! picture from one layout pass instead of two hand-kept-in-sync ones.
//! Resolving a span's role to a concrete color stays with the painter,
//! which is the only layer that knows the terminal's probed color
//! capability; this module only decides which text carries which role.

use unicode_width::UnicodeWidthChar;
use view_core::model::Tier;
use view_core::native::views::{
    PaletteRow, PaletteView, PickerView, PromptView, Span, StatuslineView, TreeRow, TreeView,
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
    /// One span-vec per rect row, top to bottom. Each row's spans join to
    /// exactly the rect's width in terminal display cells, padded with a
    /// trailing plain space span, so a painter can blit a row without
    /// measuring it and no cell of the rect keeps whatever was underneath.
    /// A content span carries whatever style role its producer assigned
    /// (e.g. a statusline's diagnostic glyph); frame chrome built in this
    /// module (borders, padding, the selection marker) is always a plain
    /// span. Use [`line_text`] where only the joined text is needed.
    pub lines: Vec<Vec<Span>>,
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
        return content_rows(kind, &body, width, height, borders);
    }

    // one blank column inside each vertical edge, dropped entirely when the
    // rect is too narrow to spare it: padding that eats the last two cells
    // of content is worse than an unpadded box
    let pad = u16::from(width >= 6);
    let text_width = width
        .saturating_sub(2)
        .saturating_sub(pad.saturating_mul(2));
    let interior = height - 2;
    let laid = content_rows(kind, &body, text_width, interior, borders);

    let mut lines: Vec<Vec<Span>> = Vec::with_capacity(usize::from(height));
    lines.push(vec![Span::plain(top_edge(width, borders, &body.title))]);
    let blank = " ".repeat(usize::from(pad));
    for row in 0..interior {
        let content = laid
            .lines
            .get(usize::from(row))
            .cloned()
            .unwrap_or_default();
        let mut line: Vec<Span> = vec![Span::plain(format!("{}{blank}", borders.vertical))];
        line.extend(content);
        line.push(Span::plain(format!("{blank}{}", borders.vertical)));
        lines.push(line);
    }
    lines.push(vec![Span::plain(bottom_edge(width, borders))]);
    Rows {
        lines,
        selected: laid.selected.map(|r| r.saturating_add(1)),
        framed: true,
    }
}

/// The lowest interior width, in display cells, a picker's preview pane is
/// worth splitting off a column for. Below this, the results list and a
/// sliver of preview would both be unreadable, so the picker falls back to
/// the single results column it had before a preview existed rather than
/// painting an illegible pane.
const MIN_PREVIEW_SPLIT_WIDTH: u16 = 20;

/// Cuts `kind`'s interior into exactly `height` rows of exactly `width`
/// cells, the same total [`rows`] guarantees for its own caller: a picker
/// carrying preview lines gets a second, right-hand column for them (see
/// [`picker_split_rows`]); every other layer kind, and a picker with no
/// preview to show, still goes through the single-column [`lay_out`] this
/// module always used.
fn content_rows(
    kind: &LayerKind,
    body: &Body,
    width: u16,
    height: u16,
    borders: BorderSet,
) -> Rows {
    if let LayerKind::Picker(view) = kind {
        if !view.preview.is_empty() {
            return picker_split_rows(view, body, width, height, borders);
        }
    }
    lay_out(body, width, height, borders)
}

/// Splits a picker's interior into two columns separated by one frame-glyph
/// rule: the results list (left, `body`, unchanged from the no-preview
/// layout) and the RPC-read buffer preview (right, `view.preview`) -- the
/// same split-region shape the plan assigned this feature, painted by
/// reusing [`lay_out`] and [`Line`]'s existing per-row column machinery
/// rather than inventing a second layout primitive. Both painters
/// (`view-tui`'s real terminal backend and `view-oracle`'s rasterizer)
/// consume this module's [`rows`] for every other overlay already; a picker
/// preview reaches them the same way, with no separate wiring on either
/// side.
///
/// Falls back to the single-column layout below [`MIN_PREVIEW_SPLIT_WIDTH`]:
/// a rect too narrow to hold two legible columns shows the results list
/// alone, the same degrade an unopened preview already produces.
fn picker_split_rows(
    view: &PickerView,
    body: &Body,
    width: u16,
    height: u16,
    borders: BorderSet,
) -> Rows {
    if width < MIN_PREVIEW_SPLIT_WIDTH {
        return lay_out(body, width, height, borders);
    }
    // three-fifths to the results list, the rest (less the separator
    // column) to the preview: the list's rows are what a picker is
    // navigated by, so it keeps the larger share
    let list_width = width * 3 / 5;
    let preview_width = width - list_width - 1;

    let list = lay_out(body, list_width, height, borders);
    let preview_body = Body {
        title: String::new(),
        header: Vec::new(),
        items: view
            .preview
            .iter()
            .cloned()
            .map(plain_spans)
            .map(Line::Text)
            .collect(),
        selected: None,
    };
    let preview = lay_out(&preview_body, preview_width, height, borders);

    let separator = Span::plain(borders.vertical.to_string());
    let mut lines: Vec<Vec<Span>> = Vec::with_capacity(usize::from(height));
    for row in 0..usize::from(height) {
        let mut line = list.lines.get(row).cloned().unwrap_or_default();
        line.push(separator.clone());
        line.extend(preview.lines.get(row).cloned().unwrap_or_default());
        lines.push(line);
    }
    Rows {
        lines,
        selected: list.selected,
        framed: false,
    }
}

/// The frame's top row: the two corners with the title, if it fits, set
/// into the horizontal run between them.
///
/// The title is blanked here rather than where a [`Body`] is built, for the
/// same reason [`fit`] blanks a column: this is the one place a title
/// becomes painted cells, so a title carrying a control character cannot
/// reach a row no matter which builder produced it or what a later feature
/// puts in a view's title.
fn top_edge(width: u16, borders: BorderSet, title: &str) -> String {
    let span = width - 2;
    let mut middle = String::new();
    let clean = sanitized(title);
    let label = if clean.trim().is_empty() {
        String::new()
    } else {
        format!(" {clean} ")
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
///
/// A row's columns are separate values, never runs of text inside one
/// string with a marker byte between them. Which variant built the row is
/// the only thing that decides how its parts are placed, so no byte of
/// feature-supplied text -- a filename holding a control character, a
/// label copied out of a buffer -- can name a layout. Column *spacing*
/// cannot be decided where these rows are built anyway, because it depends
/// on the interior width the frame leaves.
enum Line {
    /// One column, flush with the row's left edge.
    Text(Vec<Span>),
    /// Two columns: the first flush left, the second against the right
    /// edge.
    Split(Vec<Span>, Vec<Span>),
    /// Three columns: the first flush left, the second centered on the row
    /// itself, the third against the right edge.
    Spread(Vec<Span>, Vec<Span>, Vec<Span>),
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
    /// The scrolling rows, each carrying the selection marker once
    /// [`lay_out`] knows which of them is selected.
    items: Vec<Line>,
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
    let mut lines: Vec<Vec<Span>> = Vec::with_capacity(usize::from(height));
    for line in &body.header {
        if lines.len() >= usize::from(height) {
            break;
        }
        lines.push(fit(line, width, borders));
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
        lines.push(fit(&marked(item, marker), width, borders));
    }
    while lines.len() < usize::from(height) {
        lines.push(vec![Span::plain(" ".repeat(usize::from(width)))]);
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
            Line::Text(plain_spans(format!("{PROMPT_MARK} {}", view.query))),
            Line::Rule,
        ],
        items: view.rows.iter().cloned().map(Line::Text).collect(),
        selected: view.selected,
    }
}

fn tree_body(view: &TreeView) -> Body {
    Body {
        title: view.title.clone(),
        header: Vec::new(),
        items: view
            .rows
            .iter()
            .map(tree_row_text)
            .map(plain_spans)
            .map(Line::Text)
            .collect(),
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
        header: vec![Line::Spread(
            view.left.clone(),
            view.center.clone(),
            view.right.clone(),
        )],
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
            Line::Text(plain_spans(view.message.clone())),
            Line::Text(plain_spans(format!("{PROMPT_MARK} {}", view.input))),
            Line::Rule,
        ],
        items: view
            .choices
            .iter()
            .cloned()
            .map(plain_spans)
            .map(Line::Text)
            .collect(),
        selected: view.selected,
    }
}

fn palette_body(view: &PaletteView) -> Body {
    Body {
        title: view.title.clone(),
        header: vec![
            Line::Text(plain_spans(format!("{PROMPT_MARK} {}", view.query))),
            Line::Rule,
        ],
        items: view.rows.iter().map(palette_row_line).collect(),
        selected: view.selected,
    }
}

/// One palette row: the command's name, plus its binding as a second
/// column pushed against the row's right edge when it has one.
fn palette_row_line(row: &PaletteRow) -> Line {
    match &row.binding {
        Some(binding) => Line::Split(plain_spans(row.label.clone()), plain_spans(binding.clone())),
        None => Line::Text(plain_spans(row.label.clone())),
    }
}

/// Wraps plain text as a single [`view_core::native::views::StyleRole::Plain`]
/// span: the honest representation for a column with no per-segment
/// structure to preserve (a picker candidate, a tree row, a prompt choice).
fn plain_spans(text: String) -> Vec<Span> {
    vec![Span::plain(text)]
}

/// `line` with `marker` prefixed to its leftmost column, which is the one
/// flush with the row's left edge in every variant. The marker is always
/// its own leading plain span rather than merged into the first span's
/// text, since it is frame chrome (the selection indicator), not part of
/// whatever role the column's own content carries.
fn marked(line: &Line, marker: &str) -> Line {
    match line {
        Line::Text(spans) => Line::Text(prefixed(spans, marker)),
        Line::Split(left, right) => Line::Split(prefixed(left, marker), right.clone()),
        Line::Spread(left, center, right) => {
            Line::Spread(prefixed(left, marker), center.clone(), right.clone())
        }
        Line::Rule => Line::Rule,
    }
}

/// `marker` as a leading plain span in front of `spans`.
fn prefixed(spans: &[Span], marker: &str) -> Vec<Span> {
    let mut out = vec![Span::plain(marker.to_string())];
    out.extend(spans.iter().cloned());
    out
}

/// Renders `line` as exactly `width` display cells.
///
/// Every column is sanitized before it is measured or placed, so a control
/// character in feature-supplied text becomes a plain space here rather
/// than reaching a consumer: the terminal painter replaces one anyway, but
/// the oracle's rasterizer writes what it is given straight into a screen
/// dump, and a raw control byte in a golden is not a picture of anything.
///
/// Display cells, never characters: a wide (CJK) glyph occupies two
/// columns, and a row measured in characters would leave the frame's right
/// edge one column out of place for every wide glyph on it. A glyph that
/// would straddle the last column is dropped rather than half-drawn, which
/// is what the terminal painter does with one too.
///
/// A centered column is centered on the row itself rather than on the
/// space left between its neighbours, so a long left column does not drag
/// it off centre. Columns wider than the row keep a single separating
/// space and let the clip below truncate, rather than producing a negative
/// gap.
fn fit(line: &Line, width: u16, borders: BorderSet) -> Vec<Span> {
    match line {
        Line::Text(spans) => clip_spans(sanitize_spans(spans), width),
        Line::Split(left, right) => {
            let (left, right) = (sanitize_spans(left), sanitize_spans(right));
            let (left_cells, right_cells) = (span_cells(&left), span_cells(&right));
            let gap = gap_before_right(left_cells, right_cells, width);
            let mut combined = left;
            combined.push(Span::plain(" ".repeat(usize::from(gap))));
            combined.extend(right);
            clip_spans(combined, width)
        }
        Line::Spread(left, center, right) => {
            let (left, center, right) = (
                sanitize_spans(left),
                sanitize_spans(center),
                sanitize_spans(right),
            );
            let (left_cells, center_cells, right_cells) =
                (span_cells(&left), span_cells(&center), span_cells(&right));
            let start = width.saturating_sub(center_cells) / 2;
            let lead = min_gap(start.saturating_sub(left_cells), center_cells == 0);
            let placed = left_cells.saturating_add(lead).saturating_add(center_cells);
            let gap = gap_before_right(placed, right_cells, width);
            let mut combined = left;
            combined.push(Span::plain(" ".repeat(usize::from(lead))));
            combined.extend(center);
            combined.push(Span::plain(" ".repeat(usize::from(gap))));
            combined.extend(right);
            clip_spans(combined, width)
        }
        Line::Rule => vec![Span::plain(
            borders.horizontal.to_string().repeat(usize::from(width)),
        )],
    }
}

/// `spans` with every control character in every span's text replaced by a
/// plain space, so it occupies the one cell a terminal gives it and carries
/// no meaning of its own. Roles are preserved: sanitizing never reclassifies
/// a span, only the bytes inside it.
fn sanitize_spans(spans: &[Span]) -> Vec<Span> {
    spans
        .iter()
        .map(|s| Span::new(sanitized(&s.text), s.role))
        .collect()
}

/// `text` with every control character replaced by a plain space, so it
/// occupies the one cell a terminal gives it and carries no meaning of its
/// own.
fn sanitized(text: &str) -> String {
    text.chars()
        .map(|ch| if ch.is_control() { ' ' } else { ch })
        .collect()
}

/// `spans` truncated or space-padded to exactly `width` display cells,
/// preserving span (and therefore role) boundaries: a span that survives
/// the cut keeps its role, a span cut mid-glyph is dropped from the point
/// of the cut, and any span past the cut is dropped entirely. Padding past
/// the last span's content is always a trailing plain span, never merged
/// into whatever role the row's last real content span carries.
fn clip_spans(spans: Vec<Span>, width: u16) -> Vec<Span> {
    let mut out: Vec<Span> = Vec::new();
    let mut used = 0_u16;
    for span in spans {
        if used >= width {
            break;
        }
        let mut text = String::new();
        for ch in span.text.chars() {
            let w = cell_width(ch);
            if used.saturating_add(w) > width {
                // this span's own room is exhausted, but a span carries no
                // meaning that would make a later one unreachable, so the
                // outer loop still gets a chance to add nothing rather than
                // stopping the walk early
                break;
            }
            text.push(ch);
            used = used.saturating_add(w);
        }
        if !text.is_empty() {
            out.push(Span::new(text, span.role));
        }
    }
    if used < width {
        out.push(Span::plain(" ".repeat(usize::from(width - used))));
    }
    out
}

/// `spans`' text, concatenated in order with no separator: the plain-text
/// view of a row for a caller that needs its content or width but not its
/// per-span style, e.g. sizing a layer or a golden that pins text only.
#[must_use]
pub fn line_text(spans: &[Span]) -> String {
    spans.iter().map(|s| s.text.as_str()).collect()
}

/// `spans`' combined width in terminal display cells.
fn span_cells(spans: &[Span]) -> u16 {
    spans
        .iter()
        .map(|s| cells(&s.text))
        .fold(0_u16, |acc, c| acc.saturating_add(c))
}

/// The run of spaces that pushes a `right`-wide column flush against the
/// far edge of a `width`-wide row already carrying `placed` cells.
fn gap_before_right(placed: u16, right: u16, width: u16) -> u16 {
    min_gap(
        width.saturating_sub(right).saturating_sub(placed),
        right == 0,
    )
}

/// `gap`, or one cell when the row has no room left and the column that
/// follows has content: two columns running together read as one word, and
/// a single space keeps them apart for the truncation that follows.
fn min_gap(gap: u16, next_is_empty: bool) -> u16 {
    if next_is_empty {
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

/// One character's width in terminal display cells. A combining mark
/// occupies none (control characters never reach here: [`sanitized`]
/// replaced each with a space before anything was measured).
fn cell_width(ch: char) -> u16 {
    u16::try_from(ch.width().unwrap_or(0)).unwrap_or(u16::MAX)
}

#[cfg(test)]
mod tests;
