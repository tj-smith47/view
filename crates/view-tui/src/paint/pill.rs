//! The top pill: the row above everything else, naming what is open.
//!
//! Placement is [`PillView::row_slots`]'s, the same answer the mouse router
//! spends, so the name a click selects is the name that was drawn under
//! the pointer.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Style;
use view_core::native::pill::{edge_cells, PillView};
use view_core::theme::{ChromeGroup, Theme};

use super::text::{cluster_width, clusters, set_cluster};
use super::{ratatui_style, rgb};

/// Draws the pill across `area`, which is the terminal's own top row.
///
/// The row is filled in `TabLineFill` first, so every column the names do
/// not reach carries the group nvim names for exactly that: the row behind
/// the tabs.
///
/// Laid out on [`PillView::width`], the terminal's own width, and written
/// only into the cells `area` holds. An `area` the compositor clipped
/// narrower -- the one frame between a resize reaching the model and
/// reaching the backend -- loses the columns off its right edge and moves
/// none of the others, so a name a click reaches is the name that was
/// drawn under the pointer.
pub(super) fn paint_pill(pill: &PillView, theme: &Theme, area: Rect, buf: &mut Buffer) {
    let fill = ratatui_style(theme.chrome(ChromeGroup::TabLineFill));
    fill_run(buf, area, 0, area.width, fill);
    // the accent over the row's own background, whatever `TabLineFill`'s
    // foreground is: the host is the one thing here that says which machine
    // the session is on, and a colorscheme that dims the tab row would
    // take it down with the rest
    let edge = theme.accent().fg.map_or(fill, |fg| fill.fg(rgb(fg)));
    write_at(buf, area, 1, &pill.host, edge);
    let agent = pill.width.saturating_sub(edge_cells(pill.agent));
    write_at(buf, area, agent.saturating_add(1), pill.agent, edge);

    let tab = ratatui_style(theme.chrome(ChromeGroup::TabLine));
    let selected = ratatui_style(theme.chrome(ChromeGroup::TabLineSel));
    for slot in pill.row_slots() {
        // by the index the slot names, never by the loop's own count: a
        // list longer than the row is a window into it, and its first slot
        // is not its first entry
        let Some(entry) = pill.entries.get(slot.entry) else {
            continue;
        };
        let style = if slot.current { selected } else { tab };
        fill_run(buf, area, slot.col, slot.cells, style);
        write_at(buf, area, slot.col.saturating_add(1), &entry.label, style);
    }
}

/// Paints `cells` blank columns from `col`, which is what gives a selected
/// name its own background either side of the text.
///
/// `ratatui::buffer::Cell::reset` leaves a blank behind, so this writes no
/// symbol of its own and the names are the only text the row carries.
fn fill_run(buf: &mut Buffer, area: Rect, col: u16, cells: u16, style: Style) {
    for at in col..col.saturating_add(cells).min(area.width) {
        let cell = &mut buf[(area.x.saturating_add(at), area.y)];
        cell.reset();
        cell.set_style(style);
    }
}

