//! Moves frames between the shadow and the session DVR's ring: the frame
//! just committed goes in as the cells it changed, and a recorded frame
//! comes back as the shadow's next frame to emit.

use ratatui::buffer::Cell;
use ratatui::layout::Rect;
use view_core::model::Model;
use view_core::theme::{ChromeGroup, Theme};

use super::{paint_text_row, ratatui_style, Damage, Shadow};
use crate::dvr::{place, FrameRing};

/// Records the frame [`Shadow::commit`] just promoted into `into` and
/// returns its seq.
///
/// The rows the frame repainted are the only rows where `front` and `back`
/// can differ, so a delta compares those rows alone and clones a cell only
/// where it changed. A frame that repainted every row, changed size, or
/// changed more cells than the open group has room for is a keyframe.
pub(crate) fn capture(
    shadow: &Shadow,
    into: &mut FrameRing,
    at_us: u64,
    cursor: Option<(u16, u16)>,
) -> u64 {
    let area = shadow.front.area;
    let size = (area.width, area.height);
    let width = usize::from(area.width);
    if !shadow.painted.full {
        if let Some(group) = into.open_delta(size) {
            let mut fits = true;
            'rows: for (y, row) in (area.y..area.bottom()).enumerate() {
                if !shadow.painted.covers(row) {
                    continue;
                }
                let start = y * width;
                let (Some(now), Some(was)) = (
                    shadow.front.content.get(start..start + width),
                    shadow.back.content.get(start..start + width),
                ) else {
                    continue;
                };
                for (x, (now, was)) in (area.x..).zip(now.iter().zip(was)) {
                    if now != was && !group.push_cell(x, row, now) {
                        fits = false;
                        break 'rows;
                    }
                }
            }
            if fits {
                return into.close_delta(at_us, cursor);
            }
            group.abort();
        }
    }
    into.push_key(at_us, size, cursor, shadow.front.content.iter().cloned())
}

/// Writes frame `seq` of `ring` into the shadow's `back` and marks every
/// row painted, so the next emission repaints the whole screen from it. A
/// cell outside the current screen is dropped. Returns false, leaving the
/// shadow untouched, when the ring no longer holds the frame.
pub(crate) fn load(shadow: &mut Shadow, ring: &FrameRing, seq: u64) -> bool {
    let Some((group, index)) = ring.locate(seq) else {
        return false;
    };
    let back = &mut shadow.back;
    back.reset();
    let area = (back.area.width, back.area.height);
    group.replay(index, |x, y, cell| {
        place(&mut back.content, area, x, y, cell)
    });
    shadow.painted = Damage::full();
    true
}

/// Paints `text` across the last row of the shadow's `back` in the status
/// line's colors.
pub(crate) fn paint_bar(shadow: &mut Shadow, model: &Model, text: &str) {
    let area = shadow.back.area;
    let Some(y) = area.bottom().checked_sub(1).filter(|_| area.width > 0) else {
        return;
    };
    let style =
        ratatui_style(Theme::from_hl(model.engine.painted_hl()).chrome(ChromeGroup::StatusLine));
    let row = Rect::new(area.x, y, area.width, 1);
    for x in area.x..area.right() {
        if let Some(cell) = shadow.back.cell_mut((x, y)) {
            *cell = Cell::EMPTY;
        }
    }
    shadow.back.set_style(row, style);
    paint_text_row(text, style, row, 0, &mut shadow.back);
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use view_core::grid::GridOp;

    use super::*;

    const W: u16 = 20;
    const H: u16 = 6;

    fn model() -> Model {
        let mut model = Model::with_term_size(W, H);
        model.engine.apply_grid(GridOp::Resize {
            width: W,
            height: H,
        });
        model
    }

    fn put(model: &mut Model, row: u16, text: &str) {
        model.engine.apply_grid(GridOp::PutLine {
            row,
            col_start: 0,
            cells: vec![(text.into(), 0, 1)],
        });
    }

    /// Composes, emits and commits one frame the way the terminal does.
    fn paint(shadow: &mut Shadow, model: &Model, damage: &Damage) {
        let area = Rect::new(0, 0, W, H);
        shadow.resize(area);
        let surface = view_surface::render(model);
        let mut rows = shadow.overlay_damage(&surface);
        shadow.native_pane_damage(model, &surface, area, &mut rows);
        shadow.compose(model, &surface, damage);
        shadow.emit_updates(&mut Vec::new()).unwrap();
        shadow.commit();
    }

    fn rows(rows: &[u16]) -> Damage {
        Damage {
            full: false,
            rows: rows.to_vec(),
        }
    }

    #[test]
    fn a_frame_reconstructs_cell_for_cell_from_its_keyframe_and_deltas() {
        let mut model = model();
        let mut shadow = Shadow::new();
        let mut ring = FrameRing::new(64 << 20);
        put(&mut model, 0, "a");
        paint(&mut shadow, &model, &Damage::full());
        // a frame repaints the rows the frame before it did as well, so the
        // first frame after a full one is still a whole-screen repaint
        paint(&mut shadow, &model, &rows(&[]));
        assert_eq!(capture(&shadow, &mut ring, 0, None), 1);
        put(&mut model, 1, "b");
        paint(&mut shadow, &model, &rows(&[1]));
        let second = shadow.front.clone();
        assert_eq!(capture(&shadow, &mut ring, 10, Some((0, 1))), 2);
        put(&mut model, 0, "c");
        paint(&mut shadow, &model, &rows(&[0]));
        let third = shadow.front.clone();
        assert_eq!(capture(&shadow, &mut ring, 20, None), 3);
        assert_ne!(second, third);
        let deltas = ring.snapshot();
        let keys: Vec<_> = deltas.frames().map(|f| (f.seq, f.key)).collect();
        assert_eq!(keys, [(1, true), (2, false), (3, false)]);

        assert!(load(&mut shadow, &ring, 2));
        assert_eq!(shadow.back, second);
        assert!(shadow.painted.full);
        assert!(load(&mut shadow, &ring, 3));
        assert_eq!(shadow.back, third);
        assert!(!load(&mut shadow, &ring, 4));
    }

    #[test]
    fn paint_bar_writes_the_last_row_only() {
        let model = model();
        let mut shadow = Shadow::new();
        paint(&mut shadow, &model, &Damage::full());
        let before = shadow.back.clone();
        paint_bar(&mut shadow, &model, "DVR");
        let last = usize::from(H - 1) * usize::from(W);
        assert_eq!(shadow.back.content[last].symbol(), "D");
        assert_eq!(shadow.back.content[..last], before.content[..last]);
    }
}
