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
use view_core::model::TermCaps;
use view_core::native::devicons::{self, TreeIcons};
use view_core::native::geometry::LIST_MARKER_COLS;
use view_core::native::text::{
    cluster_width, clusters, cut_before_mark, text_width_by, TRUNCATION_MARK,
};
use view_core::native::views::{
    AiPanelView, GitMark, PaletteRow, PaletteView, PickerView, PromptChoices, PromptView, Span,
    StatuslineView, StyleRole, TreeRow, TreeView, INLINE_CHOICE_GAP,
};

use crate::{Layer, LayerKind, OpenSide};

mod selection;
use selection::standout_row;

/// The horizontal edge glyph of the box-drawing border. Named beside
/// [`LINE_V`] because the two are one decision: a frame whose edges came
/// from separate literals could drift into a horizontal and a vertical run
/// drawn at different weights, which no user could name but everyone sees.
const LINE_H: char = '─';
/// The vertical edge glyph of the box-drawing border; see [`LINE_H`].
const LINE_V: char = '│';

/// The character a prompt line opens with, on every tier: one ASCII cell,
/// so the line reads the same whether or not the terminal renders
/// box-drawing glyphs.
///
/// `pub(crate)` so `lib.rs`'s palette cursor placement can measure the same
/// glyph this module paints with, instead of a second literal that could
/// drift from it.
pub(crate) const PROMPT_MARK: char = '>';

/// The glyphs an overlay's frame is drawn from.
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
    /// The mark a frozen toast stack sets into its top box's border run
    /// (spec 7.1, motion rule 5). A charset field rather than a literal at
    /// the painter, so it degrades down exactly the path the corners do: a
    /// terminal that cannot account for `╭` cannot account for `⏸` either,
    /// and one glyph of a frame drawn from a set the terminal does not have
    /// straddles a column boundary and shifts the whole run.
    ///
    /// One residual the box-glyph probe cannot answer for: `⏸` is
    /// `East_Asian_Width=Neutral`, so one cell everywhere the width tables
    /// are followed, but it is also `Emoji=Yes, Emoji_Presentation=No`, and
    /// a terminal that forces emoji presentation on every `Emoji=Yes`
    /// codepoint draws it two cells wide and shifts the run it sits in. The
    /// probe writes a box-drawing glyph, which is `Ambiguous` and a
    /// different question entirely, so it cannot see that -- a report of a
    /// shifted toast border run starts here.
    pub pause: char,
}

impl BorderSet {
    /// Rounded box-drawing corners: the frame every terminal that draws
    /// box-drawing glyphs gets, whatever else it can or cannot do.
    pub const ROUNDED: Self = Self {
        top_left: '╭',
        top_right: '╮',
        bottom_left: '╰',
        bottom_right: '╯',
        horizontal: LINE_H,
        vertical: LINE_V,
        pause: '⏸',
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
        // `|` is this set's own vertical edge and would read as a corner
        // artifact in a horizontal run; `=` is the one-cell ASCII shape
        // closest to the pause glyph's two parallel bars, and it stands out
        // against a run of `-`.
        pause: '=',
    };

