//! A colorscheme nvim cannot find changes no painted cell but the notice's.
//!
//! `[ui] theme` names a scheme, nvim fails to load it, and the session keeps
//! the chrome it derived from whatever colorscheme the user's own config
//! ended on. That claim is about the *frame*, and this crate's incident
//! history is frames going stale while the model behind them was right: the
//! border charset keyed on `caps.tier`, and `cache::Inputs` holding a
//! projection of `Messages` rather than the struct. Both were correct model
//! state painting wrong. Asserting the derived `Theme` is unchanged proves
//! the input to the chrome, not the chrome.
//!
//! So the comparison is made where a user would see the difference: two
//! sessions identical but for `Msg::ColorSchemeMissing`, both composited
//! through `view_surface::render` and `view-tui`'s real painter, compared
//! cell by cell on symbol *and* style. Every cell outside the notice's own
//! rect must be byte-identical -- a chrome that silently re-derived from a
//! view-side palette, a grid that shifted under the notice, or a border
//! charset that moved would each part a cell here.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use ratatui::buffer::Buffer;
use ratatui::layout::Rect as TuiRect;
use view_core::events::UiEvent;
use view_core::grid::GridOp;
use view_core::model::Model;
use view_core::msg::Msg;
use view_core::native::statusline::SegmentUpdate;
use view_core::update::update;
use view_surface::{LayerKind, Rect, Surface};
use view_tui::paint::{composite_into, Damage};

/// The scheme the fixture names and nvim cannot find.
const SCHEME: &str = "nonexistent-scheme";

const WIDTH: u16 = 80;
const HEIGHT: u16 = 24;

/// How far down the fixture's buffer text starts: a notice box of this
/// message's length claims the top rows, and the assertion excludes exactly
/// the rect it claims.
const NOTICE_ROWS: u16 = 3;

/// A session already painting real chrome off a real highlight table: a
/// grid with themed cells, a statusline bar with several `StyleRole`
/// segments, and the builtin groups a colorscheme actually broadcasts. A
/// fixture on the all-default table would compare blank cells against blank
/// cells and could not observe a chrome change at all.
fn session() -> Model {
    let mut model = Model::with_term_size(WIDTH, HEIGHT);
    model.statusline_enabled = true;
    model.engine.apply_grid(GridOp::Resize {
        width: WIDTH,
        height: HEIGHT - 1,
    });
    // below the rows a notice box claims, so the themed buffer text is
    // inside the compared region rather than under the one rect the
    // assertion excludes
    model.engine.apply_grid(GridOp::PutLine {
        row: NOTICE_ROWS,
        col_start: 0,
        cells: vec![
            ("f".into(), 1, 1),
            ("n".into(), 1, 1),
            (" ".into(), 0, 1),
            ("m".into(), 0, 1),
            ("a".into(), 0, 1),
            ("i".into(), 0, 1),
            ("n".into(), 0, 1),
        ],
    });
    let _ = update(
        &mut model,
        Msg::Redraw(vec![
            UiEvent::DefaultColorsSet {
                fg: Some(0x00eb_dbb2),
                bg: Some(0x0028_2828),
                sp: None,
            },
            UiEvent::HlAttrDefine {
                id: 1,
                fg: Some(0x00fa_bd2f),
                bg: None,
                bold: true,
                italic: false,
                underline: false,
                reverse: false,
            },
            UiEvent::HlAttrDefine {
                id: 2,
                fg: Some(0x0028_2828),
                bg: Some(0x00a8_9984),
                bold: false,
                italic: false,
                underline: false,
                reverse: false,
            },
            UiEvent::HlGroupSet {
                name: "StatusLine".to_string(),
                hl_id: 2,
            },
            UiEvent::HlGroupSet {
                name: "ModeMsg".to_string(),
                hl_id: 1,
            },
            UiEvent::HlGroupSet {
                name: "MsgArea".to_string(),
                hl_id: 1,
            },
        ]),
    );
    // the probe reply the derivation trusts: without it `Theme::from_hl`
    // holds the ambiguous wire background back and the fixture paints an
    // unset one
    let generation = model.engine.hl().probe_generation();
    let _ = update(
        &mut model,
        Msg::HlProbeReply {
            generation,
            fg: Some(0x00eb_dbb2),
            bg: Some(0x0028_2828),
        },
    );
    for segment in [
        SegmentUpdate::Mode("NORMAL".to_string()),
        SegmentUpdate::Buffer {
            name: "src/main.rs".to_string(),
            modified: true,
        },
        SegmentUpdate::Ruler("12:4".to_string()),
    ] {
        model.engine.statusline.apply(segment);
    }
    model
}

