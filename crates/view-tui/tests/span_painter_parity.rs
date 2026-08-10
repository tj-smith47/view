//! Cross-painter parity for span-carrying overlay rows: `view-tui`'s real
//! terminal compositor and `view-oracle`'s pure-text raster must paint
//! identical text for the same `Surface`, not just each independently
//! matching the shared layout pass.
//!
//! Before the `Span`/`StyleRole` reshape, every overlay row was one
//! `String`; both painters simply blitted it and could not disagree about
//! its text. After the reshape, each painter walks a row's `Vec<Span>`
//! independently -- `view-tui`'s `paint_span_row` places cells span by
//! span, `view-oracle`'s `paint_text` still writes `overlay::line_text`'s
//! flattened `String` -- and a bug in either walk (see `view-surface`'s
//! `clip_spans` early-break bug, fixed alongside this reshape) would show
//! up as exactly this: the two painters disagreeing about what one row
//! says, while each still believes it painted the row it was handed. That
//! is the divergence class this test exists to catch.
//!
//! The statusline is the layer under test because it is the only overlay
//! whose rows carry more than one `StyleRole` in real content (every other
//! overlay -- picker, tree, prompt, palette, messages -- is `StyleRole::
//! Plain` throughout, see each `LayerKind` variant's own doc comment), so
//! it is the one row shape that actually exercises `paint_span_row`'s
//! per-span walk end to end.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect as TuiRect;
use view_core::grid::Grid;
use view_core::model::{Model, Tier};
use view_core::native::views::{PickerView, Span, StatuslineView, StyleRole};
use view_surface::{Layer, LayerKind, Rect, Surface};
use view_tui::paint::{composite_into, Damage};

/// Where the layer sits on the shared canvas both painters render into.
/// Non-zero on both axes so a divergence at the layer's own offset (not
/// just at the canvas origin) would still be caught.
const AT_ROW: u16 = 1;
const AT_COL: u16 = 2;

/// A statusline exercising every role a painter must resolve: mode, file,
/// the modified marker, both diagnostic roles side by side, a git branch,
/// and the ruler -- the exact segment set the task brief's own consumer
/// mock-up names.
fn spanful_statusline() -> LayerKind {
    LayerKind::Statusline(StatuslineView::from_spans(
        vec![Span::new("-- INSERT --", StyleRole::Mode)],
        vec![
            Span::new("paint.rs", StyleRole::File),
            Span::new(" [+]", StyleRole::Modified),
            Span::plain("  "),
            Span::new("\u{25cf} 2", StyleRole::DiagnosticError),
            Span::plain("  "),
            Span::new("\u{25b2} 1", StyleRole::DiagnosticWarning),
            Span::plain("  "),
            Span::new("main", StyleRole::GitBranch),
        ],
        vec![Span::new("42:7", StyleRole::Ruler)],
    ))
}

/// A picker, whose rows are all `StyleRole::Plain`: parity must hold there
/// too, so a fix aimed only at the statusline's new roles cannot regress
/// the plain-span path every other overlay still relies on.
fn plain_picker() -> LayerKind {
    LayerKind::Picker(
        PickerView::new("Files")
            .with_query("ma")
            .with_rows(vec!["src/main.rs".to_string(), "src/lib.rs".to_string()])
            .with_selected(1),
    )
}

/// Composites `kind` through `view-tui`'s real terminal path and reads back
/// the layer's own rows as plain text, ignoring style: `view-oracle`'s
/// raster models no color for a native overlay (see its own module docs),
/// so a style disagreement is not something this parity check could ever
/// observe on that side, and comparing text only is what both sides can
/// legitimately agree or disagree about.
fn painted_rows(
    tier: Tier,
    truecolor: bool,
    width: u16,
    height: u16,
    kind: LayerKind,
) -> Vec<String> {
    let layer = Layer::new(Rect::new(AT_ROW, AT_COL, width, height), kind, tier);
    let surface = Surface::from_layers(vec![layer]);

    let mut model = Model::new();
    model.caps.tier = tier;
    model.caps.truecolor = truecolor;

    let mut buf = Buffer::empty(TuiRect::new(0, 0, AT_COL + width + 2, AT_ROW + height + 1));
    composite_into(&mut buf, &model, &surface, &Damage::full());

    (0..height)
        .map(|r| {
            (0..width)
                .map(|c| buf[(AT_COL + c, AT_ROW + r)].symbol().to_string())
                .collect::<String>()
        })
        .collect()
}

/// Renders the same layer through `view-oracle`'s pure-text raster.
fn rastered_rows(tier: Tier, width: u16, height: u16, kind: LayerKind) -> Vec<String> {
    let layer = Layer::new(Rect::new(AT_ROW, AT_COL, width, height), kind, tier);
    let surface = Surface::from_layers(vec![layer]);
    view_oracle::raster::screen_rows(&surface, &Grid::new())
        .into_iter()
        .skip(usize::from(AT_ROW))
        .take(usize::from(height))
        .map(|row| {
            row.chars()
                .skip(usize::from(AT_COL))
                .take(usize::from(width))
                .collect()
        })
        .collect()
}

fn assert_parity(tier: Tier, truecolor: bool, width: u16, height: u16, kind: LayerKind) {
    let painted = painted_rows(tier, truecolor, width, height, kind.clone());
    let rastered = rastered_rows(tier, width, height, kind);

    // A silent upstream regression (an empty Surface, a layer that never
    // reached the compositor) would make both painters agree on all-blank
    // rows and the assert_eq below would pass without ever exercising
    // paint_span_row's per-span walk -- anchor that real, non-blank
    // content actually reached the painter before trusting parity on it.
    assert!(
        painted.iter().any(|row| !row.trim().is_empty()),
        "painted_rows produced no non-blank content for tier {tier:?}; the parity check below would be vacuous"
    );

    assert_eq!(
        painted, rastered,
        "view-tui and view-oracle disagree on tier {tier:?} truecolor={truecolor}"
    );
}

#[test]
fn statusline_spans_paint_identically_in_both_painters() {
    for (tier, truecolor) in [
        (Tier::Full, true),
        (Tier::Standard, true),
        (Tier::Basic, false),
    ] {
        assert_parity(tier, truecolor, 46, 1, spanful_statusline());
    }
}

#[test]
fn plain_span_overlays_paint_identically_in_both_painters() {
    for (tier, truecolor) in [
        (Tier::Full, true),
        (Tier::Standard, true),
        (Tier::Basic, false),
    ] {
        assert_parity(tier, truecolor, 24, 6, plain_picker());
    }
}