    /// The border charset for `caps`: [`TermCaps::unicode_boxes`] and
    /// nothing else.
    ///
    /// Corner glyphs are font coverage, not a terminal capability: a
    /// terminal that draws `┌` draws `╭`, so square corners are no
    /// fallback for rounded ones and view ships no third set. The honest
    /// predicate is whether the terminal accounts for a box-drawing glyph
    /// as one cell, which the box-glyph probe answers directly -- the
    /// color-depth, synchronization and keyboard-protocol answers a tier is
    /// made of never could. A terminal that draws box glyphs on a 16-color
    /// connection gets rounded corners, and a truecolor one that cannot
    /// gets ASCII; `--tier basic` still forces ASCII because
    /// `caps_for_override` sets the bit false, which is the user's own
    /// claim about their terminal rather than an inference from its color
    /// depth.
    #[must_use]
    pub fn for_caps(caps: TermCaps) -> Self {
        if caps.unicode_boxes {
            Self::ROUNDED
        } else {
            Self::ASCII
        }
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
    /// span, with the single exception of the title set into the top edge,
    /// which carries [`StyleRole::Title`]. Use [`line_text`] where only the
    /// joined text is needed.
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

/// `layer`'s rows: [`rows`] laid out on its [`Layer::frame_rect`], less
/// the column [`Layer::open`] leaves out, so each row is exactly as wide
/// as `layer.rect`. Empty for a layer with no frame.
#[must_use]
pub fn layer_rows(layer: &Layer) -> Rows {
    let Some(borders) = layer.borders else {
        return Rows::default();
    };
    let frame = layer.frame_rect();
    let mut laid = rows(frame.width, frame.height, &layer.kind, borders);
    let Some(open) = layer.open else {
        return laid;
    };
    for line in &mut laid.lines {
        match open {
            OpenSide::Left => {
                if let Some(span) = line.iter_mut().find(|span| !span.text.is_empty()) {
                    span.text.remove(0);
                }
            }
            OpenSide::Right => {
                if let Some(span) = line.iter_mut().rev().find(|span| !span.text.is_empty()) {
                    span.text.pop();
                }
            }
        }
    }
    laid
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
///
/// A [`Layer`] is laid out with [`layer_rows`], which accounts for
/// [`Layer::open`].
#[must_use]
pub fn rows(width: u16, height: u16, kind: &LayerKind, borders: BorderSet) -> Rows {
    if width == 0 || height == 0 {
        return Rows::default();
    }
    let (text_width, interior) = interior_size(width, height);
    if width < 2 || height < 2 {
        let Some(body) = body(kind, text_width, interior) else {
            return Rows::default();
        };
        return content_rows(kind, &body, text_width, interior, borders);
    }

    // one blank column inside each vertical edge, dropped entirely when the
    // rect is too narrow to spare it: padding that eats the last two cells
    // of content is worse than an unpadded box
    let pad = u16::from(width >= 6);
    let Some(body) = body(kind, text_width, interior) else {
        return Rows::default();
    };
    let laid = content_rows(kind, &body, text_width, interior, borders);

    let mut lines: Vec<Vec<Span>> = Vec::with_capacity(usize::from(height));
    lines.push(top_edge(width, borders, &body.title));
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

/// Lays `kind` out into a `width` by `height` rect with no frame of its
/// own: rows of content, edge to edge.
///
/// What a surface drawn inside a tile needs. The tile's own frame is
/// already on screen around it, and a second border inside that one is two
/// boxes where a person sees one.
#[must_use]
pub fn unframed_rows(width: u16, height: u16, kind: &LayerKind, borders: BorderSet) -> Rows {
    let Some(body) = body(kind, width, height) else {
        return Rows::default();
    };
    if width == 0 || height == 0 {
        return Rows::default();
    }
    content_rows(kind, &body, width, height, borders)
}

/// Where a rect's first content cell lands relative to the rect's own
/// origin, once [`rows`] has framed it: the row past the top border, and
/// the column past the left border plus the same one-cell pad `rows` grants
/// interior text at `width >= 6`.
///
/// `rows` computes this arithmetic to lay text out; a cursor that needs to
/// land on a specific character of that text (the palette's query line) has
/// to land in the same place `rows` painted it, so this is exposed rather
/// than re-derived a second time from the same two size thresholds.
#[must_use]
pub(crate) fn interior_origin(width: u16, height: u16) -> (u16, u16) {
    if width < 2 || height < 2 {
        return (0, 0);
    }
    let pad = u16::from(width >= 6);
    (1, 1 + pad)
}

/// The text width and rows [`rows`] lays a `width` by `height` rect's
/// content into: the whole rect when it is too small for a frame, the
/// interior inside the frame and its padding otherwise.
#[must_use]
pub(crate) fn interior_size(width: u16, height: u16) -> (u16, u16) {
    if width < 2 || height < 2 {
        return (width, height);
    }
    (
        view_core::native::geometry::interior_text_width(width),
        height - 2,
    )
}

/// Where the windowed palette band's first content cell lands relative to
/// the band's own origin, once `paint_windowed_palette`-shaped painting (a
/// `box_edge` border, content laid out through [`unframed_rows`]) has
/// framed it: the row past the top border, and the column past the left
/// border with no pad -- `unframed_rows` grants none, unlike [`rows`]' one-
/// cell pad at `width >= 6`, because the border is already drawn by a
/// separate call there, outside the laid rows.
///
/// The palette's own cursor placement and the band's painter both read this
/// one derivation of the two size thresholds, so a caret placed against one
/// border convention can never sit behind a border drawn under the other.
#[must_use]
pub fn windowed_interior_origin(width: u16, height: u16) -> (u16, u16) {
    if width < 2 || height < 2 {
        return (0, 0);
    }
    (1, 1)
}

/// The lowest interior width, in display cells, a picker's preview pane is
/// worth splitting off a column for. Below this, the results list and a
/// sliver of preview would both be unreadable, so the picker falls back to
/// the single results column it had before a preview existed rather than
/// painting an illegible pane.
const MIN_PREVIEW_SPLIT_WIDTH: u16 = 20;

/// The marker opening the selected item row, and the blank of the same
/// width every other item row opens with, so a list's content starts at one
/// column whether or not the row is the selected one.
///
/// Their width is [`LIST_MARKER_COLS`], which is what a feature wrapping its
/// own rows breaks at -- welded by the assertions below rather than left as
/// two literals a later edit could widen without the wrap hearing about it.
///
/// The weld counts bytes, because `unicode_width` is not a `const fn` and
/// bytes is the only measure a compile-time assertion has. It is exact while
/// these markers stay ASCII, and errs toward failing to build on a marker
/// that is one cell but several bytes -- the safe direction for a proxy: a
/// compile error naming this line, never a wrap silently two columns wrong.
const SELECTED_MARK: &str = "> ";
const UNSELECTED_MARK: &str = "  ";
const _: () = assert!(SELECTED_MARK.len() == LIST_MARKER_COLS as usize);
const _: () = assert!(UNSELECTED_MARK.len() == LIST_MARKER_COLS as usize);

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
/// layout) and the RPC-read buffer preview (right, the window
/// [`PickerView::preview_window`] cuts around the selected line), painted
/// by reusing [`lay_out`] and [`Line`]'s existing per-row column machinery
/// (called once per side) rather than inventing a second layout primitive.
/// Both painters
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
    let (window, marked) = view.preview_window(usize::from(height));
    let preview_body = Body {
        title: String::new(),
        header: Vec::new(),
        items: window
            .iter()
            .take(usize::from(height))
            .enumerate()
            .map(|(i, text)| {
                if marked == Some(i) {
                    Line::Text(vec![Span::new(text.clone(), StyleRole::Match)])
                } else {
                    Line::Text(plain_spans(text.clone()))
                }
            })
            .collect(),
        selected: marked,
        in_view: None,
        header_keep_tail: false,
        header_first: false,
        rule: false,
        footer: Vec::new(),
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

/// The frame's top row: the two corners with the title, in the longest form
/// that fits, set into the horizontal run between them.
///
/// The title is blanked here rather than where a [`Body`] is built, for the
/// same reason [`fit`] blanks a column: this is the one place a title
/// becomes painted cells, so a title carrying a control character cannot
/// reach a row no matter which builder produced it or what a later feature
/// puts in a view's title.
///
/// Three spans when some form of the title fits, one when none does: the
/// label carries [`StyleRole::Title`] so a painter can give it the
/// colorscheme's own float-title style, while the runs on either side of it
/// stay plain and take the frame's. One span for the whole row would force
/// the title to inherit the frame's deliberately dimmed border color --
/// the row's only readable text, painted in the row's least readable style.
fn top_edge(width: u16, borders: BorderSet, title: &str) -> Vec<Span> {
    let span = width - 2;
    let (first, last) = title_cells(width);
    let (label, label_cells) = title_label(
        &sanitized(title),
        last.saturating_add(1).saturating_sub(first),
    );
    if label_cells > 0 {
        let lead = borders.top_left.to_string();
        let mut tail = String::new();
        // no wrap: every non-empty label is at most the budget above plus
        // its own two blank columns, which is the whole run at most
        push_run(&mut tail, borders.horizontal, span - label_cells);
        tail.push(borders.top_right);
        vec![
            Span::plain(lead),
            Span::new(label, StyleRole::Title),
            Span::plain(tail),
        ]
    } else {
        let mut edge = String::new();
        edge.push(borders.top_left);
        push_run(&mut edge, borders.horizontal, span);
        edge.push(borders.top_right);
        vec![Span::plain(edge)]
    }
}

/// The first and last cell a name set into a frame edge `width` cells wide
/// may take, as offsets from the edge's left corner: the corner and one
/// blank cell stand on each side of it.
///
/// An overlay's title and a tile's buffer name both read this, so the two
/// names sit the same way when they stand on one row.
#[must_use]
pub const fn title_cells(width: u16) -> (u16, u16) {
    (2, width.saturating_sub(3))
}

/// The label [`top_edge`] sets into an edge with `budget` cells to spare
/// for the title's own text, and that label's own width in cells.
///
/// Degrades in steps rather than in one drop to nothing: the whole title
/// while it fits, otherwise the longest prefix of it that leaves room for
/// [`TRUNCATION_MARK`], and where not one glyph of the title survives that
/// cut, its first word alone -- whole, so it reads as a name rather than as
/// a word broken off mid-syllable. Empty, and only then, when no form of
/// the title fits at all, which is the one case the caller draws an
/// anonymous edge for.
///
/// A box that names itself only above some width is a box a user meets
/// unnamed on the terminal they actually have: the panel's own share of a
/// laptop-width terminal is a narrower edge than its title is long.
///
/// The blank column on each side of the label is part of every non-empty
/// answer, which is why `budget` excludes them: a title abutting the edge
/// glyphs reads as a run of the border rather than as a name set into it.
/// A non-empty answer is therefore always `budget + 2` cells or fewer, and
/// always carries at least one cell the reader can see -- a title of
/// nothing but zero-width marks is no name, and the two blank columns are
/// not worth spending on it.
///
/// One forward pass over the title, whichever tier answers: the cut walks
/// back off the end of the prefix the first pass already measured rather
/// than measuring the title again, and the label is measured once more,
/// at most `budget + 2` cells. This is on the layout pass every framed
/// overlay takes every frame, and the panel's own share of a common
/// terminal is in the cut tier.
fn title_label(title: &str, budget: u16) -> (String, u16) {
    let title = title.trim();
    // a lone mark still takes the cell the painter gives its cluster, so
    // what is seen is asked of the characters; the first one usually answers
    if title.chars().all(|ch| ch.width() == Some(0)) {
        return (String::new(), 0);
    }
    let (kept, kept_cells) = take_cells(title, budget);
    // what came back is a prefix, so equal byte lengths mean nothing was
    // cut -- the whole title fit
    if kept.len() == title.len() {
        return measured(format!(" {title} "));
    }
    let (kept, kept_cells) = cut_before_mark(&kept, kept_cells, budget);
    if kept_cells > 0 {
        return measured(format!(" {kept}{TRUNCATION_MARK} "));
    }
    match title.split_whitespace().next().map(|w| (w, cells(w))) {
        // a word can measure a cell wide and still be only a mark (the same
        // reason the whole-title guard above exists), so the visibility
        // check applies here too
        Some((word, word_cells))
            if (1..=budget).contains(&word_cells)
                && !word.chars().all(|ch| ch.width() == Some(0)) =>
        {
            measured(format!(" {word} "))
        }
        _ => (String::new(), 0),
    }
}

/// `label` and the cells the painter gives it, measured on the padded
/// label it paints: a text opening on a mark, a variation selector or a
/// joiner shares one cluster with the blank column before it.
fn measured(label: String) -> (String, u16) {
    let label_cells = cells(&label);
    (label, label_cells)
}

/// The longest prefix of `text` that fits in `budget` display cells, and
/// the cells it occupies.
///
/// A glyph that would straddle the last cell is dropped rather than
/// half-drawn, which is what the terminal painter does with one too. Shared
/// by [`clip_spans`] and [`title_label`] so a row's content and the title
/// over it are cut by one rule: a title measured any other way would be the
/// one text on the frame that could overrun it.
fn take_cells(text: &str, budget: u16) -> (String, u16) {
    // an ASCII head one byte past the budget holds the kept prefix whole,
    // one cell a byte, and ends no cluster early, except a CR LF pair the
    // head can split; the row is sanitized after the clip, so a split pair
    // still counts as the two spaces it becomes. Testing only that head
    // keeps a clipped span from being scanned to its end
    let reach = usize::from(budget).saturating_add(1).min(text.len());
    if text.get(..reach).is_some_and(str::is_ascii) {
        let kept = text.get(..usize::from(budget)).unwrap_or(text);
        return (
            kept.to_string(),
            u16::try_from(kept.len()).unwrap_or(budget),
        );
    }
    let mut out = String::new();
    let mut used = 0_u16;
    for cluster in clusters(text) {
        let w = cluster_cells(cluster);
        if used.saturating_add(w) > budget {
            break;
        }
        out.push_str(cluster);
        used = used.saturating_add(w);
    }
    (out, used)
}

/// Appends `n` cells of `glyph` to `out`.
///
/// Grows one buffer rather than building an intermediate one: this runs
/// once per framed overlay per frame, on the layout pass the paint path
/// calls, and `char::to_string().repeat(n)` allocates a throwaway string
/// for every run it produces.
fn push_run(out: &mut String, glyph: char, n: u16) {
    out.extend(std::iter::repeat_n(glyph, usize::from(n)));
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
    /// The always-shown rows a short overlay must choose among, in
    /// priority order -- see [`Self::header_keep_tail`] for which end
    /// [`lay_out`] keeps first. Never includes the rule itself: see
    /// [`Self::rule`] for why that row is never in this race at all.
    header: Vec<Line>,
    /// The scrolling rows, each carrying the selection marker once
    /// [`lay_out`] knows which of them is selected.
    items: Vec<Line>,
    selected: Option<usize>,
    /// An item the window keeps on screen, unmarked, while nothing is
    /// `selected`.
    in_view: Option<usize>,
    /// Whether this body is laid out as a chat ([`lay_out_chat`]): `false`
    /// (every kind but [`ai_body`]) keeps the first `height` header rows,
    /// the shape a prompt or picker's own message-then-input ordering
    /// wants, and the first items. `true` keeps the header's last rows and
    /// the newest items. The header's last rows are the crash banner and
    /// the pending permission's answerable options, which a crashed session
    /// or a request blocking the agent's own turn cannot be shown without.
    /// The question above them goes first. The newest items are where a
    /// reader is, and losing them reads as a dead panel.
    header_keep_tail: bool,
    /// Whether the header's last row outranks the footer's last row for a
    /// row of a short panel ([`chat_fit`]). Set while a permission is
    /// pending, since the keys answer its options and the composer takes
    /// none of them.
    header_first: bool,
    /// Whether this body draws the rule that separates its header from its
    /// scrolling items.
    ///
    /// For `header_keep_tail: false` (every kind but [`ai_body`]) the rule
    /// is pure chrome, drawn only with whatever row of budget is left once
    /// `header`'s own kept rows have had theirs -- the lowest priority in
    /// the row, nothing to lose by disappearing first.
    ///
    /// For `header_keep_tail: true` ([`ai_body`]) the rule separates the
    /// transcript from the [`Self::footer`] under it, and ranks below the
    /// footer's last row and the header's last row (the crash banner, or
    /// the last-pushed permission option) and above everything else. See
    /// [`chat_fit`] for the exact tier order.
    rule: bool,
    /// Rows pinned to the bottom of a `header_keep_tail` body, under the
    /// rule: the agent panel's composer, so the newest transcript row sits
    /// directly above what the user is typing. Kept from the tail, since
    /// the last row is where the cursor is. Empty for every other kind.
    footer: Vec<Line>,
}

/// How a run of rows was spent against a row budget: how many of the rows
/// above its last one were kept (always the tail of them), and whether the
/// last row got a slot of its own.
struct RunFit {
    rest: usize,
    last: bool,
}

/// How a `header_keep_tail` body's rows were spent: its header, the rule
/// and its footer.
struct ChatFit {
    header: RunFit,
    rule: bool,
    footer: RunFit,
}

impl ChatFit {
    /// The footer rows kept, painted at the bottom of the rect.
    fn footer_rows(&self) -> usize {
        self.footer.rest + usize::from(self.footer.last)
    }
}

/// Priority order, most important first: the footer's last row (the
/// composer row the cursor is usually on), the header's last row (the
/// crash banner, or the last permission option), the rule, the rest of the
/// header from most recent to least, and the rest of the footer from most
/// recent to least. With `header_first` the first two swap, because a
/// pending permission is where the keys go. The rule is spent from the
/// same `budget` as the content, one slot at a time, in that order.
///
/// Its own function rather than [`lay_out`]'s locals, because [`ai_caret`]
/// has to name the row this arithmetic put the composer on. Two copies of
/// it is a caret that walks off its own text on exactly the panels short
/// enough for the truncation to bite.
///
/// Takes row counts rather than rows: the counts are all this reads, and
/// [`ai_caret`] runs on every frame the user is typing on -- building a
/// header there only to measure it would clone the panel's chrome spans
/// per keystroke.
fn chat_fit(
    header_len: usize,
    footer_len: usize,
    rule: bool,
    header_first: bool,
    budget: usize,
) -> ChatFit {
    let mut left = budget;
    let mut take = |wanted: bool| {
        let taken = wanted && left > 0;
        left -= usize::from(taken);
        taken
    };
    let (header_last, footer_last) = if header_first {
        let header_last = take(header_len >= 1);
        (header_last, take(footer_len >= 1))
    } else {
        let footer_last = take(footer_len >= 1);
        (take(header_len >= 1), footer_last)
    };
    let rule = take(rule);
    let header_rest = left.min(header_len.saturating_sub(1));
    left -= header_rest;
    let footer_rest = left.min(footer_len.saturating_sub(1));
    ChatFit {
        header: RunFit {
            rest: header_rest,
            last: header_last,
        },
        rule,
        footer: RunFit {
            rest: footer_rest,
            last: footer_last,
        },
    }
}

impl RunFit {
    /// Which of the kept rows holds line `index` of a `header_len`-line
    /// run, counted from the first kept row, or `None` when the truncation
    /// dropped that line entirely.
    fn row_of(&self, index: usize, header_len: usize) -> Option<usize> {
        if index + 1 >= header_len {
            return self.last.then_some(self.rest);
        }
        // `then`, never `then_some`: the row a truncation dropped is exactly
        // the one whose subtraction underflows, and `then_some` evaluates
        // its argument before the test that rules it out
        let rest_start = header_len - 1 - self.rest;
        (index >= rest_start).then(|| index - rest_start)
    }
}

/// Cuts `body` down to exactly `height` rows of exactly `width` cells: the
/// header first (see [`Body::rule`] for where its own row fits into that),
/// then a window over the items that keeps the selection on screen.
///
/// The window is the smallest scroll that shows the selection: it stays at
/// the top of the list while the selection is already visible, and
/// otherwise moves down just far enough. Anchoring the window on the
/// selection instead (centering it, or starting from it) would jump the
/// list on every cursor move.
fn lay_out(body: &Body, width: u16, height: u16, borders: BorderSet) -> Rows {
    if body.header_keep_tail {
        return lay_out_chat(body, width, height, borders);
    }
    let mut lines: Vec<Vec<Span>> = Vec::with_capacity(usize::from(height));
    let budget = usize::from(height);
    let header_budget = budget.min(body.header.len());
    for line in &body.header[..header_budget] {
        lines.push(fit(line, width, borders));
    }
    if body.rule && lines.len() < budget {
        lines.push(fit(&Line::Rule, width, borders));
    }
    let header_rows = lines.len();
    let item_rows = usize::from(height).saturating_sub(header_rows);
    // an index past the end selects nothing rather than being clamped onto
    // a row the feature never chose
    let selected = body.selected.filter(|i| *i < body.items.len());
    let first = match selected.or(body.in_view.filter(|i| *i < body.items.len())) {
        Some(i) if item_rows > 0 && i >= item_rows => i + 1 - item_rows,
        _ => 0,
    };
    for (offset, item) in body.items.iter().skip(first).take(item_rows).enumerate() {
        let marker = if selected == Some(first + offset) {
            SELECTED_MARK
        } else {
            UNSELECTED_MARK
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

/// [`lay_out`] for a `header_keep_tail` body: the header at the top, the
/// footer at the bottom with the rule above it, and the newest items
/// directly above the rule. Blank rows go between the header and the
/// items, so a short transcript reads upwards from the composer the way a
/// chat does.
fn lay_out_chat(body: &Body, width: u16, height: u16, borders: BorderSet) -> Rows {
    let budget = usize::from(height);
    let kept = chat_fit(
        body.header.len(),
        body.footer.len(),
        body.rule,
        body.header_first,
        budget,
    );
    let mut lines: Vec<Vec<Span>> = Vec::with_capacity(budget);
    for line in kept_tail(&body.header, &kept.header) {
        lines.push(fit(line, width, borders));
    }
    let mut bottom: Vec<Vec<Span>> = Vec::new();
    if kept.rule {
        bottom.push(fit(&Line::Rule, width, borders));
    }
    for line in kept_tail(&body.footer, &kept.footer) {
        bottom.push(fit(line, width, borders));
    }
    let item_rows = budget.saturating_sub(lines.len() + bottom.len());
    let first = body.items.len().saturating_sub(item_rows);
    let shown = body.items.len() - first;
    for _ in shown..item_rows {
        lines.push(vec![Span::plain(" ".repeat(usize::from(width)))]);
    }
    for item in &body.items[first..] {
        lines.push(fit(&marked(item, UNSELECTED_MARK), width, borders));
    }
    lines.extend(bottom);
    Rows {
        lines,
        selected: None,
        framed: false,
    }
}

/// The rows of `run` that `fit` kept: the tail of the rows above its last,
/// then its last.
fn kept_tail<'a>(run: &'a [Line], fit: &RunFit) -> impl Iterator<Item = &'a Line> {
    let (last, rest) = run
        .split_last()
        .map_or((None, &[][..]), |(last, rest)| (Some(last), rest));
    rest.len()
        .checked_sub(fit.rest)
        .and_then(|start| rest.get(start..))
        .unwrap_or_default()
        .iter()
        .chain(last.filter(|_| fit.last))
}

/// The [`Body`] for a native overlay layer, or `None` for a layer kind that
/// is not a native overlay at all. `text_width` is the cells a row of it
/// holds, which a prompt's message wraps at, and `height` the rows it is
/// laid into.
///
/// Exhaustive rather than wildcarded, so the `Some` arms here and
/// [`LayerKind::is_native_overlay`]'s `true` arms cannot drift: a variant
/// added to one without the other stops compiling instead of quietly
/// producing a framed layer with nothing in it.
fn body(kind: &LayerKind, text_width: u16, height: u16) -> Option<Body> {
    match kind {
        LayerKind::Picker(view) => Some(picker_body(view)),
        LayerKind::Tree(view) => Some(tree_body(view)),
        LayerKind::Statusline(view) => Some(statusline_body(view)),
        LayerKind::Prompt(view) => Some(prompt_body(view, text_width, height)),
        LayerKind::Palette(view) => Some(palette_body(view)),
        LayerKind::Stream(view) => Some(stream_body(view)),
        LayerKind::Ai(view) => Some(ai_body(view)),
        LayerKind::EngineGrid
        | LayerKind::Cmdline(_)
        | LayerKind::Toast { .. }
        | LayerKind::Popupmenu(_)
        | LayerKind::Speculated(_)
        | LayerKind::Pill(_)
        | LayerKind::Gutter
        | LayerKind::Shell => None,
    }
}

/// The rows above `view.top` are left out, so [`lay_out`] scrolls the rest
/// only when the selection would leave the window.
fn picker_body(view: &PickerView) -> Body {
    let top = view.selected.map_or(0, |selected| view.top.min(selected));
    Body {
        title: view.title.clone(),
        header: vec![Line::Text(plain_spans(format!(
            "{PROMPT_MARK} {}",
            view.query
        )))],
        items: view
            .rows
            .iter()
            .skip(top)
            .cloned()
            .map(Line::Text)
            .collect(),
        selected: view.selected.map(|selected| selected - top),
        in_view: None,
        header_keep_tail: false,
        header_first: false,
        rule: true,
        footer: Vec::new(),
    }
}

fn tree_body(view: &TreeView) -> Body {
    Body {
        title: view.title.clone(),
        header: Vec::new(),
        items: view
            .rows
            .iter()
            .map(|row| tree_row_spans(row, view.icons))
            .map(Line::Text)
            .collect(),
        selected: view.selected,
        in_view: None,
        header_keep_tail: false,
        header_first: false,
        rule: false,
        footer: Vec::new(),
    }
}

/// One tree row's spans: indentation, the row's opening glyphs, its git
/// state, then the label (plain text) and, for a symbolic link, where it
/// points.
///
/// Under [`TreeIcons::Nerd`] a folder opens with an arrow and its folder
/// glyph, both in [`StyleRole::TreeFolder`], and a file with two blank
/// columns and the icon its row carries, in that icon's colour. The row's
/// [`view_core::native::views::GitIcons`] follow as glyphs, each followed
/// by a space. Under
/// [`TreeIcons::None`] the row opens with a `-` on an open folder, a `+`
/// on a closed one and a blank on a file. A file's [`GitMark`] letter
/// follows, and a folder draws one letter per state it holds, in the order
/// the glyphs take (see [`view_core::native::views::GitIcon::mark`]). The
/// git state sits between the icon and the name, where
/// nvim-tree's default renderer places it.
fn tree_row_spans(row: &TreeRow, icons: TreeIcons) -> Vec<Span> {
    let mut spans = vec![Span::plain("  ".repeat(usize::from(row.depth)))];
    match icons {
        TreeIcons::Nerd => {
            let (arrow, glyph) = match row.expanded {
                Some(open) => {
                    let arrow = if open {
                        devicons::ARROW_OPEN
                    } else {
                        devicons::ARROW_CLOSED
                    };
                    let glyph = match (row.link.is_some(), row.empty, open) {
                        (true, _, _) => devicons::FOLDER_LINK,
                        (false, true, true) => devicons::FOLDER_EMPTY_OPEN,
                        (false, true, false) => devicons::FOLDER_EMPTY,
                        (false, false, true) => devicons::FOLDER_OPEN,
                        (false, false, false) => devicons::FOLDER_CLOSED,
                    };
                    (
                        Span::new(format!("{arrow} "), StyleRole::TreeFolder),
                        Span::new(glyph, StyleRole::TreeFolder),
                    )
                }
                None if row.link.is_some() => (Span::plain("  "), Span::plain(devicons::FILE_LINK)),
                None => {
                    let icon = row.icon.unwrap_or(devicons::DEFAULT_FILE);
                    (
                        Span::plain("  "),
                        Span::new(icon.glyph, StyleRole::Devicon(icon.color)),
                    )
                }
            };
            spans.extend([arrow, glyph, Span::plain(" ")]);
            spans.extend(
                row.git
                    .iter()
                    .map(|icon| Span::new(format!("{} ", icon.glyph()), icon.style_role())),
            );
        }
        _ => {
            let marker = match row.expanded {
                Some(true) => "-",
                Some(false) => "+",
                None => " ",
            };
            spans.extend([Span::plain(marker), Span::plain(" ")]);
            if row.expanded.is_some() {
                spans.extend(row.git.iter().map(|icon| tree_git_glyph_span(icon.mark())));
            } else if let Some(mark) = row.status {
                spans.push(tree_git_glyph_span(mark));
            }
        }
    }
    spans.push(Span::plain(row.label.clone()));
    if let Some(target) = &row.link {
        let arrow = match icons {
            TreeIcons::Nerd => devicons::LINK_ARROW,
            _ => " -> ",
        };
        spans.push(Span::plain(format!("{arrow}{target}")));
    }
    spans
}

/// The single styled glyph a decorated tree row's [`GitMark`] paints before
/// its label -- see [`GitMark::glyph`] and [`GitMark::style_role`].
fn tree_git_glyph_span(mark: GitMark) -> Span {
    Span::new(format!("{} ", mark.glyph()), mark.style_role())
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
        in_view: None,
        header_keep_tail: false,
        header_first: false,
        rule: false,
        footer: Vec::new(),
    }
}

/// `width` is the interior text width the message wraps at, the same one
/// `Model::overlay_rect` counted the box's rows at, and `height` the
/// interior rows [`PromptView::fit`] lays the prompt into.
fn prompt_body(view: &PromptView, width: u16, height: u16) -> Body {
    let fit = view.fit(width, height);
    let mut header: Vec<Line> = fit
        .message
        .into_iter()
        .map(|row| Line::Text(plain_spans(row)))
        .collect();
    header.push(Line::Text(plain_spans(format!(
        "{PROMPT_MARK} {}",
        view.input
    ))));
    let one_row = match fit.choices {
        PromptChoices::Inline => Some(inline_choices(view, None)),
        PromptChoices::One(index) => Some(inline_choices(view, Some(index))),
        _ => None,
    };
    let inline = one_row.is_some();
    if let Some(row) = one_row {
        if fit.rule {
            header.push(Line::Rule);
        }
        header.push(Line::Text(plain_spans(row)));
    }
    Body {
        title: view.title.clone(),
        // the typed line above the rule and the selectable rows below it.
        // The prompt mark and the selection marker are the same glyph, so
        // the rule is what sets the input line apart from a selected row
        header,
        items: view
            .choices
            .iter()
            .filter(|_| fit.choices == PromptChoices::Stacked)
            .cloned()
            .map(plain_spans)
            .map(Line::Text)
            .collect(),
        selected: view.selected,
        in_view: None,
        header_keep_tail: false,
        header_first: false,
        rule: fit.rule && !inline,
        footer: Vec::new(),
    }
}

/// Every choice on one row, or the choice at `only` alone, each behind the
/// marker it would carry on a row of its own, since the row highlight
/// cannot say which answer it means.
fn inline_choices(view: &PromptView, only: Option<usize>) -> String {
    let marked: Vec<String> = view
        .choices
        .iter()
        .enumerate()
        .filter(|(i, _)| only.is_none_or(|index| index == *i))
        .map(|(i, choice)| {
            let marker = if view.selected == Some(i) {
                SELECTED_MARK
            } else {
                UNSELECTED_MARK
            };
            format!("{marker}{choice}")
        })
        .collect();
    marked.join(INLINE_CHOICE_GAP)
}

fn palette_body(view: &PaletteView) -> Body {
    Body {
        title: view.title.clone(),
        header: vec![Line::Text(plain_spans(format!(
            "{PROMPT_MARK} {}",
            view.query
        )))],
        items: if view.drawn.is_empty() {
            view.rows.iter().map(palette_row_line).collect()
        } else {
            view.drawn.iter().cloned().map(Line::Text).collect()
        },
        selected: view.selected,
        in_view: standout_row(&view.drawn),
        header_keep_tail: false,
        header_first: false,
        rule: true,
        footer: Vec::new(),
    }
}

/// The rows of a titled list that takes no query: the same items
/// [`palette_body`] lists, under the title, with no query header and no
/// rule. A query row with nothing typed into it would stand for an input
/// the list never takes.
fn stream_body(view: &PaletteView) -> Body {
    Body {
        title: view.title.clone(),
        header: Vec::new(),
        items: view.rows.iter().map(palette_row_line).collect(),
        selected: view.selected,
        in_view: None,
        header_keep_tail: false,
        header_first: false,
        rule: false,
        footer: Vec::new(),
    }
}

/// `selected` stays `None`: a transcript has no actionable row the way a
/// picker match or a palette command does, so there is nothing here for a
/// cursor position to point at.
///
/// The crash banner and the pending permission prompt's rows, when either
/// is present, are part of the header that always shows, never a scrolling
/// item, since a crashed session or a request blocking the agent's own turn
/// must both stay visible however far the transcript has scrolled. The
/// banner is drawn last: a crash already cleared any pending permission
/// (see `update/ai.rs`'s `on_ai_event`), so the two never actually appear
/// together, and this ordering is what a future case where they did would
/// fall back to.
///
/// The composer is the footer, under the transcript and the rule, so the
/// newest row the agent wrote sits directly above what the user types.
fn ai_body(view: &AiPanelView) -> Body {
    Body {
        title: view.title.clone(),
        header: ai_header(view),
        items: view.rows.iter().cloned().map(Line::Text).collect(),
        selected: None,
        in_view: None,
        header_keep_tail: true,
        header_first: !view.pending_permission.is_empty(),
        rule: AI_RULE,
        footer: composer_lines(&view.input),
    }
}

/// Whether the panel draws the rule between its transcript and its
/// composer.
///
/// Named rather than written at both call sites because it costs a row of
/// the same budget the header rows compete for (see [`Body::rule`]), so
/// [`ai_caret`] cannot say which row the composer landed on without it.
const AI_RULE: bool = true;

/// The always-shown rows the panel puts above its transcript, in the order
/// [`lay_out`] sacrifices them in.
///
/// Header order is truncation order, because `header_keep_tail` keeps the
/// tail: the first row here is the first sacrificed and the last is the last
/// standing. Session accounting first (it answers a question nobody is
/// currently blocked on), then the review's own summary, then a pending
/// permission's question and options, and the crash banner last of all. A
/// dead session is the one thing that explains why nothing else on this
/// panel will ever answer, so it outranks a review the user can still scroll
/// to and a request whose agent is already gone.
///
/// Its own function because [`ai_header_len`] counts exactly these groups
/// and `the_counted_ai_header_is_as_long_as_the_built_one_for_every_row_group`
/// holds the two to each other -- a row group added here and not there is a
/// caret one row off the text it belongs to.
fn ai_header(view: &AiPanelView) -> Vec<Line> {
    let mut header: Vec<Line> = view.usage.iter().cloned().map(Line::Text).collect();
    header.extend(view.review.iter().cloned().map(Line::Text));
    header.extend(view.pending_permission.iter().cloned().map(Line::Text));
    header.extend(view.local_error.iter().cloned().map(Line::Text));
    header
}

/// How many rows [`ai_header`] yields, counted rather than built.
///
/// [`ai_caret`] runs on every frame the panel owns input, and the only thing
/// it reads off the header is its length; assembling one there would clone
/// every chrome span the panel is showing once per keystroke.
///
/// Held to [`ai_header`] by a test rather than by construction, which is the
/// honest statement of the arrangement: the two must agree, and the test is
/// what fails when a future row group reaches one and not the other.
fn ai_header_len(view: &AiPanelView) -> usize {
    view.usage
        .len()
        .saturating_add(view.review.len())
        .saturating_add(view.pending_permission.len())
        .saturating_add(view.local_error.len())
}

/// How many rows [`composer_lines`] paints for `rows`: its own, or the one
/// empty prompt line it draws for a view carrying none.
fn composer_row_count(rows: &[String]) -> usize {
    rows.len().max(1)
}

/// The composer's rows, the prompt mark on the first and an indent of the
/// same width under it on every wrapped row after it -- so a wrapped prompt
/// reads as one field rather than as a new prompt per row, and every row
/// breaks at the column `AiPanelState::view` wrapped it to.
///
/// A view carrying no composer rows at all (one built straight from
/// `AiPanelView::new`, never through the panel's own state) still draws the
/// empty prompt line: the composer is chrome the panel always has, and a
/// missing row would shift every header row under it.
///
/// Their order is truncation order, and it is the right one already: a
/// short panel drops the composer's oldest rows first (see
/// [`Body::header_keep_tail`]) and keeps the last, which is where the
/// cursor is.
fn composer_lines(rows: &[String]) -> Vec<Line> {
    if rows.is_empty() {
        return vec![Line::Text(plain_spans(format!("{PROMPT_MARK} ")))];
    }
    rows.iter()
        .enumerate()
        .map(|(i, row)| {
            let lead = if i == 0 {
                format!("{PROMPT_MARK} ")
            } else {
                " ".repeat(view_core::native::ai_panel::PROMPT_COLS)
            };
            Line::Text(plain_spans(format!("{lead}{row}")))
        })
        .collect()
}

/// Where the agent panel's caret lands inside the panel's own rect: at the
/// composer's insertion point, or -- while a permission question is pending
/// -- on the digit that answers it (see [`ai_caret_target`]).
///
/// Resolved through the same [`chat_fit`] [`rows`] laid the panel out
/// with, and against the same painted view: which row the caret is on
/// depends on the composer, the accounting row, the review summary, a
/// pending question and the crash banner all being counted exactly as they
/// were drawn.
///
/// `None` only for a rect with no cells at all. A panel too short to have painted the caret's own
/// row still owns the keyboard, so the caret stays inside it -- on its first interior cell, since
/// the row it belongs to is above every row such a panel kept -- rather than falling back to the
/// engine grid, where it would tell the user their keys are the editor's. That is reachable, not
/// theoretical: the panel's height is the terminal's less the tabline and the statusline rows, so a
/// four-row pane with both on leaves the frame its two border rows and no interior at all, and its
/// first interior cell is then the frame's own bottom edge -- a real cell, which is what
/// `CursorSpec` requires, and inside the surface that holds the keys.
pub(crate) fn ai_caret(view: &AiPanelView, width: u16, height: u16) -> Option<(u16, u16)> {
    let (row_off, col_off) = interior_origin(width, height);
    let interior = interior_size(width, height).1;
    ai_caret_at(view, width, height, interior, row_off, col_off)
}

/// [`ai_caret`] for a windowed tile: the tile's own frame is drawn by the
/// pane compositor, outside [`rows`], so there is no border row or pad column
/// of this function's own to skip past -- the caret lands at row/col 0 of
/// whatever room `width`/`height` already are.
pub(crate) fn ai_caret_unframed(view: &AiPanelView, width: u16, height: u16) -> Option<(u16, u16)> {
    ai_caret_at(view, width, height, height, 0, 0)
}

fn ai_caret_at(
    view: &AiPanelView,
    width: u16,
    height: u16,
    interior: u16,
    row_off: u16,
    col_off: u16,
) -> Option<(u16, u16)> {
    if width == 0 || height == 0 {
        return None;
    }
    let (target, cells) = ai_caret_target(view);
    let col = u16::try_from(cells)
        .unwrap_or(u16::MAX)
        .saturating_add(col_off)
        .min(width.saturating_sub(1));
    let header_len = ai_header_len(view);
    let footer_len = composer_row_count(&view.input);
    let budget = usize::from(interior);
    let fit = chat_fit(
        header_len,
        footer_len,
        AI_RULE,
        !view.pending_permission.is_empty(),
        budget,
    );
    let row = match target {
        CaretRow::Header(index) => fit.header.row_of(index, header_len),
        CaretRow::Composer(index) => fit
            .footer
            .row_of(index, footer_len)
            .map(|row| budget - fit.footer_rows() + row),
    };
    let Some(row) = row.and_then(|row| u16::try_from(row).ok()) else {
        return Some((row_off, col_off));
    };
    Some((row.saturating_add(row_off), col))
}

/// Which run of the panel's rows the caret is on, and its index there.
enum CaretRow {
    Header(usize),
    Composer(usize),
}

/// Which of the panel's rows the caret belongs on, and how far into that
/// row in cells.
///
/// Two answers, because the panel has two states that take keys. With
/// nothing pending it is the composer's insertion point, past the prompt
/// mark [`composer_lines`] painted.
///
/// While a permission question stands it is that question's own answer cell
/// instead -- `AiPanelView::permission_answer`, which the prompt that built
/// those rows named. The composer refuses every printable until the question
/// is answered (`update::route_key` swallows them so the prompt cannot be
/// typed past), so a caret left on it -- wearing the bar an editor uses to
/// say *type here* -- would invite exactly the keystrokes the panel is about
/// to eat. Static, like the confirm prompt's own caret
/// ([`crate::prompt_cursor`]), because no key moves it: one press ends the
/// question.
fn ai_caret_target(view: &AiPanelView) -> (CaretRow, usize) {
    use view_core::native::ai_panel::PROMPT_COLS;

    if view.pending_permission.is_empty() {
        let (row, cells) = view.composer_cursor();
        return (CaretRow::Composer(row), PROMPT_COLS.saturating_add(cells));
    }
    let (row, cells) = view.permission_answer;
    let question = view.usage.len().saturating_add(view.review.len());
    (CaretRow::Header(question.saturating_add(row)), cells)
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
/// Every column is sanitized, so a control character in feature-supplied
/// text becomes a plain space here rather than reaching a consumer: the
/// terminal painter replaces one anyway, but the oracle's rasterizer writes
/// what it is given straight into a screen dump, and a raw control byte in
/// a golden is not a picture of anything.
///
/// A one-line row is clipped first and sanitized after, which is the same
/// text either way -- [`cluster_cells`] measures a control character as the
/// one column the space it becomes will take -- and copies only the columns
/// that survive. What a row holds is bounded by the frame; what a span
/// holds is not, since a pasted prompt echoed into the transcript is as
/// long as the clipboard was.
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
        Line::Text(spans) => sanitize_spans(&clip_spans(spans, width)),
        Line::Split(left, right) => {
            let (left, right) = (sanitize_spans(left), sanitize_spans(right));
            let (left_cells, right_cells) = (span_cells(&left), span_cells(&right));
            let gap = gap_before_right(left_cells, right_cells, width);
            let mut combined = left;
            combined.push(Span::plain(" ".repeat(usize::from(gap))));
            combined.extend(right);
            clip_spans(&combined, width)
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
            clip_spans(&combined, width)
        }
        Line::Rule => {
            let mut rule = String::new();
            push_run(&mut rule, borders.horizontal, width);
            vec![Span::plain(rule)]
        }
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
fn clip_spans(spans: &[Span], width: u16) -> Vec<Span> {
    let mut out: Vec<Span> = Vec::new();
    let mut used = 0_u16;
    for span in spans {
        if used >= width {
            break;
        }
        // a span whose own room is exhausted adds nothing, but carries no
        // meaning that would make a later one unreachable, so the walk goes
        // on rather than stopping at the first span that does not fit
        let (text, took) = take_cells(&span.text, width - used);
        used = used.saturating_add(took);
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
    text_width_by(text, cluster_cells)
}

/// One grapheme cluster's width in terminal display cells, the columns the
/// painter advances by for it. A cluster holding a control character
/// occupies one column per character, the spaces [`sanitized`] replaces
/// them with, so a row measures the same before and after that replacement.
fn cluster_cells(cluster: &str) -> u16 {
    // a control character stands in a cluster of its own, or beside the
    // one other in a CR LF pair
    if cluster.len() > 1 && cluster.chars().any(char::is_control) {
        return u16::try_from(cluster.chars().count()).unwrap_or(u16::MAX);
    }
    cluster_width(cluster)
}

#[cfg(test)]
mod tests;