/// Writes `text` from column `col`, one grapheme cluster per cell,
/// stopping at the row's own end.
fn write_at(buf: &mut Buffer, area: Rect, col: u16, text: &str, style: Style) {
    let mut at = col;
    for cluster in clusters(text) {
        let width = cluster_width(cluster);
        if at.saturating_add(width) > area.width {
            return;
        }
        set_cluster(buf, area.x.saturating_add(at), area.y, cluster, style);
        // ratatui's own convention for the cell a two-cell glyph covers:
        // the diff skips it, and anything left in it would be drawn one
        // column right of where it was written
        if width == 2 {
            buf[(area.x.saturating_add(at).saturating_add(1), area.y)].reset();
        }
        at = at.saturating_add(width);
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use ratatui::backend::TestBackend;
    use ratatui::Terminal;
    use view_core::model::Model;

    use super::{paint_pill, PillView};
    use crate::paint::{ratatui_style, rgb, ChromeGroup, Theme};

    /// The accent this fixture names, which is what the host cell has to
    /// carry over the row's own foreground.
    const ACCENT: u32 = 0x44_44_44;

    /// A session on a remote host with two tabpages on a terminal `width`
    /// cells wide, its three pill groups three different colours so a cell
    /// says which one painted it.
    ///
    /// The terminal's width is the row's own layout width, so a fixture
    /// that left it at zero would place every name nowhere.
    fn two_tabs(width: u16) -> Model {
        let mut model = Model::with_term_size(width, 24).with_remote(Some("prod".to_string()));
        model.engine.set_accent_token(Some(ACCENT));
        for (id, group, fg) in [
            (1_u64, ChromeGroup::TabLine, 0x11_11_11_u32),
            (2, ChromeGroup::TabLineSel, 0x22_22_22),
            (3, ChromeGroup::TabLineFill, 0x33_33_33),
        ] {
            for event in [
                view_core::events::UiEvent::HlAttrDefine {
                    id,
                    fg: Some(fg),
                    bg: None,
                    bold: false,
                    italic: false,
                    underline: false,
                    reverse: false,
                },
                view_core::events::UiEvent::HlGroupSet {
                    name: group.hl_name().to_string(),
                    hl_id: id,
                },
            ] {
                let _ =
                    view_core::update::update(&mut model, view_core::msg::Msg::Redraw(vec![event]));
            }
        }
        let _ = view_core::update::update(
            &mut model,
            view_core::msg::Msg::Redraw(vec![view_core::events::UiEvent::TablineUpdate {
                current: view_core::events::TabHandle(2),
                tabs: vec![
                    view_core::events::TabEntry {
                        tab: view_core::events::TabHandle(1),
                        name: "one".to_string(),
                    },
                    view_core::events::TabEntry {
                        tab: view_core::events::TabHandle(2),
                        name: "two".to_string(),
                    },
                ],
            }]),
        );
        model
    }

    /// Every `.rs` file under `path`, however deep.
    fn sources(path: &std::path::Path, found: &mut Vec<std::path::PathBuf>) {
        let Ok(entries) = std::fs::read_dir(path) else {
            return;
        };
        for entry in entries.flatten() {
            let at = entry.path();
            if at.is_dir() {
                sources(&at, found);
            } else if at.extension().is_some_and(|ext| ext == "rs") {
                found.push(at);
            }
        }
    }

    /// One painter for the top row, under either look. A second one drew
    /// the same names left-aligned from column 0 while the mouse router
    /// hit-tested the centred layout, so a click on that row selected a
    /// tabpage the user was not pointing at.
    #[test]
    fn no_second_painter_for_the_top_row_remains() {
        // spelled in halves so this pin is not its own match
        let gone = concat!("paint_", "tabline");
        let crates = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("every crate sits inside the crates directory");
        let mut files = Vec::new();
        sources(crates, &mut files);
        let carriers: Vec<_> = files
            .iter()
            .filter(|at| std::fs::read_to_string(at).is_ok_and(|text| text.contains(gone)))
            .collect();
        assert!(
            carriers.is_empty(),
            "a second painter for the top row is back in {carriers:?}"
        );
    }

    /// A row too narrow for every name is a window into the list, so its
    /// first run is not the first entry. Painting along the list instead
    /// of by the index each run names put a neighbour's name under every
    /// one of them.
    #[test]
    fn a_row_too_narrow_for_the_list_paints_the_names_its_slots_name() {
        // room for two of the four names, so the window holds the current
        // one and the one before it
        let width = 20;
        let mut model = two_tabs(width);
        model.ai_enabled = false;
        let _ = view_core::update::update(
            &mut model,
            view_core::msg::Msg::Redraw(vec![view_core::events::UiEvent::TablineUpdate {
                current: view_core::events::TabHandle(4),
                tabs: (1..=4)
                    .map(|at| view_core::events::TabEntry {
                        tab: view_core::events::TabHandle(at),
                        name: format!("name{at}"),
                    })
                    .collect(),
            }]),
        );
        let theme = Theme::from_hl(model.engine.hl());
        let pill = PillView::from_model(&model);
        let slots = pill.row_slots();
        assert_eq!(slots.len(), 2, "the fixture drew {} names", slots.len());
        let mut terminal = Terminal::new(TestBackend::new(width, 1)).unwrap();
        terminal
            .draw(|frame| paint_pill(&pill, &theme, frame.area(), frame.buffer_mut()))
            .unwrap();
        let buf = terminal.backend().buffer().clone();
        let row: String = (0..width).map(|col| buf[(col, 0)].symbol()).collect();
        assert_eq!(
            row.trim(),
            "prod  name3  name4",
            "the row reads {row:?} where its slots name entries 2 and 3"
        );
    }

    /// The row the mouse router hit-tests is the row the painter draws,
    /// on the terminal's own width. A painter that laid the names out on
    /// the area handed to it re-centred them for the one frame between a
    /// resize reaching the model and reaching the backend, and a click in
    /// that frame landed on a name drawn at another column.
    #[test]
    fn a_clipped_row_paints_the_columns_the_router_would_hit() {
        let model = two_tabs(40);
        let theme = Theme::from_hl(model.engine.hl());
        let pill = PillView::from_model(&model);
        let row = |area: u16| {
            let mut terminal = Terminal::new(TestBackend::new(area, 1)).unwrap();
            terminal
                .draw(|frame| paint_pill(&pill, &theme, frame.area(), frame.buffer_mut()))
                .unwrap();
            let buf = terminal.backend().buffer().clone();
            (0..area)
                .map(|col| buf[(col, 0)].symbol().to_string())
                .collect::<Vec<_>>()
        };

        let clipped = 24;
        assert_eq!(
            row(clipped),
            row(40)[..usize::from(clipped)],
            "a row clipped to {clipped} columns re-flowed the names"
        );
        // the router's own reading of the same view: every column a slot
        // covers answers with that slot's id
        for slot in pill.row_slots() {
            for col in slot.col..slot.col.saturating_add(slot.cells) {
                assert_eq!(
                    pill.hit(col),
                    Some(slot.id),
                    "column {col} was painted for tab {} and hit-tests as something else",
                    slot.id
                );
            }
        }
    }

    /// The bool on the slot is not the colour on the screen: only this
    /// composite says the current name is the one lit, and the goldens
    /// carry text without styling.
    #[test]
    fn the_current_name_is_the_one_the_selected_group_paints() {
        let model = two_tabs(40);
        let theme = Theme::from_hl(model.engine.hl());
        let pill = PillView::from_model(&model);
        let slots = pill.row_slots();
        assert_eq!(slots.len(), 2, "the fixture drew {} names", slots.len());
        let mut terminal = Terminal::new(TestBackend::new(40, 1)).unwrap();
        terminal
            .draw(|frame| paint_pill(&pill, &theme, frame.area(), frame.buffer_mut()))
            .unwrap();
        let buf = terminal.backend().buffer().clone();
        let fg_at = |col: u16| buf[(col, 0)].style().fg;

        let plain = ratatui_style(theme.chrome(ChromeGroup::TabLine)).fg;
        let lit = ratatui_style(theme.chrome(ChromeGroup::TabLineSel)).fg;
        let fill = ratatui_style(theme.chrome(ChromeGroup::TabLineFill)).fg;
        assert_ne!(plain, lit, "the fixture gave the two groups one colour");
        for col in slots[0].col..slots[0].col + slots[0].cells {
            assert_eq!(fg_at(col), plain, "column {col} of the first name");
        }
        for col in slots[1].col..slots[1].col + slots[1].cells {
            assert_eq!(fg_at(col), lit, "column {col} of the current name");
        }
        assert_eq!(
            fg_at(1),
            Some(rgb(ACCENT)),
            "the host carries the accent rather than the row's own foreground"
        );
        assert_eq!(
            fg_at(slots[0].col - 1),
            fill,
            "the gap before the first name is not the row behind the tabs"
        );
        assert_eq!(
            (buf[(1, 0)].symbol(), buf[(slots[1].col + 1, 0)].symbol()),
            ("p", "t"),
            "the host and the current name are not where the slots put them"
        );
    }
}