/// The same session after nvim refused the scheme `[ui] theme` named.
fn refused() -> Model {
    let mut model = session();
    model.colorscheme = Some(SCHEME.to_string());
    let _ = update(
        &mut model,
        Msg::ColorSchemeMissing {
            name: SCHEME.to_string(),
        },
    );
    model
}

/// Composites `model` through the real painter and hands back both the
/// surface (for the notice's own rect) and the frame.
fn paint(model: &Model) -> (Surface, Buffer) {
    let surface = view_surface::render(model);
    let mut buf = Buffer::empty(TuiRect::new(0, 0, WIDTH, HEIGHT));
    composite_into(&mut buf, model, &surface, &Damage::full());
    (surface, buf)
}

fn notice_rects(surface: &Surface) -> Vec<Rect> {
    surface
        .layers
        .iter()
        .filter(|layer| matches!(layer.kind, LayerKind::Toast { .. }))
        .map(|layer| layer.rect)
        .collect()
}

fn covers(rect: Rect, col: u16, row: u16) -> bool {
    (rect.col..rect.col.saturating_add(rect.width)).contains(&col)
        && (rect.row..rect.row.saturating_add(rect.height)).contains(&row)
}

fn text_of(buf: &Buffer, rect: Rect) -> String {
    (rect.row..rect.row.saturating_add(rect.height))
        .flat_map(|row| {
            (rect.col..rect.col.saturating_add(rect.width))
                .map(move |col| buf[(col, row)].symbol().to_string())
        })
        .collect()
}

#[test]
fn a_missing_colorscheme_repaints_no_cell_outside_its_own_notice() {
    let (auto_surface, auto_frame) = paint(&session());
    let (refused_surface, refused_frame) = paint(&refused());

    assert!(
        notice_rects(&auto_surface).is_empty(),
        "the derived run must carry no notice for the comparison below to mean anything"
    );
    let rects = notice_rects(&refused_surface);
    assert_eq!(rects.len(), 1, "exactly one notice box, got {rects:?}");
    assert!(
        !covers(rects[0], 0, NOTICE_ROWS),
        "the notice box {:?} grew over the fixture's own buffer text, which would leave the \
         themed grid cells outside the compared region",
        rects[0]
    );
    let notice = text_of(&refused_frame, rects[0]);
    for fact in [SCHEME, "[ui] theme"] {
        assert!(
            notice.contains(fact),
            "{fact} is missing from the painted notice {notice:?}"
        );
    }

    let mut compared = 0usize;
    for row in 0..HEIGHT {
        for col in 0..WIDTH {
            if rects.iter().any(|rect| covers(*rect, col, row)) {
                continue;
            }
            let (auto, refused) = (&auto_frame[(col, row)], &refused_frame[(col, row)]);
            assert_eq!(
                (auto.symbol(), auto.style()),
                (refused.symbol(), refused.style()),
                "cell ({col},{row}) parts: a scheme nvim never loaded moved no highlight, so \
                 every cell the notice does not cover is the one a session that named nothing \
                 painted"
            );
            if auto.symbol() != " " || auto.style() != ratatui::style::Style::default() {
                compared += 1;
            }
        }
    }
    assert!(
        compared > usize::from(WIDTH),
        "only {compared} cells outside the notice carried any glyph or style; the fixture is \
         painting blanks and the comparison above is vacuous"
    );
}
